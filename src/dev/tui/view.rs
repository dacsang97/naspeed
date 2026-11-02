use std::collections::{btree_set, vec_deque};

use super::super::log_bus::LogSource;
use super::app_state::{ScrollPosition, Tab, TuiState, UiLine};

pub(super) struct VisibleLines<'a> {
    state: &'a TuiState,
    ids: Ids<'a>,
}

enum Ids<'a> {
    Merged(btree_set::Iter<'a, u64>),
    Source(vec_deque::Iter<'a, u64>),
    Empty,
}

impl Iterator for Ids<'_> {
    type Item = u64;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Ids::Merged(ids) => ids.next().copied(),
            Ids::Source(ids) => ids.next().copied(),
            Ids::Empty => None,
        }
    }
}

impl<'a> Iterator for VisibleLines<'a> {
    type Item = &'a UiLine;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let id = self.ids.next()?;
            let line = self.state.lines.get(&id)?;
            if self.state.in_active_tab(line) && self.state.passes_filters(line) {
                return Some(line);
            }
        }
    }
}

impl TuiState {
    pub(super) fn visible_lines(&self) -> VisibleLines<'_> {
        let ids = match self.active_tab() {
            Tab::App(bin) => self
                .by_source
                .iter()
                .find(|(source, _)| matches!(source, LogSource::App(name) if name == bin))
                .map_or(Ids::Empty, |(_, lines)| Ids::Source(lines.iter())),
            Tab::All | Tab::Build => Ids::Merged(self.merged.iter()),
        };
        VisibleLines { state: self, ids }
    }

    pub(super) fn active_tab(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    pub(super) fn in_active_tab(&self, line: &UiLine) -> bool {
        match self.active_tab() {
            Tab::All => true,
            Tab::App(bin) => matches!(&line.source, LogSource::App(name) if name == bin),
            Tab::Build => matches!(&line.source, LogSource::Build | LogSource::Test),
        }
    }

    /// The composer's query filters as it is typed: only matching lines show.
    pub(super) fn passes_filters(&self, line: &UiLine) -> bool {
        self.level_filter.accepts(line.level)
            && self
                .query
                .as_ref()
                .is_none_or(|query| query.matches(&line.plain))
    }

    pub(super) fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub(super) fn choose_tab(&mut self, index: usize) {
        if index >= self.tabs.len() || self.active_tab == index {
            return;
        }
        self.active_tab = index;
        self.invalidate_view();
        self.scroll = ScrollPosition::Bottom;
        self.autoscroll = true;
    }

    pub(super) fn cycle_tab(&mut self, delta: isize) {
        let len = self.tabs.len();
        let next = (self.active_tab as isize + delta).rem_euclid(len as isize) as usize;
        self.choose_tab(next);
    }

    pub(super) fn restart_bin(&self) -> Option<String> {
        match self.active_tab() {
            Tab::App(bin) => Some(bin.clone()),
            Tab::All | Tab::Build => None,
        }
    }

    pub(super) fn clear_active(&mut self) {
        let doomed: Vec<u64> = self
            .lines
            .iter()
            .filter(|(_, line)| self.in_active_tab(line))
            .map(|(id, _)| *id)
            .collect();
        for id in doomed {
            self.lines.remove(&id);
            self.merged.remove(&id);
        }
        let lines = &self.lines;
        for ids in self.by_source.values_mut() {
            ids.retain(|id| lines.contains_key(id));
        }
        self.invalidate_view();
        self.scroll = ScrollPosition::Bottom;
        self.autoscroll = true;
    }
}
