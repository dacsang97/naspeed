//! Mouse text selection over the log area, as in a terminal: press, drag,
//! release copies, and a double-click picks the word under the pointer.
//! A selection holds the last painted viewport, while rows visited by wheel
//! scrolling are retained by logical log-line position.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ratatui::{
    buffer::{Buffer, Cell},
    layout::{Position, Rect},
    style::Modifier,
    text::Span,
};
use unicode_segmentation::UnicodeSegmentation;

/// How a painted log row relates to its log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RowInfo {
    pub(super) id: u64,
    pub(super) offset: u16,
    /// The row continues the line wrapped from the row above.
    pub(super) continues: bool,
    /// Columns of `[source] ` tag (or its indent) before the text.
    pub(super) indent: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RowKey {
    id: u64,
    offset: u16,
}

impl From<RowInfo> for RowKey {
    fn from(row: RowInfo) -> Self {
        Self {
            id: row.id,
            offset: row.offset,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SelectionPoint {
    row: RowKey,
    x: u16,
}

struct CapturedRow {
    info: RowInfo,
    cells: Vec<Cell>,
}

pub(super) struct TextSelection {
    frame: Buffer,
    rows: Vec<Option<RowInfo>>,
    captured: BTreeMap<RowKey, CapturedRow>,
    anchor: SelectionPoint,
    head: SelectionPoint,
    dragging: bool,
    /// A double-click word pick; a one-cell word keeps anchor == head.
    word: bool,
}

impl TextSelection {
    /// Start at `at` on `frame`, using the nearest painted log row when the
    /// pointer is in blank space below short output.
    pub(super) fn start(frame: Buffer, rows: Vec<Option<RowInfo>>, at: Position) -> Option<Self> {
        let at = clamp(frame.area, at);
        let anchor = point_at(frame.area, &rows, at)?;
        let mut selection = Self {
            frame,
            rows,
            captured: BTreeMap::new(),
            anchor,
            head: anchor,
            dragging: true,
            word: false,
        };
        selection.capture_rows();
        Some(selection)
    }

    pub(super) fn area(&self) -> Rect {
        self.frame.area
    }

    pub(super) fn dragging(&self) -> bool {
        self.dragging
    }

    /// Move the free end; a drag past the log area stops at its nearest row.
    pub(super) fn drag_to(&mut self, at: Position) {
        let at = clamp(self.frame.area, at);
        if let Some(head) = point_at(self.frame.area, &self.rows, at) {
            self.head = head;
        }
        self.word = false;
    }

    /// Expand to the word under the anchor (a double-click).
    /// False when the anchor sits on blank space.
    pub(super) fn select_word(&mut self) -> bool {
        let area = self.frame.area;
        // The logical line around the anchor: its wrapped rows in paint order.
        let first = RowKey {
            id: self.anchor.row.id,
            offset: 0,
        };
        // Each character's first cell and its byte offset in the joined text.
        let mut marks: Vec<(SelectionPoint, usize)> = Vec::new();
        let mut text = String::new();
        for (key, row) in self.captured.range(first..) {
            if key.id != self.anchor.row.id {
                break;
            }
            let mut x = area.x;
            if row.info.continues {
                x += row.info.indent;
            }
            for cell in &row.cells[usize::from(x - area.x)..] {
                let symbol = cell.symbol();
                if symbol.is_empty() {
                    x += 1;
                    continue;
                }
                marks.push((SelectionPoint { row: *key, x }, text.len()));
                text.push_str(symbol);
                x += (Span::raw(symbol).width() as u16).max(1);
            }
        }
        // The character under the anchor; blank space cancels the word pick.
        let Some(index) = marks
            .iter()
            .take_while(|(point, _)| *point <= self.anchor)
            .count()
            .checked_sub(1)
        else {
            return false;
        };
        let (point, offset) = marks[index];
        let cell_end = match marks.get(index + 1).filter(|(p, _)| p.row == point.row) {
            Some((next, _)) => next.x,
            None => {
                let symbol = text[offset..].chars().next().unwrap();
                point.x + (Span::raw(symbol.to_string()).width() as u16).max(1)
            }
        };
        if point.row != self.anchor.row || self.anchor.x >= cell_end {
            return false;
        }
        let Some(word) = text
            .split_word_bound_indices()
            .map(|(start, word)| start..start + word.len())
            .find(|range| range.contains(&offset))
        else {
            return false;
        };
        if text[word.clone()].trim().is_empty() {
            return false;
        }
        let at = |byte: usize| {
            marks
                .iter()
                .take_while(|(_, b)| *b <= byte)
                .last()
                .map(|(point, _)| *point)
        };
        let Some(start) = at(word.start) else {
            return false;
        };
        let Some(end) = word.end.checked_sub(1).and_then(at) else {
            return false;
        };
        self.anchor = start;
        self.head = end;
        self.word = true;
        true
    }

    /// Replace the held viewport after wheel scrolling and retain its rows for
    /// copying a selection that spans more than one viewport.
    pub(super) fn update_viewport(&mut self, frame: Buffer, rows: Vec<Option<RowInfo>>) {
        debug_assert_eq!(self.frame.area, frame.area);
        self.frame = frame;
        self.rows = rows;
        self.capture_rows();
    }

    /// Release the button. `None` for a click (no text selected).
    pub(super) fn release(&mut self) -> Option<String> {
        self.dragging = false;
        let text = self.text();
        (!text.is_empty()).then_some(text)
    }

    /// Release at the final pointer position. A double-click without a drag
    /// keeps the whole word instead of collapsing its free end to one cell.
    pub(super) fn release_at(&mut self, at: Position) -> Option<String> {
        if !self.word {
            self.drag_to(at);
        }
        self.release()
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
        if self.anchor == self.head && !self.word {
            return String::new();
        }
        let (start, end) = self.bounds();
        let area = self.frame.area;
        let mut selected = self.captured.range(start.row..=end.row).peekable();
        let mut text = String::new();
        while let Some((key, row)) = selected.next() {
            let mut x0 = if *key == start.row { start.x } else { area.x };
            if row.info.continues {
                x0 = x0.max(area.x + row.info.indent);
            }
            let x1 = if *key == end.row {
                end.x
            } else {
                area.right() - 1
            };
            let row_start = text.len();
            let mut x = x0;
            while x <= x1 {
                let Some(cell) = row.cells.get(usize::from(x - area.x)) else {
                    break;
                };
                let symbol = cell.symbol();
                text.push_str(symbol);
                x += (Span::raw(symbol).width() as u16).max(1);
            }
            let wraps_on = selected.peek().is_some_and(|(_, next)| {
                next.info.id == row.info.id
                    && next.info.offset == row.info.offset.saturating_add(1)
                    && next.info.continues
            });
            if !wraps_on {
                let kept = text[row_start..].trim_end().len();
                text.truncate(row_start + kept);
                if selected.peek().is_some() {
                    text.push('\n');
                }
            }
        }
        text
    }

    fn capture_rows(&mut self) {
        let area = self.frame.area;
        for (index, info) in self.rows.iter().copied().enumerate() {
            let Some(info) = info else {
                continue;
            };
            let key = RowKey::from(info);
            self.captured.entry(key).or_insert_with(|| {
                let y = area.y + index as u16;
                let cells = (area.x..area.right())
                    .map(|x| self.frame[(x, y)].clone())
                    .collect();
                CapturedRow { info, cells }
            });
        }
    }

    /// The ends in reading order; the later end is the last selected cell.
    fn bounds(&self) -> (SelectionPoint, SelectionPoint) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    fn contains(&self, position: Position) -> bool {
        if self.anchor == self.head && !self.word {
            return false;
        }
        let Some(point) = painted_point_at(self.frame.area, &self.rows, position) else {
            return false;
        };
        let (start, end) = self.bounds();
        start <= point && point <= end
    }
}

fn painted_point_at(area: Rect, rows: &[Option<RowInfo>], at: Position) -> Option<SelectionPoint> {
    if !area.contains(at) {
        return None;
    }
    let index = usize::from(at.y - area.y);
    let info = rows.get(index).copied().flatten()?;
    Some(SelectionPoint {
        row: RowKey::from(info),
        x: at.x,
    })
}

fn point_at(area: Rect, rows: &[Option<RowInfo>], at: Position) -> Option<SelectionPoint> {
    painted_point_at(area, rows, at).or_else(|| {
        let index = usize::from(at.y.saturating_sub(area.y));
        let info = rows[..index.min(rows.len())]
            .iter()
            .rev()
            .copied()
            .flatten()
            .next()
            .or_else(|| rows.iter().skip(index).copied().flatten().next())?;
        Some(SelectionPoint {
            row: RowKey::from(info),
            x: at.x,
        })
    })
}

fn clamp(area: Rect, at: Position) -> Position {
    Position::new(
        at.x.clamp(area.x, area.right().saturating_sub(1)),
        at.y.clamp(area.y, area.bottom().saturating_sub(1)),
    )
}

/// Repeat presses on one cell within 400 ms build up a click count; 2 is a
/// double-click. Terminals report no double-click event, only
/// presses, so the count is kept here.
pub(super) fn click_count(
    last_click: &mut Option<(Instant, u16, u16, u8)>,
    column: u16,
    row: u16,
) -> u8 {
    let now = Instant::now();
    let clicks = last_click
        .filter(|(at, x, y, _)| {
            now.duration_since(*at) < Duration::from_millis(400) && *x == column && *y == row
        })
        .map_or(1, |(_, _, _, clicks)| clicks.saturating_add(1));
    *last_click = Some((now, column, row, clicks));
    clicks
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

    fn row(id: u64, offset: u16, continues: bool) -> Option<RowInfo> {
        Some(RowInfo {
            id,
            offset,
            continues,
            indent: 6,
        })
    }

    #[test]
    fn a_drag_copies_from_the_press_to_the_release_cell_in_reading_order() {
        let mut selection = TextSelection::start(
            frame(&["[api] listening on :8080", "[api] ready"]),
            vec![row(1, 0, false), row(2, 0, false)],
            Position::new(6, 3),
        )
        .unwrap();
        selection.drag_to(Position::new(16, 2));
        assert_eq!(selection.release().as_deref(), Some("on :8080\n[api] r"));
    }

    #[test]
    fn wrapped_rows_rejoin_without_their_indent() {
        let mut selection = TextSelection::start(
            frame(&["[api] a long line th", "      at wraps here"]),
            vec![row(1, 0, false), row(1, 1, true)],
            Position::new(6, 2),
        )
        .unwrap();
        selection.drag_to(Position::new(40, 9));
        assert_eq!(
            selection.release().as_deref(),
            Some("a long line that wraps here")
        );
    }

    #[test]
    fn a_click_selects_nothing_and_a_drag_stops_at_the_area_edge() {
        let mut click = TextSelection::start(
            frame(&["[api] x"]),
            vec![row(1, 0, false)],
            Position::new(2, 2),
        )
        .unwrap();
        assert_eq!(click.release(), None);

        let mut selection = TextSelection::start(
            frame(&["one", "two"]),
            vec![row(1, 0, false), row(2, 0, false)],
            Position::new(0, 0),
        )
        .unwrap();
        selection.drag_to(Position::new(99, 99));
        assert_eq!(selection.release().as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn painting_reverses_only_the_selected_cells() {
        let mut selection = TextSelection::start(
            frame(&["abcd"]),
            vec![row(1, 0, false)],
            Position::new(1, 2),
        )
        .unwrap();
        selection.drag_to(Position::new(2, 2));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 4));
        selection.paint(&mut buffer);
        let reversed: Vec<bool> = (0..4)
            .map(|x| buffer[(x, 2)].modifier.contains(Modifier::REVERSED))
            .collect();
        assert_eq!(reversed, [false, true, true, false]);
        assert_eq!(buffer[(0, 2)].symbol(), "a");
    }

    #[test]
    fn painting_does_not_map_blank_viewport_rows_to_the_last_log_row() {
        let area = Rect::new(0, 2, 12, 4);
        let mut painted = Buffer::empty(area);
        painted.set_string(0, 2, "alpha beta", ratatui::style::Style::default());
        let mut selection = TextSelection::start(
            painted,
            vec![row(1, 0, false), None, None, None],
            Position::new(1, 2),
        )
        .unwrap();
        assert!(selection.select_word());

        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 6));
        selection.paint(&mut buffer);
        assert!(buffer[(1, 2)].modifier.contains(Modifier::REVERSED));
        for y in 3..6 {
            assert!(
                !(0..12).any(|x| buffer[(x, y)].modifier.contains(Modifier::REVERSED)),
                "blank row {y} must stay unselected"
            );
        }
    }

    #[test]
    fn a_double_click_selects_the_word_under_the_pointer() {
        let mut selection = TextSelection::start(
            frame(&["[api] listening on :8080"]),
            vec![row(1, 0, false)],
            Position::new(10, 2),
        )
        .unwrap();
        assert!(selection.select_word());
        assert_eq!(
            selection.release_at(Position::new(10, 2)).as_deref(),
            Some("listening")
        );
    }

    #[test]
    fn a_double_click_on_blank_space_selects_nothing() {
        let mut after_text = TextSelection::start(
            frame(&["[api] ready     "]),
            vec![row(1, 0, false)],
            Position::new(13, 2),
        )
        .unwrap();
        assert!(!after_text.select_word());
        assert_eq!(after_text.release(), None);

        let mut short_line = TextSelection::start(
            frame(&["[api] x", "[api] listening"]),
            vec![row(1, 0, false), row(2, 0, false)],
            Position::new(10, 2),
        )
        .unwrap();
        assert!(!short_line.select_word());
        assert_eq!(short_line.release(), None);
    }

    #[test]
    fn a_double_click_word_spans_wrapped_rows_and_marks_one_cell() {
        let mut selection = TextSelection::start(
            frame(&["[api] a long line th", "      at wraps here"]),
            vec![row(1, 0, false), row(1, 1, true)],
            Position::new(18, 2),
        )
        .unwrap();
        assert!(selection.select_word());
        assert_eq!(selection.release().as_deref(), Some("that"));

        let mut one_cell = TextSelection::start(
            frame(&["[api] a b"]),
            vec![row(1, 0, false)],
            Position::new(8, 2),
        )
        .unwrap();
        assert!(one_cell.select_word());
        assert_eq!(one_cell.release().as_deref(), Some("b"));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 9, 3));
        one_cell.paint(&mut buffer);
        assert!(buffer[(8, 2)].modifier.contains(Modifier::REVERSED));
        assert!(!buffer[(7, 2)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn clicks_on_one_cell_within_400_ms_count_up() {
        let mut last = None;
        assert_eq!(click_count(&mut last, 3, 2), 1);
        assert_eq!(click_count(&mut last, 3, 2), 2);
        assert_eq!(click_count(&mut last, 4, 2), 1);
        last.as_mut().unwrap().0 = Instant::now() - Duration::from_secs(1);
        assert_eq!(click_count(&mut last, 3, 2), 1);
    }
}
