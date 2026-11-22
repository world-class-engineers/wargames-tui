//! Real PTY-backed SSH session with a vt100 screen buffer.

use anyhow::{Context, Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc::Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use vt100::Parser;

/// Never spend more than this joining the reader thread during teardown. If it
/// is still blocked (for example, a grandchild kept the PTY slave open), the
/// handle is detached rather than blocking the UI thread.
const READER_JOIN_TIMEOUT: Duration = Duration::from_millis(500);
const CHILD_EXIT_GRACE: Duration = Duration::from_millis(150);

pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    /// Shared with the reader thread so the one-time password injection uses
    /// the same PTY writer as keyboard input.
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    child: Box<dyn Child + Send + Sync>,
    parser: Arc<Mutex<Parser>>,
    reader_handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    #[cfg(unix)]
    wake_writer: Option<std::fs::File>,
}

impl PtySession {
    /// Spawn a prepared `ssh` command in a new PTY and start the output reader
    /// thread. `auto_password` is supplied only for the plain-`ssh` fallback,
    /// when the reader must inject a saved password at the remote prompt.
    /// `make_exit` maps the session generation into the caller's message type.
    pub fn spawn<T: Send + 'static>(
        level_label: &str,
        command: CommandBuilder,
        auto_password: Option<String>,
        size: PtySize,
        generation: u64,
        exit_tx: Sender<T>,
        make_exit: impl FnOnce(u64) -> T + Send + 'static,
    ) -> Result<Self> {
        Self::spawn_command(
            command,
            auto_password,
            size,
            generation,
            exit_tx,
            make_exit,
            format!("spawning SSH for {level_label}"),
        )
    }

    fn spawn_command<T: Send + 'static>(
        command: CommandBuilder,
        mut auto_password: Option<String>,
        size: PtySize,
        generation: u64,
        exit_tx: Sender<T>,
        make_exit: impl FnOnce(u64) -> T + Send + 'static,
        spawn_context: String,
    ) -> Result<Self> {
        let rows = NonZeroU16::new(size.rows.max(1)).expect("max(1) is nonzero");
        let columns = NonZeroU16::new(size.cols.max(1)).expect("max(1) is nonzero");
        let system = native_pty_system();
        let pair = system.openpty(size).context("opening PTY")?;

        #[cfg(unix)]
        let (wake_reader, wake_writer) = wake_pipe().context("creating PTY wake pipe")?;
        #[cfg(unix)]
        let master_fd = pair.master.as_raw_fd(); // Option<RawFd>

        let child = pair.slave.spawn_command(command).context(spawn_context)?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("cloning PTY reader")?;
        let writer = Arc::new(Mutex::new(Some(
            pair.master.take_writer().context("taking PTY writer")?,
        )));
        let parser = Arc::new(Mutex::new(Parser::new(rows, columns, 2000)));
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        let reader_writer = Arc::clone(&writer);

        let reader_parser = Arc::clone(&parser);
        let reader_handle = thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            let mut prompt_carry = Vec::new();
            loop {
                if reader_stop.load(Ordering::Relaxed) {
                    break;
                }
                #[cfg(unix)]
                {
                    // Poll the master together with a wake pipe: teardown can
                    // unblock the reader immediately instead of waiting for the
                    // slave to be closed by an exiting grandchild.
                    use std::os::fd::AsRawFd;
                    let poll_result = poll_pty(master_fd, wake_reader.as_raw_fd());
                    if poll_result == PollResult::Stop {
                        break;
                    }
                    if poll_result == PollResult::Wait {
                        continue;
                    }
                }
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if let Ok(mut parser) = reader_parser.lock() {
                            parser.process(&buffer[..count]);
                        }
                        // Injection belongs to this reader thread (and thus
                        // this generation). The stop flag is set before a new
                        // session is spawned, so a stale reader cannot type
                        // into a replacement session.
                        if !reader_stop.load(Ordering::Relaxed)
                            && let Ok(mut guard) = reader_writer.lock()
                            && let Some(writer) = guard.as_mut()
                        {
                            let _ = maybe_inject_password(
                                &buffer[..count],
                                &mut prompt_carry,
                                &mut auto_password,
                                writer,
                            );
                        }
                    }
                }
            }
            // A deliberate teardown must not report a spurious session exit.
            if !reader_stop.load(Ordering::Relaxed) {
                let _ = exit_tx.send(make_exit(generation));
            }
        });

        Ok(Self {
            master: pair.master,
            writer,
            child,
            parser,
            reader_handle: Some(reader_handle),
            stop,
            #[cfg(unix)]
            wake_writer: Some(wake_writer),
        })
    }

    pub fn screen(&self) -> Arc<Mutex<Parser>> {
        Arc::clone(&self.parser)
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| anyhow!("PTY writer lock poisoned"))?;
        let writer = guard
            .as_mut()
            .context("PTY writer is closed (session ended)")?;
        writer.write_all(bytes).context("writing to PTY")?;
        writer.flush().context("flushing PTY")?;
        Ok(())
    }

    /// Keep both the emulator and the OS PTY in sync with the pane size.
    pub fn resize(&mut self, size: PtySize) -> Result<()> {
        if let Ok(mut parser) = self.parser.lock() {
            parser.set_size(
                NonZeroU16::new(size.rows.max(1)).expect("max(1) is nonzero"),
                NonZeroU16::new(size.cols.max(1)).expect("max(1) is nonzero"),
            );
        }
        self.master.resize(size).context("resizing PTY")?;
        Ok(())
    }

    /// Signal the whole PTY session group. portable-pty calls `setsid()` before
    /// exec, so the direct child is the session/group leader and sshpass/ssh
    /// descendants share its process group.
    #[cfg(unix)]
    fn signal_group(pid: u32, signal: libc::c_int) {
        if pid > 1 {
            unsafe {
                libc::kill(-(pid as libc::pid_t), signal);
            }
        }
    }

    #[cfg(not(unix))]
    fn signal_group(_pid: u32, _signal: i32) {}

    /// Kill and reap the child, close the writer, and join the reader thread
    /// with a deadline. Teardown never blocks the UI thread indefinitely.
    pub fn terminate(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        #[cfg(unix)]
        if let Some(mut wake) = self.wake_writer.take() {
            // The reader may already have exited (natural EOF or a previous
            // terminate), so the wake is best-effort. Taking the writer makes
            // repeated teardown idempotent and keeps EPIPE out of the TUI.
            let _ = wake.write_all(&[1]);
        }

        // Dropping our writer lets the reader observe EOF once the child exits.
        if let Ok(mut guard) = self.writer.lock() {
            guard.take();
        }

        let pid = self.child.process_id().unwrap_or(0);
        #[cfg(unix)]
        Self::signal_group(pid, libc::SIGHUP);
        #[cfg(not(unix))]
        {
            Self::signal_group(0, 0);
            let _ = self.child.kill();
        }

        let deadline = Instant::now() + CHILD_EXIT_GRACE;
        let mut reaped = false;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    reaped = true;
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(_) => break,
            }
        }

        // SIGKILL the whole group only while the direct child is unreaped:
        // after `try_wait` reaps it, its pid (and process-group id) may already
        // have been recycled for an unrelated process. SIGHUP above still
        // reaches the session's descendants.
        if !reaped {
            #[cfg(unix)]
            Self::signal_group(pid, libc::SIGKILL);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }

        if let Some(handle) = self.reader_handle.take() {
            let deadline = Instant::now() + READER_JOIN_TIMEOUT;
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
            // Otherwise the thread is detached: group SIGKILL plus the wake
            // pipe mean this is only a last-resort safety net.
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollResult {
    Wait,
    Ready,
    Stop,
}

#[cfg(unix)]
fn wake_pipe() -> std::io::Result<(std::os::fd::OwnedFd, std::fs::File)> {
    use std::os::fd::FromRawFd;

    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Close the pipe on exec so SSH descendants never hold an extra copy.
    for fd in fds {
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    let reader = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[0]) };
    let writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    Ok((reader, writer))
}

#[cfg(unix)]
fn poll_pty(master_fd: Option<i32>, wake_fd: i32) -> PollResult {
    let Some(master_fd) = master_fd else {
        // No pollable descriptor: fall back to a normal blocking read.
        return PollResult::Ready;
    };
    let mut fds = [
        libc::pollfd {
            fd: master_fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 100) };
    if result < 0 {
        return if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            PollResult::Wait
        } else {
            PollResult::Stop
        };
    }
    if fds[1].revents != 0 {
        PollResult::Stop
    } else if fds[0].revents != 0 {
        PollResult::Ready
    } else {
        PollResult::Wait
    }
}

const PASSWORD_PROMPT_WORD: &[u8] = b"password";
/// Enough bytes to recognize `password`, a few spaces, and the colon even when
/// a prompt is split across PTY reads. Bounded so output cannot grow it.
const PASSWORD_PROMPT_CARRY: usize = 24;

/// Case-insensitive `password:` detection, allowing spaces before the colon.
fn contains_password_prompt(bytes: &[u8]) -> bool {
    (0..bytes.len()).any(|start| {
        let tail = &bytes[start..];
        if tail.len() < PASSWORD_PROMPT_WORD.len()
            || !tail[..PASSWORD_PROMPT_WORD.len()].eq_ignore_ascii_case(PASSWORD_PROMPT_WORD)
        {
            return false;
        }
        let rest = &tail[PASSWORD_PROMPT_WORD.len()..];
        let rest = rest
            .iter()
            .position(|byte| !matches!(byte, b' ' | b'\t'))
            .map(|index| &rest[index..])
            .unwrap_or_default();
        rest.first() == Some(&b':')
    })
}

/// If a password prompt appears in `bytes` (including one split across reads),
/// write the pending password plus Enter exactly once. No retry loop: a second
/// prompt after a bad password is left for the user to handle.
fn maybe_inject_password(
    bytes: &[u8],
    carry: &mut Vec<u8>,
    pending: &mut Option<String>,
    writer: &mut dyn Write,
) -> bool {
    if pending.is_none() {
        return false;
    }

    let mut scan = std::mem::take(carry);
    scan.extend_from_slice(bytes);
    if !contains_password_prompt(&scan) {
        let keep = PASSWORD_PROMPT_CARRY.min(scan.len());
        *carry = scan[scan.len() - keep..].to_vec();
        return false;
    }

    let password = pending.take().expect("pending was checked above");
    let mut line = password.into_bytes();
    line.push(b'\r');
    let written = writer
        .write_all(&line)
        .and_then(|()| writer.flush())
        .is_ok();
    carry.clear();
    written
}

/// Translate a crossterm key event into terminal input bytes for the PTY.
pub fn key_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let mut bytes = Vec::new();
    if alt {
        bytes.push(0x1b);
    }

    match key.code {
        KeyCode::Char(c) => {
            if ctrl && c.is_ascii_alphabetic() {
                bytes.push((c.to_ascii_lowercase() as u8) & 0x1f);
            } else {
                let mut encoded = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut encoded).as_bytes());
            }
        }
        KeyCode::Enter => bytes.push(b'\r'),
        KeyCode::Backspace => bytes.push(0x7f),
        KeyCode::Tab => bytes.push(b'\t'),
        KeyCode::BackTab => bytes.extend_from_slice(b"\x1b[Z"),
        KeyCode::Esc => bytes.push(0x1b),
        KeyCode::Up => bytes.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => bytes.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => bytes.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => bytes.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => bytes.extend_from_slice(b"\x1b[H"),
        KeyCode::End => bytes.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => bytes.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => bytes.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => bytes.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => bytes.extend_from_slice(b"\x1b[2~"),
        KeyCode::F(number) => {
            let sequence = match number {
                1 => "\x1bOP",
                2 => "\x1bOQ",
                3 => "\x1bOR",
                4 => "\x1bOS",
                5 => "\x1b[15~",
                6 => "\x1b[17~",
                7 => "\x1b[18~",
                8 => "\x1b[19~",
                9 => "\x1b[20~",
                10 => "\x1b[21~",
                11 => "\x1b[23~",
                12 => "\x1b[24~",
                _ => return None,
            };
            bytes.extend_from_slice(sequence.as_bytes());
        }
        _ => return None,
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn pty_size() -> PtySize {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    #[test]
    fn control_and_navigation_keys() {
        assert_eq!(
            key_bytes(key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Some(vec![0x04])
        );
        assert_eq!(
            key_bytes(key(KeyCode::Char('i'), KeyModifiers::ALT)),
            Some(vec![0x1b, b'i'])
        );
        assert_eq!(
            key_bytes(key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(vec![b'\r'])
        );
        assert_eq!(
            key_bytes(key(KeyCode::Left, KeyModifiers::NONE)),
            Some(b"\x1b[D".to_vec())
        );
        assert_eq!(
            key_bytes(key(KeyCode::Backspace, KeyModifiers::NONE)),
            Some(vec![0x7f])
        );
    }

    #[test]
    fn repeat_events_are_forwarded_but_releases_are_not() {
        let mut repeat = key(KeyCode::Char('a'), KeyModifiers::NONE);
        repeat.kind = KeyEventKind::Repeat;
        assert_eq!(key_bytes(repeat), Some(vec![b'a']));
        let mut release = key(KeyCode::Char('a'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(key_bytes(release), None);
    }

    #[test]
    fn password_prompt_detection_is_case_insensitive() {
        assert!(contains_password_prompt(b"bandit0@host's password: "));
        assert!(contains_password_prompt(b"PASSWORD: "));
        assert!(contains_password_prompt(b"Password : "));
        assert!(!contains_password_prompt(b"passwords: "));
        assert!(!contains_password_prompt(b"enter your passphrase: "));
        assert!(!contains_password_prompt(b""));
    }

    #[test]
    fn injection_writes_once_and_handles_split_prompts() {
        const TEST_SECRET: &str = "test-only-placeholder";
        let mut output = Vec::new();
        let mut carry = Vec::new();
        let mut pending = Some(TEST_SECRET.to_owned());

        assert!(!maybe_inject_password(
            b"connecting to host\npass",
            &mut carry,
            &mut pending,
            &mut output,
        ));
        assert!(output.is_empty());
        assert!(maybe_inject_password(
            b"word: ",
            &mut carry,
            &mut pending,
            &mut output,
        ));
        assert_eq!(output, b"test-only-placeholder\r");
        assert!(pending.is_none());

        // A repeated prompt (wrong password) must not cause a second injection.
        assert!(!maybe_inject_password(
            b"Permission denied, please try again.\nPassword: ",
            &mut carry,
            &mut pending,
            &mut output,
        ));
        assert_eq!(output, b"test-only-placeholder\r");
        assert!(carry.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn auto_password_is_injected_into_local_stub() {
        const TEST_SECRET: &str = "test-only-placeholder";
        let script = format!(
            "printf \"bandit0@host's password: \"; read line; \
             if [ \"$line\" = '{TEST_SECRET}' ]; then printf ' LOGIN_OK\\n'; \
             else printf ' LOGIN_FAIL\\n'; fi"
        );
        let mut command = CommandBuilder::new("sh");
        command.arg("-c");
        command.arg(script);
        let (tx, _rx) = mpsc::channel();
        let mut session = PtySession::spawn_command(
            command,
            Some(TEST_SECRET.to_owned()),
            pty_size(),
            12,
            tx,
            |generation| generation,
            "spawning password-injection stub".to_owned(),
        )
        .unwrap();

        let screen = session.screen();
        let started = Instant::now();
        let mut saw_ok = false;
        while Instant::now().duration_since(started) < Duration::from_secs(3) {
            if let Ok(parser) = screen.lock()
                && parser.screen().contents().contains("LOGIN_OK")
            {
                saw_ok = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        session.terminate();
        assert!(
            saw_ok,
            "the local stub did not receive the injected password"
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminate_is_bounded_and_does_not_report_exit() {
        let mut command = CommandBuilder::new("sh");
        command.arg("-c");
        command.arg("sleep 30 & sleep 30 & wait");
        let (tx, rx) = mpsc::channel();
        let mut session = PtySession::spawn_command(
            command,
            None,
            pty_size(),
            7,
            tx,
            |generation| generation,
            "spawning stub".to_owned(),
        )
        .unwrap();
        let started = Instant::now();
        session.terminate();
        assert!(session.wake_writer.is_none());
        session.terminate();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "PTY teardown blocked for {:?}",
            started.elapsed()
        );
        assert!(rx.try_recv().is_err(), "teardown reported a spurious exit");
    }
}
