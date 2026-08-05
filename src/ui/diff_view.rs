//! Renders the diff pane: file/hunk headers and gutter-numbered, syntax
//! highlighted content lines. All layout math (gutter width, truncation)
//! goes through `ui::text`'s display-width helpers so CJK content lines
//! neither misalign the gutter nor wrap.

use crate::diff::{DiffLineKind, RenderRow};
use crate::highlight::{HighlightKind, Language, LineHighlighter, Span as HlSpan};
use crate::ui::app::App;
use crate::ui::text::{display_width, truncate_to_width};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// Old/new line numbers are right-aligned in a field this wide, which
/// comfortably covers files up to 99,999 lines before numbers start
/// crowding the separator.
const LINE_NUMBER_WIDTH: usize = 5;

pub fn render(frame: &mut Frame, area: Rect, app: &App, highlighter: &mut LineHighlighter) {
    let block = Block::default().borders(Borders::LEFT).title(" diff ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let content_width = inner.width as usize;
    let visible = app
        .rows
        .iter()
        .enumerate()
        .skip(app.scroll_offset)
        .take(inner.height as usize);

    let lines: Vec<Line> = visible
        .map(|(idx, row)| render_row(app, *row, idx == app.cursor, content_width, highlighter))
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_row(
    app: &App,
    row: RenderRow,
    is_cursor: bool,
    width: usize,
    highlighter: &mut LineHighlighter,
) -> Line<'static> {
    match row {
        RenderRow::FileHeader { file_idx } => file_header_line(app, file_idx, width, is_cursor),
        RenderRow::HunkHeader { file_idx, hunk_idx } => {
            hunk_header_line(app, file_idx, hunk_idx, width, is_cursor)
        }
        RenderRow::Line {
            file_idx,
            hunk_idx,
            row_idx,
        } => content_line(
            app,
            file_idx,
            hunk_idx,
            row_idx,
            width,
            is_cursor,
            highlighter,
        ),
    }
}

fn cursor_style(base: Style, is_cursor: bool) -> Style {
    if is_cursor {
        base.add_modifier(Modifier::REVERSED)
    } else {
        base
    }
}

fn file_header_line(app: &App, file_idx: usize, width: usize, is_cursor: bool) -> Line<'static> {
    let file = &app.files[file_idx];
    let (added, deleted) = file.stat();
    let status = if file.is_new {
        " [new]"
    } else if file.is_deleted {
        " [deleted]"
    } else if file.is_renamed {
        " [renamed]"
    } else {
        ""
    };
    let text = format!(
        "{}{status} (+{added} -{deleted})",
        truncate_to_width(file.display_path(), width.saturating_sub(20))
    );
    let style = cursor_style(
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(Color::White),
        is_cursor,
    );
    Line::from(Span::styled(pad_or_truncate(&text, width), style))
}

fn hunk_header_line(
    app: &App,
    file_idx: usize,
    hunk_idx: usize,
    width: usize,
    is_cursor: bool,
) -> Line<'static> {
    let hunk = &app.files[file_idx].hunks[hunk_idx];
    let text = format!(
        "  @@ -{},{} +{},{} @@ {}",
        hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines, hunk.header
    );
    let style = cursor_style(Style::default().fg(Color::Cyan), is_cursor);
    Line::from(Span::styled(pad_or_truncate(&text, width), style))
}

fn content_line(
    app: &App,
    file_idx: usize,
    hunk_idx: usize,
    row_idx: usize,
    width: usize,
    is_cursor: bool,
    highlighter: &mut LineHighlighter,
) -> Line<'static> {
    let file = &app.files[file_idx];
    let row = &file.hunks[hunk_idx].rows[row_idx];

    let (marker, marker_color, bg) = match row.kind {
        DiffLineKind::Add => ('+', Color::Green, Some(Color::Rgb(0, 40, 0))),
        DiffLineKind::Del => ('-', Color::Red, Some(Color::Rgb(45, 0, 0))),
        DiffLineKind::Context => (' ', Color::Reset, None),
    };

    let gutter = format!(
        "{marker} {} {} \u{2502} ",
        format_line_number(row.old_line),
        format_line_number(row.new_line),
    );
    let gutter_width = display_width(&gutter);
    let content_width = width.saturating_sub(gutter_width);

    let language = Language::detect(file.display_path());
    let spans = highlighter.highlight_line(language, &row.text);
    let spans = truncate_spans_to_width(spans, content_width);

    let mut line_spans = vec![Span::styled(
        gutter,
        base_style(bg).fg(marker_color).add_modifier(Modifier::BOLD),
    )];
    for span in spans {
        line_spans.push(Span::styled(
            span.text,
            base_style(bg).fg(highlight_color(span.kind)),
        ));
    }

    let rendered_width: usize = line_spans.iter().map(ratatui::text::Span::width).sum();
    if rendered_width < width {
        line_spans.push(Span::styled(
            " ".repeat(width - rendered_width),
            base_style(bg),
        ));
    }

    let mut line = Line::from(line_spans);
    if is_cursor {
        line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
    }
    line
}

fn base_style(bg: Option<Color>) -> Style {
    match bg {
        Some(color) => Style::default().bg(color),
        None => Style::default(),
    }
}

fn highlight_color(kind: HighlightKind) -> Color {
    match kind {
        HighlightKind::Keyword => Color::Magenta,
        HighlightKind::String => Color::Yellow,
        HighlightKind::Comment => Color::DarkGray,
        HighlightKind::Function => Color::Blue,
        HighlightKind::Type => Color::Cyan,
        HighlightKind::Number => Color::LightMagenta,
        HighlightKind::Operator => Color::Gray,
        HighlightKind::Variable | HighlightKind::Plain => Color::Reset,
    }
}

fn format_line_number(n: Option<u32>) -> String {
    match n {
        Some(n) => format!("{n:>LINE_NUMBER_WIDTH$}"),
        None => " ".repeat(LINE_NUMBER_WIDTH),
    }
}

fn pad_or_truncate(s: &str, width: usize) -> String {
    let truncated = truncate_to_width(s, width);
    let pad = width.saturating_sub(display_width(&truncated));
    truncated + &" ".repeat(pad)
}

/// Walks highlighted spans in order, dropping/truncating them once the
/// cumulative display width reaches `max_width`, so a highlighted line never
/// overflows the pane even when the cut falls mid-span.
fn truncate_spans_to_width(spans: Vec<HlSpan>, max_width: usize) -> Vec<HlSpan> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        if used >= max_width {
            break;
        }
        let remaining = max_width - used;
        let span_width = display_width(&span.text);
        if span_width <= remaining {
            used += span_width;
            out.push(span);
        } else {
            let truncated = truncate_to_width(&span.text, remaining);
            out.push(HlSpan {
                text: truncated,
                kind: span.kind,
            });
            break;
        }
    }
    out
}
