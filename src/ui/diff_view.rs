//! Renders the diff pane: file/hunk headers and gutter-numbered, syntax
//! highlighted content lines. All layout math (gutter width, truncation)
//! goes through `ui::text`'s display-width helpers so CJK content lines
//! neither misalign the gutter nor wrap.

use crate::comments::{CommentAnnotation, CommentIndex, Status as CommentStatus};
use crate::diff::{
    DiffFile, DiffLineKind, DiffRow, RenderRow, SideBySideRow, SideCell, lsp_target,
    side_by_side_scroll_start,
};
use crate::highlight::{Language, LineHighlighter};
use crate::lsp::DiagnosticsStore;
use crate::ui::app::{App, Layout};
use crate::ui::symbols;
use crate::ui::text::{
    display_width, highlight_color, mark_range, truncate_spans_to_width, truncate_to_width,
    wrap_text,
};
use lsp_types::DiagnosticSeverity;
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
    diagnostics: &DiagnosticsStore,
    comments: &CommentIndex,
) {
    let block = Block::default().borders(Borders::LEFT).title(" diff ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match layout {
        Layout::Unified => render_unified(frame, inner, app, highlighter, diagnostics, comments),
        Layout::SideBySide => {
            render_side_by_side(frame, inner, app, highlighter, diagnostics, comments)
        }
    }
}

/// The comments anchored (after relocation) to `row`'s current line, or
/// `&[]` for anything but a [`RenderRow::Line`] — file/hunk headers and
/// binary notices have no `(file, new_line)` a comment could be anchored
/// to. Shared by the gutter marker (always drawn) and the inline body block
/// (drawn only when `App::comments_visible`), so both agree on exactly
/// which comments belong to a row.
fn comments_for_row<'a>(
    app: &App,
    row: RenderRow,
    comments: &'a CommentIndex,
) -> &'a [CommentAnnotation] {
    let RenderRow::Line {
        file_idx,
        hunk_idx,
        row_idx,
    } = row
    else {
        return &[];
    };
    let file = &app.files[file_idx];
    let diff_row = &file.hunks[hunk_idx].rows[row_idx];
    match diff_row.new_line {
        Some(line) => comments.at(file.display_path(), line),
        None => &[],
    }
}

/// Renders the unified layout's visible window, interleaving each row with
/// its inline comment block (when `App::comments_visible`) directly
/// underneath it rather than as a fixed one-row-per-`RenderRow` mapping —
/// unlike every other row kind, a commented row can occupy more than one
/// terminal line. `app.scroll_offset`/`app.cursor` still index `app.rows`
/// itself (a comment block is never a distinct, independently-scrollable
/// row), so a heavily annotated diff's scroll position is measured in
/// `RenderRow`s the same way it always has been — the comment blocks below
/// the fold just consume more of the *visible* window per row than an
/// uncommented one would, a deliberate simplification over teaching the
/// scroll/cursor model about sub-row lines (see M6's notes for a possible
/// M7 revisit if that trade-off proves visually confusing in practice).
fn render_unified(
    frame: &mut Frame,
    inner: Rect,
    app: &App,
    highlighter: &mut LineHighlighter,
    diagnostics: &DiagnosticsStore,
    comments: &CommentIndex,
) {
    let content_width = inner.width as usize;
    let viewport_height = inner.height as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(viewport_height);

    let mut idx = app.scroll_offset;
    while lines.len() < viewport_height && idx < app.rows.len() {
        let row = app.rows[idx];
        lines.push(render_row(
            app,
            row,
            idx == app.cursor,
            content_width,
            highlighter,
            diagnostics,
            comments,
        ));
        if app.comments_visible {
            for block_line in
                comment_block_lines(comments_for_row(app, row, comments), content_width)
            {
                if lines.len() >= viewport_height {
                    break;
                }
                lines.push(block_line);
            }
        }
        idx += 1;
    }

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
    diagnostics: &DiagnosticsStore,
    comments: &CommentIndex,
) {
    let width = inner.width as usize;
    let divider_width = 1;
    let left_width = width.saturating_sub(divider_width) / 2;
    let right_width = width.saturating_sub(divider_width + left_width);
    let viewport_height = inner.height as usize;

    let start = side_by_side_scroll_start(&app.side_by_side_rows, app.scroll_offset);
    let mut lines: Vec<Line> = Vec::with_capacity(viewport_height);

    let mut idx = start;
    while lines.len() < viewport_height && idx < app.side_by_side_rows.len() {
        let row = app.side_by_side_rows[idx];
        lines.push(side_by_side_row_line(
            app,
            row,
            left_width,
            right_width,
            highlighter,
            diagnostics,
            comments,
        ));
        if app.comments_visible {
            let annotations = side_by_side_row_comments(app, row, comments);
            for block_line in comment_block_lines(annotations, width) {
                if lines.len() >= viewport_height {
                    break;
                }
                lines.push(block_line);
            }
        }
        idx += 1;
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

/// As [`comments_for_row`], for one [`SideBySideRow`] — only the *new* side
/// carries comment anchors (comments are only ever left on a `Context`/`Add`
/// row, which always has a `new_line` — see `App::comment_target`), so a
/// `Paired` row's old-side cell is never consulted here.
fn side_by_side_row_comments<'a>(
    app: &App,
    row: SideBySideRow,
    comments: &'a CommentIndex,
) -> &'a [CommentAnnotation] {
    let flat_idx = match row {
        SideBySideRow::Full { flat_idx } => Some(flat_idx),
        SideBySideRow::Paired {
            new: SideCell::Line { flat_idx },
            ..
        } => Some(flat_idx),
        SideBySideRow::Paired {
            new: SideCell::Empty,
            ..
        } => None,
    };
    match flat_idx {
        Some(flat_idx) => comments_for_row(app, app.rows[flat_idx], comments),
        None => &[],
    }
}

#[allow(clippy::too_many_arguments)] // mirrors `content_line`'s: every
// parameter is a distinct piece of what one paired row needs — see that
// function's comment for why bundling into a struct wouldn't help.
fn side_by_side_row_line(
    app: &App,
    row: SideBySideRow,
    left_width: usize,
    right_width: usize,
    highlighter: &mut LineHighlighter,
    diagnostics: &DiagnosticsStore,
    comments: &CommentIndex,
) -> Line<'static> {
    match row {
        SideBySideRow::Full { flat_idx } => render_row(
            app,
            app.rows[flat_idx],
            flat_idx == app.cursor,
            left_width + 1 + right_width,
            highlighter,
            diagnostics,
            comments,
        ),
        SideBySideRow::Paired { old, new } => {
            let mut spans =
                side_cell_spans(app, old, left_width, highlighter, diagnostics, comments).spans;
            spans.push(Span::styled(
                "\u{2502}",
                Style::default().fg(Color::DarkGray),
            ));
            spans.extend(
                side_cell_spans(app, new, right_width, highlighter, diagnostics, comments).spans,
            );
            Line::from(spans)
        }
    }
}

/// One column's worth of spans for a [`SideCell`]: the same rendering
/// `render_row` already produces for that flat row when populated, or a
/// blank filler line at `width` when the other side has no counterpart
/// here.
#[allow(clippy::too_many_arguments)] // see `content_line`'s comment
fn side_cell_spans(
    app: &App,
    cell: SideCell,
    width: usize,
    highlighter: &mut LineHighlighter,
    diagnostics: &DiagnosticsStore,
    comments: &CommentIndex,
) -> Line<'static> {
    match cell {
        SideCell::Line { flat_idx } => render_row(
            app,
            app.rows[flat_idx],
            flat_idx == app.cursor,
            width,
            highlighter,
            diagnostics,
            comments,
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
    diagnostics: &DiagnosticsStore,
    comments: &CommentIndex,
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
            diagnostics,
            comments_for_row(app, row, comments),
        ),
    }
}

/// The most severe diagnostic touching `row`'s current-file line, if any —
/// `None` for a `Del` row (no `new_line` to look up) or a row whose file
/// has no diagnostics loaded at all.
fn row_diagnostic_severity(
    app: &App,
    file: &DiffFile,
    row: &DiffRow,
    diagnostics: &DiagnosticsStore,
) -> Option<DiagnosticSeverity> {
    let (relative, line) = lsp_target(row, file)?;
    diagnostics.severity_at(&app.repo_root.join(relative), line)
}

/// The single-character (plus trailing space) gutter glyph for a
/// diagnostic severity: `●` red for an error, `▲` yellow for a warning,
/// `·` blue for anything less — a space when there's nothing to show, so
/// every content row's gutter is the same width whether or not it has a
/// diagnostic. `bg` matches the add/del tint the rest of that row's gutter
/// uses, so the glyph doesn't sit in a visibly different-colored cell.
fn diagnostic_glyph_span(severity: Option<DiagnosticSeverity>, bg: Option<Color>) -> Span<'static> {
    let (glyph, color) = match severity {
        Some(DiagnosticSeverity::ERROR) => ("\u{25CF}", Color::Red),
        Some(DiagnosticSeverity::WARNING) => ("\u{25B2}", Color::Yellow),
        Some(_) => ("\u{00B7}", Color::Blue),
        None => (" ", Color::Reset),
    };
    Span::styled(format!("{glyph} "), base_style(bg).fg(color))
}

/// The gutter's comment marker: a bright cyan `◆` when `annotations`
/// includes at least one open, still-anchored comment, a dim `◇` when it
/// only has resolved and/or detached ones, or a blank space when there's
/// nothing to show — matching [`diagnostic_glyph_span`]'s "always reserve
/// the width" convention so every content row's gutter stays the same size
/// whether or not it happens to carry a comment.
fn comment_marker_span(annotations: &[CommentAnnotation], bg: Option<Color>) -> Span<'static> {
    if annotations.is_empty() {
        return Span::styled(" ", base_style(bg));
    }
    let has_active_open = annotations
        .iter()
        .any(|a| !a.detached && a.status == CommentStatus::Open);
    if has_active_open {
        Span::styled("\u{25C6}", base_style(bg).fg(Color::Cyan))
    } else {
        Span::styled("\u{25C7}", base_style(bg).fg(Color::DarkGray))
    }
}

/// Renders the inline comment block for one row's `annotations`: one
/// dimmed/labeled header line per comment (`[open]`/`[resolved]`/
/// `[detached]` plus a short id prefix, so `ktmr comments resolve <id>`'s
/// argument is visible without leaving the TUI) followed by its
/// word-wrapped body, indented so it reads as subordinate to the diff line
/// above it rather than another row of the diff itself. A resolved
/// comment's body renders struck through and dimmed; an open one in plain
/// (but still slightly indented) text.
fn comment_block_lines(annotations: &[CommentAnnotation], width: usize) -> Vec<Line<'static>> {
    const INDENT: &str = "      ";
    let body_width = width.saturating_sub(display_width(INDENT));
    let mut out = Vec::new();

    for annotation in annotations {
        let label = if annotation.detached {
            "detached"
        } else if annotation.status == CommentStatus::Resolved {
            "resolved"
        } else {
            "open"
        };
        let dimmed = annotation.detached || annotation.status == CommentStatus::Resolved;
        let header_style = if dimmed {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::Cyan)
        };
        let id_prefix = &annotation.id[..annotation.id.len().min(8)];
        out.push(Line::from(Span::styled(
            format!("{INDENT}[{label} {id_prefix}]"),
            header_style,
        )));

        let mut body_style = Style::default().fg(Color::Gray);
        if annotation.status == CommentStatus::Resolved {
            body_style = body_style.add_modifier(Modifier::CROSSED_OUT);
        }
        if dimmed {
            body_style = body_style.fg(Color::DarkGray);
        }
        for wrapped in wrap_text(&annotation.body, body_width) {
            out.push(Line::from(Span::styled(
                format!("{INDENT}{wrapped}"),
                body_style,
            )));
        }
    }
    out
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

#[allow(clippy::too_many_arguments)] // every parameter is a distinct piece
// of what one content row needs to render (which row, whether it's under
// the cursor, how wide it can be, and the lookaside tables — highlighter,
// diagnostics, comment annotations, active-symbol state via `app` — it
// draws from); bundling them into a struct would just move the same fields
// one level down.
fn content_line(
    app: &App,
    file_idx: usize,
    hunk_idx: usize,
    row_idx: usize,
    width: usize,
    is_cursor: bool,
    highlighter: &mut LineHighlighter,
    diagnostics: &DiagnosticsStore,
    comments: &[CommentAnnotation],
) -> Line<'static> {
    let file = &app.files[file_idx];
    let row = &file.hunks[hunk_idx].rows[row_idx];

    let (marker, marker_color, bg) = match row.kind {
        DiffLineKind::Add => ('+', Color::Green, Some(Color::Rgb(0, 40, 0))),
        DiffLineKind::Del => ('-', Color::Red, Some(Color::Rgb(45, 0, 0))),
        DiffLineKind::Context => (' ', Color::Reset, None),
    };

    let severity = row_diagnostic_severity(app, file, row, diagnostics);
    let diagnostic_span = diagnostic_glyph_span(severity, bg);
    let comment_span = comment_marker_span(comments, bg);
    let gutter = format!(
        "{marker} {} {} \u{2502} ",
        format_line_number(row.old_line),
        format_line_number(row.new_line),
    );
    let gutter_width = display_width(&gutter)
        + display_width(diagnostic_span.content.as_ref())
        + display_width(comment_span.content.as_ref());
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

    let mut line_spans = vec![
        diagnostic_span,
        comment_span,
        Span::styled(
            gutter,
            base_style(bg).fg(marker_color).add_modifier(Modifier::BOLD),
        ),
    ];
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
