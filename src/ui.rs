//! Ratatui layout and rendering.

use crate::app::{App, AppMode, LevelState, VerifyState};
use crate::credentials::MAX_LEVEL;
use crate::widgets::vt100_widget::Vt100Widget;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

const TITLE_STYLE: Style = Style::new().fg(Color::Cyan).bold();
const HEADING_STYLE: Style = Style::new().fg(Color::Yellow).bold();
const MUTED_STYLE: Style = Style::new().fg(Color::Gray);
const ERROR_STYLE: Style = Style::new().fg(Color::Red).bold();
const HINT_STYLE: Style = Style::new().fg(Color::LightYellow);

/// Width/height the help overlay prefers. It is clamped to the terminal, so
/// it stays usable even below 80x24.
const HELP_OVERLAY_WIDTH: u16 = 68;
const HELP_OVERLAY_HEIGHT: u16 = 21;

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
/// Exposed so mouse clicks can be hit-tested against exactly what is rendered.
pub fn secret_block_area(terminal: Rect) -> Rect {
    let [left, _] = panes(main_and_footer(terminal)[0]);
    left_sections(left)[1]
}

/// The right pane: the embedded SSH terminal block.
pub fn ssh_block_area(terminal: Rect) -> Rect {
    panes(main_and_footer(terminal)[0])[1]
}

/// Check a **0-based** terminal-cell coordinate against a rendered block.
/// Callers with crossterm mouse events must subtract 1 from `column`/`row`
/// first. Uses subtraction instead of `x + width` to avoid overflowing on
/// degenerate rectangles.
pub fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && row >= area.y
        && column.saturating_sub(area.x) < area.width
        && row.saturating_sub(area.y) < area.height
}

/// The exact area available to the vt100 screen inside the SSH block. Used by
/// the app to keep the parser and PTY sizes in sync with the rendered border.
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

pub fn render(frame: &mut Frame, app: &App) {
    let [main, footer] = main_and_footer(frame.area());
    let [left, right] = panes(main);
    let [level_area, secret_area, status_area] = left_sections(left);

    render_level(frame, level_area, app);
    render_secret(frame, secret_area, app);
    render_status(frame, status_area, app);
    render_terminal(frame, right, app);
    render_footer(frame, footer, app);
    if app.show_help {
        render_help(frame);
    }
}

/// Compact progress line. `cleared` is the number of finished levels and
/// `current` is the bandit number being played; they coincide because levels
/// start at 0, so `MAX_LEVEL` is the final level number, not a level count.
fn format_progress(cleared: u16, current: u16) -> String {
    format!("Cleared: {cleared}  |  Now: bandit{current} / {MAX_LEVEL}")
}

fn render_level(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        format_progress(app.current_level, app.current_level),
        MUTED_STYLE,
    ))];
    match &app.level {
        LevelState::Loading => {
            lines.push(Line::from(Span::styled(
                "Loading level instructions…",
                MUTED_STYLE,
            )));
        }
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
            format!(" Bandit {} ", app.current_level),
            if focused { MUTED_STYLE } else { TITLE_STYLE },
        )));
    let paragraph = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
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
    let kind_line = Line::from(vec![
        Span::styled("Type: ", MUTED_STYLE),
        Span::styled(app.secret_kind.label(), HEADING_STYLE),
        Span::styled("  (Tab toggles password / private key path)", MUTED_STYLE),
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
            format!("Verifying bandit{level}…"),
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
    let block = Block::new().borders(Borders::ALL).title(" Status ");
    frame.render_widget(
        Paragraph::new(text)
            .block(block)
            .style(HINT_STYLE)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// SSH block title, with an explicit indicator whenever history is in view.
fn ssh_title(level: u16, scrollback: usize) -> String {
    let base = format!(" SSH — bandit{level}@bandit.labs.overthewire.org:2220 ");
    if scrollback == 0 {
        base
    } else {
        format!("{base}[scrollback: {scrollback} lines up] ")
    }
}

fn render_terminal(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.mode == AppMode::TerminalFocus;
    let scrollback = app.scrollback_offset();
    let title = ssh_title(app.current_level, scrollback);
    let border_style = if scrollback > 0 {
        Style::new().fg(Color::LightYellow).bold()
    } else if focused {
        Style::new().fg(Color::Yellow)
    } else {
        MUTED_STYLE
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(title, border_style)))
        .border_style(border_style);
    frame.render_widget(block, area);

    let inner = inner_block_area(area);
    frame.render_widget(Clear, inner);
    if let Some(parser) = app.parser() {
        let guard = match parser.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        let screen = guard.screen();
        frame.render_widget(Vt100Widget { screen }, inner);
        // `cell()` follows the scrollback offset, but `cursor_position()` does
        // not; moving the real cursor there would jump away from live output.
        if focused && !app.show_help && scrollback == 0 {
            let (row, column) = screen.cursor_position();
            if row < inner.height && column < inner.width {
                frame.set_cursor_position((inner.x + column, inner.y + row));
            }
        }
    } else {
        frame.render_widget(
            Paragraph::new("No SSH session is running.").style(ERROR_STYLE),
            inner,
        );
    }
}

const TERMINAL_HINT_WIDE: &str = "PgUp/PgDn or wheel: scroll SSH   Alt+I/Ctrl+P: secret   Ctrl+Q: quit   F5: retry   F1/Ctrl+?: keys";
const TERMINAL_HINT_NARROW: &str =
    "PgUp/PgDn: scroll Alt+I/Ctrl+P: secret Ctrl+Q: quit F5: retry F1/Ctrl+?: keys";
const SECRET_HINT_WIDE: &str = "Tab: password/key path   Enter: verify   Esc: back to SSH   Ctrl+Q: quit   F5: retry fetch   F1/Ctrl+?: keys";
const SECRET_HINT_NARROW: &str =
    "Tab: type Enter: verify Esc: back Ctrl+Q: quit F5: retry F1/Ctrl+?: keys";

/// Pick the most complete footer hint that fits the available width. Both
/// variants advertise the exact help keys; the compact one abbreviates other
/// labels on 80-column terminals rather than clipping essential hints.
fn footer_hint(mode: AppMode, width: u16) -> &'static str {
    match mode {
        AppMode::SecretInput => {
            if width >= 110 {
                SECRET_HINT_WIDE
            } else {
                SECRET_HINT_NARROW
            }
        }
        AppMode::TerminalFocus => {
            if width >= 100 {
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

/// All keybindings, grouped by context. Kept short enough for an 80x24
/// terminal: 19 content lines plus the overlay's two border rows.
fn help_lines() -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    {
        let mut section = |title: &'static str, body: &[&'static str]| {
            lines.push(Line::from(Span::styled(title, HEADING_STYLE)));
            lines.extend(body.iter().map(|item| Line::raw(*item)));
            lines.push(Line::raw(""));
        };
        section(
            "SSH terminal",
            &[
                "  type / paste -> remote shell",
                "  Alt+I / Ctrl+P / click secret box -> secret input",
                "  click SSH box -> SSH terminal",
                "  PgUp / PgDn / wheel -> scroll; typing -> live",
                "  Ctrl+C -> remote interrupt   Ctrl+Q -> quit",
                "  F5 / Alt+R -> retry fetch (or r on fetch error)",
            ],
        );
        section(
            "Secret input",
            &[
                "  type / paste -> insert   Tab -> password/key path",
                "  Backspace / Delete / arrows / Home / End -> edit",
                "  Enter -> verify   Esc / Ctrl+G -> back to SSH",
                "  Ctrl+C / Ctrl+Q -> quit   F5 / Alt+R -> retry",
            ],
        );
        section("Help", &["  F1 / Ctrl+? -> open   any key / Esc -> close"]);
    }
    lines
}

/// Last thing drawn: clears and covers the panes without touching parser state
/// or scrollback, so closing the overlay restores the previous view unchanged.
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

    #[test]
    fn progress_line_covers_start_resume_and_final_level() {
        assert_eq!(format_progress(0, 0), "Cleared: 0  |  Now: bandit0 / 33");
        assert_eq!(format_progress(1, 1), "Cleared: 1  |  Now: bandit1 / 33");
        assert_eq!(
            format_progress(33, 33),
            "Cleared: 33  |  Now: bandit33 / 33"
        );
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
    fn scrollback_is_visible_in_title() {
        assert!(!ssh_title(0, 0).contains("scrollback"));
        let title = ssh_title(0, 12);
        assert!(title.contains("bandit0"));
        assert!(title.contains("[scrollback: 12 lines up]"));
    }

    #[test]
    fn terminal_footer_advertises_scrolling_and_help() {
        assert!(
            footer_hint(AppMode::TerminalFocus, 120).contains("PgUp/PgDn or wheel: scroll SSH")
        );
        assert!(footer_hint(AppMode::TerminalFocus, 120).contains("F1/Ctrl+?: keys"));
        assert!(!footer_hint(AppMode::SecretInput, 80).contains("PgUp/PgDn"));
        for width in [80, 92, 100, 120] {
            for mode in [AppMode::TerminalFocus, AppMode::SecretInput] {
                let hint = footer_hint(mode, width);
                assert!(
                    hint.chars().count() <= usize::from(width),
                    "hint must fit {width} columns: {hint}"
                );
                assert!(
                    hint.contains("F1/Ctrl+?: keys"),
                    "hint must advertise help keys: {hint}"
                );
            }
        }
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
        assert_eq!(secret.width, left.width);
        assert!(level.y < secret.y);
        assert!(secret.y + secret.height <= status.y);
        assert_eq!(right.x, secret.x + secret.width);
    }

    #[test]
    fn click_hit_testing_matches_rendered_areas() {
        let terminal = Rect::new(0, 0, 80, 24);
        let secret = secret_block_area(terminal);
        let ssh = ssh_block_area(terminal);
        // crossterm clicks are 1-based; rect_contains expects 0-based cells.
        let secret_click = (secret.x + secret.width, secret.y + secret.height);
        let ssh_click = (ssh.x + ssh.width, ssh.y + ssh.height);
        let secret_cell = (secret_click.0 - 1, secret_click.1 - 1);
        let ssh_cell = (ssh_click.0 - 1, ssh_click.1 - 1);

        assert!(rect_contains(secret, secret_cell.0, secret_cell.1));
        assert!(!rect_contains(ssh, secret_cell.0, secret_cell.1));
        assert!(rect_contains(ssh, ssh_cell.0, ssh_cell.1));
        assert!(!rect_contains(secret, ssh_cell.0, ssh_cell.1));

        let footer_row = terminal.y + terminal.height - 1;
        assert!(!rect_contains(secret, secret_cell.0, footer_row));
        assert!(!rect_contains(ssh, ssh_cell.0, footer_row));
        assert!(rect_contains(secret, secret.x, secret.y), "borders count");
        assert!(!rect_contains(secret, secret.x + secret.width, secret.y));
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
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        for binding in [
            "type / paste",
            "Alt+I / Ctrl+P",
            "click secret box",
            "PgUp / PgDn / wheel",
            "Ctrl+C",
            "Ctrl+Q",
            "F5 / Alt+R",
            "Tab",
            "Backspace / Delete",
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
