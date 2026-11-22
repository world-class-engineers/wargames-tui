//! Draw a `vt100::Screen` into a ratatui buffer, preserving colors and styles.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use vt100::{Cell as VtCell, Color as VtColor, Screen};

/// Inclusive range of selected columns on one screen row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionRow {
    pub row: u16,
    pub first_column: u16,
    pub last_column: u16,
}

/// Line-oriented selection: each row carries exactly the columns whose text is
/// copied. Keeping the highlight in the same shape as text extraction prevents
/// the highlighted cells from drifting away from the inserted/copied text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub rows: Vec<SelectionRow>,
}

pub struct Vt100Widget<'a> {
    pub screen: &'a Screen,
    /// Optional in-app selection; rendered by reversing cell styles without
    /// mutating the vt100 parser state.
    pub selection: Option<Selection>,
}

impl Widget for Vt100Widget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for row in 0..area.height {
            for column in 0..area.width {
                let Some(cell) = self.screen.cell(row, column) else {
                    continue;
                };
                if let Some(target) = buf.cell_mut((area.x + column, area.y + row)) {
                    target.set_symbol(cell_symbol(cell));
                    // Apply styles to blank cells too, so an erased cell cannot
                    // inherit styling from the glyph it replaces.
                    let mut style = cell_style(cell);
                    // Continuation cells carry no text, so they must not be
                    // highlighted: extraction skips them as well.
                    if !cell.is_wide_continuation()
                        && self
                            .selection
                            .as_ref()
                            .is_some_and(|selection| selection.contains(row, column))
                    {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                    target.set_style(style);
                }
            }
        }
    }
}

impl Selection {
    fn contains(&self, row: u16, column: u16) -> bool {
        self.rows.iter().any(|selected| {
            selected.row == row && column >= selected.first_column && column <= selected.last_column
        })
    }
}

/// Text to write into the ratatui buffer for a vt100 cell.
///
/// A blank vt100 cell has no contents, but ratatui's buffer diff only emits an
/// erase when the new symbol is non-empty. Writing a space makes cleared text
/// actually disappear from the real terminal. Wide-character continuation cells
/// are the exception: ratatui needs an empty symbol there to keep the wide glyph
/// from being drawn twice.
fn cell_symbol(cell: &VtCell) -> &str {
    if cell.has_contents() {
        cell.contents()
    } else if cell.is_wide_continuation() {
        ""
    } else {
        " "
    }
}

fn cell_style(cell: &VtCell) -> Style {
    let mut style = Style::default()
        .fg(map_color(cell.fgcolor()))
        .bg(map_color(cell.bgcolor()));
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    if cell.dim() {
        style = style.add_modifier(Modifier::DIM);
    }
    style
}

fn map_color(color: VtColor) -> Color {
    match color {
        VtColor::Default => Color::Reset,
        VtColor::Idx(index) => Color::Indexed(index),
        VtColor::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU16;

    const AREA: Rect = Rect::new(0, 0, 8, 2);

    fn parser() -> vt100::Parser {
        vt100::Parser::new(
            NonZeroU16::new(AREA.height).unwrap(),
            NonZeroU16::new(AREA.width).unwrap(),
            0,
        )
    }

    fn render_screen(parser: &vt100::Parser, buf: &mut Buffer, selection: Option<Selection>) {
        Vt100Widget {
            screen: parser.screen(),
            selection,
        }
        .render(AREA, buf);
    }

    #[test]
    fn cleared_cells_render_as_spaces_instead_of_stale_text() {
        let mut parser = parser();
        let mut buf = Buffer::empty(AREA);
        parser.process(b"stale");
        render_screen(&parser, &mut buf, None);
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "s");

        parser.process(b"\x1b[2J");
        assert!(!parser.screen().cell(0, 0).unwrap().has_contents());
        render_screen(&parser, &mut buf, None);
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((4, 0)).unwrap().symbol(), " ");
    }

    #[test]
    fn wide_character_continuation_still_renders_as_empty() {
        let mut parser = parser();
        let mut buf = Buffer::empty(AREA);
        parser.process("界".as_bytes());
        assert!(parser.screen().cell(0, 1).unwrap().is_wide_continuation());
        render_screen(&parser, &mut buf, None);
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "界");
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), "");
    }

    #[test]
    fn selection_reverses_cells_without_mutating_parser_state() {
        let mut parser = parser();
        let mut buf = Buffer::empty(AREA);
        parser.process(b"abcde\r\nfghij");
        let selection = Selection {
            rows: vec![
                SelectionRow {
                    row: 0,
                    first_column: 1,
                    last_column: 7,
                },
                SelectionRow {
                    row: 1,
                    first_column: 0,
                    last_column: 2,
                },
            ],
        };
        render_screen(&parser, &mut buf, Some(selection));
        assert!(
            buf.cell((1, 0))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            buf.cell((2, 1))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !buf.cell((0, 0))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !buf.cell((3, 1))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(parser.screen().contents(), "abcde\nfghij");
    }
}
