//! Renders the diff pane: file/hunk headers and gutter-numbered, syntax
//! highlighted content lines. All layout math (gutter width, truncation)
//! goes through `ui::text`'s display-width helpers so CJK content lines
//! neither misalign the gutter nor wrap.

use crate::diff::{DiffLineKind, RenderRow, SideBySideRow, SideCell, side_by_side_scroll_start};
use crate::highlight::{Language, LineHighlighter};
use crate::ui::app::{App, Layout};
use crate::ui::symbols;
use crate::ui::text::{
    display_width, highlight_color, mark_range, truncate_spans_to_width, truncate_to_width,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// Old/new line numbers are right-aligned in a field this wide, which
/// comfortably covers files up to 99,999 lines before numbers start
/// crowding the separator.
const LINE_NUMBER_WIDTH: usize = 5;

/// Below this pane width, two side-by-side columns plus gutters would be too
/// cramped to read, so `effective_layout` falls back to unified.
pub const MIN_SIDE_BY_SIDE_WIDTH: u16 = 100;

/// The layout to actually render: `requested`, unless it's
/// [`Layout::SideBySide`] and `available_width` is too narrow for two
/// readable columns, in which case unified is used instead. Kept separate
/// from `App.layout` (the user's *requested* layout, which `s` toggles)
/// because the fallback is a property of the current terminal size, not a
/// decision the user made — resizing wider should bring side-by-side back
/// without the user having to press `s` again.
pub fn effective_layout(requested: Layout, available_width: u16) -> Layout {
    match requested {
        Layout::SideBySide if available_width < MIN_SIDE_BY_SIDE_WIDTH => Layout::Unified,
        other => other,
    }
}

pub fn render(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    highlighter: &mut LineHighlighter,
    layout: Layout,
) {
    let block = Block::default().borders(Borders::LEFT).title(" diff ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match layout {
        Layout::Unified => render_unified(frame, inner, app, highlighter),
        Layout::SideBySide => render_side_by_side(frame, inner, app, highlighter),
    }
}

fn render_unified(frame: &mut Frame, inner: Rect, app: &App, highlighter: &mut LineHighlighter) {
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

/// Renders each hunk's deletions and insertions in row-aligned old/new
/// columns (see [`crate::diff::flatten_side_by_side`]), separated by a
/// divider. File/hunk/binary-notice rows span both columns, reusing
/// [`render_row`] at the pane's full width exactly as the unified layout
/// does.
fn render_side_by_side(
    frame: &mut Frame,
    inner: Rect,
    app: &App,
    highlighter: &mut LineHighlighter,
) {
    let width = inner.width as usize;
    let divider_width = 1;
    let left_width = width.saturating_sub(divider_width) / 2;
    let right_width = width.saturating_sub(divider_width + left_width);

    let start = side_by_side_scroll_start(&app.side_by_side_rows, app.scroll_offset);
    let visible = app
        .side_by_side_rows
        .iter()
        .skip(start)
        .take(inner.height as usize);

    let lines: Vec<Line> = visible
        .map(|row| side_by_side_row_line(app, *row, left_width, right_width, highlighter))
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn side_by_side_row_line(
    app: &App,
    row: SideBySideRow,
    left_width: usize,
    right_width: usize,
    highlighter: &mut LineHighlighter,
) -> Line<'static> {
    match row {
        SideBySideRow::Full { flat_idx } => render_row(
            app,
            app.rows[flat_idx],
            flat_idx == app.cursor,
            left_width + 1 + right_width,
            highlighter,
        ),
        SideBySideRow::Paired { old, new } => {
            let mut spans = side_cell_spans(app, old, left_width, highlighter).spans;
            spans.push(Span::styled(
                "\u{2502}",
                Style::default().fg(Color::DarkGray),
            ));
            spans.extend(side_cell_spans(app, new, right_width, highlighter).spans);
            Line::from(spans)
        }
    }
}

/// One column's worth of spans for a [`SideCell`]: the same rendering
/// `render_row` already produces for that flat row when populated, or a
/// blank filler line at `width` when the other side has no counterpart
/// here.
fn side_cell_spans(
    app: &App,
    cell: SideCell,
    width: usize,
    highlighter: &mut LineHighlighter,
) -> Line<'static> {
    match cell {
        SideCell::Line { flat_idx } => render_row(
            app,
            app.rows[flat_idx],
            flat_idx == app.cursor,
            width,
            highlighter,
        ),
        SideCell::Empty => Line::from(Span::raw(" ".repeat(width))),
    }
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
        RenderRow::BinaryNotice { file_idx } => binary_notice_line(app, file_idx, width, is_cursor),
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

fn binary_notice_line(
    _app: &App,
    _file_idx: usize,
    width: usize,
    is_cursor: bool,
) -> Line<'static> {
    let style = cursor_style(Style::default().fg(Color::DarkGray), is_cursor);
    Line::from(Span::styled(
        pad_or_truncate("  binary file (contents not shown)", width),
        style,
    ))
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
    let spans = truncate_spans_to_width(&spans, content_width);

    let mut content_spans: Vec<Span<'static>> = spans
        .into_iter()
        .map(|span| Span::styled(span.text, base_style(bg).fg(highlight_color(span.kind))))
        .collect();
    if is_cursor && let Some(active) = symbols::scan(&row.text).get(app.active_symbol) {
        content_spans = mark_range(
            content_spans,
            active.display_start,
            active.display_end,
            Style::default().add_modifier(Modifier::UNDERLINED),
        );
    }

    let mut line_spans = vec![Span::styled(
        gutter,
        base_style(bg).fg(marker_color).add_modifier(Modifier::BOLD),
    )];
    line_spans.extend(content_spans);

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
