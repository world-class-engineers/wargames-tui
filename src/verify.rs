//! Non-blocking-friendly secret verification. Runs in a worker thread.

use crate::credentials::Secret;
use crate::ssh;
use crate::wargame::Wargame;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(20);

/// Shared cancellation flag for one verification attempt. The owner can cancel
/// when the user switches games; the worker signals `Verified` as usual so the
/// existing id/game staleness checks discard the result.
#[derive(Debug, Clone, Default)]
pub struct VerifyCancel {
    cancelled: Arc<AtomicBool>,
}

impl VerifyCancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
const STDERR_DRAIN_LIMIT: u64 = 8192;

/// Verify a secret by attempting a single, non-interactive SSH command against
/// the next level. Always reaps the child, including on timeout or cancellation;
/// its process group is signalled only while the child is still unreaped.
/// Passwords work with `sshpass` when installed and otherwise through the
/// OpenSSH `SSH_ASKPASS` helper created in `config_dir`.
pub fn verify_secret(
    game: &Wargame,
    level: u16,
    secret: &Secret,
    timeout: Duration,
    known_hosts: &Path,
    config_dir: &Path,
    cancel: &VerifyCancel,
) -> Result<(), String> {
    ssh::ensure_known_hosts(known_hosts)
        .map_err(|error| format!("could not prepare known_hosts: {error:#}"))?;
    let command = ssh::verification_command(game, level, secret, known_hosts, config_dir)
        .map_err(|error| format!("could not prepare password verification: {error:#}"))?;
    let result = verify_command(&game.level_label(level), command, timeout, cancel);
    // Defensive: SSH should never echo a password, but a misbehaving helper or
    // host must not leak the secret into the status box via stderr.
    match (result, secret) {
        (Err(error), Secret::Password(password)) => Err(redact(&error, password)),
        (result, _) => result,
    }
}

fn redact(message: &str, secret: &str) -> String {
    if secret.is_empty() {
        return message.to_owned();
    }
    message.replace(secret, "[redacted]")
}

/// Run a prepared command with process-group teardown and a bounded stderr
/// drain. Kept separate from `verify_secret` so tests can use local stubs.
fn verify_command(
    level_label: &str,
    mut command: Command,
    timeout: Duration,
    cancel: &VerifyCancel,
) -> Result<(), String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        // The verifier and all of its descendants share a private process
        // group; killing the group cannot leave a grandchild behind.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start SSH verifier: {error}"))?;
    let pid = child.id();

    // Drain stderr on a helper thread. The channel read below is bounded, so a
    // grandchild holding the pipe cannot hang the worker forever.
    let (stderr_tx, stderr_rx) = mpsc::channel::<String>();
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            let mut text = String::new();
            // A grandchild holding the pipe must not be able to grow this
            // buffer without bound, even before the main thread times out.
            let _ = stderr.take(STDERR_DRAIN_LIMIT).read_to_string(&mut text);
            let _ = stderr_tx.send(text);
        });
    }

    let deadline = Instant::now() + timeout;
    loop {
        if cancel.is_cancelled() {
            // The direct child is still unreaped here, so its pid (and thus
            // the process-group id) cannot have been recycled yet.
            kill_group(pid, true);
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("verification for {level_label} was cancelled"));
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // `try_wait` reaped the child. Signalling -pid now could hit a
                // recycled process group, so skip the group signal: SSH's own
                // descendants exit with the direct child on the success/failure
                // path. Timeout and cancellation signal the group before reaping.
                if status.success() {
                    return Ok(());
                }
                let detail = read_detail(&stderr_rx);
                return Err(format!(
                    "authentication/connection failed for {level_label} (ssh exit {status}){}",
                    detail
                        .map(|text| format!(": {}", truncate(&text, 400)))
                        .unwrap_or_default()
                ));
            }
            Ok(None) if Instant::now() >= deadline => {
                kill_group(pid, false);
                thread::sleep(Duration::from_millis(50));
                kill_group(pid, true);
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "verification timed out after {}s; the SSH process group was terminated",
                    timeout.as_secs()
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                kill_group(pid, true);
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not poll SSH verifier: {error}"));
            }
        }
    }
}

#[cfg(unix)]
fn kill_group(pid: u32, force: bool) {
    if pid > 1 {
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        unsafe {
            libc::kill(-(pid as libc::pid_t), signal);
        }
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: u32, _force: bool) {}

fn read_detail(stderr: &Receiver<String>) -> Option<String> {
    let text = stderr.recv_timeout(STDERR_DRAIN_TIMEOUT).ok()?;
    let text = text.trim().to_owned();
    if text.is_empty() { None } else { Some(text) }
}

fn truncate(text: &str, limit: usize) -> String {
    let mut shortened: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        shortened.push('…');
    }
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn truncates_long_output() {
        let long = "x".repeat(1000);
        let short = truncate(&long, 10);
        assert_eq!(short.chars().count(), 11);
        assert_eq!(truncate("tiny", 10), "tiny");
    }

    #[test]
    fn redaction_removes_secret_from_error_details() {
        let secret = "test-only-placeholder";
        let error = format!("remote said {secret} was wrong");
        let cleaned = redact(&error, secret);
        assert!(!cleaned.contains(secret));
        assert!(cleaned.contains("[redacted]"));
        assert_eq!(redact("no secret here", ""), "no secret here");
    }

    #[cfg(unix)]
    #[test]
    fn stubbed_verifier_reports_stderr_on_failure() {
        let result = verify_command(
            "bandit1",
            stub("printf 'stub authentication failed\\n' >&2; exit 1"),
            Duration::from_secs(3),
            &VerifyCancel::new(),
        );
        let error = result.unwrap_err();
        assert!(error.contains("stub authentication failed"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn stubbed_verifier_accepts_success() {
        verify_command(
            "bandit1",
            stub("exit 0"),
            Duration::from_secs(3),
            &VerifyCancel::new(),
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_tears_down_stubbed_verifier() {
        let cancel = VerifyCancel::new();
        let cancellation = cancel.clone();
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            cancellation.cancel();
        });
        let started = Instant::now();
        let error = verify_command(
            "bandit1",
            stub("sleep 30 & wait"),
            Duration::from_secs(10),
            &cancel,
        )
        .unwrap_err();
        stopper.join().unwrap();
        assert!(error.contains("cancelled"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "cancelled verifier blocked for {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn stubbed_verifier_timeout_is_bounded() {
        let started = Instant::now();
        let result = verify_command(
            "bandit1",
            stub("sleep 30 & sleep 30 & wait"),
            Duration::from_millis(300),
            &VerifyCancel::new(),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("timed out"));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "verifier teardown blocked for {:?}",
            started.elapsed()
        );
    }
}
