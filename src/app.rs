//! Application state and event orchestration.

use crate::credentials::{
    Credentials, MAX_LEVEL, Paths, Secret, load_credentials, save_credentials,
};
use crate::pty::PtySession;
use crate::scraper::{LevelInfo, fetch_level, page_for_level};
use crate::ssh;
use crate::ui;
use crate::verify;
use anyhow::{Context, Result};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use portable_pty::PtySize;
use ratatui::layout::Rect;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use vt100::Parser;

/// Maximum size of the secret input, measured in bytes (UTF-8).
const MAX_SECRET_BYTES: usize = 4096;
/// Lines moved per mouse-wheel tick. Keyboard PageUp/PageDown move a full page.
const WHEEL_SCROLL_LINES: u16 = 3;

/// Which way to move the vt100 viewport through scrollback history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
}

/// Move a vt100 parser's viewport by `lines` rows, returning its new offset.
///
/// `vt100` clamps the offset to the available history, so overshooting either
/// end is harmless. Keeping this a free function makes the scroll math testable
/// without a PTY or a network connection.
pub fn scroll_parser(parser: &mut Parser, direction: ScrollDirection, lines: u16) -> usize {
    let lines = usize::from(lines.max(1));
    let offset = parser.screen().scrollback();
    let next = match direction {
        ScrollDirection::Up => offset.saturating_add(lines),
        ScrollDirection::Down => offset.saturating_sub(lines),
    };
    parser.screen_mut().set_scrollback(next);
    parser.screen().scrollback()
}

/// Return a parser to the live screen (offset 0), yielding the final offset.
pub fn snap_parser_to_live(parser: &mut Parser) -> usize {
    parser.screen_mut().set_scrollback(0);
    parser.screen().scrollback()
}

/// Current scrollback offset of a parser: 0 means the live screen is in view.
pub fn parser_scrollback_offset(parser: &Parser) -> usize {
    parser.screen().scrollback()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    TerminalFocus,
    SecretInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    Password,
    PrivateKey,
}

impl SecretKind {
    pub fn label(self) -> &'static str {
        match self {
            SecretKind::Password => "password",
            SecretKind::PrivateKey => "private key path",
        }
    }
}

#[derive(Debug, Clone)]
pub enum LevelState {
    Loading,
    Ready(LevelInfo),
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyState {
    Idle,
    Verifying { id: u64, level: u16 },
}

#[derive(Debug)]
pub enum Message {
    LevelFetched {
        id: u64,
        level: u16,
        result: Result<LevelInfo, String>,
    },
    Verified {
        id: u64,
        level: u16,
        secret: Secret,
        result: Result<(), String>,
    },
    PtyExited {
        generation: u64,
    },
}

pub struct App {
    pub mode: AppMode,
    pub current_level: u16,
    pub level: LevelState,
    pub status: String,
    pub secret_input: String,
    /// Character index (not byte offset), so UTF-8 input stays on boundaries.
    pub secret_cursor: usize,
    pub secret_kind: SecretKind,
    pub verify: VerifyState,
    pub credentials: Credentials,
    pub paths: Paths,
    pub pty: Option<PtySession>,
    pub should_quit: bool,
    pub pane_size: PtySize,
    /// Full terminal geometry, used to hit-test mouse clicks against the
    /// rendered panes. Kept in sync with `Event::Resize`.
    pub terminal_area: Rect,
    /// When true the keybinding overlay owns all input until any key closes it.
    pub show_help: bool,
    tx: Sender<Message>,
    fetch_id: u64,
    next_request_id: u64,
    generation: u64,
}

impl App {
    pub fn new(
        paths: Paths,
        terminal_area: Rect,
        pane_size: PtySize,
        tx: Sender<Message>,
    ) -> Result<Self> {
        let mut startup_warning = None;
        let credentials = match load_credentials(&paths.credentials_file()) {
            Ok(credentials) => credentials,
            Err(error) => {
                startup_warning = Some(format!("Could not load credentials: {error:#}"));
                Credentials::default()
            }
        };
        let current_level = credentials.last_level.min(MAX_LEVEL);
        let mut app = Self {
            mode: AppMode::TerminalFocus,
            current_level,
            level: LevelState::Loading,
            status: startup_warning.unwrap_or_default(),
            secret_input: String::new(),
            secret_cursor: 0,
            secret_kind: SecretKind::Password,
            verify: VerifyState::Idle,
            credentials,
            paths,
            pty: None,
            should_quit: false,
            pane_size,
            terminal_area,
            show_help: false,
            tx,
            fetch_id: 0,
            next_request_id: 0,
            generation: 0,
        };
        app.spawn_pty()?;
        app.request_level(false);
        Ok(app)
    }

    pub fn messages() -> (Sender<Message>, Receiver<Message>) {
        mpsc::channel()
    }

    pub fn parser(&self) -> Option<Arc<Mutex<Parser>>> {
        self.pty.as_ref().map(|pty| pty.screen())
    }

    /// Run a closure against the live parser if its lock is available. Lock
    /// errors (poisoning) are ignored: losing a scroll action is preferable to
    /// taking down the UI.
    fn with_parser(&self, action: impl FnOnce(&mut Parser)) {
        let Some(parser) = self.parser() else {
            return;
        };
        let Ok(mut guard) = parser.lock() else {
            return;
        };
        action(&mut guard);
    }

    fn scroll_terminal(&self, direction: ScrollDirection, lines: u16) {
        self.with_parser(|parser| {
            let _ = scroll_parser(parser, direction, lines);
        });
    }

    fn page_terminal(&self, direction: ScrollDirection) {
        // A degenerate pane must still scroll by at least one line.
        self.scroll_terminal(direction, self.pane_size.rows);
    }

    fn snap_terminal_to_live(&self) {
        self.with_parser(|parser| {
            let _ = snap_parser_to_live(parser);
        });
    }

    /// Offset exposed to the UI so it can show that history is being viewed.
    pub fn scrollback_offset(&self) -> usize {
        let Some(parser) = self.parser() else {
            return 0;
        };
        match parser.lock() {
            Ok(guard) => parser_scrollback_offset(&guard),
            Err(_) => 0,
        }
    }

    fn spawn_pty(&mut self) -> Result<()> {
        let level = self.current_level;
        let secret = self.credentials.get(level).cloned();
        let known_hosts = self.paths.known_hosts_file();
        if let Err(error) = ssh::ensure_known_hosts(&known_hosts) {
            self.status = format!("Could not prepare known_hosts: {error:#}");
        }
        // With sshpass installed it feeds the password itself; otherwise the
        // PTY reader must inject it once at the plain-ssh prompt. Key secrets
        // and unknown levels leave the password to the user as before.
        let auto_password = fallback_password(secret.as_ref(), ssh::sshpass_available());
        let command = ssh::interactive_command(
            level,
            secret.as_ref(),
            &known_hosts,
            auto_password.is_some(),
        );
        self.generation += 1;
        match PtySession::spawn(
            level,
            command,
            auto_password,
            self.pane_size,
            self.generation,
            self.tx.clone(),
            |generation| Message::PtyExited { generation },
        ) {
            Ok(pty) => {
                self.pty = Some(pty);
                Ok(())
            }
            Err(error) => {
                self.status = format!("Could not start SSH for bandit{level}: {error:#}");
                self.pty = None;
                Err(error)
            }
        }
    }

    fn request_level(&mut self, force: bool) {
        self.fetch_id += 1;
        let id = self.fetch_id;
        let level = self.current_level;
        if level >= MAX_LEVEL {
            // The game really ends at bandit33; bandit34.html is only a
            // placeholder. Show completion locally instead of fetching it.
            self.level = LevelState::Ready(LevelInfo {
                level,
                page: page_for_level(level),
                title: format!("Bandit Level {level}"),
                goal: "Game complete — bandit33 is the final level. There is no next password to find or verify.".to_owned(),
                commands: vec![],
                reading: vec![],
                notes: vec![],
            });
            self.status = format!(
                "Bandit {level} is the final level — game complete; no next secret to verify."
            );
            return;
        }
        let dir = self.paths.levels_dir();
        let tx = self.tx.clone();
        self.level = LevelState::Loading;
        thread::spawn(move || {
            let result = fetch_level(level, &dir, force).map_err(|error| error.to_string());
            let _ = tx.send(Message::LevelFetched { id, level, result });
        });
    }

    fn next_request_id(&mut self) -> u64 {
        self.next_request_id += 1;
        self.next_request_id
    }

    /// Send bytes to the PTY. A dead session is a status message, never a fatal
    /// error: keystrokes can race with `PtyExited` or a remote logout.
    fn write_pty(&mut self, bytes: &[u8]) {
        let result = match self.pty.as_mut() {
            Some(pty) => pty.write_bytes(bytes),
            None => {
                self.status = "No active SSH session (PTY failed to start).".to_owned();
                return;
            }
        };
        if let Err(error) = result {
            self.status = format!("SSH session ended; input was not sent: {error:#}");
            self.pty.take();
        }
    }

    /// Handle resize, quit/retry hotkeys, mouse focus changes, and mode-specific input.
    pub fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Resize(width, height) => {
                self.terminal_area = Rect::new(0, 0, width, height);
                let inner = ui::ssh_inner_area(self.terminal_area);
                self.pane_size = PtySize {
                    rows: inner.height.max(1),
                    cols: inner.width.max(1),
                    pixel_width: 0,
                    pixel_height: 0,
                };
                let result = self.pty.as_mut().map(|pty| pty.resize(self.pane_size));
                if let Some(Err(error)) = result {
                    self.status = format!("SSH PTY resize failed: {error:#}");
                    self.pty.take();
                }
            }
            Event::Paste(text) if !self.show_help => {
                if self.mode == AppMode::SecretInput {
                    self.insert_secret_text(&text);
                } else {
                    // Pasting means "I want to interact again" — return to
                    // live output before the text reaches the remote shell.
                    self.snap_terminal_to_live();
                    self.write_pty(text.as_bytes());
                }
            }
            Event::Key(key) => self.handle_key(key)?,
            // While help is up, mouse events are consumed: no scrolling and no
            // focus change. Only a key closes the overlay.
            Event::Mouse(_) if self.show_help => {}
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                // crossterm reports 1-based column/row; ratatui Rects are
                // 0-based terminal-cell coordinates.
                let column = mouse.column.saturating_sub(1);
                let row = mouse.row.saturating_sub(1);
                let secret = ui::secret_block_area(self.terminal_area);
                let ssh = ui::ssh_block_area(self.terminal_area);
                if ui::rect_contains(secret, column, row) {
                    self.mode = AppMode::SecretInput;
                    self.status =
                        "Secret box focused: type the secret, then press Enter to verify."
                            .to_owned();
                } else if ui::rect_contains(ssh, column, row) {
                    self.mode = AppMode::TerminalFocus;
                    self.status = "SSH terminal focused: type to return to live output.".to_owned();
                }
                // Clicks are UI focus changes only; never forwarded to the PTY.
            }
            Event::Mouse(mouse) if self.mode == AppMode::TerminalFocus => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.scroll_terminal(ScrollDirection::Up, WHEEL_SCROLL_LINES)
                }
                MouseEventKind::ScrollDown => {
                    self.scroll_terminal(ScrollDirection::Down, WHEEL_SCROLL_LINES)
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        // Help owns the keyboard: any press closes it and is swallowed, so the
        // closing key cannot reach the PTY or mutate the secret field.
        if self.show_help {
            if key.kind != KeyEventKind::Release {
                self.show_help = false;
            }
            return Ok(());
        }

        // Key releases are never commands (and are already ignored by
        // `key_bytes`); in particular they must not move the viewport.
        if key.kind == KeyEventKind::Release {
            return Ok(());
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if is_help_open_key(key, ctrl) {
            self.show_help = true;
            return Ok(());
        }

        // PageUp/PageDown belong to the scrollback, not the remote shell.
        if self.mode == AppMode::TerminalFocus
            && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
        {
            let direction = if key.code == KeyCode::PageUp {
                ScrollDirection::Up
            } else {
                ScrollDirection::Down
            };
            self.page_terminal(direction);
            return Ok(());
        }

        // Every other key resumes live output before its usual action, so
        // typing or hotkeys can never leave the view stuck in history.
        self.snap_terminal_to_live();

        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let plain = key.modifiers.is_empty();

        // Global quit: Ctrl+Q works in both modes; Ctrl+C quits only while
        // typing a secret so that Ctrl+C still reaches the remote shell.
        if ctrl && key.code == KeyCode::Char('q')
            || (self.mode == AppMode::SecretInput && ctrl && key.code == KeyCode::Char('c'))
        {
            self.should_quit = true;
            return Ok(());
        }

        let retry = key.code == KeyCode::F(5)
            || (alt && key.code == KeyCode::Char('r'))
            || (plain
                && self.mode == AppMode::TerminalFocus
                && key.code == KeyCode::Char('r')
                && matches!(self.level, LevelState::Error { .. }));
        if retry {
            self.status = "Retrying level fetch…".to_owned();
            self.request_level(true);
            return Ok(());
        }

        if self.mode == AppMode::SecretInput {
            return self.handle_secret_key(key, ctrl, alt);
        }

        // Alt+I is the primary escape chord; Ctrl+P is kept for compatibility.
        if (alt && key.code == KeyCode::Char('i')) || (ctrl && key.code == KeyCode::Char('p')) {
            self.mode = AppMode::SecretInput;
            self.status = "Enter the next level secret, then press Enter to verify.".to_owned();
            return Ok(());
        }

        if let Some(bytes) = crate::pty::key_bytes(key) {
            self.write_pty(&bytes);
        }
        Ok(())
    }

    fn handle_secret_key(&mut self, key: KeyEvent, ctrl: bool, alt: bool) -> Result<()> {
        if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('g')) {
            self.mode = AppMode::TerminalFocus;
            return Ok(());
        }
        if key.code == KeyCode::Tab && !ctrl && !alt {
            self.secret_kind = match self.secret_kind {
                SecretKind::Password => SecretKind::PrivateKey,
                SecretKind::PrivateKey => SecretKind::Password,
            };
            return Ok(());
        }
        if key.code == KeyCode::Enter && !ctrl && !alt {
            self.submit_secret();
            return Ok(());
        }
        if ctrl || alt {
            return Ok(());
        }

        let char_count = self.secret_input.chars().count();
        self.secret_cursor = self.secret_cursor.min(char_count);
        match key.code {
            KeyCode::Char(c) => self.insert_secret_text(&c.to_string()),
            KeyCode::Backspace => {
                if self.secret_cursor > 0 {
                    self.secret_cursor -= 1;
                    let byte_index = byte_index(&self.secret_input, self.secret_cursor);
                    self.secret_input.remove(byte_index);
                }
            }
            KeyCode::Delete => {
                if self.secret_cursor < char_count {
                    let byte_index = byte_index(&self.secret_input, self.secret_cursor);
                    self.secret_input.remove(byte_index);
                }
            }
            KeyCode::Left => self.secret_cursor = self.secret_cursor.saturating_sub(1),
            KeyCode::Right => self.secret_cursor = (self.secret_cursor + 1).min(char_count),
            KeyCode::Home => self.secret_cursor = 0,
            KeyCode::End => self.secret_cursor = char_count,
            _ => {}
        }
        Ok(())
    }

    fn insert_secret_text(&mut self, text: &str) {
        let mut remaining = MAX_SECRET_BYTES.saturating_sub(self.secret_input.len());
        let mut insert = String::new();
        for character in text.chars() {
            let size = character.len_utf8();
            if size > remaining {
                break;
            }
            insert.push(character);
            remaining -= size;
        }
        if insert.is_empty() {
            return;
        }
        let cursor = self.secret_cursor.min(self.secret_input.chars().count());
        let byte_index = byte_index(&self.secret_input, cursor);
        self.secret_input.insert_str(byte_index, &insert);
        self.secret_cursor = cursor + insert.chars().count();
    }

    fn submit_secret(&mut self) {
        // Repeated Enter (or a held key) must not launch overlapping verifiers.
        if !matches!(self.verify, VerifyState::Idle) {
            self.status = "A verification is already in progress.".to_owned();
            return;
        }
        if self.current_level >= MAX_LEVEL {
            self.status = format!(
                "Bandit {MAX_LEVEL} is the final level — game complete; there is no next secret to verify."
            );
            return;
        }
        let input = self.secret_input.trim().to_owned();
        if input.is_empty() {
            self.status =
                "Enter a secret first (password or path to a local private key).".to_owned();
            return;
        }
        let secret = match self.secret_kind {
            SecretKind::Password => Secret::Password(input),
            SecretKind::PrivateKey => {
                let path = ssh::expand_key_path(&PathBuf::from(input));
                if !path.is_file() {
                    self.status = format!("Private key file not found: {}", path.display());
                    return;
                }
                Secret::PrivateKeyPath(path)
            }
        };

        let id = self.next_request_id();
        let level = self.current_level + 1;
        self.verify = VerifyState::Verifying { id, level };
        self.status = format!("Verifying {level} with {}…", secret.summary());

        let tx = self.tx.clone();
        let verifier_secret = secret.clone();
        let known_hosts = self.paths.known_hosts_file();
        let config_dir = self.paths.config_dir.clone();
        thread::spawn(move || {
            let result = verify::verify_secret(
                level,
                &verifier_secret,
                verify::VERIFY_TIMEOUT,
                &known_hosts,
                &config_dir,
            );
            let _ = tx.send(Message::Verified {
                id,
                level,
                secret: verifier_secret,
                result,
            });
        });
    }

    /// Apply a background-thread result. Stale results are ignored by id.
    pub fn handle_message(&mut self, message: Message) -> Result<()> {
        match message {
            Message::LevelFetched { id, level, result } => {
                if id != self.fetch_id || level != self.current_level {
                    return Ok(());
                }
                self.level = match result {
                    Ok(info) => LevelState::Ready(info),
                    Err(message) => LevelState::Error { message },
                };
            }
            Message::Verified {
                id,
                level,
                secret,
                result,
            } => {
                if !matches!(self.verify, VerifyState::Verifying { id: active, .. } if active == id)
                    || level != self.current_level.saturating_add(1)
                {
                    return Ok(());
                }
                self.verify = VerifyState::Idle;
                match result {
                    Ok(()) => self.advance(level, secret)?,
                    Err(error) => {
                        // Keep the current PTY and the entered secret intact.
                        self.status = format!("Verification failed: {error}");
                    }
                }
            }
            Message::PtyExited { generation } => {
                if generation == self.generation {
                    // The reader thread is finished; drop the dead session so
                    // subsequent keystrokes cannot write into a closed PTY.
                    self.pty = None;
                    self.status = format!(
                        "SSH session for bandit{} ended (terminal input is unavailable).",
                        self.current_level
                    );
                }
            }
        }
        Ok(())
    }

    fn advance(&mut self, next_level: u16, secret: Secret) -> Result<()> {
        self.credentials.insert(next_level, secret.clone());
        self.credentials.last_level = next_level;
        let save_warning = save_credentials(&self.paths.credentials_file(), &self.credentials)
            .err()
            .map(|error| format!(" WARNING: credentials were not saved: {error:#}"));

        if let Some(mut pty) = self.pty.take() {
            pty.terminate();
        }
        self.current_level = next_level.min(MAX_LEVEL);
        self.secret_input.clear();
        self.secret_cursor = 0;
        self.mode = AppMode::TerminalFocus;
        self.status = format!(
            "Verified! Connecting to bandit{next_level} with {}.{}",
            secret.summary(),
            save_warning.unwrap_or_default()
        );

        if let Err(error) = self.spawn_pty().with_context(|| "spawning next level PTY") {
            self.status = format!("Could not start bandit{next_level} PTY: {error:#}");
        }
        self.request_level(false);
        Ok(())
    }

    /// Called on shutdown to avoid leaving SSH children behind.
    pub fn shutdown(&mut self) {
        if let Some(mut pty) = self.pty.take() {
            pty.terminate();
        }
    }
}

/// Password handed to the PTY reader for one-shot injection. Only the plain
/// `ssh` fallback needs it: `sshpass` already supplies the password, and key
/// secrets authenticate without one.
fn fallback_password(secret: Option<&Secret>, sshpass_available: bool) -> Option<String> {
    match secret {
        Some(Secret::Password(password)) if !sshpass_available => Some(password.clone()),
        _ => None,
    }
}

/// F1 and Ctrl+? open the keybinding overlay. SHIFT is allowed alongside CTRL
/// because terminals may report Ctrl+? as `Char('?')` with both modifiers.
fn is_help_open_key(key: KeyEvent, ctrl: bool) -> bool {
    key.code == KeyCode::F(1) || (ctrl && key.code == KeyCode::Char('?'))
}

/// Convert a character index into a UTF-8 byte offset (input length for an
/// at-or-past-end index).
fn byte_index(input: &str, char_index: usize) -> usize {
    input
        .char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or(input.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::PtySession;
    use crossterm::event::MouseEvent;
    use portable_pty::CommandBuilder;
    use std::num::NonZeroU16;
    use std::time::{Duration, Instant};

    fn test_app() -> App {
        let (tx, _rx) = mpsc::channel();
        App {
            mode: AppMode::TerminalFocus,
            current_level: 0,
            level: LevelState::Loading,
            status: String::new(),
            secret_input: String::new(),
            secret_cursor: 0,
            secret_kind: SecretKind::Password,
            verify: VerifyState::Idle,
            credentials: Credentials::default(),
            paths: Paths {
                config_dir: PathBuf::from("/tmp/bandit-tui-unit-config"),
                cache_dir: PathBuf::from("/tmp/bandit-tui-unit-cache"),
            },
            pty: None,
            should_quit: false,
            pane_size: PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            terminal_area: Rect::new(0, 0, 80, 24),
            show_help: false,
            tx,
            fetch_id: 0,
            next_request_id: 0,
            generation: 0,
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn pty_password_injection_only_for_plain_ssh_fallback() {
        const TEST_SECRET: &str = "test-only-placeholder";
        let password = Secret::Password(TEST_SECRET.to_owned());
        assert_eq!(
            fallback_password(Some(&password), false),
            Some(TEST_SECRET.to_owned())
        );
        // sshpass feeds the password itself; injecting as well would double it.
        assert_eq!(fallback_password(Some(&password), true), None);

        let key = Secret::PrivateKeyPath(PathBuf::from("/tmp/key.private"));
        assert_eq!(fallback_password(Some(&key), false), None);
        assert_eq!(fallback_password(None, false), None);
    }

    #[test]
    fn byte_index_respects_utf8_boundaries() {
        assert_eq!(byte_index("éomain", 0), 0);
        assert_eq!(byte_index("éomain", 1), 2);
        assert_eq!(byte_index("éomain", 6), 7);
        assert_eq!(byte_index("éomain", 99), 7);
    }

    #[test]
    fn unicode_secret_editing_never_panics() {
        let mut app = test_app();
        app.mode = AppMode::SecretInput;
        app.insert_secret_text("éomain");
        assert_eq!(app.secret_input, "éomain");
        assert_eq!(app.secret_cursor, 6);

        // Move to just after the two-byte é, then delete it with Backspace.
        for _ in 0..5 {
            app.handle_key(key(KeyCode::Left, KeyModifiers::NONE))
                .unwrap();
        }
        assert_eq!(app.secret_cursor, 1);
        app.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.secret_input, "omain");
        assert_eq!(app.secret_cursor, 0);

        app.handle_key(key(KeyCode::Right, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(key(KeyCode::Delete, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.secret_input, "oain");
        assert_eq!(app.secret_cursor, 1);

        // Inserting at a non-zero character index must land on a boundary.
        app.handle_key(key(KeyCode::Home, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(key(KeyCode::Char('é'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(key(KeyCode::Right, KeyModifiers::NONE))
            .unwrap();
        app.insert_secret_text("x");
        assert_eq!(app.secret_input, "éoxain");
        assert_eq!(app.secret_cursor, 3);
    }

    #[test]
    fn secret_input_is_capped_by_utf8_bytes() {
        let mut app = test_app();
        app.insert_secret_text(&"é".repeat(3000));
        assert_eq!(app.secret_input.len(), MAX_SECRET_BYTES);
        assert_eq!(app.secret_cursor, MAX_SECRET_BYTES / 2);
        app.insert_secret_text("x");
        assert_eq!(app.secret_input.len(), MAX_SECRET_BYTES);
    }

    #[test]
    fn repeated_enter_does_not_start_another_verifier() {
        let mut app = test_app();
        app.verify = VerifyState::Verifying { id: 42, level: 1 };
        app.secret_input = "password".to_owned();
        app.secret_cursor = 8;
        app.submit_secret();
        assert_eq!(app.verify, VerifyState::Verifying { id: 42, level: 1 });
        assert_eq!(app.next_request_id, 0);
    }

    #[test]
    fn stale_messages_are_ignored() {
        let mut app = test_app();
        app.generation = 3;
        app.verify = VerifyState::Verifying { id: 9, level: 1 };

        app.handle_message(Message::PtyExited { generation: 2 })
            .unwrap();
        assert!(app.status.is_empty());

        app.handle_message(Message::Verified {
            id: 8,
            level: 1,
            secret: Secret::Password("old".to_owned()),
            result: Ok(()),
        })
        .unwrap();
        app.handle_message(Message::Verified {
            id: 9,
            level: 2,
            secret: Secret::Password("old".to_owned()),
            result: Ok(()),
        })
        .unwrap();
        assert_eq!(app.verify, VerifyState::Verifying { id: 9, level: 1 });
        assert!(app.credentials.secrets.is_empty());
        assert_eq!(app.current_level, 0);

        app.handle_message(Message::LevelFetched {
            id: 1,
            level: 0,
            result: Ok(LevelInfo {
                level: 0,
                page: 1,
                title: "stale".to_owned(),
                goal: String::new(),
                commands: vec![],
                reading: vec![],
                notes: vec![],
            }),
        })
        .unwrap();
        assert!(matches!(app.level, LevelState::Loading));
    }

    #[test]
    fn current_generation_pty_exit_clears_session() {
        let mut app = test_app();
        app.generation = 4;
        app.handle_message(Message::PtyExited { generation: 4 })
            .unwrap();
        assert!(app.pty.is_none());
        assert!(app.status.contains("ended"));
        // A later keystroke must be a no-op rather than a fatal write error.
        app.handle_key(key(KeyCode::Char('a'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.status.contains("No active SSH session"));
    }

    #[test]
    fn mode_switch_hotkeys_work() {
        let mut app = test_app();
        app.handle_key(key(KeyCode::Char('i'), KeyModifiers::ALT))
            .unwrap();
        assert_eq!(app.mode, AppMode::SecretInput);
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, AppMode::TerminalFocus);
        app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(app.mode, AppMode::SecretInput);
        app.handle_key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(app.mode, AppMode::TerminalFocus);
    }

    /// Mimic crossterm: coordinates are 1-based. Clicking the block's final
    /// cell catches a missing 0-based conversion — (x + width, y + height)
    /// is outside the block if it is passed through unchanged.
    fn left_click(area: Rect) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x.saturating_add(area.width),
            row: area.y.saturating_add(area.height),
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn clicking_secret_and_ssh_boxes_switches_focus() {
        let mut app = test_app();
        let secret = ui::secret_block_area(app.terminal_area);
        let ssh = ui::ssh_block_area(app.terminal_area);

        app.handle_event(Event::Mouse(left_click(secret))).unwrap();
        assert_eq!(app.mode, AppMode::SecretInput);
        assert!(app.status.contains("Secret box focused"));

        // Focus alone is enough: typing and paste now target the secret field.
        app.handle_event(Event::Key(key(KeyCode::Char('a'), KeyModifiers::NONE)))
            .unwrap();
        app.handle_event(Event::Paste("bc".to_owned())).unwrap();
        assert_eq!(app.secret_input, "abc");

        app.handle_event(Event::Mouse(left_click(ssh))).unwrap();
        assert_eq!(app.mode, AppMode::TerminalFocus);
        assert!(app.status.contains("SSH terminal focused"));
        assert_eq!(app.secret_input, "abc", "focus change keeps entered text");
    }

    #[test]
    fn resize_updates_click_geometry() {
        let mut app = test_app();
        app.handle_event(Event::Resize(100, 30)).unwrap();
        assert_eq!(app.terminal_area, Rect::new(0, 0, 100, 30));
        let secret = ui::secret_block_area(app.terminal_area);
        assert!(ui::rect_contains(
            secret,
            secret.x + secret.width / 2,
            secret.y + secret.height / 2
        ));
    }

    #[test]
    fn help_toggles_open_and_any_key_closes_without_side_effects() {
        let mut app = test_app();
        for open in [
            key(KeyCode::F(1), KeyModifiers::NONE),
            key(KeyCode::Char('?'), KeyModifiers::CONTROL),
            key(
                KeyCode::Char('?'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        ] {
            app.handle_event(Event::Key(open)).unwrap();
            assert!(app.show_help, "{open:?} should open help");

            let mut release = key(KeyCode::Char('x'), KeyModifiers::NONE);
            release.kind = KeyEventKind::Release;
            app.handle_event(Event::Key(release)).unwrap();
            assert!(app.show_help, "key releases must not close help");

            app.handle_event(Event::Key(key(KeyCode::Char('x'), KeyModifiers::NONE)))
                .unwrap();
            assert!(!app.show_help, "any key press should close help");
            assert!(app.secret_input.is_empty());
            assert_eq!(app.mode, AppMode::TerminalFocus);
        }

        app.mode = AppMode::SecretInput;
        app.handle_event(Event::Key(key(KeyCode::F(1), KeyModifiers::NONE)))
            .unwrap();
        app.handle_event(Event::Key(key(KeyCode::Char('z'), KeyModifiers::NONE)))
            .unwrap();
        assert!(!app.show_help);
        assert_eq!(app.mode, AppMode::SecretInput, "help must not change mode");
        assert_eq!(app.secret_input, "", "closing key must not enter secret");

        app.handle_event(Event::Key(key(KeyCode::F(1), KeyModifiers::NONE)))
            .unwrap();
        app.handle_event(Event::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        assert!(!app.show_help);
    }

    #[test]
    fn help_consumes_paste_and_clicks_in_secret_mode() {
        let mut app = test_app();
        app.mode = AppMode::SecretInput;
        app.show_help = true;
        let secret = ui::secret_block_area(app.terminal_area);

        app.handle_event(Event::Paste("should-not-appear".to_owned()))
            .unwrap();
        app.handle_event(Event::Mouse(left_click(secret))).unwrap();
        assert!(app.show_help);
        assert_eq!(app.mode, AppMode::SecretInput);
        assert!(app.secret_input.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn key_that_closes_help_is_not_written_to_pty() {
        let mut app = stub_pty_app(6);
        assert!(
            wait_for_screen(&app, "READY", Duration::from_secs(3)),
            "local stub shell never produced output"
        );

        app.handle_event(Event::Key(key(KeyCode::F(1), KeyModifiers::NONE)))
            .unwrap();
        assert!(app.show_help);
        app.handle_event(Event::Key(key(KeyCode::Char('a'), KeyModifiers::NONE)))
            .unwrap();
        assert!(!app.show_help);

        // Enter now reaches the stub shell. If 'a' had leaked while help was
        // open, the shell would echo `WROTE:a` instead of an empty `WROTE:`.
        app.handle_event(Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .unwrap();
        assert!(
            wait_for_screen(&app, "WROTE:", Duration::from_secs(3)),
            "post-help typing was not forwarded to the PTY"
        );
        assert!(
            !screen_text(&app).contains("WROTE:a"),
            "the help-closing key was forwarded to the PTY"
        );
    }

    #[test]
    fn final_level_rejects_submission() {
        let mut app = test_app();
        app.current_level = MAX_LEVEL;
        app.secret_input = "anything".to_owned();
        app.submit_secret();
        assert_eq!(app.verify, VerifyState::Idle);
        assert!(app.status.contains("final level"));
    }

    #[test]
    fn final_level_shows_completion_without_a_fetch() {
        let mut app = test_app();
        app.current_level = MAX_LEVEL;
        app.request_level(false);

        let LevelState::Ready(info) = &app.level else {
            panic!(
                "final level should be ready without fetching: {:?}",
                app.level
            );
        };
        assert_eq!(info.level, MAX_LEVEL);
        assert_eq!(info.page, page_for_level(MAX_LEVEL));
        assert_eq!(info.title, format!("Bandit Level {MAX_LEVEL}"));
        assert!(info.goal.contains("Game complete"));
        assert!(info.commands.is_empty());
        assert!(app.status.contains("final level"));
        assert_eq!(app.fetch_id, 1);
    }

    fn history_parser() -> Parser {
        let mut parser = Parser::new(
            NonZeroU16::new(4).unwrap(),
            NonZeroU16::new(20).unwrap(),
            100,
        );
        for line in 0..20 {
            parser.process(format!("line {line}\r\n").as_bytes());
        }
        parser
    }

    #[test]
    fn parser_scroll_pages_clamp_and_reset_to_live() {
        let mut parser = history_parser();
        let history = parser.screen().scrollback_len();
        assert!(history > 4);
        let live = parser.screen().contents();

        assert_eq!(parser_scrollback_offset(&parser), 0);
        assert_eq!(scroll_parser(&mut parser, ScrollDirection::Up, 4), 4);
        assert_ne!(parser.screen().contents(), live, "history must be visible");
        assert_eq!(scroll_parser(&mut parser, ScrollDirection::Up, 4), 8);
        assert_eq!(
            scroll_parser(&mut parser, ScrollDirection::Up, 1000),
            history
        );
        assert_eq!(
            scroll_parser(&mut parser, ScrollDirection::Down, 4),
            history - 4
        );
        assert_eq!(snap_parser_to_live(&mut parser), 0);
        assert_eq!(parser.screen().contents(), live);
        assert_eq!(scroll_parser(&mut parser, ScrollDirection::Down, 1), 0);
        // Callers with a zero page size still advance by one line.
        assert_eq!(scroll_parser(&mut parser, ScrollDirection::Up, 0), 1);
    }

    #[cfg(unix)]
    fn stub_pty_app(rows: u16) -> App {
        let mut app = test_app();
        app.pane_size = PtySize {
            rows,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut command = CommandBuilder::new("sh");
        command.arg("-c");
        command.arg(
            "i=1; while [ $i -le 80 ]; do echo line$i; i=$((i+1)); done; \
             echo READY; while IFS= read -r line; do echo WROTE:$line; done",
        );
        let (tx, _rx) = mpsc::channel();
        // A local `sh` stub: no SSH, no network, no credentials.
        app.pty = Some(
            PtySession::spawn(0, command, None, app.pane_size, 7, tx, |generation| {
                generation
            })
            .unwrap(),
        );
        app
    }

    #[cfg(unix)]
    fn wait_for_screen(app: &App, needle: &str, timeout: Duration) -> bool {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if let Some(parser) = app.parser()
                && let Ok(guard) = parser.lock()
                && guard.screen().contents().contains(needle)
            {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[cfg(unix)]
    fn history_len(app: &App) -> usize {
        app.parser()
            .and_then(|parser| {
                parser
                    .lock()
                    .ok()
                    .map(|guard| guard.screen().scrollback_len())
            })
            .unwrap_or(0)
    }

    #[cfg(unix)]
    fn screen_text(app: &App) -> String {
        app.parser()
            .and_then(|parser| parser.lock().ok().map(|guard| guard.screen().contents()))
            .unwrap_or_default()
    }

    #[cfg(unix)]
    #[test]
    fn page_up_scrolls_without_pty_writes_and_typing_returns_to_live() {
        let mut app = stub_pty_app(6);
        assert!(
            wait_for_screen(&app, "READY", Duration::from_secs(3)),
            "local stub shell never produced output"
        );
        assert!(history_len(&app) >= 6);
        let live = screen_text(&app);
        assert_eq!(app.scrollback_offset(), 0);

        app.handle_event(Event::Key(key(KeyCode::PageUp, KeyModifiers::NONE)))
            .unwrap();
        assert_eq!(app.scrollback_offset(), 6);
        assert_ne!(screen_text(&app), live, "history should be rendered");
        assert!(!app.status.contains("No active SSH session"));

        // Give any mistakenly forwarded bytes time to reach the stub's echo.
        thread::sleep(Duration::from_millis(150));
        assert!(
            !screen_text(&app).contains("WROTE"),
            "PageUp must not be forwarded to the PTY"
        );

        let mut release = key(KeyCode::PageUp, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        app.handle_event(Event::Key(release)).unwrap();
        assert_eq!(app.scrollback_offset(), 6, "releases must be ignored");

        app.handle_event(Event::Key(key(KeyCode::Char('a'), KeyModifiers::NONE)))
            .unwrap();
        // The stub shell reads whole lines; Enter commits the typed 'a'.
        app.handle_event(Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .unwrap();
        assert_eq!(app.scrollback_offset(), 0, "typing must snap to live");
        assert!(
            wait_for_screen(&app, "WROTE:a", Duration::from_secs(3)),
            "normal typing was not forwarded to the PTY"
        );
    }

    #[cfg(unix)]
    #[test]
    fn mouse_wheel_scrolls_only_while_terminal_is_focused() {
        let mut app = stub_pty_app(6);
        assert!(
            wait_for_screen(&app, "READY", Duration::from_secs(3)),
            "local stub shell never produced output"
        );

        app.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        assert_eq!(app.scrollback_offset(), WHEEL_SCROLL_LINES as usize);

        app.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        assert_eq!(app.scrollback_offset(), 0);

        app.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        app.mode = AppMode::SecretInput;
        app.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        assert_eq!(app.scrollback_offset(), WHEEL_SCROLL_LINES as usize);
    }
}
