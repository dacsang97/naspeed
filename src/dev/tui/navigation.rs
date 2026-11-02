use super::app_state::{ScrollPosition, TuiState};

impl TuiState {
    pub(super) fn scroll_by(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }
        self.autoscroll = false;
        let distance = delta.unsigned_abs();
        self.scroll = match (self.scroll, delta.signum()) {
            (ScrollPosition::Top, -1) => ScrollPosition::Top,
            (ScrollPosition::Top, 1) => ScrollPosition::RowsFromTop(distance),
            (ScrollPosition::Bottom, -1) => ScrollPosition::RowsFromBottom(distance),
            (ScrollPosition::Bottom, 1) => ScrollPosition::Bottom,
            (ScrollPosition::RowsFromTop(rows), -1) => {
                let next = rows.saturating_sub(distance);
                if next == 0 {
                    ScrollPosition::Top
                } else {
                    ScrollPosition::RowsFromTop(next)
                }
            }
            (ScrollPosition::RowsFromTop(rows), 1) => {
                ScrollPosition::RowsFromTop(rows.saturating_add(distance))
            }
            (ScrollPosition::RowsFromBottom(rows), -1) => {
                ScrollPosition::RowsFromBottom(rows.saturating_add(distance))
            }
            (ScrollPosition::RowsFromBottom(rows), 1) => {
                let next = rows.saturating_sub(distance);
                if next == 0 {
                    ScrollPosition::Bottom
                } else {
                    ScrollPosition::RowsFromBottom(next)
                }
            }
            (ScrollPosition::EnsureCursor, -1) => ScrollPosition::RowsFromBottom(distance),
            (ScrollPosition::EnsureCursor, 1) => ScrollPosition::RowsFromBottom(0),
            (_, 0) => self.scroll,
            _ => unreachable!("scroll direction is -1, 0, or 1"),
        };
    }

    pub(super) fn scroll_home(&mut self) {
        self.autoscroll = false;
        self.scroll = ScrollPosition::Top;
    }

    pub(super) fn scroll_end(&mut self) {
        self.autoscroll = true;
        self.scroll = ScrollPosition::Bottom;
        self.cursor = self.visible_lines().last().map(|line| line.id);
    }

    pub(super) fn toggle_autoscroll(&mut self) {
        self.autoscroll = !self.autoscroll;
        if self.autoscroll {
            self.scroll_end();
        }
    }

    pub(super) fn set_cursor(&mut self, id: u64) {
        self.cursor = Some(id);
        self.scroll = ScrollPosition::EnsureCursor;
        self.autoscroll = false;
    }

    pub(super) fn move_cursor(&mut self, delta: isize) {
        let ids: Vec<u64> = self.visible_lines().map(|line| line.id).collect();
        if ids.is_empty() {
            self.cursor = None;
            return;
        }
        let current_id = self.selection.cursor().or(self.cursor);
        let current = current_id.and_then(|id| ids.iter().position(|candidate| *candidate == id));
        let next = match current {
            Some(index) => (index as isize + delta).clamp(0, ids.len() as isize - 1) as usize,
            None if delta < 0 => ids.len() - 1,
            None => 0,
        };
        self.set_cursor(ids[next]);
    }

    pub(super) fn move_to_match(&mut self, reverse: bool) {
        let Some(query) = self.query.as_ref() else {
            self.move_cursor(if reverse { -1 } else { 1 });
            return;
        };
        let ids: Vec<u64> = self.visible_lines().map(|line| line.id).collect();
        let current = self.selection.cursor().or(self.cursor);
        let current_index =
            current.and_then(|id| ids.iter().position(|candidate| *candidate == id));
        let matching: Vec<usize> = ids
            .iter()
            .enumerate()
            .filter_map(|(index, id)| {
                self.lines
                    .get(id)
                    .is_some_and(|line| query.matches(&line.plain))
                    .then_some(index)
            })
            .collect();
        let target = if reverse {
            matching
                .iter()
                .rev()
                .copied()
                .find(|index| current_index.is_none_or(|current| *index < current))
                .or_else(|| matching.last().copied())
        } else {
            matching
                .iter()
                .copied()
                .find(|index| current_index.is_none_or(|current| *index > current))
                .or_else(|| matching.first().copied())
        };
        if let Some(index) = target {
            self.reset_selection();
            self.set_cursor(ids[index]);
        }
    }

    pub(super) fn reset_selection(&mut self) {
        self.selection.clear();
    }
}
