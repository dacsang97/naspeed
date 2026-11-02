use super::app_state::{ScrollPosition, TuiState};
use super::search::{LevelFilter, Query};

impl TuiState {
    pub(super) fn begin_search(&mut self) {
        self.search_draft.clone_from(&self.query_text);
        self.search_error = None;
        self.search_editing = true;
    }

    pub(super) fn search_char(&mut self, ch: char) {
        self.search_draft.push(ch);
        self.search_error = None;
    }

    pub(super) fn search_backspace(&mut self) {
        self.search_draft.pop();
        self.search_error = None;
    }

    pub(super) fn submit_search(&mut self) -> Result<(), String> {
        match Query::parse(&self.search_draft) {
            Ok(query) => {
                self.query_text.clone_from(&self.search_draft);
                self.query = (!self.search_draft.is_empty()).then_some(query);
                self.search_error = None;
                self.search_editing = false;
                self.invalidate_view();
                self.reset_selection();
                self.cursor = self.visible_lines().last().map(|line| line.id);
                self.scroll = ScrollPosition::Bottom;
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                self.search_error = Some(message.clone());
                Err(message)
            }
        }
    }

    pub(super) fn clear_search(&mut self) {
        self.query_text.clear();
        self.query = None;
        self.search_draft.clear();
        self.search_error = None;
        self.search_editing = false;
        self.invalidate_view();
        self.reset_selection();
        self.cursor = self.visible_lines().last().map(|line| line.id);
        self.scroll = ScrollPosition::Bottom;
    }

    pub(super) fn cancel_search(&mut self) {
        self.search_draft.clone_from(&self.query_text);
        self.search_error = None;
        self.search_editing = false;
    }

    pub(super) fn toggle_filter(&mut self) {
        self.filter_mode = !self.filter_mode;
        self.invalidate_view();
        self.reset_selection();
        self.cursor = self.visible_lines().last().map(|line| line.id);
        self.scroll = ScrollPosition::Bottom;
    }

    pub(super) fn cycle_level_filter(&mut self) {
        self.level_filter = self.level_filter.next();
        self.invalidate_view();
        self.reset_selection();
        self.cursor = self.visible_lines().last().map(|line| line.id);
        self.scroll = ScrollPosition::Bottom;
    }

    pub(super) fn level_filter(&self) -> LevelFilter {
        self.level_filter
    }

    pub(super) fn search_display(&self) -> (&str, Option<&str>, bool) {
        if self.search_editing {
            (&self.search_draft, self.search_error.as_deref(), true)
        } else {
            (&self.query_text, None, false)
        }
    }

    pub(super) fn search_is_active(&self) -> bool {
        self.query.is_some()
    }
}
