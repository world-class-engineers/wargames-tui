//! Ratatui layout and rendering.

use crate::app::{App, AppMode, LevelState, VerifyState};
use crate::credentials::Credentials;
use crate::wargame::Wargame;
use crate::widgets::vt100_widget::Vt100Widget;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use std::sync::OnceLock;

const TITLE_STYLE: Style = Style::new().fg(Color::Cyan).bold();
const HEADING_STYLE: Style = Style::new().fg(Color::Yellow).bold();
const MUTED_STYLE: Style = Style::new().fg(Color::Gray);
const ERROR_STYLE: Style = Style::new().fg(Color::Red).bold();
const HINT_STYLE: Style = Style::new().fg(Color::LightYellow);
const SELECTED_STYLE: Style = Style::new().fg(Color::Black).bg(Color::Cyan).bold();

/// Preferred size of the help and picker overlays. Both clamp to the terminal,
/// so they stay usable even below 80x24.
const HELP_OVERLAY_WIDTH: u16 = 72;
const HELP_OVERLAY_HEIGHT: u16 = 23;
const PICKER_OVERLAY_WIDTH: u16 = 92;
const PICKER_OVERLAY_HEIGHT: u16 = 20;
/// Rows of the picker list that fit inside the overlay.
pub const PICKER_VISIBLE_ROWS: u16 = 8;
/// Fixed columns in a picker row, before the clipped homepage URL.
const PICKER_NAME_WIDTH: usize = 24;
const PICKER_PROGRESS_WIDTH: usize = 12;

/// ANSI-styled ASCII art shown above the wargame list. Lines are 88 cells wide;
/// the overlay clips them gracefully on narrower terminals.
const PICKER_BANNER: [&str; 6] = [
    "\x1b[0;33m█\x1b[0;90;43m ░▒\x1b[0;37m \x1b[0;90;43m░ ░\x1b[0;33m█\x1b[0;37m \x1b[0;90;43m░ ░\x1b[0;33m█\x1b[0;37m \x1b[0;33m▄\x1b[0;90;43m ░▒\x1b[0;33m▀\x1b[0;90;43m░ ░\x1b[0;33m▄\x1b[0;37m \x1b[0;90;43m░ ░▒\x1b[0;33m▀\x1b[0;90;43m░ ░\x1b[0;33m▄\x1b[0;37m \x1b[0;33m▄\x1b[0;90;43m░ ░▒░ ░▒\x1b[0;37m \x1b[0;33m▄\x1b[0;90;43m ░▒\x1b[0;33m▀\x1b[0;90;43m░ ░\x1b[0;33m▄\x1b[0;37m \x1b[0;33m▄\x1b[0;90;43m ░▒\x1b[0;33m▀\x1b[0;90;43m░\x1b[0;33m▄██▀\x1b[0;90;43m░ ░\x1b[0;33m▄\x1b[0;37m \x1b[0;33m▄\x1b[0;90;43m░ ░▒░ ░\x1b[0;37m \x1b[0;33m▄\x1b[0;90;43m ░▒\x1b[0;33m▀\x1b[0;90;43m░ ░\x1b[0;33m█\x1b[0m",
    "\x1b[0;90;43m▒▒▓▒\x1b[0;37m \x1b[0;90;43m▓▒▓▓\x1b[0;37m \x1b[0;90;43m▓▒▓▓\x1b[0;37m \x1b[0;90;43m▒▒▓▒\x1b[0;37m \x1b[0;90;43m▓▒▓▓\x1b[0;37m \x1b[0;90;43m▒▒▓▒\x1b[0;37m \x1b[0;90;43m▓▒▓▓\x1b[0;37m \x1b[0;90;43m▓▒▓▓\x1b[0;37m \x1b[0;90;43m░▓▒▓\x1b[0;37m \x1b[0;90;43m▒▒▓▒\x1b[0;37m \x1b[0;90;43m▓▒▓▓\x1b[0;37m \x1b[0;90;43m▒▒▓▒\x1b[0;37m \x1b[0;90;43m▓▒▓▓\x1b[0;37m \x1b[0;90;43m▓▒▓▓\x1b[0;37m \x1b[0;90;43m▓▒▓▓\x1b[0;37m     \x1b[0;90;43m▒▒▓▒\x1b[0;37m \x1b[0;90;43m▓▒▓▓\x1b[0m",
    "\x1b[0;90m▀█\x1b[0;90;43m▓\x1b[0;90m▀\x1b[0;37m \x1b[0;90;43m▓\x1b[0;90m▀█▀\x1b[0;37m \x1b[0;90;43m▓\x1b[0;90m▀█▀\x1b[0;37m \x1b[0;90m█\x1b[0;90;43m▓\x1b[0;90m▀\x1b[0;90;43m▓\x1b[0;37m \x1b[0;90m█\x1b[0;90;43m▓\x1b[0;90m▀▀\x1b[0;37m \x1b[0;90m▀▓█▀▓▀\x1b[0;90;43m▓\x1b[0;90m█▀\x1b[0;37m \x1b[0;90m█\x1b[0;90;43m▓\x1b[0;90m▀▀\x1b[0;31m▄▄▄▄▄\x1b[0;37m \x1b[0;90m█\x1b[0;90;43m▓\x1b[0;90m▀\x1b[0;90;43m▓\x1b[0;37m \x1b[0;90m█\x1b[0;90;43m▓\x1b[0;90m▀▀\x1b[0;37m \x1b[0;90m▀█\x1b[0;90;43m▓\x1b[0;90m▀\x1b[0;37m \x1b[0;90;43m▓\x1b[0;90m▀█▀\x1b[0;37m \x1b[0;90;43m▓\x1b[0;90m▀█▀\x1b[0;37m \x1b[0;90m█\x1b[0;90;43m▓\x1b[0;90m▀▀█\x1b[0;90;43m▓\x1b[0;90m▀▀\x1b[0;37m \x1b[0;90m█\x1b[0;90;43m▓\x1b[0;90m▀\x1b[0;90;43m▓\x1b[0;37m \x1b[0;90m▀▀▀▀\x1b[0m",
    "\x1b[0;93;43m░\x1b[0;33m▄▄▓\x1b[0;37m \x1b[0;33m▄▓\x1b[0;31m▄\x1b[0;93;41m░\x1b[0;37m \x1b[0;33m▄▓\x1b[0;31m▄\x1b[0;93;41m░\x1b[0;37m \x1b[0;33m▄▄\x1b[0;93;43m \x1b[0;33m▄\x1b[0;90;43m▀\x1b[0;33m▄▄\x1b[0;93;43m ░\x1b[0;37m \x1b[0;93;43m░\x1b[0;33m▄▄\x1b[0;93;43m░\x1b[0;33m▀\x1b[0;93;43m░\x1b[0;33m▄\x1b[0;93;43m░\x1b[0;33m▄\x1b[0;37m \x1b[0;33m▄▄\x1b[0;93;43m ░\x1b[0;37m \x1b[0;93;41m░\x1b[0;31m███\x1b[0;37m \x1b[0;33m▄▄\x1b[0;93;43m \x1b[0;33m▄\x1b[0;90;43m▀\x1b[0;33m▄▄\x1b[0;93;43m ░\x1b[0;37m \x1b[0;93;43m░\x1b[0;33m▄▄▓\x1b[0;37m \x1b[0;33m▄▓\x1b[0;31m▄\x1b[0;93;41m░\x1b[0;37m \x1b[0;33m▄▓\x1b[0;31m▄\x1b[0;93;41m░\x1b[0;37m \x1b[0;33m▄▄\x1b[0;93;43m ░\x1b[0;33m▄▄\x1b[0;93;43m ░\x1b[0;37m \x1b[0;33m▄▄\x1b[0;93;43m \x1b[0;33m▄\x1b[0;90;43m▀\x1b[0;33m▄\x1b[0;93;41m░\x1b[0;93;43m \x1b[0;31m▄\x1b[0m",
    "\x1b[0;93;43m▒░ ░\x1b[0;37m \x1b[0;93;43m░ ▒░\x1b[0;37m \x1b[0;93;43m░ ▒░\x1b[0;37m \x1b[0;93;43m ░░ \x1b[0;37m \x1b[0;93;43m░▓▄▓\x1b[0;37m \x1b[0;93;43m▒░ ▓\x1b[0;37m \x1b[0;93;43m▒░▄░\x1b[0;37m \x1b[0;93;43m░ ▒▒\x1b[0;37m \x1b[0;93;41m▓▒░\x1b[0;31m█\x1b[0;37m \x1b[0;93;43m ░░ \x1b[0;37m \x1b[0;93;43m░▓▄▓\x1b[0;37m \x1b[0;93;43m▒░ ░\x1b[0;37m \x1b[0;93;43m░ ▒░\x1b[0;37m \x1b[0;93;43m░ ▒░\x1b[0;37m \x1b[0;93;43m░ ▒▒\x1b[0;37m     \x1b[0;33m▄▄▄▄\x1b[0;37m \x1b[0;93;43m░▓▄▓\x1b[0m",
    "\x1b[0;93m▀\x1b[0;93;43m█▄▒\x1b[0;93m▄\x1b[0;93;43m▓\x1b[0;93m█▀█▄\x1b[0;93;43m▓▓▒\x1b[0;93m▀\x1b[0;37m \x1b[0;93;43m░▓▒▓\x1b[0;37m \x1b[0;93;43m▓█▓█\x1b[0;37m \x1b[0;93;43m▓█▓█\x1b[0;37m \x1b[0;93m█▓█▓\x1b[0;37m \x1b[0;93m▀\x1b[0;93;43m▓█▓\x1b[0;93m█\x1b[0;93;43m▓█▓\x1b[0;93;41m▒\x1b[0;37m \x1b[0;93;43m░▓▒▓\x1b[0;37m \x1b[0;93;43m▓█▓█\x1b[0;37m \x1b[0;93m▓\x1b[0;93;43m█▄▒\x1b[0;37m \x1b[0;93m█\x1b[0;93;43m▓▒\x1b[0;93m▓\x1b[0;37m \x1b[0;93m█\x1b[0;93;43m▓▒\x1b[0;93m▓\x1b[0;37m \x1b[0;93m▀\x1b[0;93;43m▓█▓\x1b[0;93m█\x1b[0;93;43m▓█▓\x1b[0;37m \x1b[0;93;43m░▓▒▓\x1b[0;93m▄\x1b[0;93;43m▓█▓\x1b[0;93m▀\x1b[0m",
];

/// Split the terminal into the main panes and the one-line footer.
fn main_and_footer(terminal: Rect) -> [Rect; 2] {
    Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(terminal)
}

/// Split the main area into the left (level/secret/status) and SSH panes.
fn panes(main: Rect) -> [Rect; 2] {
    Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(main)
}

/// Split the left pane into level, secret, and status blocks.
fn left_sections(left: Rect) -> [Rect; 3] {
    Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(9),
        Constraint::Length(3),
    ])
    .areas(left)
}

/// The middle vertical block of the left pane: the "Next Level Secret" box.
pub fn secret_block_area(terminal: Rect) -> Rect {
    let [left, _] = panes(main_and_footer(terminal)[0]);
    left_sections(left)[1]
}

/// The right pane: the embedded SSH terminal block.
pub fn ssh_block_area(terminal: Rect) -> Rect {
    panes(main_and_footer(terminal)[0])[1]
}

/// Check a terminal-cell coordinate against a rendered block. Both ratatui
/// rectangles and crossterm mouse events use 0-based coordinates, so callers
/// can pass `MouseEvent::column`/`row` through unchanged.
pub fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && row >= area.y
        && column.saturating_sub(area.x) < area.width
        && row.saturating_sub(area.y) < area.height
}

/// The exact area available to the vt100 screen inside the SSH block.
pub fn ssh_inner_area(terminal: Rect) -> Rect {
    inner_block_area(ssh_block_area(terminal))
}

pub fn inner_block_area(block: Rect) -> Rect {
    Rect {
        x: block.x.saturating_add(1),
        y: block.y.saturating_add(1),
        width: block.width.saturating_sub(2),
        height: block.height.saturating_sub(2),
    }
}

/// Parse the fixed banner's SGR colors once into ratatui spans.
fn banner_lines() -> &'static [Line<'static>] {
    static LINES: OnceLock<Vec<Line<'static>>> = OnceLock::new();
    LINES.get_or_init(|| {
        PICKER_BANNER
            .iter()
            .map(|raw| parse_banner_line(raw))
            .collect()
    })
}

fn parse_banner_line(raw: &'static str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut style = Style::new();
    let mut remaining = raw;
    while let Some(escape) = remaining.find('\x1b') {
        let (text, sequence) = remaining.split_at(escape);
        if !text.is_empty() {
            spans.push(Span::styled(text, style));
        }
        let Some(end) = sequence.find('m') else {
            break;
        };
        style = apply_banner_sgr(style, sequence.get(2..end).unwrap_or_default());
        remaining = &sequence[end + 1..];
    }
    if !remaining.is_empty() {
        spans.push(Span::styled(remaining, style));
    }
    Line::from(spans)
}

fn apply_banner_sgr(mut style: Style, params: &str) -> Style {
    let codes: Vec<u16> = params
        .split(';')
        .filter_map(|code| code.parse().ok())
        .collect();
    if codes.contains(&0) {
        style = Style::new();
    }
    for code in codes {
        style = match code {
            31 => style.fg(Color::Red),
            33 => style.fg(Color::Yellow),
            37 => style.fg(Color::White),
            90 => style.fg(Color::DarkGray),
            93 => style.fg(Color::LightYellow),
            41 => style.bg(Color::Red),
            43 => style.bg(Color::Yellow),
            _ => style,
        };
    }
    style
}

/// Center a preferred-size rectangle inside `area`, clamping to its bounds.
fn centered_rect(preferred_width: u16, preferred_height: u16, area: Rect) -> Rect {
    let width = preferred_width.min(area.width);
    let height = preferred_height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn overlay_area(terminal: Rect) -> Rect {
    centered_rect(PICKER_OVERLAY_WIDTH, PICKER_OVERLAY_HEIGHT, terminal)
}

/// Banner area at the top of the picker overlay. On short terminals the banner
/// is shortened so the list and hint stay usable.
fn picker_banner_area(terminal: Rect) -> Rect {
    let overlay = overlay_area(terminal);
    let max_rows = overlay.height.saturating_sub(7);
    Rect {
        x: overlay.x.saturating_add(2),
        y: overlay.y.saturating_add(2),
        width: overlay.width.saturating_sub(4),
        height: max_rows.min(PICKER_BANNER.len() as u16),
    }
}

/// List area inside the picker overlay, below the banner.
fn picker_list_area(terminal: Rect) -> Rect {
    let overlay = overlay_area(terminal);
    let banner = picker_banner_area(terminal);
    let list_y = banner.y.saturating_add(banner.height).saturating_add(1);
    let hint_y = overlay.y.saturating_add(overlay.height.saturating_sub(2));
    Rect {
        x: overlay.x.saturating_add(2),
        y: list_y,
        width: overlay.width.saturating_sub(4),
        height: PICKER_VISIBLE_ROWS.min(hint_y.saturating_sub(list_y).saturating_sub(1)),
    }
}

/// Rectangle of one visible picker row, or `None` if it is scrolled out.
pub fn picker_row_area(terminal: Rect, index: usize, offset: usize) -> Option<Rect> {
    let list = picker_list_area(terminal);
    if index < offset || index >= offset.saturating_add(usize::from(list.height)) {
        return None;
    }
    Some(Rect {
        x: list.x,
        y: list.y.saturating_add((index - offset) as u16),
        width: list.width,
        height: 1,
    })
}

/// Pure mouse hit test for picker rows. Coordinates are 0-based, matching
/// crossterm `MouseEvent` fields directly.
pub fn picker_row_at(terminal: Rect, offset: usize, column: u16, row: u16) -> Option<usize> {
    let list = picker_list_area(terminal);
    (offset..offset.saturating_add(usize::from(list.height))).find(|&index| {
        picker_row_area(terminal, index, offset)
            .is_some_and(|area| rect_contains(area, column, row))
    })
}

pub fn render(frame: &mut Frame, app: &App) {
    let [main, footer] = main_and_footer(frame.area());
    let [left, right] = panes(main);
    let [level_area, secret_area, status_area] = left_sections(left);

    render_level(frame, level_area, app);
    render_secret(frame, secret_area, app);
    render_status(frame, status_area, app);
    render_terminal(frame, right, app);
    render_footer(frame, footer, app);
    if app.show_picker {
        render_picker(frame, app);
    }
    if app.show_help {
        render_help(frame);
    }
}

/// Compact progress line derived entirely from the active wargame. Completed
/// games show a single status so "Cleared: 14" is not paired with a level-15
/// congratulations screen and mistaken for an unfinished level.
fn format_progress(game: &Wargame, level: u16) -> String {
    if game.is_complete(level) {
        return format!(
            "Complete — {} / {}",
            game.level_label(level),
            game.final_level
        );
    }
    format!(
        "Cleared: {}  |  Now: {} / {}",
        level.saturating_sub(game.first_level),
        game.level_label(level),
        game.final_level
    )
}

fn render_level(frame: &mut Frame, area: Rect, app: &App) {
    let game = app.game();
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        format_progress(game, app.current_level),
        MUTED_STYLE,
    ))];
    match &app.level {
        LevelState::Loading => lines.push(Line::from(Span::styled(
            "Loading level instructions…",
            MUTED_STYLE,
        ))),
        LevelState::Error { message } => {
            lines.push(Line::from(Span::styled(
                "Could not load level instructions",
                ERROR_STYLE,
            )));
            lines.push(Line::from(Span::styled(message.clone(), ERROR_STYLE)));
            lines.push(Line::from(Span::styled(
                "Press r (or F5) to retry. The SSH session is still usable.",
                HINT_STYLE,
            )));
        }
        LevelState::Ready(info) => {
            lines.push(Line::from(Span::styled(
                format!("{} — Level Goal", info.title),
                TITLE_STYLE,
            )));
            if !info.goal.is_empty() {
                lines.push(Line::from(Span::styled("Goal", HEADING_STYLE)));
                lines.extend(info.goal.lines().map(|line| Line::raw(line.to_owned())));
            }
            if !info.commands.is_empty() {
                lines.push(Line::from(Span::styled(
                    "Commands you may need",
                    HEADING_STYLE,
                )));
                lines.push(Line::raw(info.commands.join(", ")));
            }
            if !info.reading.is_empty() {
                lines.push(Line::from(Span::styled("Helpful reading", HEADING_STYLE)));
                lines.extend(info.reading.iter().map(|line| Line::raw(line.clone())));
            }
            if !info.notes.is_empty() {
                lines.push(Line::from(Span::styled("Notes", HEADING_STYLE)));
                lines.extend(info.notes.iter().map(|line| Line::raw(line.clone())));
            }
        }
    }

    let focused = app.mode == AppMode::SecretInput;
    let block = Block::new()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            format!(" {} ", game.name),
            if focused { MUTED_STYLE } else { TITLE_STYLE },
        )));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_secret(frame: &mut Frame, area: Rect, app: &App) {
    let editing = app.mode == AppMode::SecretInput;
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(if editing {
            Style::new().fg(Color::Yellow)
        } else {
            MUTED_STYLE
        })
        .title(Line::from(Span::styled(
            " Next Level Secret ",
            if editing {
                Style::new().fg(Color::Yellow).bold()
            } else {
                MUTED_STYLE
            },
        )));
    let inner = inner_block_area(area);
    let available = inner.width.saturating_sub(1) as usize;
    let visible: String = app.secret_input.chars().take(available).collect();
    let input_empty = visible.is_empty();
    let kind_hint = if app.game().allows_private_key() {
        "  (Tab toggles password / private key path)"
    } else {
        "  (password only)"
    };
    let kind_line = Line::from(vec![
        Span::styled("Type: ", MUTED_STYLE),
        Span::styled(app.secret_kind.label(), HEADING_STYLE),
        Span::styled(kind_hint, MUTED_STYLE),
    ]);
    let input_line = Line::from(vec![Span::styled(
        if input_empty {
            "Enter secret…".to_owned()
        } else {
            visible
        },
        if input_empty {
            MUTED_STYLE
        } else {
            Style::new().fg(Color::White)
        },
    )]);
    let help_line = match app.verify {
        VerifyState::Verifying { level, .. } => Line::from(Span::styled(
            format!("Verifying {}…", app.game().level_label(level)),
            HINT_STYLE,
        )),
        VerifyState::Idle => Line::from(Span::styled(
            if editing {
                "Enter: verify    Esc: back to SSH    Ctrl+Q: quit"
            } else {
                "Alt+I (or Ctrl+P): enter secret mode"
            },
            MUTED_STYLE,
        )),
    };
    frame.render_widget(
        Paragraph::new(Text::from(vec![kind_line, input_line, help_line]))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
    if editing && !app.show_help {
        let cursor = secret_cursor_column(&app.secret_input, app.secret_cursor, available);
        frame.set_cursor_position((inner.x + cursor as u16, inner.y + 1));
    }
}

/// Display column for a character-index cursor, clamped to the visible input.
fn secret_cursor_column(secret_input: &str, char_cursor: usize, available: usize) -> usize {
    let visible: String = secret_input.chars().take(available).collect();
    let prefix: String = visible
        .chars()
        .take(char_cursor.min(visible.chars().count()))
        .collect();
    Span::raw(prefix).width().min(available)
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    let text = if app.status.is_empty() {
        match &app.level {
            LevelState::Error { .. } => {
                "Level instructions unavailable; SSH session is live.".to_owned()
            }
            _ => "Ready. Type in the SSH pane to play the level.".to_owned(),
        }
    } else {
        app.status.clone()
    };
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::new().borders(Borders::ALL).title(" Status "))
            .style(HINT_STYLE)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// SSH block title, with the configured endpoint and a history indicator.
fn ssh_title(game: &Wargame, level: u16, scrollback: usize) -> String {
    let base = format!(
        " SSH — {}@{}:{} ",
        game.username(level),
        game.ssh_host,
        game.ssh_port
    );
    if scrollback == 0 {
        base
    } else {
        format!("{base}[scrollback: {scrollback} lines up] ")
    }
}

fn render_terminal(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.mode == AppMode::TerminalFocus;
    let scrollback = app.scrollback_offset();
    let title = ssh_title(app.game(), app.current_level, scrollback);
    let border_style = if scrollback > 0 {
        Style::new().fg(Color::LightYellow).bold()
    } else if focused {
        Style::new().fg(Color::Yellow)
    } else {
        MUTED_STYLE
    };
    frame.render_widget(
        Block::new()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(title, border_style)))
            .border_style(border_style),
        area,
    );

    let inner = inner_block_area(area);
    frame.render_widget(Clear, inner);
    if let Some(parser) = app.parser() {
        // Compute the highlight before locking the parser: the helper locks it
        // briefly to read the screen size, and std Mutex is not reentrant.
        let selection = App::selection_highlight(app);
        let Ok(guard) = parser.lock() else {
            return;
        };
        let screen = guard.screen();
        frame.render_widget(Vt100Widget { screen, selection }, inner);
        // `cell()` follows the scrollback offset, but `cursor_position()` does
        // not; only show the cursor on the live screen in terminal mode.
        if focused && !app.show_help && scrollback == 0 {
            let (row, column) = screen.cursor_position();
            if row < inner.height && column < inner.width {
                frame.set_cursor_position((inner.x + column, inner.y + row));
            }
        }
    } else {
        frame.render_widget(
            Paragraph::new(if app.show_picker {
                "Choose a wargame to begin."
            } else {
                "No SSH session is running. Press F5 to reconnect."
            })
            .style(ERROR_STYLE),
            inner,
        );
    }
}

const TERMINAL_HINT_WIDE: &str = "F2: games   PgUp/PgDn/wheel: scroll   drag: select   Ctrl+P: secret   Alt+M: native   Ctrl+Q: quit   F5: retry/reconnect   F1/Ctrl+?: keys";
const TERMINAL_HINT_NARROW: &str =
    "F2 games PgUp/PgDn scroll Drag/Ctrl+P secret Alt+M native F1/Ctrl+? keys";
const SECRET_HINT_WIDE: &str = "Tab: password/key path   Enter: verify   Esc: back   Ctrl+P: use selection   F2: games   Ctrl+Q: quit   F1/Ctrl+?: keys";
const SECRET_HINT_NARROW: &str =
    "Tab type Enter verify Esc back Ctrl+Q quit F2 games Alt+M native F1/Ctrl+? keys";

fn footer_hint(mode: AppMode, width: u16) -> &'static str {
    match mode {
        AppMode::SecretInput => {
            if width >= 120 {
                SECRET_HINT_WIDE
            } else {
                SECRET_HINT_NARROW
            }
        }
        AppMode::TerminalFocus => {
            if width >= 130 {
                TERMINAL_HINT_WIDE
            } else {
                TERMINAL_HINT_NARROW
            }
        }
    }
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let hint = footer_hint(app.mode, area.width);
    let style = if app.mode == AppMode::SecretInput {
        Style::new().fg(Color::Yellow)
    } else {
        MUTED_STYLE
    };
    frame.render_widget(
        Paragraph::new(Text::from(Line::from(Span::styled(hint, style))))
            .style(Style::new().bg(Color::DarkGray)),
        area,
    );
}

fn picker_hint(from_startup: bool) -> &'static str {
    if from_startup {
        "Up/Down: choose   Enter/Esc/F2: start   click: start"
    } else {
        "Up/Down: choose   Enter: start   Esc: cancel   F2: close   click: start"
    }
}

/// Per-game progress shown in the picker. Counts cleared levels rather than
/// displaying the current level name, so an unstarted Krypton game reads
/// `Cleared 0/6` instead of looking like level 1 is already complete.
fn picker_progress(game: &Wargame, credentials: &Credentials) -> String {
    let level = game.clamp_level(credentials.progress(&game.id).last_level);
    if game.is_complete(level) {
        "Done".to_owned()
    } else {
        let cleared = level.saturating_sub(game.first_level);
        let total = game.final_level.saturating_sub(game.first_level);
        format!("Cleared {cleared}/{total}")
    }
}

fn truncate_with_ellipsis(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut truncated: String = text.chars().take(width - 1).collect();
    truncated.push('…');
    truncated
}

fn render_picker(frame: &mut Frame, app: &App) {
    let area = overlay_area(frame.area());
    frame.render_widget(Clear, area);
    let block = Block::new()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(" Choose a Wargame ", TITLE_STYLE)))
        .border_style(Style::new().fg(Color::Cyan).bold())
        .style(Style::new().bg(Color::Black));
    frame.render_widget(block, area);

    let banner = picker_banner_area(frame.area());
    for (index, line) in banner_lines()
        .iter()
        .take(usize::from(banner.height))
        .enumerate()
    {
        frame.render_widget(
            Paragraph::new(line.clone()),
            Rect {
                x: banner.x,
                y: banner.y.saturating_add(index as u16),
                width: banner.width,
                height: 1,
            },
        );
    }

    let list = picker_list_area(frame.area());
    let visible = usize::from(list.height);
    for (row, game) in app
        .catalog
        .wargames
        .iter()
        .enumerate()
        .skip(app.picker_offset)
        .take(visible)
    {
        let selected = row == app.picker_index;
        let marker = if selected { ">" } else { " " };
        let name = format!("{:<width$}", game.name, width = PICKER_NAME_WIDTH);
        let progress = format!(
            "{:>width$}",
            picker_progress(game, &app.credentials),
            width = PICKER_PROGRESS_WIDTH
        );
        let fixed = 2 + PICKER_NAME_WIDTH + 1 + PICKER_PROGRESS_WIDTH + 1;
        let homepage = truncate_with_ellipsis(
            &game.homepage,
            usize::from(list.width).saturating_sub(fixed),
        );
        let label = format!("{marker} {name} {progress} {homepage}");
        let span = if selected {
            Span::styled(label, SELECTED_STYLE)
        } else {
            Span::raw(label)
        };
        frame.render_widget(
            Paragraph::new(Line::from(span)),
            picker_row_area(frame.area(), row, app.picker_offset).unwrap_or(list),
        );
    }

    let hint = Rect {
        x: area.x.saturating_add(2),
        y: area.y.saturating_add(area.height.saturating_sub(2)),
        width: area.width.saturating_sub(4),
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(picker_hint(app.picker_from_startup)).style(MUTED_STYLE),
        hint,
    );
}

/// All keybindings, grouped by context. Kept short enough for an 80x24
/// terminal: 21 content lines plus the overlay's two border rows.
fn help_lines() -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    {
        let mut section = |title: &'static str, body: &[&'static str]| {
            lines.push(Line::from(Span::styled(title, HEADING_STYLE)));
            lines.extend(body.iter().map(|item| Line::raw(*item)));
            lines.push(Line::raw(""));
        };
        section(
            "Wargame picker",
            &[
                "  F2 -> open (Esc: cancel when reopened)",
                "  Up/Down / mouse row -> choose; Enter / click -> start",
                "  At startup Enter / Esc / F2 -> start the chosen game",
            ],
        );
        section(
            "SSH terminal",
            &[
                "  type / paste -> remote shell",
                "  drag -> select text   Ctrl+P -> use it as the secret + copy",
                "  Alt+I / click secret box -> secret input",
                "  PgUp / PgDn / wheel -> scroll; typing -> live",
                "  Alt+M -> native terminal selection (Alt+M restores)",
            ],
        );
        section(
            "Secret input",
            &[
                "  type / paste -> insert   Tab -> password/key path",
                "  Backspace / Delete / arrows / Home / End -> edit",
                "  Enter -> verify   Esc / Ctrl+G -> back to SSH",
                "  Ctrl+C / Ctrl+Q -> quit   F5 / Alt+R -> retry fetch / reconnect",
            ],
        );
        section("Help", &["  F1 / Ctrl+? -> open   any key / Esc -> close"]);
    }
    lines
}

/// Last thing drawn: covers the panes without touching parser state.
pub fn render_help(frame: &mut Frame) {
    let area = centered_rect(HELP_OVERLAY_WIDTH, HELP_OVERLAY_HEIGHT, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::new()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            " Keybindings (any key closes) ",
            TITLE_STYLE,
        )))
        .border_style(Style::new().fg(Color::Cyan).bold())
        .style(Style::new().bg(Color::Black));
    frame.render_widget(
        Paragraph::new(Text::from(help_lines()))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wargame::Catalog;

    fn catalog() -> Catalog {
        Catalog::parse(crate::wargame::DEFAULT_CATALOG).unwrap()
    }

    #[test]
    fn progress_line_is_config_driven() {
        let games = catalog();
        let bandit = games.get("bandit").unwrap();
        let century = games.get("century").unwrap();
        assert_eq!(
            format_progress(bandit, 0),
            "Cleared: 0  |  Now: bandit0 / 33"
        );
        assert_eq!(format_progress(bandit, 33), "Complete — bandit33 / 33");
        assert_eq!(
            format_progress(century, 3),
            "Cleared: 2  |  Now: Century 3 / 15"
        );
        assert_eq!(format_progress(century, 15), "Complete — Century 15 / 15");
    }

    #[test]
    fn picker_progress_reflects_each_games_own_credentials() {
        let games = catalog();
        let mut credentials = Credentials::default();
        assert_eq!(
            picker_progress(games.get("bandit").unwrap(), &credentials),
            "Cleared 0/33"
        );
        assert_eq!(
            picker_progress(games.get("century").unwrap(), &credentials),
            "Cleared 0/14"
        );
        assert_eq!(
            picker_progress(games.get("krypton").unwrap(), &credentials),
            "Cleared 0/6"
        );

        credentials.set_last_level("bandit", 12);
        credentials.set_last_level("century", 3);
        assert_eq!(
            picker_progress(games.get("bandit").unwrap(), &credentials),
            "Cleared 12/33"
        );
        assert_eq!(
            picker_progress(games.get("century").unwrap(), &credentials),
            "Cleared 2/14"
        );

        credentials.set_last_level("bandit", 33);
        credentials.set_last_level("century", 99);
        assert_eq!(
            picker_progress(games.get("bandit").unwrap(), &credentials),
            "Done"
        );
        assert_eq!(
            picker_progress(games.get("century").unwrap(), &credentials),
            "Done"
        );
    }

    #[test]
    fn picker_homepage_is_clipped_to_the_available_columns() {
        assert_eq!(truncate_with_ellipsis("short", 10), "short");
        assert_eq!(truncate_with_ellipsis("abcdefghij", 10), "abcdefghij");
        assert_eq!(truncate_with_ellipsis("abcdefghijk", 10), "abcdefghi…");
        assert_eq!(truncate_with_ellipsis("abc", 1), "…");
        assert_eq!(truncate_with_ellipsis("abc", 0), "");
    }

    #[test]
    fn secret_cursor_column_uses_character_indices() {
        assert_eq!(secret_cursor_column("éomain", 1, 10), 1);
        assert_eq!(secret_cursor_column("éomain", 2, 10), 2);
        assert_eq!(secret_cursor_column("éomain", 99, 10), 6);
        assert_eq!(secret_cursor_column("中éx", 2, 10), 3);
        assert_eq!(secret_cursor_column("éomain", 6, 3), 3);
    }

    #[test]
    fn scrollback_and_endpoint_are_visible_in_title() {
        let games = catalog();
        let bandit = games.get("bandit").unwrap();
        let century = games.get("century").unwrap();
        assert_eq!(
            ssh_title(bandit, 0, 0),
            " SSH — bandit0@bandit.labs.overthewire.org:2220 "
        );
        let title = ssh_title(century, 3, 12);
        assert!(title.contains("century3@century.underthewire.tech:22"));
        assert!(title.contains("[scrollback: 12 lines up]"));
    }

    #[test]
    fn footer_hints_advertise_new_features_and_fit() {
        assert!(footer_hint(AppMode::TerminalFocus, 140).contains("F2: games"));
        assert!(footer_hint(AppMode::TerminalFocus, 140).contains("Alt+M"));
        assert!(footer_hint(AppMode::TerminalFocus, 140).contains("Ctrl+P"));
        for width in [80, 92, 100, 120, 140] {
            for mode in [AppMode::TerminalFocus, AppMode::SecretInput] {
                let hint = footer_hint(mode, width);
                assert!(
                    hint.chars().count() <= usize::from(width),
                    "hint must fit {width} columns: {hint}"
                );
                assert!(
                    hint.contains("F1/Ctrl+?"),
                    "hint must advertise help: {hint}"
                );
            }
        }
        assert!(!footer_hint(AppMode::SecretInput, 80).contains("PgUp/PgDn"));
    }

    #[test]
    fn ssh_inner_area_matches_border() {
        let terminal = Rect::new(0, 0, 120, 40);
        let block = ssh_block_area(terminal);
        let inner = ssh_inner_area(terminal);
        assert_eq!(inner.x, block.x + 1);
        assert_eq!(inner.y, block.y + 1);
        assert_eq!(inner.width, block.width.saturating_sub(2));
        assert_eq!(inner.height, block.height.saturating_sub(2));
    }

    #[test]
    fn secret_area_is_the_middle_left_pane_block() {
        let terminal = Rect::new(0, 0, 120, 40);
        let [main, footer] = main_and_footer(terminal);
        let [left, right] = panes(main);
        let [level, secret, status] = left_sections(left);
        assert_eq!(footer.height, 1);
        assert_eq!(secret_block_area(terminal), secret);
        assert_eq!(ssh_block_area(terminal), right);
        assert!(level.y < secret.y);
        assert!(secret.y + secret.height <= status.y);
    }

    #[test]
    fn click_hit_testing_matches_rendered_areas() {
        let terminal = Rect::new(0, 0, 80, 24);
        let secret = secret_block_area(terminal);
        let ssh = ssh_block_area(terminal);
        let secret_cell = (secret.x + secret.width - 1, secret.y + secret.height - 1);
        let ssh_cell = (ssh.x + ssh.width - 1, ssh.y + ssh.height - 1);
        assert!(rect_contains(secret, secret_cell.0, secret_cell.1));
        assert!(!rect_contains(ssh, secret_cell.0, secret_cell.1));
        assert!(rect_contains(ssh, ssh_cell.0, ssh_cell.1));
        let footer_row = terminal.y + terminal.height - 1;
        assert!(!rect_contains(secret, secret_cell.0, footer_row));
    }

    #[test]
    fn picker_rows_are_hit_testable_and_clamped() {
        let terminal = Rect::new(0, 0, 80, 24);
        let row = picker_row_area(terminal, 1, 0).unwrap();
        assert_eq!(row.height, 1);
        assert_eq!(picker_row_at(terminal, 0, row.x, row.y), Some(1));
        assert_eq!(picker_row_at(terminal, 0, row.x + row.width, row.y), None);
        let visible = usize::from(picker_list_area(terminal).height);
        assert_eq!(picker_row_area(terminal, 3 + visible, 3), None);
        let scrolled = picker_row_area(terminal, 5, 3).unwrap();
        assert_eq!(picker_row_at(terminal, 3, scrolled.x, scrolled.y), Some(5));
        let tiny = Rect::new(0, 0, 20, 8);
        let clamped = picker_row_area(tiny, 0, 0).unwrap();
        assert!(clamped.width > 0 && clamped.height == 1);
        assert!(centered_rect(PICKER_OVERLAY_WIDTH, PICKER_OVERLAY_HEIGHT, tiny).width <= 20);
    }

    #[test]
    fn picker_banner_sits_above_the_list_with_its_colors() {
        let terminal = Rect::new(0, 0, 160, 45);
        let overlay = overlay_area(terminal);
        let banner = picker_banner_area(terminal);
        let list = picker_list_area(terminal);
        assert_eq!(banner.height, 6);
        assert_eq!(banner.width, 88);
        assert_eq!(banner.x, overlay.x + 2);
        assert!(banner.y + banner.height < list.y);

        let lines = banner_lines();
        assert_eq!(lines.len(), 6);
        let first = &lines[0];
        assert_eq!(
            first
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .chars()
                .count(),
            88
        );
        assert!(
            first
                .spans
                .iter()
                .any(|span| span.style.fg == Some(Color::Yellow))
        );
        assert!(
            first
                .spans
                .iter()
                .any(|span| span.style.bg == Some(Color::Yellow))
        );
        assert!(
            lines[3]
                .spans
                .iter()
                .any(|span| span.style.bg == Some(Color::Red))
        );
    }

    #[test]
    fn picker_banner_shortens_on_tiny_terminals() {
        let tiny = Rect::new(0, 0, 20, 8);
        assert_eq!(picker_banner_area(tiny).height, 1);
        assert_eq!(picker_list_area(tiny).height, 1);
    }

    #[test]
    fn picker_hint_distinguishes_startup_from_reopened() {
        assert!(picker_hint(true).contains("Esc/F2: start"));
        assert!(!picker_hint(true).contains("cancel"));
        assert!(picker_hint(false).contains("Esc: cancel"));
        assert!(picker_hint(false).contains("F2: close"));
    }

    #[test]
    fn help_overlay_fits_at_80x24_and_clamps_to_small_terminals() {
        let lines = help_lines();
        assert!(
            lines.len() + 2 <= 24,
            "overlay plus borders must fit 24 rows"
        );
        assert!(
            lines
                .iter()
                .all(|line| line.width() <= usize::from(HELP_OVERLAY_WIDTH - 2))
        );
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        for binding in [
            "F2",
            "Alt+M",
            "Ctrl+P",
            "type / paste",
            "PgUp / PgDn / wheel",
            "Tab",
            "Enter",
            "Esc / Ctrl+G",
            "F1 / Ctrl+?",
            "any key / Esc",
        ] {
            assert!(text.contains(binding), "help is missing {binding}");
        }
        for area in [
            Rect::new(0, 0, 80, 24),
            Rect::new(3, 2, 40, 10),
            Rect::new(5, 5, 10, 4),
        ] {
            let overlay = centered_rect(HELP_OVERLAY_WIDTH, HELP_OVERLAY_HEIGHT, area);
            assert!(overlay.x >= area.x && overlay.y >= area.y);
            assert!(overlay.x + overlay.width <= area.x + area.width);
            assert!(overlay.y + overlay.height <= area.y + area.height);
        }
    }
}
