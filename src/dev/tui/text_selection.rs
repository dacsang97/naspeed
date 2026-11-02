//! Mouse text selection over the log area, as in a terminal: press, drag,
//! release copies. A selection holds the log area as it was painted when
//! the button went down, so lines arriving meanwhile never move the text
//! under the mouse; it is dropped on the next key, click or scroll.

use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::Modifier,
    text::Span,
};

/// How a painted log row relates to its log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RowInfo {
    /// The row continues the line wrapped from the row above.
    pub(super) continues: bool,
    /// Columns of `[source] ` tag (or its indent) before the text.
    pub(super) indent: u16,
}

pub(super) struct TextSelection {
    frame: Buffer,
    rows: Vec<Option<RowInfo>>,
    anchor: Position,
    head: Position,
    dragging: bool,
}

impl TextSelection {
    /// Start at `at` on `frame`, the painted log area, whose rows `rows`
    /// describe top to bottom.
    pub(super) fn start(frame: Buffer, rows: Vec<Option<RowInfo>>, at: Position) -> Self {
        let at = clamp(frame.area, at);
        Self {
            frame,
            rows,
            anchor: at,
            head: at,
            dragging: true,
        }
    }

    pub(super) fn area(&self) -> Rect {
        self.frame.area
    }

    pub(super) fn dragging(&self) -> bool {
        self.dragging
    }

    /// Move the free end; a drag past the log area stops at its edge.
    pub(super) fn drag_to(&mut self, at: Position) {
        self.head = clamp(self.frame.area, at);
    }

    /// Release the button. `None` for a click (no text selected).
    pub(super) fn release(&mut self) -> Option<String> {
        self.dragging = false;
        let text = self.text();
        (!text.is_empty()).then_some(text)
    }

    /// Paint the held frame with the selected cells reversed.
    pub(super) fn paint(&self, buffer: &mut Buffer) {
        let area = self.frame.area.intersection(buffer.area);
        for position in area.positions() {
            let mut cell = self.frame[position].clone();
            if self.contains(position) {
                cell.modifier.insert(Modifier::REVERSED);
            }
            buffer[position] = cell;
        }
    }

    /// The selected text: wrapped rows rejoin into their line without the
    /// tag indent, line ends lose trailing blanks, lines end with `\n`.
    pub(super) fn text(&self) -> String {
        if self.anchor == self.head {
            return String::new();
        }
        let (start, end) = self.bounds();
        let area = self.frame.area;
        let mut text = String::new();
        for y in start.y..=end.y {
            let info = self.row(y);
            let mut x0 = if y == start.y { start.x } else { area.x };
            if info.is_some_and(|info| info.continues) {
                x0 = x0.max(area.x + info.map_or(0, |info| info.indent));
            }
            let x1 = if y == end.y { end.x } else { area.right() - 1 };
            let row_start = text.len();
            let mut x = x0;
            while x <= x1 {
                let symbol = self.frame[(x, y)].symbol();
                text.push_str(symbol);
                x += (Span::raw(symbol).width() as u16).max(1);
            }
            let wraps_on = y < end.y && self.row(y + 1).is_some_and(|next| next.continues);
            if !wraps_on {
                let kept = text[row_start..].trim_end().len();
                text.truncate(row_start + kept);
                if y < end.y {
                    text.push('\n');
                }
            }
        }
        text
    }

    fn row(&self, y: u16) -> Option<RowInfo> {
        self.rows
            .get(usize::from(y - self.frame.area.y))
            .copied()
            .flatten()
    }

    /// The ends in reading order; the later end is the last selected cell.
    fn bounds(&self) -> (Position, Position) {
        let key = |p: Position| (p.y, p.x);
        if key(self.anchor) <= key(self.head) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    fn contains(&self, position: Position) -> bool {
        if self.anchor == self.head {
            return false;
        }
        let (start, end) = self.bounds();
        let key = |p: Position| (p.y, p.x);
        key(start) <= key(position) && key(position) <= key(end)
    }
}

fn clamp(area: Rect, at: Position) -> Position {
    Position::new(
        at.x.clamp(area.x, area.right().saturating_sub(1)),
        at.y.clamp(area.y, area.bottom().saturating_sub(1)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(rows: &[&str]) -> Buffer {
        let width = rows.iter().map(|row| row.len()).max().unwrap() as u16;
        let mut buffer = Buffer::empty(Rect::new(0, 2, width, rows.len() as u16));
        for (index, row) in rows.iter().enumerate() {
            buffer.set_string(0, 2 + index as u16, row, ratatui::style::Style::default());
        }
        buffer
    }

    const LINE: Option<RowInfo> = Some(RowInfo {
        continues: false,
        indent: 6,
    });
    const WRAP: Option<RowInfo> = Some(RowInfo {
        continues: true,
        indent: 6,
    });

    #[test]
    fn a_drag_copies_from_the_press_to_the_release_cell_in_reading_order() {
        let mut selection = TextSelection::start(
            frame(&["[api] listening on :8080", "[api] ready"]),
            vec![LINE, LINE],
            Position::new(6, 3),
        );
        selection.drag_to(Position::new(16, 2));
        assert_eq!(selection.release().as_deref(), Some("on :8080\n[api] r"));
    }

    #[test]
    fn wrapped_rows_rejoin_without_their_indent() {
        let mut selection = TextSelection::start(
            frame(&["[api] a long line th", "      at wraps here"]),
            vec![LINE, WRAP],
            Position::new(6, 2),
        );
        selection.drag_to(Position::new(40, 9));
        assert_eq!(
            selection.release().as_deref(),
            Some("a long line that wraps here")
        );
    }

    #[test]
    fn a_click_selects_nothing_and_a_drag_stops_at_the_area_edge() {
        let mut click = TextSelection::start(frame(&["[api] x"]), vec![LINE], Position::new(2, 2));
        assert_eq!(click.release(), None);

        let mut selection = TextSelection::start(
            frame(&["one", "two"]),
            vec![LINE, LINE],
            Position::new(0, 0),
        );
        assert_eq!(selection.anchor, Position::new(0, 2));
        selection.drag_to(Position::new(99, 99));
        assert_eq!(selection.release().as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn painting_reverses_only_the_selected_cells() {
        let mut selection = TextSelection::start(frame(&["abcd"]), vec![LINE], Position::new(1, 2));
        selection.drag_to(Position::new(2, 2));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 4));
        selection.paint(&mut buffer);
        let reversed: Vec<bool> = (0..4)
            .map(|x| buffer[(x, 2)].modifier.contains(Modifier::REVERSED))
            .collect();
        assert_eq!(reversed, [false, true, true, false]);
        assert_eq!(buffer[(0, 2)].symbol(), "a");
    }
}
