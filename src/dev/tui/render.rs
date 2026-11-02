use std::collections::HashMap;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::Text,
    widgets::Paragraph,
};

use super::app_state::{ScrollPosition, TuiState};
use super::render_status;
use super::wrap;

const WRAP_CACHE_LIMIT: usize = 100_000;

#[derive(Default)]
pub(super) struct ScreenMap {
    pub(super) log_area: Rect,
    rows: Vec<Option<(u64, u16)>>,
    widths: HashMap<u64, (u16, u16)>,
    cached_width: Option<u16>,
    previous_scroll: Option<ScrollPosition>,
    previous_generation: Option<u64>,
}

impl ScreenMap {
    pub(super) fn line_at(&self, row: u16) -> Option<u64> {
        if row < self.log_area.y || row >= self.log_area.y.saturating_add(self.log_area.height) {
            return None;
        }
        self.rows
            .get(usize::from(row - self.log_area.y))
            .copied()
            .flatten()
            .map(|(id, _)| id)
    }

    fn height(&mut self, id: u64, plain: &str, prefix: &str, width: u16) -> u16 {
        if self.cached_width != Some(width) {
            self.widths.clear();
            self.cached_width = Some(width);
        }
        if let Some((_, height)) = self.widths.get(&id).filter(|(cached, _)| *cached == width) {
            return *height;
        }
        if self.widths.len() >= WRAP_CACHE_LIMIT {
            self.widths.clear();
        }
        let height = wrap::height(plain, prefix, width);
        self.widths.insert(id, (width, height));
        height
    }

    fn paused_anchor(&self, state: &TuiState) -> Option<(u64, u16)> {
        self.rows.iter().flatten().copied().find(|(id, _)| {
            state
                .lines
                .get(id)
                .is_some_and(|line| state.in_active_tab(line) && state.passes_filters(line))
        })
    }

    fn preserve_paused_scroll(
        &self,
        state: &mut TuiState,
        preserve_anchor: bool,
        anchor_position: Option<usize>,
    ) {
        if state.autoscroll {
            return;
        }
        if preserve_anchor {
            state.scroll = ScrollPosition::RowsFromTop(anchor_position.unwrap_or(0));
        } else if state.scroll == ScrollPosition::Bottom {
            state.scroll = ScrollPosition::RowsFromBottom(0);
        }
    }
}

pub(super) fn render(frame: &mut Frame<'_>, state: &mut TuiState, screen: &mut ScreenMap) {
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(frame.area());
    frame.render_widget(Paragraph::new(render_status::status_line(state)), areas[0]);
    frame.render_widget(render_status::tabs(state), areas[1]);
    frame.render_widget(Paragraph::new(render_status::search_line(state)), areas[2]);
    frame.render_widget(Paragraph::new(render_status::footer(state)), areas[4]);
    render_logs(frame, state, screen, areas[3]);
}

fn render_logs(frame: &mut Frame<'_>, state: &mut TuiState, screen: &mut ScreenMap, area: Rect) {
    screen.log_area = area;
    if area.width == 0 || area.height == 0 {
        screen.rows.clear();
        screen.previous_scroll = None;
        screen.previous_generation = None;
        return;
    }

    let preserve_anchor = !state.autoscroll
        && screen.previous_generation == Some(state.view_generation)
        && screen.previous_scroll == Some(state.scroll);
    let anchor = if preserve_anchor {
        screen.paused_anchor(state)
    } else {
        None
    };
    let mut total = 0usize;
    let mut cursor_rows = None;
    let mut anchor_position = None;
    for line in state.visible_lines() {
        let prefix = format!("[{}] ", TuiState::source_name(&line.source));
        let height = usize::from(screen.height(line.id, &line.plain, &prefix, area.width));
        if state.cursor == Some(line.id) {
            cursor_rows = Some((total, total.saturating_add(height)));
        }
        if let Some((id, row_offset)) = anchor
            && id == line.id
        {
            anchor_position =
                Some(total.saturating_add(usize::from(row_offset).min(height.saturating_sub(1))));
        }
        total = total.saturating_add(height);
    }
    screen.preserve_paused_scroll(state, preserve_anchor, anchor_position);
    let (start, end) = window(state, total, usize::from(area.height), cursor_rows);
    screen.previous_generation = Some(state.view_generation);
    screen.previous_scroll = Some(state.scroll);
    screen.rows.clear();
    screen.rows.resize(usize::from(area.height), None);
    let selected = state.selected_line_ids();
    let mut offset = 0usize;
    for line in state.visible_lines() {
        let prefix = format!("[{}] ", TuiState::source_name(&line.source));
        let line_height = usize::from(screen.height(line.id, &line.plain, &prefix, area.width));
        let line_end = offset.saturating_add(line_height);
        let visible_start = offset.max(start);
        let visible_end = line_end.min(end);
        if visible_start < visible_end {
            let highlight = state.line_matches_query(line);
            let content = wrap::styled_lines(
                &line.raw,
                &line.plain,
                &prefix,
                area.width,
                highlight,
                selected.contains(&line.id),
            );
            let skip = visible_start
                .saturating_sub(offset)
                .min(usize::from(u16::MAX)) as u16;
            let height = visible_end - visible_start;
            let rect = Rect::new(
                area.x,
                area.y.saturating_add((visible_start - start) as u16),
                area.width,
                height.min(usize::from(u16::MAX)) as u16,
            );
            frame.render_widget(
                Paragraph::new(Text::from(content))
                    .scroll((skip, 0))
                    .style(Style::default().fg(Color::Reset)),
                rect,
            );
            for row in visible_start..visible_end {
                if let Some(slot) = screen.rows.get_mut(row - start) {
                    let row_offset = row.saturating_sub(offset).min(usize::from(u16::MAX)) as u16;
                    *slot = Some((line.id, row_offset));
                }
            }
        }
        offset = line_end;
        if offset >= end {
            break;
        }
    }
}

fn window(
    state: &mut TuiState,
    total: usize,
    height: usize,
    cursor_rows: Option<(usize, usize)>,
) -> (usize, usize) {
    let bottom = total.saturating_sub(height);
    match state.scroll {
        ScrollPosition::Top => (0, total.min(height)),
        ScrollPosition::RowsFromTop(rows) => {
            let start = rows.min(bottom);
            state.scroll = if bottom == 0 {
                ScrollPosition::RowsFromTop(0)
            } else if start == bottom {
                ScrollPosition::RowsFromBottom(0)
            } else {
                ScrollPosition::RowsFromTop(rows)
            };
            (start, start.saturating_add(height).min(total))
        }
        ScrollPosition::Bottom => (bottom, total),
        ScrollPosition::RowsFromBottom(rows) => {
            let offset = rows.min(bottom);
            let end = total.saturating_sub(offset);
            (end.saturating_sub(height), end)
        }
        ScrollPosition::EnsureCursor => {
            let (start, end) = match cursor_rows {
                Some((cursor_start, _)) if cursor_start < bottom => {
                    (cursor_start, cursor_start.saturating_add(height).min(total))
                }
                _ => (bottom, total),
            };
            state.scroll = ScrollPosition::RowsFromBottom(total.saturating_sub(end));
            (start, end)
        }
    }
}
