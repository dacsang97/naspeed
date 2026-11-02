use super::app_state::{ScrollPosition, TuiState};
use super::search::{LevelFilter, Query};

impl TuiState {
    /// Typing in the composer edits the query and filters at once.
    pub(super) fn type_char(&mut self, ch: char) {
        self.query_text.push(ch);
        self.apply_query();
    }

    pub(super) fn backspace(&mut self) {
        if self.query_text.pop().is_some() {
            self.apply_query();
        }
    }

    pub(super) fn clear_query(&mut self) {
        if self.query_text.is_empty() && self.search_error.is_none() {
            return;
        }
        self.query_text.clear();
        self.apply_query();
    }

    /// Re-filter on the composer's text. An incomplete `re:` pattern keeps
    /// the last valid filter and shows the error until the text parses.
    fn apply_query(&mut self) {
        match Query::parse(&self.query_text) {
            Ok(query) => {
                self.query = (!self.query_text.is_empty()).then_some(query);
                self.search_error = None;
                self.refilter();
            }
            Err(error) => self.search_error = Some(error.to_string()),
        }
    }

    pub(super) fn cycle_level_filter(&mut self) {
        self.level_filter = self.level_filter.next();
        self.refilter();
    }

    pub(super) fn level_filter(&self) -> LevelFilter {
        self.level_filter
    }

    fn refilter(&mut self) {
        self.invalidate_view();
        self.scroll = ScrollPosition::Bottom;
        self.autoscroll = true;
    }
}
