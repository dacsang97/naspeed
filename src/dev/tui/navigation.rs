use super::app_state::{ScrollPosition, TuiState};

impl TuiState {
    /// Scroll by rows; reaching the bottom again resumes following new lines.
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
            (_, 0) => self.scroll,
            _ => unreachable!("scroll direction is -1, 0, or 1"),
        };
        if delta > 0 && self.scroll == ScrollPosition::Bottom {
            self.autoscroll = true;
        }
    }

    pub(super) fn scroll_home(&mut self) {
        self.autoscroll = false;
        self.scroll = ScrollPosition::Top;
    }

    pub(super) fn scroll_end(&mut self) {
        self.autoscroll = true;
        self.scroll = ScrollPosition::Bottom;
    }
}
