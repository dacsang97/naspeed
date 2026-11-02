use std::collections::BTreeSet;
use std::fmt::Write as _;

use super::super::log_bus::LogSource;
use super::app_state::{ScrollPosition, TuiState};

impl TuiState {
    pub(super) fn start_selection(&mut self) {
        let id = self
            .cursor
            .or_else(|| self.visible_lines().last().map(|line| line.id));
        if let Some(id) = id {
            self.selection.start(id);
            self.cursor = Some(id);
            self.scroll = ScrollPosition::EnsureCursor;
            self.autoscroll = false;
        }
    }

    pub(super) fn extend_selection(&mut self, delta: isize) {
        if !self.selection.active() {
            self.start_selection();
        }
        let current = self.selection.cursor().or(self.cursor);
        let ids: Vec<u64> = self.visible_lines().map(|line| line.id).collect();
        let Some(current) = current else { return };
        let Some(index) = ids.iter().position(|id| *id == current) else {
            return;
        };
        let next = (index as isize + delta).clamp(0, ids.len() as isize - 1) as usize;
        let id = ids[next];
        self.selection.extend_to(id);
        self.cursor = Some(id);
        self.scroll = ScrollPosition::EnsureCursor;
        self.autoscroll = false;
    }

    pub(super) fn select_clicked_line(&mut self, id: u64) {
        self.selection.start(id);
        self.cursor = Some(id);
        self.scroll = ScrollPosition::EnsureCursor;
        self.autoscroll = false;
    }

    pub(super) fn clear_selection(&mut self) {
        self.reset_selection();
    }

    pub(super) fn selected_line_ids(&self) -> BTreeSet<u64> {
        if !self.selection.active() {
            return BTreeSet::new();
        }
        let groups = self.selected_groups();
        self.lines
            .values()
            .filter(|line| {
                self.in_active_tab(line) && groups.contains(&(line.source.clone(), line.group))
            })
            .map(|line| line.id)
            .collect()
    }

    pub(super) fn copy_payload(&self) -> (usize, String) {
        let groups = self.selected_groups();
        let mut count = 0;
        let mut payload = String::new();
        for line in self.lines.values().filter(|line| {
            self.in_active_tab(line) && groups.contains(&(line.source.clone(), line.group))
        }) {
            if count > 0 {
                payload.push('\n');
            }
            let _ = write!(
                payload,
                "[{}] {}",
                TuiState::source_name(&line.source),
                line.plain
            );
            count += 1;
        }
        (count, payload)
    }

    fn selected_groups(&self) -> BTreeSet<(LogSource, u64)> {
        let Some(anchor) = self.selection.anchor().or(self.cursor) else {
            return BTreeSet::new();
        };
        let end = self.selection.cursor().unwrap_or(anchor);
        let mut positions = (None, None);
        for (index, line) in self.visible_lines().enumerate() {
            if line.id == anchor {
                positions.0 = Some(index);
            }
            if line.id == end {
                positions.1 = Some(index);
            }
        }
        let Some(start) = positions.0 else {
            return self.single_group(anchor);
        };
        let Some(finish) = positions.1 else {
            return self.single_group(anchor);
        };
        let (low, high) = (start.min(finish), start.max(finish));
        self.visible_lines()
            .enumerate()
            .filter(|(index, _)| *index >= low && *index <= high)
            .map(|(_, line)| (line.source.clone(), line.group))
            .collect()
    }

    fn single_group(&self, id: u64) -> BTreeSet<(LogSource, u64)> {
        self.lines
            .get(&id)
            .map(|line| BTreeSet::from([(line.source.clone(), line.group)]))
            .unwrap_or_default()
    }
}
