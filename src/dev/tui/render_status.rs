use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Tabs,
};

use super::super::log_bus::AppState;
use super::app_state::{BuildStatus, Tab, TuiState};

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

pub(super) fn tabs(state: &TuiState) -> Tabs<'static> {
    let failed = state.build_status.is_some_and(|status| !status.ok);
    let titles = state
        .tabs()
        .iter()
        .map(|tab| match tab {
            Tab::Build if failed => {
                Line::styled(tab.label().to_string(), Style::default().fg(Color::Red))
            }
            _ => Line::raw(tab.label().to_string()),
        })
        .collect::<Vec<_>>();
    Tabs::new(titles)
        .select(state.active_tab)
        .divider(" │ ")
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
}

pub(super) fn search_line(state: &TuiState) -> Line<'static> {
    let (text, error, editing) = state.search_display();
    let mut spans = vec![Span::styled(" /", Style::default().fg(Color::Cyan))];
    if text.is_empty() && !editing {
        spans.push(Span::styled("search", Style::default().fg(Color::DarkGray)));
    } else {
        spans.push(Span::raw(text.to_string()));
        if editing {
            spans.push(Span::styled("▏", Style::default().fg(Color::Yellow)));
        }
    }
    if let Some(error) = error {
        spans.push(Span::styled(
            format!("  ! {error}"),
            Style::default().fg(Color::Red),
        ));
    } else {
        let mode = if state.filter_mode {
            "filter"
        } else {
            "highlight"
        };
        spans.push(Span::styled(
            format!("  {mode} · {}", state.level_filter().label()),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

pub(super) fn footer(state: &TuiState) -> Line<'static> {
    if let Some(toast) = state.toast() {
        return Line::styled(
            toast.to_string(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        );
    }
    let auto = if state.autoscroll { "auto" } else { "paused" };
    Line::styled(
        format!(
            "↑↓/PgUp/PgDn scroll · a {auto} · / search · f filter · e level · v select · c copy · r restart · C clear · q quit"
        ),
        Style::default().fg(Color::DarkGray),
    )
}
