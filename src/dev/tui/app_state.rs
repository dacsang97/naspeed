use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use super::super::ansi_strip::strip_ansi;
use super::super::log_bus::{AppState as ProcessState, DevEvent, LogLine, LogSource};
use super::search::{LevelFilter, LogLevel, Query};

pub(super) const SOURCE_CAPACITY: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Tab {
    All,
    App(String),
    Build,
}

impl Tab {
    pub(super) fn label(&self) -> &str {
        match self {
            Tab::All => "All",
            Tab::App(bin) => bin,
            Tab::Build => "build",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ScrollPosition {
    Bottom,
    Top,
    RowsFromTop(usize),
    RowsFromBottom(usize),
}

pub(super) struct UiLine {
    pub(super) id: u64,
    pub(super) source: LogSource,
    pub(super) raw: String,
    pub(super) plain: String,
    pub(super) level: LogLevel,
}

#[derive(Clone, Copy)]
pub(super) struct BuildStatus {
    pub(super) ok: bool,
    pub(super) elapsed: Duration,
    pub(super) errors: usize,
}

struct Toast {
    text: String,
    expires: Instant,
}

pub(super) struct TuiState {
    pub(super) bins: Vec<String>,
    pub(super) tabs: Vec<Tab>,
    pub(super) active_tab: usize,
    pub(super) by_source: BTreeMap<LogSource, VecDeque<u64>>,
    pub(super) merged: BTreeSet<u64>,
    pub(super) lines: BTreeMap<u64, UiLine>,
    pub(super) statuses: BTreeMap<String, ProcessState>,
    pub(super) build_status: Option<BuildStatus>,
    pub(super) next_id: u64,
    pub(super) scroll: ScrollPosition,
    pub(super) autoscroll: bool,
    pub(super) query_text: String,
    pub(super) query: Option<Query>,
    pub(super) search_error: Option<String>,
    pub(super) level_filter: LevelFilter,
    toast: Option<Toast>,
    /// Background of the composer, shaded from the terminal's own.
    pub(super) surface: Option<ratatui::style::Color>,
    /// The shortcut list replaces the footer hint (`?` on an empty filter).
    pub(super) show_shortcuts: bool,
    pub(super) view_generation: u64,
    pub(super) shutdown: bool,
}

impl TuiState {
    pub(super) fn new(bins: &[String]) -> TuiState {
        let mut tabs = Vec::with_capacity(bins.len() + 2);
        tabs.push(Tab::All);
        tabs.extend(bins.iter().cloned().map(Tab::App));
        tabs.push(Tab::Build);
        TuiState {
            bins: bins.to_vec(),
            tabs,
            active_tab: 0,
            by_source: BTreeMap::new(),
            merged: BTreeSet::new(),
            lines: BTreeMap::new(),
            statuses: BTreeMap::new(),
            build_status: None,
            next_id: 1,
            scroll: ScrollPosition::Bottom,
            autoscroll: true,
            query_text: String::new(),
            query: None,
            search_error: None,
            level_filter: LevelFilter::All,
            toast: None,
            surface: None,
            show_shortcuts: false,
            view_generation: 0,
            shutdown: false,
        }
    }

    pub(super) fn apply_event(&mut self, event: DevEvent) {
        match event {
            DevEvent::Log(log) => self.push_log(log),
            DevEvent::Status { bin, state } => {
                self.statuses.insert(bin, state);
            }
            DevEvent::Cycle {
                ok,
                elapsed,
                errors,
            } => {
                self.build_status = Some(BuildStatus {
                    ok,
                    elapsed,
                    errors,
                });
            }
            DevEvent::Shutdown => self.shutdown = true,
        }
    }

    fn push_log(&mut self, log: LogLine) {
        let mut rows = log.raw.lines();
        if let Some(first) = rows.next() {
            self.push_line(log.source.clone(), first.to_string());
            for row in rows {
                self.push_line(log.source.clone(), row.to_string());
            }
        } else {
            self.push_line(log.source, String::new());
        }
    }

    fn push_line(&mut self, source: LogSource, raw: String) {
        let plain = strip_ansi(&raw);
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let level = LogLevel::detect(&plain);
        self.lines.insert(
            id,
            UiLine {
                id,
                source: source.clone(),
                raw,
                plain,
                level,
            },
        );
        self.merged.insert(id);
        let source_lines = self.by_source.entry(source).or_default();
        source_lines.push_back(id);
        if source_lines.len() > SOURCE_CAPACITY
            && let Some(evicted) = source_lines.pop_front()
        {
            self.lines.remove(&evicted);
            self.merged.remove(&evicted);
        }
        if self.autoscroll {
            self.scroll = ScrollPosition::Bottom;
        }
    }

    pub(super) fn toast(&self) -> Option<&str> {
        self.toast.as_ref().map(|toast| toast.text.as_str())
    }

    pub(super) fn show_toast(&mut self, text: String, now: Instant) {
        self.toast = Some(Toast {
            text,
            expires: now + Duration::from_secs(2),
        });
    }

    pub(super) fn expire_toast(&mut self, now: Instant) -> bool {
        if self
            .toast
            .as_ref()
            .is_some_and(|toast| now >= toast.expires)
        {
            self.toast = None;
            true
        } else {
            false
        }
    }
    pub(super) fn invalidate_view(&mut self) {
        self.view_generation = self.view_generation.wrapping_add(1);
    }

    #[cfg(test)]
    pub(super) fn source_count(&self, source: &LogSource) -> usize {
        self.by_source.get(source).map_or(0, VecDeque::len)
    }

    /// The `[source] ` tag shown before a line. Status lines already start
    /// with their own coloured `[ns]`; with a single app its lines need no
    /// tag either, every app line being that app's.
    pub(super) fn line_prefix(&self, source: &LogSource) -> String {
        let single_app = matches!(source, LogSource::App(_)) && self.bins.len() == 1;
        if single_app || *source == LogSource::Ns {
            String::new()
        } else {
            format!("[{}] ", Self::source_name(source))
        }
    }

    pub(super) fn source_name(source: &LogSource) -> &str {
        match source {
            LogSource::Ns => "ns",
            LogSource::Build => "build",
            LogSource::Test => "test",
            LogSource::App(bin) => bin,
        }
    }
}
