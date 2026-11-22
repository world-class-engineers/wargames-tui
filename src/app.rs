//! Application state and event orchestration.

use crate::credentials::{Credentials, Paths, Secret, load_credentials, save_credentials};
use crate::pty::PtySession;
use crate::scraper::{LevelInfo, fetch_level};
use crate::ssh;
use crate::ui;
use crate::verify;
use crate::wargame::{Catalog, SecretKind, Wargame, load_catalog};
use crate::widgets::vt100_widget::{Selection, SelectionRow};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryAction {
    Reconnect,
    Refetch,
}

/// Keep the selected picker row inside the visible window without moving an
/// already-visible selection.
fn scroll_picker_to(index: usize, offset: usize) -> usize {
    let visible = usize::from(ui::PICKER_VISIBLE_ROWS.max(1));
    if index < offset {
        index
    } else if index >= offset.saturating_add(visible) {
        index.saturating_add(1).saturating_sub(visible)
    } else {
        offset
    }
}

/// Which way to move the vt100 viewport through scrollback history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
}

/// Move a vt100 parser's viewport by `lines` rows, returning its new offset.
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
pub struct CellPosition {
    pub row: u16,
    pub column: u16,
}

/// A completed in-app text selection in SSH-inner coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSelection {
    pub start: CellPosition,
    pub end: CellPosition,
    pub scrollback: usize,
    pub text: String,
}

/// Expand inclusive drag endpoints into the exact per-row cell ranges that are
/// both copied and highlighted. Multi-row drags use standard text-selection
/// semantics: the row where the drag starts/ends keeps its partial range while
/// the rows between them are selected in full, regardless of drag direction.
pub fn selection_rows(
    start: CellPosition,
    end: CellPosition,
    screen_rows: u16,
    screen_columns: u16,
) -> Vec<SelectionRow> {
    if screen_rows == 0 || screen_columns == 0 {
        return Vec::new();
    }
    let last_row = screen_rows - 1;
    let last_column = screen_columns - 1;
    let first_row = start.row.min(end.row).min(last_row);
    let final_row = start.row.max(end.row).min(last_row);

    if start.row == end.row || first_row == final_row {
        let first_column = start.column.min(end.column).min(last_column);
        let final_column = start.column.max(end.column).min(last_column);
        return vec![SelectionRow {
            row: first_row,
            first_column,
            last_column: final_column,
        }];
    }

    // The leading endpoint belongs to the row where the drag began in reading
    // order; the trailing endpoint belongs to the final row. Comparing the
    // (row, column) tuples makes reverse drags mirror forward drags.
    let (leading, trailing) = if (start.row, start.column) <= (end.row, end.column) {
        (start.column, end.column)
    } else {
        (end.column, start.column)
    };
    let mut rows = Vec::with_capacity(usize::from(final_row - first_row) + 1);
    rows.push(SelectionRow {
        row: first_row,
        first_column: leading.min(last_column),
        last_column,
    });
    for row in first_row + 1..final_row {
        rows.push(SelectionRow {
            row,
            first_column: 0,
            last_column,
        });
    }
    rows.push(SelectionRow {
        row: final_row,
        first_column: 0,
        last_column: trailing.min(last_column),
    });
    rows
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DragState {
    start: CellPosition,
    current: CellPosition,
    scrollback: usize,
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
        game_id: String,
        result: Result<LevelInfo, String>,
    },
    Verified {
        id: u64,
        level: u16,
        game_id: String,
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
    /// rendered panes and picker rows. Kept in sync with `Event::Resize`.
    pub terminal_area: Rect,
    /// When true the keybinding overlay owns all input until any key closes it.
    pub show_help: bool,
    pub catalog: Catalog,
    pub active_game_id: String,
    pub show_picker: bool,
    /// Credentials load failure from startup, kept so session startup does not
    /// silently replace it with a save result or a verification status.
    pub startup_warning: Option<String>,
    pub picker_index: usize,
    pub picker_offset: usize,
    /// Startup confirmation advances; F2-opened pickers cancel on Esc.
    pub picker_from_startup: bool,
    pub selection: Option<TextSelection>,
    /// When true, mouse capture is disabled so the terminal can select text.
    pub native_mouse_mode: bool,
    /// OSC 52 copies queued by selection completion and Ctrl+P.
    pub pending_clipboard: Vec<String>,
    drag: Option<DragState>,
    /// Cancellation handle for the verifier owned by `verify`, if any.
    active_verify_cancel: Option<verify::VerifyCancel>,
    tx: Sender<Message>,
    fetch_id: u64,
    next_request_id: u64,
    generation: u64,
}

impl App {
    /// Load the catalog and credentials and present the picker. No PTY is
    /// spawned and no level is fetched until a game is confirmed.
    pub fn new(
        paths: Paths,
        terminal_area: Rect,
        pane_size: PtySize,
        tx: Sender<Message>,
    ) -> Result<Self> {
        let catalog = load_catalog(&paths)?;
        let mut startup_warning = None;
        let credentials = match load_credentials(&paths.credentials_file()) {
            Ok(credentials) => credentials,
            Err(error) => {
                startup_warning = Some(format!("Could not load credentials: {error:#}"));
                Credentials::default()
            }
        };
        let active_game_id = if catalog.get(&credentials.last_wargame).is_some() {
            credentials.last_wargame.clone()
        } else {
            catalog
                .first()
                .map(|game| game.id.clone())
                .unwrap_or_default()
        };
        let picker_index = catalog
            .wargames
            .iter()
            .position(|game| game.id == active_game_id)
            .unwrap_or(0);
        let current_level = catalog
            .get(&active_game_id)
            .map(|game| game.clamp_level(credentials.progress(&active_game_id).last_level))
            .unwrap_or(0);
        let secret_kind = catalog
            .get(&active_game_id)
            .filter(|game| !game.allows(SecretKind::Password))
            .and_then(|game| game.allowed_secret_kinds.first().copied())
            .unwrap_or(SecretKind::Password);

        Ok(Self {
            mode: AppMode::TerminalFocus,
            current_level,
            level: LevelState::Loading,
            status: startup_warning.clone().unwrap_or_default(),
            secret_input: String::new(),
            secret_cursor: 0,
            secret_kind,
            verify: VerifyState::Idle,
            credentials,
            paths,
            pty: None,
            should_quit: false,
            pane_size,
            terminal_area,
            show_help: false,
            catalog,
            active_game_id,
            show_picker: true,
            startup_warning,
            picker_index,
            picker_offset: scroll_picker_to(picker_index, 0),
            picker_from_startup: true,
            selection: None,
            native_mouse_mode: false,
            pending_clipboard: Vec::new(),
            drag: None,
            active_verify_cancel: None,
            tx,
            fetch_id: 0,
            next_request_id: 0,
            generation: 0,
        })
    }

    pub fn messages() -> (Sender<Message>, Receiver<Message>) {
        mpsc::channel()
    }

    /// The active catalog entry. Catalog validation guarantees this exists.
    pub fn game(&self) -> &Wargame {
        self.catalog
            .get(&self.active_game_id)
            .expect("active wargame must exist in the validated catalog")
    }

    pub fn parser(&self) -> Option<Arc<Mutex<Parser>>> {
        self.pty.as_ref().map(|pty| pty.screen())
    }

    /// Run a closure against the live parser if its lock is available.
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

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.drag = None;
    }

    fn queue_clipboard(&mut self, text: &str) {
        if !text.is_empty() {
            self.pending_clipboard.push(text.to_owned());
        }
    }

    /// Cancel an in-flight verification and invalidate its result id. Called
    /// before a session switch so a stale verifier can neither keep the UI in
    /// `Verifying` forever nor block the next submission.
    fn cancel_verification(&mut self) {
        if self.verify == VerifyState::Idle && self.active_verify_cancel.is_none() {
            return;
        }
        if let Some(cancel) = self.active_verify_cancel.take() {
            cancel.cancel();
        }
        self.verify = VerifyState::Idle;
        // Bump the id so an already-queued `Verified` message is discarded.
        self.next_request_id();
        if self.status.starts_with("Verifying ") {
            self.status = self.startup_warning.clone().unwrap_or_default();
        }
    }

    /// Persist and start a session for the active game.
    pub fn start_session(&mut self) -> Result<()> {
        self.cancel_verification();
        let game = self.game().clone();
        self.credentials.set_last_wargame(&game.id);
        self.current_level = game.clamp_level(self.credentials.progress(&game.id).last_level);
        self.show_picker = false;
        self.picker_from_startup = false;
        self.clear_selection();
        self.secret_input.clear();
        self.secret_cursor = 0;
        self.secret_kind = if game.allows(SecretKind::Password) {
            SecretKind::Password
        } else {
            game.allowed_secret_kinds
                .first()
                .copied()
                .unwrap_or(SecretKind::Password)
        };
        self.mode = AppMode::TerminalFocus;
        self.level = LevelState::Loading;
        let save_warning = save_credentials(&self.paths.credentials_file(), &self.credentials)
            .err()
            .map(|error| format!("Could not save last wargame: {error:#}"));
        self.status = match (self.startup_warning.clone(), save_warning) {
            (Some(load), Some(save)) => format!("{load} | {save}"),
            (Some(load), None) => load,
            (None, Some(save)) => save,
            (None, None) => String::new(),
        };

        if let Some(mut pty) = self.pty.take() {
            pty.terminate();
        }
        self.spawn_pty()?;
        self.request_level(false);
        Ok(())
    }

    fn reopen_picker(&mut self) {
        self.cancel_verification();
        self.show_picker = true;
        self.picker_from_startup = false;
        self.picker_index = self
            .catalog
            .wargames
            .iter()
            .position(|game| game.id == self.active_game_id)
            .unwrap_or(0);
        self.picker_offset = scroll_picker_to(self.picker_index, self.picker_offset);
        self.clear_selection();
    }

    fn confirm_picker(&mut self) {
        let Some(selected) = self.catalog.wargames.get(self.picker_index).cloned() else {
            self.show_picker = false;
            return;
        };
        if selected.id == self.active_game_id && !self.picker_from_startup {
            self.show_picker = false;
            return;
        }
        self.active_game_id = selected.id.clone();
        if let Err(error) = self.start_session() {
            self.status = format!("Could not start {}: {error:#}", selected.name);
        }
    }

    fn move_picker(&mut self, delta: isize) {
        let len = self.catalog.wargames.len();
        if len == 0 {
            return;
        }
        let next = (self.picker_index as isize + delta).clamp(0, len as isize - 1) as usize;
        self.picker_index = next;
        self.picker_offset = scroll_picker_to(next, self.picker_offset);
    }

    fn handle_picker_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.kind == KeyEventKind::Release {
            return Ok(());
        }
        match key.code {
            KeyCode::Up => self.move_picker(-1),
            KeyCode::Down => self.move_picker(1),
            KeyCode::Home => {
                self.picker_index = 0;
                self.picker_offset = 0;
            }
            KeyCode::End => {
                self.picker_index = self.catalog.wargames.len().saturating_sub(1);
                self.move_picker(0);
            }
            KeyCode::Enter => self.confirm_picker(),
            KeyCode::Esc => {
                if self.picker_from_startup {
                    self.confirm_picker();
                } else {
                    self.show_picker = false;
                }
            }
            // At startup there is no session to return to; F2 confirms the
            // highlighted game just like Enter/Esc. Only start_session may
            // dismiss the startup picker.
            KeyCode::F(2) => {
                if self.picker_from_startup {
                    self.confirm_picker();
                } else {
                    self.show_picker = false;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// F5/Alt+R reconnect when the previous SSH child is gone; otherwise they
    /// keep their original meaning and force a level-page refetch.
    fn retry_action(&self) -> RetryAction {
        if self.pty.is_none() {
            RetryAction::Reconnect
        } else {
            RetryAction::Refetch
        }
    }

    fn spawn_pty(&mut self) -> Result<()> {
        let game = self.game().clone();
        let level = self.current_level;
        let initial = (level == game.first_level)
            .then(|| game.initial_secret())
            .flatten();
        let secret = self
            .credentials
            .progress(&game.id)
            .get(level)
            .cloned()
            .or(initial);
        let known_hosts = self.paths.known_hosts_file();
        if let Err(error) = ssh::ensure_known_hosts(&known_hosts) {
            self.status = format!("Could not prepare known_hosts: {error:#}");
        }
        let auto_password = fallback_password(secret.as_ref(), ssh::sshpass_available());
        let command = ssh::interactive_command(
            &game,
            level,
            secret.as_ref(),
            &known_hosts,
            auto_password.is_some(),
        );
        self.generation += 1;
        match PtySession::spawn(
            &game.level_label(level),
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
                self.status = format!(
                    "Could not start SSH for {}: {error:#}",
                    game.level_label(level)
                );
                self.pty = None;
                Err(error)
            }
        }
    }

    fn request_level(&mut self, force: bool) {
        self.fetch_id += 1;
        let id = self.fetch_id;
        let level = self.current_level;
        let game = self.game().clone();
        if game.is_complete(level) {
            self.level = LevelState::Ready(LevelInfo {
                game_id: game.id.clone(),
                source_cache_version: game.source_cache_version(),
                level,
                page: game.page_for_level(level),
                title: game.level_label(level),
                goal: game.completion_message.clone(),
                commands: vec![],
                reading: vec![],
                notes: vec![],
            });
            self.status = game.completion_message.clone();
            return;
        }
        let dir = self.paths.levels_dir();
        let tx = self.tx.clone();
        let game_id = game.id.clone();
        self.level = LevelState::Loading;
        thread::spawn(move || {
            let result = fetch_level(&game, level, &dir, force).map_err(|error| error.to_string());
            let _ = tx.send(Message::LevelFetched {
                id,
                level,
                game_id,
                result,
            });
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

    /// Convert a crossterm mouse coordinate into SSH-inner cell coordinates,
    /// clamped to the rendered vt100 area. Crossterm already normalizes SGR and
    /// X10 mouse reports to 0-based coordinates, so no further subtraction is
    /// needed here.
    fn ssh_cell(&self, column: u16, row: u16) -> CellPosition {
        let inner = ui::ssh_inner_area(self.terminal_area);
        CellPosition {
            column: column
                .saturating_sub(inner.x)
                .min(inner.width.saturating_sub(1)),
            row: row
                .saturating_sub(inner.y)
                .min(inner.height.saturating_sub(1)),
        }
    }

    /// Extract text from the parser viewport using the same per-row ranges the
    /// UI highlights. Rows are joined with `\n`; wide-character continuation
    /// cells are skipped and trailing spaces are trimmed.
    pub fn extract_selection_text(
        parser: &Parser,
        start: CellPosition,
        end: CellPosition,
    ) -> Option<String> {
        let (rows, columns) = parser.screen().size();
        let mut lines = Vec::new();
        for selected in selection_rows(start, end, rows.get(), columns.get()) {
            let mut line = String::new();
            for column in selected.first_column..=selected.last_column {
                if let Some(cell) = parser.screen().cell(selected.row, column) {
                    if cell.is_wide_continuation() {
                        continue;
                    }
                    line.push_str(cell.contents());
                }
            }
            lines.push(line.trim_end().to_owned());
        }
        Some(lines.join("\n"))
    }

    /// Highlight ranges for the current selection, in SSH-inner coordinates.
    pub fn selection_highlight(app: &App) -> Option<Selection> {
        let selection = app.selection.as_ref()?;
        if selection.scrollback != app.scrollback_offset() {
            return None;
        }
        let (rows, columns) = app.parser()?.lock().ok()?.screen().size();
        Some(Selection {
            rows: selection_rows(selection.start, selection.end, rows.get(), columns.get()),
        })
    }

    fn extract_selection(&self, start: CellPosition, end: CellPosition) -> Option<String> {
        let parser = self.parser()?;
        let guard = parser.lock().ok()?;
        Self::extract_selection_text(&guard, start, end)
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> Result<()> {
        let secret = ui::secret_block_area(self.terminal_area);
        let ssh = ui::ssh_block_area(self.terminal_area);

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if ui::rect_contains(secret, mouse.column, mouse.row) {
                    self.clear_selection();
                    self.mode = AppMode::SecretInput;
                    self.status =
                        "Secret box focused: type the secret, then press Enter to verify."
                            .to_owned();
                } else if ui::rect_contains(ssh, mouse.column, mouse.row) {
                    self.clear_selection();
                    self.mode = AppMode::TerminalFocus;
                    let position = self.ssh_cell(mouse.column, mouse.row);
                    let scrollback = self.scrollback_offset();
                    self.drag = Some(DragState {
                        start: position,
                        current: position,
                        scrollback,
                    });
                    self.status = "SSH terminal focused: drag to select text.".to_owned();
                } else {
                    self.clear_selection();
                }
                // Clicks are UI focus/selection changes; never sent to the PTY.
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let position = self.ssh_cell(mouse.column, mouse.row);
                if let Some(drag) = self.drag.as_mut() {
                    drag.current = position;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(mut drag) = self.drag.take() {
                    drag.current = self.ssh_cell(mouse.column, mouse.row);
                    if drag.start == drag.current {
                        return Ok(());
                    }
                    if self.scrollback_offset() != drag.scrollback {
                        self.status = "Selection discarded: the terminal view changed.".to_owned();
                        return Ok(());
                    }
                    if let Some(text) = self.extract_selection(drag.start, drag.current) {
                        self.queue_clipboard(&text);
                        self.selection = Some(TextSelection {
                            start: drag.start,
                            end: drag.current,
                            scrollback: drag.scrollback,
                            text,
                        });
                        self.status =
                            "Text selected; Ctrl+P inserts it as the secret and copies it."
                                .to_owned();
                    }
                }
            }
            MouseEventKind::ScrollUp if self.mode == AppMode::TerminalFocus => {
                self.clear_selection();
                self.scroll_terminal(ScrollDirection::Up, WHEEL_SCROLL_LINES);
            }
            MouseEventKind::ScrollDown if self.mode == AppMode::TerminalFocus => {
                self.clear_selection();
                self.scroll_terminal(ScrollDirection::Down, WHEEL_SCROLL_LINES);
            }
            _ => {}
        }
        Ok(())
    }

    /// Handle resize, quit/retry hotkeys, mouse focus changes, and mode-specific input.
    pub fn handle_event(&mut self, event: Event) -> Result<()> {
        // Help takes precedence over every other overlay and input source.
        if self.show_help {
            if let Event::Key(key) = event {
                self.handle_key(key)?;
            }
            return Ok(());
        }

        // Native selection mode: the terminal owns the mouse entirely.
        if self.native_mouse_mode && matches!(event, Event::Mouse(_)) {
            return Ok(());
        }

        if let Event::Resize(width, height) = event {
            self.terminal_area = Rect::new(0, 0, width, height);
            let inner = ui::ssh_inner_area(self.terminal_area);
            self.pane_size = PtySize {
                rows: inner.height.max(1),
                cols: inner.width.max(1),
                pixel_width: 0,
                pixel_height: 0,
            };
            self.clear_selection();
            let result = self.pty.as_mut().map(|pty| pty.resize(self.pane_size));
            if let Some(Err(error)) = result {
                self.status = format!("SSH PTY resize failed: {error:#}");
                self.pty.take();
            }
            return Ok(());
        }

        if self.show_picker {
            match event {
                Event::Key(key) => self.handle_picker_key(key)?,
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(index) = ui::picker_row_at(
                        self.terminal_area,
                        self.picker_offset,
                        mouse.column,
                        mouse.row,
                    ) {
                        self.picker_index = index;
                        self.confirm_picker();
                    }
                }
                // Picker consumes paste and every other mouse event.
                _ => {}
            }
            return Ok(());
        }

        match event {
            Event::Paste(text) => {
                self.clear_selection();
                if self.mode == AppMode::SecretInput {
                    self.insert_secret_text(&text);
                } else {
                    self.snap_terminal_to_live();
                    self.write_pty(text.as_bytes());
                }
            }
            Event::Key(key) => self.handle_key(key)?,
            Event::Mouse(mouse) => self.handle_mouse(mouse)?,
            _ => {}
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.show_help {
            if key.kind != KeyEventKind::Release {
                self.show_help = false;
            }
            return Ok(());
        }
        if key.kind == KeyEventKind::Release {
            return Ok(());
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // Alt+M toggles native terminal selection before any mode dispatch.
        if alt && key.code == KeyCode::Char('m') && !ctrl {
            self.native_mouse_mode = !self.native_mouse_mode;
            self.status = if self.native_mouse_mode {
                "Native mouse selection on: Alt+M again to re-enable app mouse handling.".to_owned()
            } else {
                "App mouse handling on: drag in the SSH pane to select text.".to_owned()
            };
            return Ok(());
        }

        // Ctrl+P has a selection-aware meaning and runs before mode dispatch.
        if ctrl
            && key.code == KeyCode::Char('p')
            && let Some(selection) = self.selection.take()
        {
            self.secret_input = selection.text.clone();
            self.secret_cursor = selection.text.chars().count();
            self.mode = AppMode::SecretInput;
            self.queue_clipboard(&selection.text);
            if !self.game().allows(self.secret_kind) {
                self.secret_kind = self
                    .game()
                    .allowed_secret_kinds
                    .first()
                    .copied()
                    .unwrap_or(SecretKind::Password);
            }
            if self.game().allows(SecretKind::Password) {
                self.secret_kind = SecretKind::Password;
                self.status = "Selected text inserted as the password and copied.".to_owned();
            } else {
                // Keep the current (allowed) kind: forcing Password would be
                // rejected at submission time for key-only custom catalogs.
                self.status = format!(
                    "Selected text copied; {} accepts {} only, so the secret type was kept.",
                    self.game().name,
                    self.secret_kind.label()
                );
            }
            return Ok(());
        }

        if key.code == KeyCode::F(2) {
            self.reopen_picker();
            return Ok(());
        }

        if is_help_open_key(key, ctrl) {
            self.show_help = true;
            return Ok(());
        }

        if self.mode == AppMode::TerminalFocus
            && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
        {
            self.clear_selection();
            let direction = if key.code == KeyCode::PageUp {
                ScrollDirection::Up
            } else {
                ScrollDirection::Down
            };
            self.page_terminal(direction);
            return Ok(());
        }

        // Every other key resumes live output before its usual action.
        self.snap_terminal_to_live();

        let plain = key.modifiers.is_empty();
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
            self.clear_selection();
            match self.retry_action() {
                RetryAction::Reconnect => {
                    let label = self.game().level_label(self.current_level);
                    self.status = format!("Reconnecting to {label}…");
                    if let Err(error) = self.spawn_pty() {
                        self.status = format!("Could not reconnect to {label}: {error:#}");
                    }
                    if matches!(self.level, LevelState::Error { .. }) {
                        self.request_level(true);
                    }
                }
                RetryAction::Refetch => {
                    self.status = "Retrying level fetch…".to_owned();
                    self.request_level(true);
                }
            }
            return Ok(());
        }

        if self.mode == AppMode::SecretInput {
            return self.handle_secret_key(key, ctrl, alt);
        }

        // Esc with a selection swallows the event and clears the selection.
        if key.code == KeyCode::Esc && self.selection.is_some() {
            self.clear_selection();
            return Ok(());
        }

        // Alt+I always enters secret mode; Ctrl+P without a selection still does.
        if (alt && key.code == KeyCode::Char('i')) || (ctrl && key.code == KeyCode::Char('p')) {
            self.mode = AppMode::SecretInput;
            self.status = "Enter the next level secret, then press Enter to verify.".to_owned();
            return Ok(());
        }

        self.clear_selection();
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
            if self.game().allows_private_key() {
                self.secret_kind = match self.secret_kind {
                    SecretKind::Password => SecretKind::PrivateKeyPath,
                    SecretKind::PrivateKeyPath => SecretKind::Password,
                };
            } else {
                self.status = format!("{} accepts passwords only.", self.game().name);
            }
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

    /// Validate the current input and claim a verification id. Kept separate
    /// from the worker-thread spawn so the state transition can be tested
    /// without launching an SSH process.
    fn begin_verification(&mut self) -> Option<(u64, u16, Secret, verify::VerifyCancel)> {
        let game = self.game().clone();
        if !matches!(self.verify, VerifyState::Idle) {
            self.status = "A verification is already in progress.".to_owned();
            return None;
        }
        if game.is_complete(self.current_level) {
            self.status = game.completion_message.clone();
            return None;
        }
        let input = self.secret_input.trim().to_owned();
        if input.is_empty() {
            self.status =
                "Enter a secret first (password or path to a local private key).".to_owned();
            return None;
        }
        let secret = match self.secret_kind {
            SecretKind::Password => Secret::Password(input),
            SecretKind::PrivateKeyPath => {
                if !game.allows_private_key() {
                    self.status = format!("{} accepts passwords only.", game.name);
                    return None;
                }
                let path = ssh::expand_key_path(&PathBuf::from(input));
                if !path.is_file() {
                    self.status = format!("Private key file not found: {}", path.display());
                    return None;
                }
                Secret::PrivateKeyPath(path)
            }
        };

        let id = self.next_request_id();
        let level = self.current_level.saturating_add(1).min(game.final_level);
        self.verify = VerifyState::Verifying { id, level };
        self.status = format!(
            "Verifying {} with {}…",
            game.level_label(level),
            secret.summary()
        );
        let cancel = verify::VerifyCancel::new();
        self.active_verify_cancel = Some(cancel.clone());
        Some((id, level, secret, cancel))
    }

    fn submit_secret(&mut self) {
        let Some((id, level, secret, cancel)) = self.begin_verification() else {
            return;
        };
        let game = self.game().clone();
        let tx = self.tx.clone();
        let verifier_secret = secret;
        let known_hosts = self.paths.known_hosts_file();
        let config_dir = self.paths.config_dir.clone();
        let game_id = game.id.clone();
        thread::spawn(move || {
            let result = verify::verify_secret(
                &game,
                level,
                &verifier_secret,
                verify::VERIFY_TIMEOUT,
                &known_hosts,
                &config_dir,
                &cancel,
            );
            let _ = tx.send(Message::Verified {
                id,
                level,
                game_id,
                secret: verifier_secret,
                result,
            });
        });
    }

    /// Apply a background-thread result. Stale results are ignored by id and
    /// by game identity, so switching games cannot advance the wrong progress.
    pub fn handle_message(&mut self, message: Message) -> Result<()> {
        match message {
            Message::LevelFetched {
                id,
                level,
                game_id,
                result,
            } => {
                if id != self.fetch_id
                    || level != self.current_level
                    || game_id != self.active_game_id
                {
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
                game_id,
                secret,
                result,
            } => {
                if !matches!(self.verify, VerifyState::Verifying { id: active, .. } if active == id)
                    || level != self.current_level.saturating_add(1)
                    || game_id != self.active_game_id
                {
                    return Ok(());
                }
                self.verify = VerifyState::Idle;
                self.active_verify_cancel = None;
                match result {
                    Ok(()) => self.advance(level, secret, game_id)?,
                    Err(error) => {
                        self.status = format!("Verification failed: {error}");
                    }
                }
            }
            Message::PtyExited { generation } => {
                if generation == self.generation {
                    self.clear_selection();
                    self.pty = None;
                    self.status = format!(
                        "SSH session for {} ended. Press F5 to reconnect.",
                        self.game().level_label(self.current_level)
                    );
                }
            }
        }
        Ok(())
    }

    fn advance(&mut self, next_level: u16, secret: Secret, game_id: String) -> Result<()> {
        let game = self.game().clone();
        self.credentials.set_last_level(&game_id, next_level);
        self.credentials
            .store_secret(&game_id, next_level, secret.clone());
        let save_warning = save_credentials(&self.paths.credentials_file(), &self.credentials)
            .err()
            .map(|error| format!(" WARNING: credentials were not saved: {error:#}"));

        if let Some(mut pty) = self.pty.take() {
            pty.terminate();
        }
        self.current_level = next_level.min(game.final_level);
        self.secret_input.clear();
        self.secret_cursor = 0;
        self.mode = AppMode::TerminalFocus;
        self.clear_selection();
        self.status = format!(
            "Verified! Connecting to {} with {}.{}",
            game.level_label(next_level),
            secret.summary(),
            save_warning.unwrap_or_default()
        );

        if let Err(error) = self.spawn_pty().with_context(|| "spawning next level PTY") {
            self.status = format!(
                "Could not start {} PTY: {error:#}",
                game.level_label(next_level)
            );
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

/// Password handed to the PTY reader for one-shot injection.
fn fallback_password(secret: Option<&Secret>, sshpass_available: bool) -> Option<String> {
    match secret {
        Some(Secret::Password(password)) if !sshpass_available => Some(password.clone()),
        _ => None,
    }
}

/// F1 and Ctrl+? open the keybinding overlay.
fn is_help_open_key(key: KeyEvent, ctrl: bool) -> bool {
    key.code == KeyCode::F(1) || (ctrl && key.code == KeyCode::Char('?'))
}

/// Convert a character index into a UTF-8 byte offset.
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
    use crate::widgets::vt100_widget::Vt100Widget;
    use crossterm::event::MouseEvent;
    use portable_pty::CommandBuilder;
    use ratatui::buffer::Buffer;
    use ratatui::style::Modifier;
    use ratatui::widgets::Widget;
    use std::num::NonZeroU16;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn test_app() -> App {
        let (tx, _rx) = mpsc::channel();
        let catalog = Catalog::parse(crate::wargame::DEFAULT_CATALOG).unwrap();
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
                config_dir: PathBuf::from("/tmp/wargames-tui-unit-config"),
                cache_dir: PathBuf::from("/tmp/wargames-tui-unit-cache"),
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
            catalog,
            active_game_id: "bandit".to_owned(),
            show_picker: false,
            startup_warning: None,
            picker_index: 0,
            picker_offset: 0,
            picker_from_startup: false,
            selection: None,
            native_mouse_mode: false,
            pending_clipboard: Vec::new(),
            drag: None,
            active_verify_cancel: None,
            tx,
            fetch_id: 0,
            next_request_id: 0,
            generation: 0,
        }
    }

    fn startup_app() -> App {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            cache_dir: dir.path().join("cache"),
        };
        let app = App::new(
            paths,
            Rect::new(0, 0, 80, 24),
            PtySize {
                rows: 22,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            tx,
        )
        .unwrap();
        // Keep the temp dir alive for the test's lifetime.
        std::mem::forget(dir);
        app
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(area: Rect) -> Vec<Event> {
        // crossterm reports 0-based coordinates: the last visible cell catches
        // an accidental +1/-1 conversion while staying inside the rect.
        let column = area.right().saturating_sub(1);
        let row = area.bottom().saturating_sub(1);
        vec![
            Event::Mouse(mouse(MouseEventKind::Down(MouseButton::Left), column, row)),
            Event::Mouse(mouse(MouseEventKind::Up(MouseButton::Left), column, row)),
        ]
    }

    #[test]
    fn pty_password_injection_only_for_plain_ssh_fallback() {
        const TEST_SECRET: &str = "test-only-placeholder";
        let password = Secret::Password(TEST_SECRET.to_owned());
        assert_eq!(
            fallback_password(Some(&password), false),
            Some(TEST_SECRET.to_owned())
        );
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
        for _ in 0..5 {
            app.handle_key(key(KeyCode::Left, KeyModifiers::NONE))
                .unwrap();
        }
        assert_eq!(app.secret_cursor, 1);
        app.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.secret_input, "omain");
        app.handle_key(key(KeyCode::Right, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(key(KeyCode::Delete, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.secret_input, "oain");
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
    fn stale_messages_are_ignored_including_other_games() {
        let mut app = test_app();
        app.generation = 3;
        app.verify = VerifyState::Verifying { id: 9, level: 1 };

        app.handle_message(Message::PtyExited { generation: 2 })
            .unwrap();
        assert!(app.status.is_empty());

        for game_id in ["old-game", "bandit"] {
            app.handle_message(Message::Verified {
                id: 8,
                level: 1,
                game_id: game_id.to_owned(),
                secret: Secret::Password("old".to_owned()),
                result: Ok(()),
            })
            .unwrap();
        }
        app.handle_message(Message::Verified {
            id: 9,
            level: 2,
            game_id: "bandit".to_owned(),
            secret: Secret::Password("old".to_owned()),
            result: Ok(()),
        })
        .unwrap();
        assert_eq!(app.verify, VerifyState::Verifying { id: 9, level: 1 });
        assert!(app.credentials.games.is_empty());
        assert_eq!(app.current_level, 0);

        app.handle_message(Message::LevelFetched {
            id: 1,
            level: 0,
            game_id: "bandit".to_owned(),
            result: Ok(LevelInfo {
                game_id: "bandit".to_owned(),
                source_cache_version: 1,
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
        app.handle_message(Message::LevelFetched {
            id: 1,
            level: 0,
            game_id: "century".to_owned(),
            result: Ok(LevelInfo {
                game_id: "century".to_owned(),
                source_cache_version: 1,
                level: 0,
                page: 0,
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
    fn current_generation_pty_exit_clears_session_and_points_to_f5() {
        let mut app = test_app();
        app.generation = 4;
        app.handle_message(Message::PtyExited { generation: 4 })
            .unwrap();
        assert!(app.pty.is_none());
        assert!(app.status.contains("ended"));
        assert!(app.status.contains("F5 to reconnect"));
        app.handle_key(key(KeyCode::Char('a'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.status.contains("No active SSH session"));
    }

    #[test]
    fn retry_action_reconnects_only_without_a_live_session() {
        assert_eq!(test_app().retry_action(), RetryAction::Reconnect);
    }

    #[cfg(unix)]
    #[test]
    fn retry_action_refetches_while_a_session_is_live() {
        assert_eq!(stub_pty_app(10).retry_action(), RetryAction::Refetch);
    }

    #[test]
    fn startup_picker_is_shown_before_any_session_starts() {
        let app = startup_app();
        assert!(app.show_picker);
        assert!(app.picker_from_startup);
        assert!(app.pty.is_none());
        assert_eq!(app.active_game_id, "bandit");
        assert_eq!(app.picker_index, 0);
    }

    #[test]
    fn f2_at_startup_confirms_the_game_instead_of_hiding_the_picker() {
        let mut app = startup_app();
        app.handle_event(Event::Key(key(KeyCode::F(2), KeyModifiers::NONE)))
            .unwrap();
        assert!(!app.show_picker);
        assert!(!app.picker_from_startup);
        assert_eq!(app.active_game_id, "bandit");
        assert_eq!(app.credentials.last_wargame, "bandit");
        assert_eq!(app.generation, 1, "F2 must go through start_session");

        // Once a session exists F2 reopens the picker and Esc cancels it.
        app.handle_event(Event::Key(key(KeyCode::F(2), KeyModifiers::NONE)))
            .unwrap();
        assert!(app.show_picker);
        assert!(!app.picker_from_startup);
        app.handle_event(Event::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        assert!(!app.show_picker);
        assert_eq!(app.generation, 1);
    }

    #[test]
    fn switching_games_clears_in_flight_verification() {
        let mut app = test_app();
        app.next_request_id = 9;
        let cancel = verify::VerifyCancel::new();
        app.verify = VerifyState::Verifying { id: 9, level: 1 };
        app.active_verify_cancel = Some(cancel.clone());
        app.status = "Verifying bandit1 with password…".to_owned();
        app.secret_input = "first-secret".to_owned();

        app.handle_event(Event::Key(key(KeyCode::F(2), KeyModifiers::NONE)))
            .unwrap();
        assert!(app.show_picker);
        assert_eq!(app.verify, VerifyState::Idle);
        assert!(cancel.is_cancelled(), "old verifier must be cancelled");
        assert!(app.next_request_id > 9, "old result id must be invalid");
        assert!(!app.status.contains("Verifying"));

        app.handle_event(Event::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .unwrap();
        app.handle_event(Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .unwrap();
        assert_eq!(app.active_game_id, "leviathan");
        assert_eq!(app.verify, VerifyState::Idle);

        let id_before_stale = app.next_request_id;
        app.handle_message(Message::Verified {
            id: 9,
            level: 1,
            game_id: "bandit".to_owned(),
            secret: Secret::Password("first-secret".to_owned()),
            result: Ok(()),
        })
        .unwrap();
        assert_eq!(app.verify, VerifyState::Idle);
        assert!(
            app.credentials.games.is_empty(),
            "stale result advanced progress"
        );
        // Leviathan starts at level 0, so switching keeps the initial level.
        assert_eq!(app.current_level, 0);

        app.secret_input = "second-secret".to_owned();
        let pending = app.begin_verification().unwrap();
        assert_eq!(pending.1, 1);
        assert!(app.next_request_id > id_before_stale);
        assert!(app.active_verify_cancel.is_some());
        // No worker was spawned by begin_verification; leave no background work.
        app.cancel_verification();
        assert_eq!(app.verify, VerifyState::Idle);
    }

    #[test]
    fn startup_credentials_warning_survives_session_start() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            cache_dir: dir.path().join("cache"),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.credentials_file(), "{ not json").unwrap();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            paths,
            Rect::new(0, 0, 80, 24),
            PtySize {
                rows: 22,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            tx,
        )
        .unwrap();
        assert!(app.status.contains("Could not load credentials"));
        app.start_session().unwrap();
        assert!(
            app.status.contains("Could not load credentials"),
            "startup warning was overwritten: {}",
            app.status
        );
    }

    #[test]
    fn picker_scroll_keeps_the_selected_row_visible() {
        let visible = usize::from(ui::PICKER_VISIBLE_ROWS.max(1));
        assert_eq!(scroll_picker_to(0, 0), 0);
        assert_eq!(scroll_picker_to(visible - 1, 0), 0);
        assert_eq!(scroll_picker_to(visible, 0), 1);
        assert_eq!(scroll_picker_to(11, 0), 11 + 1 - visible);
        assert_eq!(scroll_picker_to(11, 4), 4);
        assert_eq!(scroll_picker_to(3, 4), 3);
    }

    #[test]
    fn picker_keyboard_mouse_and_esc_flows() {
        let mut app = startup_app();
        app.handle_event(Event::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .unwrap();
        assert_eq!(app.picker_index, 1);
        app.handle_event(Event::Key(key(KeyCode::Up, KeyModifiers::NONE)))
            .unwrap();
        assert_eq!(app.picker_index, 0);

        // Esc at startup confirms the preselected game.
        app.handle_event(Event::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        assert!(!app.show_picker);
        assert_eq!(app.active_game_id, "bandit");
        assert_eq!(app.credentials.last_wargame, "bandit");
        assert_eq!(app.generation, 1);

        // F2 reopens; Esc there cancels without changing games.
        app.handle_event(Event::Key(key(KeyCode::F(2), KeyModifiers::NONE)))
            .unwrap();
        assert!(app.show_picker);
        app.handle_event(Event::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .unwrap();
        app.handle_event(Event::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        assert!(!app.show_picker);
        assert_eq!(app.active_game_id, "bandit");

        // Mouse row click selects and confirms a different game. Crossterm
        // coordinates are already 0-based, so use the row's exact position.
        app.handle_event(Event::Key(key(KeyCode::F(2), KeyModifiers::NONE)))
            .unwrap();
        let row = ui::picker_row_area(app.terminal_area, 1, app.picker_offset).unwrap();
        app.handle_event(Event::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            row.x,
            row.y,
        )))
        .unwrap();
        assert!(!app.show_picker);
        assert_eq!(app.active_game_id, "leviathan");
        assert_eq!(app.credentials.last_wargame, "leviathan");
        assert_eq!(app.generation, 2);
    }

    #[test]
    fn picker_consumes_keys_paste_and_mouse_and_help_wins() {
        let mut app = startup_app();
        app.show_help = true;
        app.handle_event(Event::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .unwrap();
        assert!(!app.show_help);
        assert!(app.show_picker);
        assert_eq!(app.picker_index, 0);
        app.handle_event(Event::Paste("ignored".to_owned()))
            .unwrap();
        app.handle_event(Event::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            1,
            1,
        )))
        .unwrap();
        assert!(app.show_picker);
        assert_eq!(app.picker_index, 0);
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

    #[test]
    fn ctrl_p_with_selection_replaces_secret_and_queues_copy() {
        let mut app = test_app();
        app.secret_input = "old".to_owned();
        app.secret_cursor = 3;
        app.secret_kind = SecretKind::PrivateKeyPath;
        app.selection = Some(TextSelection {
            start: CellPosition { row: 2, column: 1 },
            end: CellPosition { row: 2, column: 8 },
            scrollback: 0,
            text: "selected-secret".to_owned(),
        });
        app.handle_event(Event::Key(key(KeyCode::Char('p'), KeyModifiers::CONTROL)))
            .unwrap();
        assert_eq!(app.secret_input, "selected-secret");
        assert_eq!(app.secret_cursor, 15);
        assert_eq!(app.secret_kind, SecretKind::Password);
        assert_eq!(app.mode, AppMode::SecretInput);
        assert!(app.selection.is_none());
        assert_eq!(app.pending_clipboard, vec!["selected-secret".to_owned()]);
        assert!(app.status.contains("Selected text"));
    }

    #[test]
    fn ctrl_p_keeps_an_allowed_kind_for_key_only_catalogs() {
        let mut app = test_app();
        let mut game = app.game().clone();
        game.allowed_secret_kinds = vec![SecretKind::PrivateKeyPath];
        app.catalog.wargames[0] = game;
        app.secret_kind = SecretKind::PrivateKeyPath;
        app.selection = Some(TextSelection {
            start: CellPosition { row: 0, column: 0 },
            end: CellPosition { row: 0, column: 8 },
            scrollback: 0,
            text: "key-only-secret".to_owned(),
        });
        app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(app.secret_input, "key-only-secret");
        assert_eq!(app.secret_kind, SecretKind::PrivateKeyPath);
        assert!(
            app.status.contains("private key path only"),
            "{}",
            app.status
        );
        assert_eq!(app.pending_clipboard, vec!["key-only-secret".to_owned()]);
    }

    #[test]
    fn alt_m_toggles_native_mouse_and_ignores_mouse_while_on() {
        let mut app = test_app();
        assert!(!app.native_mouse_mode);
        app.handle_event(Event::Key(key(KeyCode::Char('m'), KeyModifiers::ALT)))
            .unwrap();
        assert!(app.native_mouse_mode);
        app.handle_event(Event::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            20,
            10,
        )))
        .unwrap();
        assert_eq!(app.mode, AppMode::TerminalFocus);
        assert!(app.drag.is_none());
        app.handle_event(Event::Key(key(KeyCode::Char('m'), KeyModifiers::ALT)))
            .unwrap();
        assert!(!app.native_mouse_mode);
    }

    #[test]
    fn selection_is_cleared_by_resize_typing_paste_and_esc() {
        let mut app = test_app();
        let set = |app: &mut App| {
            app.selection = Some(TextSelection {
                start: CellPosition { row: 0, column: 0 },
                end: CellPosition { row: 0, column: 3 },
                scrollback: 0,
                text: "text".to_owned(),
            });
        };
        set(&mut app);
        app.handle_event(Event::Resize(100, 30)).unwrap();
        assert!(app.selection.is_none());

        set(&mut app);
        app.handle_event(Event::Paste("x".to_owned())).unwrap();
        assert!(app.selection.is_none());

        set(&mut app);
        app.handle_event(Event::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        assert!(app.selection.is_none());
        assert_eq!(app.mode, AppMode::TerminalFocus);

        set(&mut app);
        app.handle_message(Message::PtyExited { generation: 0 })
            .unwrap();
        assert!(app.selection.is_none());
    }

    #[test]
    fn click_to_focus_and_resize_geometry_still_work() {
        let mut app = test_app();
        let secret = ui::secret_block_area(app.terminal_area);
        let ssh = ui::ssh_block_area(app.terminal_area);
        for event in click(secret) {
            app.handle_event(event).unwrap();
        }
        assert_eq!(app.mode, AppMode::SecretInput);
        assert!(app.status.contains("Secret box focused"));

        app.handle_event(Event::Key(key(KeyCode::Char('a'), KeyModifiers::NONE)))
            .unwrap();
        app.handle_event(Event::Paste("bc".to_owned())).unwrap();
        assert_eq!(app.secret_input, "abc");

        for event in click(ssh) {
            app.handle_event(event).unwrap();
        }
        assert_eq!(app.mode, AppMode::TerminalFocus);
        assert_eq!(app.secret_input, "abc");

        app.handle_event(Event::Resize(100, 30)).unwrap();
        assert_eq!(app.terminal_area, Rect::new(0, 0, 100, 30));
        let resized_secret = ui::secret_block_area(app.terminal_area);
        for event in click(resized_secret) {
            app.handle_event(event).unwrap();
        }
        assert_eq!(app.mode, AppMode::SecretInput);
        assert!(ui::rect_contains(
            resized_secret,
            resized_secret.x + 1,
            resized_secret.y + 1
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
            assert!(app.show_help);
            app.handle_event(Event::Key(key(KeyCode::Char('x'), KeyModifiers::NONE)))
                .unwrap();
            assert!(!app.show_help);
            assert!(app.secret_input.is_empty());
            assert_eq!(app.mode, AppMode::TerminalFocus);
        }
        app.mode = AppMode::SecretInput;
        app.handle_event(Event::Key(key(KeyCode::F(1), KeyModifiers::NONE)))
            .unwrap();
        app.handle_event(Event::Key(key(KeyCode::Char('z'), KeyModifiers::NONE)))
            .unwrap();
        assert_eq!(app.mode, AppMode::SecretInput);
        assert_eq!(app.secret_input, "");
    }

    #[test]
    fn help_consumes_paste_and_clicks_in_secret_mode() {
        let mut app = test_app();
        app.mode = AppMode::SecretInput;
        app.show_help = true;
        let secret = ui::secret_block_area(app.terminal_area);
        app.handle_event(Event::Paste("should-not-appear".to_owned()))
            .unwrap();
        for event in click(secret) {
            app.handle_event(event).unwrap();
        }
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
        app.handle_event(Event::Key(key(KeyCode::Char('a'), KeyModifiers::NONE)))
            .unwrap();
        app.handle_event(Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .unwrap();
        assert!(
            wait_for_screen(&app, "WROTE:", Duration::from_secs(3)),
            "post-help typing was not forwarded to the PTY"
        );
        assert!(!screen_text(&app).contains("WROTE:a"));
    }

    #[test]
    fn final_level_rejects_submission_and_uses_config_message() {
        let mut app = test_app();
        app.current_level = 33;
        app.secret_input = "anything".to_owned();
        app.submit_secret();
        assert_eq!(app.verify, VerifyState::Idle);
        assert!(app.status.contains("final level"));
    }

    #[test]
    fn final_level_shows_completion_without_a_fetch() {
        let mut app = test_app();
        app.current_level = 33;
        app.request_level(false);
        let LevelState::Ready(info) = &app.level else {
            panic!(
                "final level should be ready without fetching: {:?}",
                app.level
            );
        };
        assert_eq!(info.game_id, "bandit");
        assert_eq!(info.level, 33);
        assert_eq!(info.page, 34);
        assert_eq!(info.title, "bandit33");
        assert_eq!(info.goal, app.game().completion_message);
        assert!(info.commands.is_empty());
        assert!(app.status.contains("final level"));
        assert_eq!(app.fetch_id, 1);
    }

    #[test]
    fn completion_message_comes_from_another_games_config() {
        let mut app = test_app();
        app.active_game_id = "century".to_owned();
        app.current_level = 15;
        app.request_level(false);
        let LevelState::Ready(info) = &app.level else {
            panic!("final level should be ready: {:?}", app.level);
        };
        assert_eq!(info.game_id, "century");
        assert_eq!(info.page, 15);
        assert!(info.goal.contains("Century 15 is the final level"));
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
        assert_ne!(parser.screen().contents(), live);
        assert_eq!(
            scroll_parser(&mut parser, ScrollDirection::Up, 1000),
            history
        );
        assert_eq!(
            scroll_parser(&mut parser, ScrollDirection::Down, 4),
            history - 4
        );
        assert_eq!(snap_parser_to_live(&mut parser), 0);
        assert_eq!(scroll_parser(&mut parser, ScrollDirection::Down, 1), 0);
        assert_eq!(scroll_parser(&mut parser, ScrollDirection::Up, 0), 1);
    }

    #[test]
    fn selection_extraction_normalizes_and_skips_wide_continuations() {
        let mut parser = Parser::new(NonZeroU16::new(3).unwrap(), NonZeroU16::new(10).unwrap(), 0);
        parser.process(b"abc  \r\n");
        parser.process("界xyz\r\n".as_bytes());
        let text = App::extract_selection_text(
            &parser,
            CellPosition { row: 0, column: 0 },
            CellPosition { row: 1, column: 5 },
        )
        .unwrap();
        assert_eq!(text, "abc\n界xyz");
        assert_eq!(
            App::extract_selection_text(
                &parser,
                CellPosition { row: 1, column: 5 },
                CellPosition { row: 0, column: 0 },
            )
            .unwrap(),
            text
        );
    }

    fn selection_parser() -> Parser {
        let mut parser = Parser::new(NonZeroU16::new(3).unwrap(), NonZeroU16::new(10).unwrap(), 0);
        parser.process(b"abcdefghij\r\n0123456789\r\nABCDEFGHIJ");
        parser
    }

    fn cell(row: u16, column: u16) -> CellPosition {
        CellPosition { row, column }
    }

    #[test]
    fn multi_row_selection_keeps_each_rows_own_columns() {
        let parser = selection_parser();
        for (start, end, expected) in [
            (cell(0, 7), cell(1, 2), "hij\n012"),
            (cell(1, 2), cell(0, 7), "hij\n012"),
            (cell(0, 0), cell(1, 5), "abcdefghij\n012345"),
            (cell(1, 5), cell(0, 0), "abcdefghij\n012345"),
            (cell(0, 7), cell(0, 2), "cdefgh"),
            (cell(2, 1), cell(0, 8), "ij\n0123456789\nAB"),
        ] {
            assert_eq!(
                App::extract_selection_text(&parser, start, end).unwrap(),
                expected,
                "selection {start:?} -> {end:?}"
            );
        }
    }

    #[test]
    fn highlighted_cells_are_exactly_the_extracted_characters() {
        let parser = selection_parser();
        for (start, end) in [
            (cell(0, 7), cell(1, 2)),
            (cell(1, 2), cell(0, 7)),
            (cell(0, 0), cell(1, 5)),
            (cell(0, 7), cell(0, 2)),
        ] {
            let ranges = selection_rows(start, end, 3, 10);
            let area = Rect::new(0, 0, 10, 3);
            let mut buffer = Buffer::empty(area);
            Vt100Widget {
                screen: parser.screen(),
                selection: Some(Selection {
                    rows: ranges.clone(),
                }),
            }
            .render(area, &mut buffer);

            let highlighted = ranges
                .iter()
                .map(|selected| {
                    (selected.first_column..=selected.last_column)
                        .filter_map(|column| {
                            buffer
                                .cell((column, selected.row))
                                .filter(|cell| cell.modifier.contains(Modifier::REVERSED))
                                .map(|cell| cell.symbol().to_owned())
                        })
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(
                highlighted,
                App::extract_selection_text(&parser, start, end).unwrap(),
                "highlight mismatch for {start:?} -> {end:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rendering_with_an_active_selection_does_not_deadlock() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = stub_pty_app(6);
        app.selection = Some(TextSelection {
            start: cell(0, 1),
            end: cell(1, 3),
            scrollback: 0,
            text: "two lines".to_owned(),
        });
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal.draw(|frame| ui::render(frame, &app)).unwrap();
            let _ = done_tx.send(());
        });
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(5)),
            Ok(()),
            "rendering a selection must not re-lock the parser"
        );
    }

    #[test]
    fn mouse_drag_creates_selection_only_when_cells_differ() {
        let mut app = stub_pty_app(6);
        let inner = ui::ssh_inner_area(app.terminal_area);
        // Mouse coordinates are 0-based terminal cells; the inner area's origin
        // is the first vt100 cell, not one past it.
        let start = (inner.x, inner.y);
        let end = (inner.x + 3, inner.y);
        app.handle_event(Event::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            start.0,
            start.1,
        )))
        .unwrap();
        assert_eq!(app.mode, AppMode::TerminalFocus);
        app.handle_event(Event::Mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            end.0,
            end.1,
        )))
        .unwrap();
        app.handle_event(Event::Mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            end.0,
            end.1,
        )))
        .unwrap();
        let selection = app.selection.clone().unwrap();
        assert_eq!(selection.start, CellPosition { row: 0, column: 0 });
        assert_eq!(selection.end, CellPosition { row: 0, column: 3 });
        assert!(app.pending_clipboard.contains(&selection.text));
    }

    #[cfg(unix)]
    #[test]
    fn drag_highlights_the_same_screen_row_it_extracts() {
        let mut app = stub_pty_app(6);
        assert!(wait_for_screen(&app, "READY", Duration::from_secs(3)));
        app.handle_event(Event::Paste("SECRET\n".to_owned()))
            .unwrap();
        assert!(wait_for_screen(
            &app,
            "WROTE:SECRET",
            Duration::from_secs(3)
        ));

        let (screen_row, line) = {
            let parser = app.parser().unwrap();
            let guard = parser.lock().unwrap();
            let (rows, columns) = guard.screen().size();
            (0..rows.get())
                .find_map(|row| {
                    let mut line = String::new();
                    for column in 0..columns.get() {
                        if let Some(cell) = guard.screen().cell(row, column) {
                            line.push_str(cell.contents());
                        }
                    }
                    line.contains("WROTE:SECRET").then_some((row, line))
                })
                .expect("the echoed secret should be visible in the vt100 screen")
        };
        let first_column = line.find("SECRET").expect("secret text") as u16;
        let last_column = first_column + "SECRET".len() as u16 - 1;
        assert_eq!(
            &line[first_column as usize..last_column as usize + 1],
            "SECRET"
        );

        let inner = ui::ssh_inner_area(app.terminal_area);
        for (kind, column) in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ]
        .into_iter()
        .zip([first_column, last_column, last_column])
        {
            app.handle_event(Event::Mouse(mouse(
                kind,
                inner.x + column,
                inner.y + screen_row,
            )))
            .unwrap();
        }

        let selection = app.selection.as_ref().unwrap();
        assert_eq!(selection.text, "SECRET");
        assert_eq!(
            selection.start,
            CellPosition {
                row: screen_row,
                column: first_column
            }
        );
        assert_eq!(
            selection.end,
            CellPosition {
                row: screen_row,
                column: last_column
            }
        );
        assert_eq!(
            App::selection_highlight(&app).unwrap().rows,
            vec![SelectionRow {
                row: screen_row,
                first_column,
                last_column,
            }]
        );
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
        app.pty = Some(
            PtySession::spawn(
                "bandit0",
                command,
                None,
                app.pane_size,
                7,
                tx,
                |generation| generation,
            )
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
    fn screen_text(app: &App) -> String {
        app.parser()
            .and_then(|parser| parser.lock().ok().map(|guard| guard.screen().contents()))
            .unwrap_or_default()
    }

    #[cfg(unix)]
    #[test]
    fn page_up_scrolls_without_pty_writes_and_typing_returns_to_live() {
        let mut app = stub_pty_app(6);
        assert!(wait_for_screen(&app, "READY", Duration::from_secs(3)));
        let live = screen_text(&app);
        assert_eq!(app.scrollback_offset(), 0);
        app.handle_event(Event::Key(key(KeyCode::PageUp, KeyModifiers::NONE)))
            .unwrap();
        assert_eq!(app.scrollback_offset(), 6);
        assert_ne!(screen_text(&app), live);
        assert!(!app.status.contains("No active SSH session"));
        let mut release = key(KeyCode::PageUp, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        app.handle_event(Event::Key(release)).unwrap();
        assert_eq!(app.scrollback_offset(), 6, "releases must be ignored");
        app.handle_event(Event::Key(key(KeyCode::Char('a'), KeyModifiers::NONE)))
            .unwrap();
        app.handle_event(Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .unwrap();
        assert_eq!(app.scrollback_offset(), 0);
        assert!(wait_for_screen(&app, "WROTE:a", Duration::from_secs(3)));
    }

    #[cfg(unix)]
    #[test]
    fn mouse_wheel_scrolls_only_while_terminal_is_focused() {
        let mut app = stub_pty_app(6);
        assert!(wait_for_screen(&app, "READY", Duration::from_secs(3)));
        app.handle_event(Event::Mouse(mouse(MouseEventKind::ScrollUp, 1, 1)))
            .unwrap();
        assert_eq!(app.scrollback_offset(), WHEEL_SCROLL_LINES as usize);
        app.handle_event(Event::Mouse(mouse(MouseEventKind::ScrollDown, 1, 1)))
            .unwrap();
        assert_eq!(app.scrollback_offset(), 0);
        app.handle_event(Event::Mouse(mouse(MouseEventKind::ScrollUp, 1, 1)))
            .unwrap();
        app.mode = AppMode::SecretInput;
        app.handle_event(Event::Mouse(mouse(MouseEventKind::ScrollDown, 1, 1)))
            .unwrap();
        assert_eq!(app.scrollback_offset(), WHEEL_SCROLL_LINES as usize);
    }
}
