use std::ops::Range;

use ansi_to_tui::IntoText as _;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span, Text},
};

/// Columns a wrapped row is indented by: the tag's width, or a short
/// hanging indent for untagged lines so a wrapped row never reads as a
/// line of its own.
const HANGING_INDENT: usize = 2;

pub(super) fn indent_width(prefix: &str, width: u16) -> u16 {
    let max = usize::from(width).saturating_sub(1);
    let tag = Line::raw(clipped_prefix(prefix, max).as_str()).width();
    let indent = if tag == 0 { HANGING_INDENT } else { tag };
    indent.min(max) as u16
}

pub(super) fn height(plain: &str, prefix: &str, width: u16) -> u16 {
    if width == 0 {
        return 1;
    }
    let indent_width = usize::from(indent_width(prefix, width));
    let width = usize::from(width);
    let prefix = clipped_prefix(prefix, width.saturating_sub(1));
    let mut column = Line::raw(prefix.as_str()).width();
    let mut rows = 1usize;
    for grapheme in Line::raw(plain).styled_graphemes(Style::default()) {
        let grapheme_width = Line::raw(grapheme.symbol).width();
        if grapheme_width > 0 && column + grapheme_width > width && column > indent_width {
            rows += 1;
            column = indent_width;
        }
        column += grapheme_width;
    }
    rows.min(usize::from(u16::MAX)) as u16
}

/// The line's rows at `width`, with the byte ranges of `plain` in
/// `matches` (the filter's hits) highlighted.
pub(super) fn styled_lines(
    raw: &str,
    plain: &str,
    prefix: &str,
    width: u16,
    matches: &[Range<usize>],
) -> Vec<Line<'static>> {
    let indent = " ".repeat(usize::from(indent_width(prefix, width.max(1))));
    let width = usize::from(width.max(1));
    let prefix = clipped_prefix(prefix, width.saturating_sub(1));
    let background = Style::default();
    let prefix_style = Style::default().fg(Color::DarkGray).patch(background);
    let text = raw
        .into_text()
        .unwrap_or_else(|_| Text::raw(plain.to_string()));
    let mut rows = Vec::new();
    let mut builder = RowBuilder::new(prefix, indent, width, prefix_style, background);
    let match_style = Style::default().fg(Color::Black).bg(Color::Yellow);
    let mut offset = 0usize;
    for (line_index, line) in text.lines.iter().enumerate() {
        if line_index > 0 {
            builder.new_line();
        }
        for span in &line.spans {
            for grapheme in span.styled_graphemes(line.style) {
                let hit = matches.iter().any(|range| range.contains(&offset));
                offset += grapheme.symbol.len();
                let style = if hit {
                    grapheme.style.patch(match_style)
                } else {
                    grapheme.style
                };
                builder.push(grapheme.symbol, style);
            }
        }
    }
    rows.extend(builder.finish());
    if rows.is_empty() {
        rows.push(Line::default());
    }
    rows
}

fn clipped_prefix(prefix: &str, max_width: usize) -> String {
    let mut clipped = String::new();
    let mut width = 0;
    for grapheme in Line::raw(prefix).styled_graphemes(Style::default()) {
        let next = Line::raw(grapheme.symbol).width();
        if width + next > max_width {
            break;
        }
        clipped.push_str(grapheme.symbol);
        width += next;
    }
    clipped
}

struct RowBuilder {
    rows: Vec<Line<'static>>,
    row: Line<'static>,
    column: usize,
    indent_width: usize,
    width: usize,
    indent: String,
    prefix_style: Style,
    background: Style,
    pending_style: Option<Style>,
    pending: String,
}

impl RowBuilder {
    fn new(
        prefix: String,
        indent: String,
        width: usize,
        prefix_style: Style,
        background: Style,
    ) -> Self {
        let indent_width = Line::raw(indent.as_str()).width();
        let column = Line::raw(prefix.as_str()).width();
        let mut row = Line::default();
        row.push_span(Span::styled(prefix, prefix_style));
        Self {
            rows: Vec::new(),
            row,
            column,
            indent_width,
            width,
            indent,
            prefix_style,
            background,
            pending_style: None,
            pending: String::new(),
        }
    }

    fn push(&mut self, symbol: &str, style: Style) {
        let symbol_width = Line::raw(symbol).width();
        if symbol_width > 0
            && self.column + symbol_width > self.width
            && self.column > self.indent_width
        {
            self.flush();
            self.rows.push(std::mem::take(&mut self.row));
            self.row = Line::from(Span::styled(self.indent.clone(), self.prefix_style));
            self.column = self.indent_width;
        }
        let style = style.patch(self.background);
        if self.pending_style.is_some_and(|current| current != style) {
            self.flush();
        }
        self.pending_style = Some(style);
        self.pending.push_str(symbol);
        self.column += symbol_width;
    }

    fn new_line(&mut self) {
        self.flush();
        self.rows.push(std::mem::take(&mut self.row));
        self.row = Line::from(Span::styled(self.indent.clone(), self.prefix_style));
        self.column = self.indent_width;
    }

    fn flush(&mut self) {
        if let Some(style) = self.pending_style.take() {
            self.row
                .push_span(Span::styled(std::mem::take(&mut self.pending), style));
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        self.rows.push(self.row);
        self.rows
    }
}
