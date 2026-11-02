use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::super::log_bus::AppState;
use super::app_state::{BuildStatus, Tab, TuiState};
use super::search::LevelFilter;

pub(super) fn status_line(state: &TuiState) -> Line<'static> {
    let mut spans = Vec::with_capacity(state.bins.len() + 1);
    for bin in &state.bins {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        let (label, color) = match state.statuses.get(bin) {
            None => (format!("{bin} ○ waiting"), Color::DarkGray),
            Some(AppState::Starting) => (format!("{bin} … starting"), Color::Yellow),
            Some(AppState::Running { pid }) => (format!("{bin} ● {pid}"), Color::Green),
            Some(AppState::Building) => (format!("{bin} building"), Color::Cyan),
            Some(AppState::Crashed { code }) => (
                format!(
                    "{bin} ✗ exit {}",
                    code.map_or_else(|| "?".to_string(), |value| value.to_string())
                ),
                Color::Red,
            ),
            Some(AppState::Stopped) => (format!("{bin} stopped"), Color::DarkGray),
        };
        spans.push(Span::styled(label, Style::default().fg(color)));
    }
    if let Some(build) = state.build_status {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        let (label, color) = build_label(build);
        spans.push(Span::styled(label, Style::default().fg(color)));
    }
    Line::from(spans)
}

fn build_label(status: BuildStatus) -> (String, Color) {
    if status.ok {
        let elapsed = if status.elapsed < std::time::Duration::from_secs(1) {
            format!("{} ms", status.elapsed.as_millis())
        } else {
            format!("{:.1} s", status.elapsed.as_secs_f64())
        };
        (format!("build ✓ {elapsed}"), Color::Green)
    } else {
        (format!("build ✗ {} errors", status.errors), Color::Red)
    }
}

/// The tab bar and each tab's columns `[start, end)` from `x`, for clicks.
pub(super) fn tabs(state: &TuiState, x: u16) -> (Line<'static>, Vec<(u16, u16)>) {
    let failed = state.build_status.is_some_and(|status| !status.ok);
    let mut spans = Vec::new();
    let mut hits = Vec::new();
    let mut column = x;
    for (index, tab) in state.tabs().iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
            column = column.saturating_add(3);
        }
        let label = format!(" {} ", tab.label());
        let style = if index == state.active_tab {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else if matches!(tab, Tab::Build) && failed {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        };
        let width = Span::raw(label.as_str()).width() as u16;
        hits.push((column, column.saturating_add(width)));
        column = column.saturating_add(width);
        spans.push(Span::styled(label, style));
    }
    (Line::from(spans), hits)
}

/// What the composer row shows: the prompt and filter on the left, the
/// match count, severity filter or error on the right, and the column the
/// cursor sits on.
pub(super) struct Composer {
    pub(super) input: Line<'static>,
    pub(super) detail: Line<'static>,
    pub(super) cursor: u16,
}

const PROMPT: &str = " › ";

/// The composer: typing filters the logs as it goes.
pub(super) fn composer(state: &TuiState, matches: usize) -> Composer {
    let dim = Style::default().fg(Color::DarkGray);
    let mut input = vec![Span::styled(
        PROMPT,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    if state.query_text.is_empty() {
        input.push(Span::styled("Filter logs · re: for a regex", dim));
    } else {
        input.push(Span::raw(state.query_text.clone()));
    }
    let cursor = Span::raw(PROMPT).width() + Span::raw(state.query_text.as_str()).width();

    let mut detail = Vec::new();
    if let Some(error) = &state.search_error {
        detail.push(Span::styled(
            error_summary(error),
            Style::default().fg(Color::Red),
        ));
    } else {
        if state.level_filter() != LevelFilter::All {
            detail.push(Span::styled(
                state.level_filter().label(),
                Style::default().fg(Color::Yellow),
            ));
        }
        if !state.query_text.is_empty() {
            if !detail.is_empty() {
                detail.push(Span::styled(" · ", dim));
            }
            let noun = if matches == 1 { "match" } else { "matches" };
            detail.push(Span::styled(format!("{matches} {noun}"), dim));
        }
    }
    if !detail.is_empty() {
        detail.push(Span::raw(" "));
    }
    Composer {
        input: Line::from(input),
        detail: Line::from(detail),
        cursor: u16::try_from(cursor).unwrap_or(u16::MAX),
    }
}

/// The regex crate's errors span several lines; the composer has one.
fn error_summary(error: &str) -> String {
    let last = error
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(error);
    format!("regex: {}", last.trim().trim_start_matches("error: "))
}

/// The footer: a toast, else the paused notice and `? for shortcuts`.
pub(super) fn footer(state: &TuiState) -> Line<'static> {
    if let Some(toast) = state.toast() {
        return Line::styled(
            format!(" {toast}"),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        );
    }
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = Vec::new();
    if !state.autoscroll {
        spans.push(Span::styled(
            " paused · End to follow ",
            Style::default().fg(Color::Yellow),
        ));
        spans.push(Span::styled("·", dim));
    }
    spans.push(Span::styled(
        " ? ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled("for shortcuts", dim));
    Line::from(spans)
}

const SHORTCUTS: [(&str, &str); 10] = [
    ("wheel", "scroll the logs"),
    ("drag", "select text, copied on release"),
    ("tab / ⇧tab", "switch view (or click a tab)"),
    ("end / enter", "follow new lines"),
    ("type", "filter · re: for a regex"),
    ("esc / ^U", "clear the filter"),
    ("^E", "cycle the severity filter"),
    ("^R", "restart this tab's binary"),
    ("^L", "clear this view"),
    ("^C / ^D", "quit"),
];

/// Rows the shortcut list takes in two columns.
pub(super) fn shortcut_rows() -> u16 {
    SHORTCUTS.len().div_ceil(2) as u16
}

/// The shortcut list, two columns, keys bold and aligned.
pub(super) fn shortcuts() -> Vec<Line<'static>> {
    let key_width = SHORTCUTS
        .iter()
        .map(|(key, _)| Span::raw(*key).width())
        .max()
        .unwrap_or(0);
    let action_width = SHORTCUTS
        .iter()
        .map(|(_, action)| Span::raw(*action).width())
        .max()
        .unwrap_or(0);
    let rows = usize::from(shortcut_rows());
    let cell = |(key, action): (&str, &str), pad: bool| {
        let key_pad = key_width - Span::raw(key).width();
        let action_pad = if pad {
            action_width - Span::raw(action).width() + 3
        } else {
            0
        };
        [
            Span::styled(
                format!(" {key}{}", " ".repeat(key_pad)),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {action}{}", " ".repeat(action_pad)),
                Style::default().fg(Color::DarkGray),
            ),
        ]
    };
    (0..rows)
        .map(|row| {
            let mut spans = cell(SHORTCUTS[row], true).to_vec();
            if let Some(right) = SHORTCUTS.get(row + rows) {
                spans.extend(cell(*right, false));
            }
            Line::from(spans)
        })
        .collect()
}
