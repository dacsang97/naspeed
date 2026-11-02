use std::collections::HashMap;

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Color, Style},
    text::Text,
    widgets::{Block, Paragraph},
};

use super::app_state::{ScrollPosition, TuiState};
use super::render_status;
use super::text_selection::{RowInfo, TextSelection};
use super::wrap;

const WRAP_CACHE_LIMIT: usize = 100_000;

/// A painted log row: its line, the row's offset within the line, and the
/// width of the `[source] ` tag before the text.
#[derive(Clone, Copy)]
struct RowSlot {
    id: u64,
    offset: u16,
    indent: u16,
}

#[derive(Default)]
pub(super) struct ScreenMap {
    pub(super) log_area: Rect,
    rows: Vec<Option<RowSlot>>,
    /// The log area as last painted: what a new selection starts from.
    painted: Option<Buffer>,
    pub(super) selection: Option<TextSelection>,
    /// Row of the tab bar and each tab's columns `[start, end)`.
    tab_row: u16,
    tab_spans: Vec<(u16, u16)>,
    /// Lines passing the filter at the last live render.
    visible_count: usize,
    widths: HashMap<u64, (u16, u16)>,
    cached_width: Option<u16>,
    previous_scroll: Option<ScrollPosition>,
    previous_generation: Option<u64>,
}

impl ScreenMap {
    pub(super) fn in_log_area(&self, position: Position) -> bool {
        self.log_area.contains(position)
    }

    /// The tab under a click on the tab bar.
    pub(super) fn tab_at(&self, position: Position) -> Option<usize> {
        if position.y != self.tab_row {
            return None;
        }
        self.tab_spans
            .iter()
            .position(|(start, end)| (*start..*end).contains(&position.x))
    }

    /// Start a selection on the log area as last painted.
    pub(super) fn start_selection(&mut self, at: Position) {
        let Some(painted) = self.painted.clone() else {
            return;
        };
        let rows = self
            .rows
            .iter()
            .map(|slot| {
                slot.map(|slot| RowInfo {
                    continues: slot.offset > 0,
                    indent: slot.indent,
                })
            })
            .collect();
        self.selection = Some(TextSelection::start(painted, rows, at));
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
        self.rows.iter().flatten().find_map(|slot| {
            state
                .lines
                .get(&slot.id)
                .is_some_and(|line| state.in_active_tab(line) && state.passes_filters(line))
                .then_some((slot.id, slot.offset))
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

/// Header (tabs left, process status right), logs, the composer on its
/// own surface, and a footer for hints and toasts.
pub(super) fn render(frame: &mut Frame<'_>, state: &mut TuiState, screen: &mut ScreenMap) {
    let footer_rows = if state.show_shortcuts {
        render_status::shortcut_rows()
    } else {
        1
    };
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(3),
        Constraint::Length(footer_rows),
    ])
    .split(frame.area());
    render_header(frame, state, screen, areas[0]);
    // A selection holds the frame it started on until it is dropped; a
    // resize invalidates it.
    if screen
        .selection
        .as_ref()
        .is_some_and(|selection| selection.area() != areas[1])
    {
        screen.selection = None;
    }
    match &screen.selection {
        Some(selection) => selection.paint(frame.buffer_mut()),
        None => render_logs(frame, state, screen, areas[1]),
    }
    render_composer(frame, state, screen.visible_count, areas[2]);
    let footer = if state.show_shortcuts {
        render_status::shortcuts()
    } else {
        vec![render_status::footer(state)]
    };
    frame.render_widget(Paragraph::new(footer), areas[3]);
}

fn render_header(frame: &mut Frame<'_>, state: &TuiState, screen: &mut ScreenMap, area: Rect) {
    let (tabs, spans) = render_status::tabs(state, area.x);
    let used = (tabs.width() as u16).saturating_add(2);
    frame.render_widget(Paragraph::new(tabs), area);
    screen.tab_row = area.y;
    screen.tab_spans = spans;
    let room = area.width.saturating_sub(used);
    if room > 0 {
        let status = Rect::new(area.right() - room, area.y, room, 1);
        frame.render_widget(
            Paragraph::new(render_status::status_line(state)).alignment(Alignment::Right),
            status,
        );
    }
}

/// The composer: a shaded block with the prompt on its middle row, the
/// match count or error on the right, and the terminal cursor at the end
/// of the typed filter.
fn render_composer(frame: &mut Frame<'_>, state: &TuiState, matches: usize, area: Rect) {
    if area.height == 0 {
        return;
    }
    let surface = state
        .surface
        .map_or_else(Style::default, |background| Style::default().bg(background));
    frame.render_widget(Block::default().style(surface), area);
    let row = Rect::new(area.x, area.y + (area.height - 1) / 2, area.width, 1);
    let prompt = render_status::composer(state, matches);
    frame.render_widget(Paragraph::new(prompt.input).style(surface), row);
    let detail_width = (prompt.detail.width() as u16).min(row.width);
    if detail_width > 0 {
        let detail = Rect::new(row.right() - detail_width, row.y, detail_width, 1);
        frame.render_widget(Paragraph::new(prompt.detail).style(surface), detail);
    }
    let cursor = prompt.cursor.min(row.width.saturating_sub(1));
    frame.set_cursor_position(Position::new(row.x + cursor, row.y));
}

fn render_logs(frame: &mut Frame<'_>, state: &mut TuiState, screen: &mut ScreenMap, area: Rect) {
    screen.log_area = area;
    screen.painted = None;
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
    let mut count = 0usize;
    let mut anchor_position = None;
    for line in state.visible_lines() {
        count += 1;
        let prefix = state.line_prefix(&line.source);
        let height = usize::from(screen.height(line.id, &line.plain, &prefix, area.width));
        if let Some((id, row_offset)) = anchor
            && id == line.id
        {
            anchor_position =
                Some(total.saturating_add(usize::from(row_offset).min(height.saturating_sub(1))));
        }
        total = total.saturating_add(height);
    }
    screen.visible_count = count;
    if count == 0 {
        frame.render_widget(empty_state(state, area), area);
    }
    screen.preserve_paused_scroll(state, preserve_anchor, anchor_position);
    let (start, end) = window(state, total, usize::from(area.height));
    screen.previous_generation = Some(state.view_generation);
    screen.previous_scroll = Some(state.scroll);
    screen.rows.clear();
    screen.rows.resize(usize::from(area.height), None);
    let mut offset = 0usize;
    for line in state.visible_lines() {
        let prefix = state.line_prefix(&line.source);
        let line_height = usize::from(screen.height(line.id, &line.plain, &prefix, area.width));
        let line_end = offset.saturating_add(line_height);
        let visible_start = offset.max(start);
        let visible_end = line_end.min(end);
        if visible_start < visible_end {
            let hits = state
                .query
                .as_ref()
                .map(|query| query.ranges(&line.plain))
                .unwrap_or_default();
            let content = wrap::styled_lines(&line.raw, &line.plain, &prefix, area.width, &hits);
            let indent = wrap::indent_width(&prefix, area.width);
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
                    *slot = Some(RowSlot {
                        id: line.id,
                        offset: row_offset,
                        indent,
                    });
                }
            }
        }
        offset = line_end;
        if offset >= end {
            break;
        }
    }
    let buffer = frame.buffer_mut();
    let mut painted = Buffer::empty(area);
    for position in area.intersection(buffer.area).positions() {
        painted[position] = buffer[position].clone();
    }
    screen.painted = Some(painted);
}

fn window(state: &mut TuiState, total: usize, height: usize) -> (usize, usize) {
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
    }
}

/// What an empty log view says: nothing logged yet, or nothing matching.
fn empty_state(state: &TuiState, area: Rect) -> Paragraph<'static> {
    let text = if !state.query_text.is_empty() {
        format!("No lines match “{}”", state.query_text)
    } else if state.level_filter() != super::search::LevelFilter::All {
        format!("No {} lines", state.level_filter().label())
    } else {
        "Waiting for output…".to_string()
    };
    let top = usize::from(area.height.saturating_sub(1) / 2);
    let mut lines = vec![ratatui::text::Line::default(); top];
    lines.push(ratatui::text::Line::styled(
        text,
        Style::default().fg(Color::DarkGray),
    ));
    Paragraph::new(lines).alignment(Alignment::Center)
}
