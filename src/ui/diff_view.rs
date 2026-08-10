//! Renders the diff pane: file/hunk headers and gutter-numbered, syntax
//! highlighted content lines. All layout math (gutter width, truncation,
//! soft-wrapping) goes through `ui::text`'s display-width helpers so CJK
//! content lines never misalign the gutter or split a character in half.
//!
//! A content line wider than its pane either soft-wraps onto continuation
//! rows (`[ui] wrap = true`, the default — see [`content_line`] and
//! [`unified_content_width`]) or truncates at the pane edge exactly as
//! every line did before wrapping existed (`wrap = false`). Continuation
//! rows stay logically part of the same [`crate::diff::RenderRow::Line`]
//! they wrapped from: `App::cursor`/`scroll_offset` are indexed by logical
//! row, never by visual row, and [`crate::ui::scroll`]'s functions account
//! for a row's variable visual height rather than assuming one row is
//! always one terminal line — see that module's docs.

use crate::comments::{CommentAnnotation, CommentIndex, Status as CommentStatus};
use crate::diff::{
    ColumnMap, DiffFile, DiffLineKind, DiffRow, RenderRow, SideBySideRow, SideCell, lsp_target,
    side_by_side_scroll_start,
};
use crate::highlight::{HighlightKind, Language, LineHighlighter, Span as HlSpan};
use crate::lsp::DiagnosticsStore;
use crate::ui::app::{App, Layout};
use crate::ui::pane::{self, Hint as PaneHint, PaneChrome};
// Only the tests below construct a `search::SearchHighlight`/call
// `search::compute_matches` directly — `content_line`'s own render path
// reaches `search::Match`/`SearchHighlight` only through `App::search`'s
// field type, never by naming the module itself.
#[cfg(test)]
use crate::ui::search;
use crate::ui::symbols;
use crate::ui::text::{
    display_width, expand_tabs_in_spans, highlight_color, mark_range, truncate_spans_to_width,
    truncate_to_width, wrap_spans_to_width, wrap_text,
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

/// The plain, non-focusable diff pane render every milestone before #14
/// used — kept exactly as it was (`Borders::LEFT`, no focus/hint chrome)
/// because [`crate::ui::timeline_view::TimelineView`] still calls this
/// directly for its own nested diff pane, which has no sibling pane to
/// focus away from and needs none of [`render_focusable`]'s
/// [`crate::ui::pane::PaneChrome`] machinery. The root `View::Diff` — which
/// *does* have a files pane beside it — uses [`render_focusable`] instead
/// (see `ui::mod::draw`'s `View::Diff` arm).
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
    render_content(
        frame,
        inner,
        app,
        highlighter,
        layout,
        diagnostics,
        comments,
    );
}

/// Issue #14: the root diff pane's render, through
/// [`crate::ui::pane::PaneChrome`] so it gets the same focused-border/
/// bottom-hint treatment every other focusable pane does (the files pane
/// beside it, the LSP inspector's three panes, the timeline's list/diff
/// split) — see [`render`]'s own docs for why [`TimelineView`]'s nested
/// diff pane deliberately keeps using the plain version instead.
///
/// [`TimelineView`]: crate::ui::timeline_view::TimelineView
#[allow(clippy::too_many_arguments)] // mirrors `render`'s own shape plus
// the two focus/hint parameters `PaneChrome` needs; splitting this into a
// struct would just move the same fields one level down.
pub fn render_focusable(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    highlighter: &mut LineHighlighter,
    layout: Layout,
    diagnostics: &DiagnosticsStore,
    comments: &CommentIndex,
    focused: bool,
    hints: &[PaneHint<'_>],
) {
    let block = PaneChrome::new(" diff ", area.width)
        .focused(focused)
        .hints(hints)
        .block();
    let inner = block.inner(area);
    frame.render_widget(block, area);
    render_content(
        frame,
        inner,
        app,
        highlighter,
        layout,
        diagnostics,
        comments,
    );
}

fn render_content(
    frame: &mut Frame,
    inner: Rect,
    app: &App,
    highlighter: &mut LineHighlighter,
    layout: Layout,
    diagnostics: &DiagnosticsStore,
    comments: &CommentIndex,
) {
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
    let wrap = crate::config::wrap_enabled();
    let mut lines: Vec<Line> = Vec::with_capacity(viewport_height);

    let mut idx = app.scroll_offset;
    while lines.len() < viewport_height && idx < app.rows.len() {
        let row = app.rows[idx];
        for line in render_row(
            app,
            row,
            idx,
            idx == app.cursor,
            content_width,
            highlighter,
            diagnostics,
            comments,
            wrap,
        ) {
            if lines.len() >= viewport_height {
                break;
            }
            lines.push(line);
        }
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
    let wrap = crate::config::wrap_enabled();
    let mut lines: Vec<Line> = Vec::with_capacity(viewport_height);

    let mut idx = start;
    while lines.len() < viewport_height && idx < app.side_by_side_rows.len() {
        let row = app.side_by_side_rows[idx];
        for line in side_by_side_row_line(
            app,
            row,
            left_width,
            right_width,
            highlighter,
            diagnostics,
            comments,
            wrap,
        ) {
            if lines.len() >= viewport_height {
                break;
            }
            lines.push(line);
        }
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

/// One [`SideBySideRow`]'s visual rows: a `Full` header spans both columns
/// exactly as [`render_row`] already draws it, unwrapped (headers never
/// wrap — see [`render_row`]'s docs); a `Paired` old/new row wraps each
/// side independently within its own width and pairs them back up
/// row-by-row, with whichever side wrapped to fewer visual rows padded out
/// with blank filler so the `│` divider stays a straight vertical line down
/// the pane regardless of which side (if either) actually grew — the pair's
/// visual height is `max(old's rows, new's rows)`.
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
    wrap: bool,
) -> Vec<Line<'static>> {
    match row {
        SideBySideRow::Full { flat_idx } => render_row(
            app,
            app.rows[flat_idx],
            flat_idx,
            flat_idx == app.cursor,
            left_width + 1 + right_width,
            highlighter,
            diagnostics,
            comments,
            wrap,
        ),
        SideBySideRow::Paired { old, new } => {
            let left_lines = side_cell_lines(
                app,
                old,
                left_width,
                highlighter,
                diagnostics,
                comments,
                wrap,
            );
            let right_lines = side_cell_lines(
                app,
                new,
                right_width,
                highlighter,
                diagnostics,
                comments,
                wrap,
            );
            let pair_height = left_lines.len().max(right_lines.len());
            let blank_left = Line::from(Span::raw(" ".repeat(left_width)));
            let blank_right = Line::from(Span::raw(" ".repeat(right_width)));
            (0..pair_height)
                .map(|i| {
                    let mut spans = left_lines.get(i).unwrap_or(&blank_left).spans.clone();
                    spans.push(Span::styled(
                        "\u{2502}",
                        Style::default().fg(Color::DarkGray),
                    ));
                    spans.extend(right_lines.get(i).unwrap_or(&blank_right).spans.clone());
                    Line::from(spans)
                })
                .collect()
        }
    }
}

/// One column's worth of visual rows for a [`SideCell`]: the same
/// rendering [`render_row`] already produces for that flat row when
/// populated (one row, or more if it wrapped), or a single blank filler
/// line at `width` when the other side has no counterpart here — pairing
/// this up against the other side's row count is [`side_by_side_row_line`]'s
/// job, not this function's.
#[allow(clippy::too_many_arguments)] // see `content_line`'s comment
fn side_cell_lines(
    app: &App,
    cell: SideCell,
    width: usize,
    highlighter: &mut LineHighlighter,
    diagnostics: &DiagnosticsStore,
    comments: &CommentIndex,
    wrap: bool,
) -> Vec<Line<'static>> {
    match cell {
        SideCell::Line { flat_idx } => render_row(
            app,
            app.rows[flat_idx],
            flat_idx,
            flat_idx == app.cursor,
            width,
            highlighter,
            diagnostics,
            comments,
            wrap,
        ),
        SideCell::Empty => vec![Line::from(Span::raw(" ".repeat(width)))],
    }
}

/// One [`RenderRow`]'s visual rows at `width` columns: always exactly one
/// for a file/hunk header or binary notice (none of those ever wrap — a
/// path or hunk range is short enough that truncating, as
/// [`file_header_line`]/[`hunk_header_line`] already do, reads better than
/// spending a second terminal row on it), or [`content_line`]'s wrapped
/// output for an actual diff line.
///
/// `flat_idx` is `row`'s own position in `app.rows` — not derivable from
/// `row` itself (a [`RenderRow`] carries no notion of its own flat
/// position), so every call site threads it through separately from its
/// own loop variable or the [`SideCell`]/[`SideBySideRow`] it already had
/// on hand. The only thing it's for is keying a search match back to the
/// row it belongs to (see [`content_line`]'s search-mark handling) — every
/// other piece of this function ignores it entirely.
#[allow(clippy::too_many_arguments)] // see `content_line`'s comment
fn render_row(
    app: &App,
    row: RenderRow,
    flat_idx: usize,
    is_cursor: bool,
    width: usize,
    highlighter: &mut LineHighlighter,
    diagnostics: &DiagnosticsStore,
    comments: &CommentIndex,
    wrap: bool,
) -> Vec<Line<'static>> {
    match row {
        RenderRow::FileHeader { file_idx } => {
            vec![file_header_line(app, file_idx, width, is_cursor)]
        }
        RenderRow::BinaryNotice { file_idx } => {
            vec![binary_notice_line(app, file_idx, width, is_cursor)]
        }
        RenderRow::HunkHeader { file_idx, hunk_idx } => {
            vec![hunk_header_line(app, file_idx, hunk_idx, width, is_cursor)]
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
            flat_idx,
            width,
            is_cursor,
            highlighter,
            diagnostics,
            comments_for_row(app, row, comments),
            wrap,
        ),
        RenderRow::Gap { file_idx, gap_idx } => {
            vec![gap_line(app, file_idx, gap_idx, width, is_cursor)]
        }
    }
}

/// A fold row: full-width like a hunk header (no gutter — there's no line
/// number to show, since it stands for a whole run of them), dim, always
/// exactly one visual row (never wraps or truncates mid-word — see
/// [`pad_or_truncate`]). `gap_idx` out of range (shouldn't happen for
/// anything [`crate::diff::flatten`] itself produced, but this is render
/// code, not a place to panic over a stale index) falls back to a plain
/// "unchanged" label rather than crashing the frame.
///
/// Reads `app.gap_cache` (recomputed once per [`App`] rederive, not per
/// frame) rather than calling [`crate::diff::file_gaps`] itself — this runs
/// once per visible fold row on *every* redraw (any keystroke, resize, LSP
/// push, watch tick), and re-walking a whole file's hunks just to read one
/// entry and discard the rest would repeat that work for as long as the row
/// stays on screen.
fn gap_line(
    app: &App,
    file_idx: usize,
    gap_idx: usize,
    width: usize,
    is_cursor: bool,
) -> Line<'static> {
    let text = match app
        .gap_cache
        .get(file_idx)
        .and_then(|gaps| gaps.get(gap_idx))
        .and_then(crate::diff::Gap::line_count)
    {
        Some(1) => "\u{00b7}\u{00b7}\u{00b7} 1 unchanged line \u{00b7}\u{00b7}\u{00b7}".to_owned(),
        Some(n) => format!("\u{00b7}\u{00b7}\u{00b7} {n} unchanged lines \u{00b7}\u{00b7}\u{00b7}"),
        None => "\u{00b7}\u{00b7}\u{00b7} unchanged lines \u{00b7}\u{00b7}\u{00b7}".to_owned(),
    };
    let style = cursor_style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
        is_cursor,
    );
    Line::from(Span::styled(pad_or_truncate(&text, width), style))
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
    let highlight_note = if file.skip_highlighting(crate::config::highlight_max_lines()) {
        "  \u{00b7} highlight off (large file)"
    } else {
        ""
    };
    let text = format!(
        "{}{status} (+{added} -{deleted}){highlight_note}",
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

/// The exact display-column width every content row's gutter (diagnostic
/// glyph + comment marker + add/del marker + old/new line numbers +
/// separator) occupies, regardless of that row's actual marker, line
/// numbers, diagnostic severity, or comment state — every piece of it
/// renders at a fixed width (see [`diagnostic_glyph_span`]'s and
/// [`comment_marker_span`]'s "always reserve the width" convention).
/// [`content_line`] derives its highlighted-text budget from this, and
/// [`unified_content_width`] reuses it so scroll math's wrap-height lookups
/// (`App::row_visual_height`) agree with what actually renders.
fn gutter_width() -> usize {
    const DIAG_WIDTH: usize = 2; // glyph + trailing space, see `diagnostic_glyph_span`
    const COMMENT_WIDTH: usize = 1; // see `comment_marker_span`
    // marker + space + old# + space + new# + space + │ + space
    1 + 1 + LINE_NUMBER_WIDTH + 1 + LINE_NUMBER_WIDTH + 1 + 1 + 1 + DIAG_WIDTH + COMMENT_WIDTH
}

/// The display-column budget available to a *unified*-layout content row's
/// highlighted text at a pane of `pane_width` columns: the root diff pane's
/// real border columns (via [`pane::inner_rect`] — [`render_focusable`]'s
/// [`PaneChrome`] draws a full box, two columns wide, not the single
/// left-only rule the plain [`render`] draws for `TimelineView`'s nested
/// pane) and [`gutter_width`] subtracted out. Routing the border width
/// through `pane::inner_rect` rather than a hand-counted literal is
/// deliberate — issue #14 had to fix exactly one such drift (the border
/// grew from 1 column to 2 when the root diff pane moved onto
/// `PaneChrome`, and this used to hardcode the old value) and the whole
/// point of `inner_rect` is that this can't happen again silently. Exposed
/// so `App::row_visual_height` derives the same wrap width
/// [`render_unified`]'s own [`content_line`] call uses, keeping
/// cursor-visibility and half-page scroll math in agreement with what's
/// actually on screen — see `App::content_width`'s docs for why
/// side-by-side, whose two columns are each narrower still, is a
/// deliberately separate concern from this, and this function's own only
/// caller (`ui::mod`'s frame preamble, `View::Diff`-only) for why
/// `TimelineView`'s plain-`render`ed nested pane never reaches this at all.
pub fn unified_content_width(pane_width: u16) -> usize {
    let probe = pane::inner_rect(pane_width, Rect::new(0, 0, pane_width, 1));
    (probe.width as usize).saturating_sub(gutter_width())
}

/// The gutter for a wrapped content row's second-and-later visual rows:
/// blank where the diagnostic glyph, comment marker, add/del marker, and
/// line numbers would be (so a continuation row is never mistaken for a
/// diagnostic-bearing, commented, or separately-numbered diff row of its
/// own), with a `↪` marking it as a continuation of the row above — the
/// same total width as [`gutter_width`] so the highlighted text after it
/// lines up in the same column regardless of which visual row of its
/// logical line it belongs to. `bg` matches the add/del tint the row's
/// first visual row uses, so a wrapped +/- line stays visibly tinted all
/// the way down.
fn continuation_gutter(bg: Option<Color>) -> Vec<Span<'static>> {
    let marker_field = " ".repeat(1 + 1 + LINE_NUMBER_WIDTH + 1 + LINE_NUMBER_WIDTH + 1);
    vec![
        Span::styled(" ".repeat(2), base_style(bg)), // diagnostic glyph's reserved width
        Span::styled(" ", base_style(bg)),           // comment marker's reserved width
        Span::styled(
            format!("{marker_field}\u{21aa} "),
            base_style(bg).fg(Color::DarkGray),
        ),
    ]
}

#[allow(clippy::too_many_arguments)] // every parameter is a distinct piece
// of what one content row needs to render (which row, its flat position,
// whether it's under the cursor, how wide it can be, and the lookaside
// tables — highlighter, diagnostics, comment annotations, active-symbol/
// active-search state via `app` — it draws from); bundling them into a
// struct would just move the same fields one level down.
fn content_line(
    app: &App,
    file_idx: usize,
    hunk_idx: usize,
    row_idx: usize,
    flat_idx: usize,
    width: usize,
    is_cursor: bool,
    highlighter: &mut LineHighlighter,
    diagnostics: &DiagnosticsStore,
    comments: &[CommentAnnotation],
    wrap: bool,
) -> Vec<Line<'static>> {
    let file = &app.files[file_idx];
    let row = &file.hunks[hunk_idx].rows[row_idx];

    let (marker, marker_color, bg) = match row.kind {
        DiffLineKind::Add => ('+', Color::Green, Some(Color::Rgb(0, 40, 0))),
        DiffLineKind::Del => ('-', Color::Red, Some(Color::Rgb(45, 0, 0))),
        DiffLineKind::Context => (' ', Color::Reset, None),
    };

    let severity = row_diagnostic_severity(app, file, row, diagnostics);
    let gutter = format!(
        "{marker} {} {} \u{2502} ",
        format_line_number(row.old_line),
        format_line_number(row.new_line),
    );
    let content_width = width.saturating_sub(gutter_width());

    let spans = if file.skip_highlighting(crate::config::highlight_max_lines()) {
        // Large/lockfile-ish files skip tree-sitter entirely (see
        // `DiffFile::skip_highlighting`'s docs) — a single unhighlighted
        // span rendered in the theme's plain color, exactly like
        // `LineHighlighter`'s own fallback for a parse failure. The file
        // header (see `file_header_line`) carries the "highlight off"
        // status note; nothing about a single content row needs to repeat
        // it.
        vec![HlSpan {
            text: row.text.clone(),
            kind: HighlightKind::Plain,
        }]
    } else {
        let language = Language::detect(file.display_path());
        highlighter.highlight_line(language, &row.text)
    };
    let spans = expand_tabs_in_spans(spans, crate::config::tab_width());
    let visual_rows: Vec<Vec<HlSpan>> = if wrap {
        wrap_spans_to_width(&spans, content_width)
    } else {
        vec![truncate_spans_to_width(&spans, content_width)]
    };

    // Resolved once per logical line, in the line's own display-column
    // space — not per visual row — then clipped down below to whichever
    // visual row(s) it actually falls within, since a long enough active
    // symbol can itself straddle a wrap point.
    let active_symbol = is_cursor
        .then(|| symbols::scan(&row.text).get(app.active_symbol).copied())
        .flatten();

    // Issue #5's active search matches on this row, converted from
    // `search::Match`'s byte offsets into display columns via `ColumnMap` —
    // the byte→display leg `search::Match`'s own docs describe (mirroring
    // `location_to_target`'s LSP-position conversion, not `active_symbol`
    // above, which computes display columns directly since `symbols::scan`
    // is tab-aware itself and never dealt in bytes to begin with). Resolved
    // once per logical line, same reasoning as `active_symbol`: clipped
    // down to each visual row inside the loop below, since a match can
    // itself straddle a wrap point. The `ColumnMap` is only ever built when
    // this row actually has a match — every other row (the overwhelming
    // majority, even with a search active) skips it entirely. Nothing is
    // drawn at all when `highlight_visible` is off (a bare `Esc`'s `:noh`
    // — see `App::clear_search`'s docs) even though `matches`/`current`
    // are still sitting right there, ready for `n`/`N` to jump through
    // without a mark ever appearing on screen.
    let search_marks: Vec<(usize, usize, bool)> = match &app.search {
        Some(highlight) if highlight.highlight_visible => {
            // `matches` is guaranteed sorted ascending by `row_idx` (file →
            // hunk → line order, with no per-row re-sort needed — see
            // `search::compute_matches`'s own doc comment), so this row's
            // run is a single contiguous slice found by binary search
            // rather than a linear scan over every match in the confirmed
            // search. That matters here specifically because this runs
            // once per *visible* row on every redraw, and `ui::mod::run`'s
            // event loop redraws on a ~100ms idle tick even with no input
            // at all — a linear scan would re-cost O(total matches) per row
            // indefinitely for as long as a many-match search stays
            // confirmed, not just while the reviewer is actively typing.
            let start = highlight.matches.partition_point(|m| m.row_idx < flat_idx);
            let end = start + highlight.matches[start..].partition_point(|m| m.row_idx == flat_idx);
            if start == end {
                Vec::new()
            } else {
                let columns = ColumnMap::new(&row.text);
                highlight.matches[start..end]
                    .iter()
                    .enumerate()
                    .map(|(offset, m)| {
                        (
                            columns.utf8_to_display(m.start),
                            columns.utf8_to_display(m.end),
                            start + offset == highlight.current,
                        )
                    })
                    .collect()
            }
        }
        _ => Vec::new(),
    };

    // Issue #16: constant for every visual row of this logical line (it
    // depends only on `flat_idx`, never on wrap position), so resolved once
    // here rather than inside the loop below — the same "per logical line,
    // not per visual row" shape `active_symbol`/`search_marks` above already
    // use, and for the same reason: whichever wrap row(s) this line ends up
    // producing, all of them belong to the same selected-or-not source line.
    let selected = app.is_row_selected(flat_idx);

    let mut out = Vec::with_capacity(visual_rows.len());
    let mut col_offset = 0usize;
    for (i, row_spans) in visual_rows.into_iter().enumerate() {
        let row_width: usize = row_spans.iter().map(|s| display_width(&s.text)).sum();
        let mut content_spans: Vec<Span<'static>> = row_spans
            .into_iter()
            .map(|span| Span::styled(span.text, base_style(bg).fg(highlight_color(span.kind))))
            .collect();
        // Applied before the active-symbol/search marks below (see
        // `visual_selection_style`'s doc for the full precedence order) so
        // both later marks still patch cleanly over it rather than being
        // hidden underneath — every visual row of a selected wrapped line
        // gets the full-width background (req 7); on side-by-side a
        // selected `Del`/`Add` cell highlights only because it's *this*
        // `flat_idx`'s own cell that resolved `selected`, never its
        // unselected pair (req 8) — see `is_row_selected`'s docs.
        if selected {
            content_spans = mark_range(content_spans, 0, row_width, visual_selection_style());
        }
        if let Some(active) = active_symbol {
            let (start, end) = (active.display_start, active.display_end);
            if end > col_offset && start < col_offset + row_width {
                let local_start = start.saturating_sub(col_offset);
                let local_end = (end - col_offset).min(row_width);
                content_spans = mark_range(
                    content_spans,
                    local_start,
                    local_end,
                    Style::default().add_modifier(Modifier::UNDERLINED),
                );
            }
        }
        // Applied after the active-symbol mark, so a match that happens to
        // land on the selected symbol gets both styles patched together
        // (see `text::mark_range`'s docs — later calls only ever *add* to
        // whatever style a span already carries) rather than one silently
        // overwriting the other.
        for &(start, end, is_current) in &search_marks {
            if end > col_offset && start < col_offset + row_width {
                let local_start = start.saturating_sub(col_offset);
                let local_end = (end - col_offset).min(row_width);
                let style = if is_current {
                    current_match_style()
                } else {
                    other_match_style()
                };
                content_spans = mark_range(content_spans, local_start, local_end, style);
            }
        }

        let mut line_spans = if i == 0 {
            vec![
                diagnostic_glyph_span(severity, bg),
                comment_marker_span(comments, bg),
                Span::styled(
                    gutter.clone(),
                    base_style(bg).fg(marker_color).add_modifier(Modifier::BOLD),
                ),
            ]
        } else {
            continuation_gutter(bg)
        };
        line_spans.extend(content_spans);

        let rendered_width: usize = line_spans.iter().map(ratatui::text::Span::width).sum();
        if rendered_width < width {
            // The trailing fill carries the selection background too, or a
            // selected row's highlight would cut off at its last character
            // — and a selected *blank* line (whose only visual row is all
            // padding, `row_width == 0`, so the `mark_range` above had
            // nothing to mark) would show no selection at all, a visible
            // hole in the middle of a contiguous range (req 7). The cursor
            // still wins: its whole-`Line` `REVERSED` patch below lands on
            // this span like any other.
            let pad_style = if selected {
                visual_selection_style()
            } else {
                base_style(bg)
            };
            line_spans.push(Span::styled(" ".repeat(width - rendered_width), pad_style));
        }

        let mut line = Line::from(line_spans);
        if is_cursor {
            line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
        }
        out.push(line);
        col_offset += row_width;
    }
    out
}

fn base_style(bg: Option<Color>) -> Style {
    match bg {
        Some(color) => Style::default().bg(color),
        None => Style::default(),
    }
}

/// Issue #5's non-current match style: a dim, saturated highlight visible
/// against any of `content_line`'s add/del/context backgrounds. Never
/// `Modifier::UNDERLINED` — that's the active-symbol highlight's own
/// modifier (see `content_line`'s `active_symbol` handling) — reusing it
/// here would make a search match sitting on the active symbol visually
/// indistinguishable from the symbol selection itself.
fn other_match_style() -> Style {
    Style::default()
        .bg(Color::Rgb(90, 74, 0))
        .add_modifier(Modifier::DIM)
}

/// Issue #16's visual-line selection background — a cool, low-saturation
/// indigo chosen to sit apart from every other tint `content_line` can
/// paint on the same row (the warm add/del greens/reds, the yellow search
/// highlight, the diagnostic gutter glyphs) without fighting any of them for
/// attention. The full deterministic precedence a selected row's style is
/// built up out of, applied in this order (each later step patches over,
/// rather than replaces, whatever the earlier ones left — see
/// `text::mark_range`'s docs):
///
/// 1. add/del background (baked into `content_spans` at span-build time,
///    before any mark is ever applied — a `Context` row has none).
/// 2. syntax highlight foreground (also baked in at span-build time).
/// 3. this visual-selection background — applied first among the marks, so
///    every later one still shows through it.
/// 4. the active-symbol underline (additive; never a background, so it
///    always stays visible over this).
/// 5. a search match's background — `current_match_style`/`other_match_style`
///    — but *only* across its own matched range, not the row: outside that
///    range this selection's background is the last thing painted, so a
///    selected-but-unmatched row still reads as selected.
/// 6. the gutter — diagnostic glyph, comment marker, line-number field —
///    built as separate prepended spans (see `content_line`'s
///    `line_spans` assembly) that no mark ever touches, selection included:
///    a selected row's gutter looks exactly like an unselected one's.
/// 7. the cursor's reverse-video, applied last, to the whole rendered line
///    (`Line::patch_style` with `Modifier::REVERSED`) — always wins, since
///    the cursor's own row must stay unambiguous even mid-selection.
fn visual_selection_style() -> Style {
    Style::default().bg(Color::Rgb(25, 25, 70))
}

/// Issue #5's current-match style: bold plus an explicit, saturated
/// background. The current match is *always* on the cursor's own row (see
/// `App::jump_cursor_to`, which every search jump funnels through), and
/// that whole row already gets `Modifier::REVERSED` patched on
/// unconditionally at the bottom of this function — a style that only set a
/// modifier with no color of its own would be invisible once reversed.
/// Setting an explicit background here means the row still shows something
/// clearly distinct at the match either way: `Modifier::REVERSED` swaps
/// whatever's resolved for fg/bg at render time, so a deliberately
/// saturated background reads as a deliberately saturated foreground once
/// reversed, rather than blending into the rest of the reversed row the way
/// an unstyled span would.
fn current_match_style() -> Style {
    Style::default()
        .bg(Color::Yellow)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::CommentIndex;
    use crate::diff::{DiffFile, DiffHunk, DiffLineKind, DiffRow, SideBySideRow, SideCell};
    use crate::lsp::DiagnosticsStore;
    use std::path::PathBuf;

    fn row(kind: DiffLineKind, text: &str, old: Option<u32>, new: Option<u32>) -> DiffRow {
        DiffRow {
            kind,
            text: text.to_owned(),
            old_line: old,
            new_line: new,
        }
    }

    fn app_with_rows(rows: Vec<DiffRow>) -> App {
        let file = DiffFile {
            old_path: Some("f.rs".to_owned()),
            new_path: Some("f.rs".to_owned()),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: rows.len() as u32,
                new_start: 1,
                new_lines: rows.len() as u32,
                header: String::new(),
                // These render tests aren't about fold rows — no trailing
                // gap, so `app.rows` is exactly the rows this helper built.
                known_eof: true,
                rows,
            }],
            ..Default::default()
        };
        App::new("repo".to_owned(), PathBuf::from("/repo"), vec![file])
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// As [`app_with_rows`], but for a single-row file at a `.lock`
    /// filename — [`DiffFile::skip_highlighting`] always returns `true` for
    /// one (see `is_lockfile_ish`), which forces `content_line`'s plain,
    /// single-`HlSpan` fallback rather than running the real (and, for
    /// arbitrary test prose that isn't valid Rust, unpredictable)
    /// tree-sitter highlighter over it. The search-mark tests below need
    /// that determinism: they assert on an exact span's content and style
    /// after [`mark_range`] slices it out, which only holds together
    /// reliably against a known-single-span starting point.
    fn app_with_plain_row(text: &str) -> App {
        let file = DiffFile {
            old_path: Some("Cargo.lock".to_owned()),
            new_path: Some("Cargo.lock".to_owned()),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                header: String::new(),
                known_eof: true,
                rows: vec![row(DiffLineKind::Context, text, Some(1), Some(1))],
            }],
            ..Default::default()
        };
        App::new("repo".to_owned(), PathBuf::from("/repo"), vec![file])
    }

    #[test]
    fn unified_content_width_subtracts_the_border_and_the_gutter() {
        // Issue #14: the root diff pane's `PaneChrome` box is 2 columns
        // wide (left + right border), not the 1-column left-only rule the
        // pre-#14 plain `render` drew — this is the exact arithmetic that
        // changed, pinned down directly rather than only through
        // `pane::inner_rect`'s own tests.
        assert_eq!(unified_content_width(100), 100 - 2 - gutter_width());
    }

    #[test]
    fn unified_content_width_never_hand_counts_the_border_a_second_way() {
        // `unified_content_width` must always agree with
        // `pane::inner_rect`, whatever `PaneChrome`'s border width happens
        // to be — the drift issue #14 fixed by routing through the shared
        // helper instead of a literal. This would still pass even if
        // `PaneChrome`'s borders changed again later, unlike a test that
        // hardcodes an expected number.
        for pane_width in [0u16, 1, 2, 3, 30, 68, 70, 100, 250] {
            let probe = pane::inner_rect(pane_width, Rect::new(0, 0, pane_width, 1));
            let expected = (probe.width as usize).saturating_sub(gutter_width());
            assert_eq!(unified_content_width(pane_width), expected);
        }
    }

    #[test]
    fn unified_content_width_saturates_rather_than_underflowing_on_a_tiny_pane() {
        assert_eq!(unified_content_width(3), 0);
    }

    #[test]
    fn render_focusable_draws_a_focused_cyan_border() {
        use ratatui::backend::TestBackend;

        let app = app_with_plain_row("fn main() {}");
        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let comments = CommentIndex::default();
        let backend = TestBackend::new(30, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_focusable(
                    frame,
                    frame.area(),
                    &app,
                    &mut highlighter,
                    Layout::Unified,
                    &diagnostics,
                    &comments,
                    true,
                    &[],
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let corner = buffer.cell((0, 0)).expect("top-left corner");
        assert_eq!(corner.symbol(), "\u{250c}"); // ┌ — a real box, not `render`'s Borders::LEFT rule
        assert_eq!(corner.fg, Color::Cyan);
        assert!(corner.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn render_focusable_leaves_the_border_unstyled_when_not_focused() {
        use ratatui::backend::TestBackend;

        let app = app_with_plain_row("fn main() {}");
        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let comments = CommentIndex::default();
        let backend = TestBackend::new(30, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_focusable(
                    frame,
                    frame.area(),
                    &app,
                    &mut highlighter,
                    Layout::Unified,
                    &diagnostics,
                    &comments,
                    false,
                    &[],
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let corner = buffer.cell((0, 0)).expect("top-left corner");
        assert_ne!(corner.fg, Color::Cyan);
    }

    #[test]
    fn content_line_with_wrap_off_truncates_exactly_as_before_this_milestone() {
        let app = app_with_rows(vec![row(
            DiffLineKind::Context,
            &"x".repeat(50),
            Some(1),
            Some(1),
        )]);
        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let lines = content_line(
            &app,
            0,
            0,
            0,
            2, // flat_idx — see `app_with_rows`'s docs: 0 FileHeader, 1 HunkHeader, 2 this row
            30,
            false,
            &mut highlighter,
            &diagnostics,
            &[],
            false, // wrap = false
        );
        assert_eq!(
            lines.len(),
            1,
            "wrap off must never produce a continuation row"
        );
    }

    #[test]
    fn content_line_wraps_a_long_line_into_multiple_rows_preserving_every_character() {
        let app = app_with_rows(vec![row(
            DiffLineKind::Context,
            &"x".repeat(50),
            Some(1),
            Some(1),
        )]);
        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let lines = content_line(
            &app,
            0,
            0,
            0,
            2, // flat_idx — see the previous test's comment
            30,
            false,
            &mut highlighter,
            &diagnostics,
            &[],
            true,
        );
        assert!(
            lines.len() > 1,
            "50 columns of content at a much narrower pane must wrap onto continuation rows"
        );
        // Every character makes it onto some visual row — none dropped.
        let total_x: usize = lines
            .iter()
            .map(|l| line_text(l).chars().filter(|&c| c == 'x').count())
            .sum();
        assert_eq!(total_x, 50);
        // Every row after the first carries the continuation marker; the
        // first does not (it has the real gutter instead).
        assert!(!line_text(&lines[0]).contains('\u{21aa}'));
        for continuation in &lines[1..] {
            assert!(line_text(continuation).contains('\u{21aa}'));
        }
        // Every visual row is padded to exactly the requested pane width.
        for line in &lines {
            assert_eq!(display_width(&line_text(line)), 30);
        }
    }

    #[test]
    fn content_line_under_cursor_reverses_every_one_of_its_wrapped_visual_rows() {
        let app = app_with_rows(vec![row(
            DiffLineKind::Context,
            &"x".repeat(50),
            Some(1),
            Some(1),
        )]);
        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let lines = content_line(
            &app,
            0,
            0,
            0,
            2, // flat_idx — see the first `content_line` test's comment
            30,
            true, // is_cursor
            &mut highlighter,
            &diagnostics,
            &[],
            true,
        );
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(
                line.style.add_modifier.contains(Modifier::REVERSED),
                "a selected logical row highlights every one of its visual rows"
            );
        }
    }

    #[test]
    fn side_by_side_paired_row_visual_height_is_the_max_of_both_sides() {
        // The old side is a short, unwrapped line; the new side is long
        // enough to wrap at a narrow column width — the pair's visual
        // height must be driven by whichever side is taller.
        let app = app_with_rows(vec![
            row(DiffLineKind::Del, "short", Some(1), None),
            row(DiffLineKind::Add, &"y".repeat(40), None, Some(1)),
        ]);
        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let comments = CommentIndex::default();

        // app.rows: 0 = FileHeader, 1 = HunkHeader, 2 = the Del row,
        // 3 = the Add row.
        let pair = SideBySideRow::Paired {
            old: SideCell::Line { flat_idx: 2 },
            new: SideCell::Line { flat_idx: 3 },
        };
        // Both comfortably wider than `gutter_width()` — each side's cell
        // still renders a full `content_line` gutter (old *and* new line
        // number fields) even in side-by-side, so a column narrower than
        // that has no content budget left at all, which isn't a
        // configuration `MIN_SIDE_BY_SIDE_WIDTH` ever actually allows.
        let (left_width, right_width) = (30, 25);
        let lines = side_by_side_row_line(
            &app,
            pair,
            left_width,
            right_width,
            &mut highlighter,
            &diagnostics,
            &comments,
            true,
        );

        assert!(
            lines.len() > 1,
            "the wrapped new-side cell must make the whole pair more than one visual row tall"
        );

        let mut total_y = 0usize;
        for (i, line) in lines.iter().enumerate() {
            let divider_idx = line
                .spans
                .iter()
                .position(|s| s.content.as_ref() == "\u{2502}")
                .expect("every pair row carries the old/new divider");
            let left_text: String = line.spans[..divider_idx]
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            let right_text: String = line.spans[divider_idx + 1..]
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            // Both columns stay exactly their requested width on every
            // visual row, so the divider is a straight vertical line down
            // the pane regardless of which side (if either) wrapped.
            assert_eq!(display_width(&left_text), left_width);
            assert_eq!(display_width(&right_text), right_width);
            if i > 0 {
                assert!(
                    left_text.trim().is_empty(),
                    "the old side's single row is exhausted, so later rows are blank filler"
                );
            }
            total_y += right_text.chars().filter(|&c| c == 'y').count();
        }
        assert_eq!(
            total_y, 40,
            "every character of the wrapped new side survives"
        );
    }

    #[test]
    fn side_by_side_full_row_never_wraps_a_file_header() {
        let app = app_with_rows(vec![row(
            DiffLineKind::Context,
            "unrelated",
            Some(1),
            Some(1),
        )]);
        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let comments = CommentIndex::default();

        let lines = side_by_side_row_line(
            &app,
            SideBySideRow::Full { flat_idx: 0 }, // the file header row
            20,
            20,
            &mut highlighter,
            &diagnostics,
            &comments,
            true,
        );
        assert_eq!(lines.len(), 1);
    }

    // -- Gap row rendering --------------------------------------------

    /// One file, two hunks (new 1..=3, new `second_hunk_start`..=`+2`) — the
    /// same shape `ui::app`'s own fold fixture uses. Parameterized over the
    /// second hunk's start so a single helper covers both the multi-line
    /// between-hunks gap (`app_with_a_gap(10)`, the same fixture `ui::app`
    /// uses) and a one-line gap (`app_with_a_gap(5)`) without two near-
    /// identical `mk_hunk` closures in this module.
    fn app_with_a_gap(second_hunk_start: u32) -> App {
        let mk_hunk = |start: u32, lines: u32| DiffHunk {
            old_start: start,
            old_lines: lines,
            new_start: start,
            new_lines: lines,
            header: String::new(),
            known_eof: false,
            rows: (0..lines)
                .map(|i| row(DiffLineKind::Context, "x", Some(start + i), Some(start + i)))
                .collect(),
        };
        let file = DiffFile {
            old_path: Some("f.txt".to_owned()),
            new_path: Some("f.txt".to_owned()),
            hunks: vec![mk_hunk(1, 3), mk_hunk(second_hunk_start, 3)],
            ..Default::default()
        };
        App::new("repo".to_owned(), PathBuf::from("/repo"), vec![file])
    }

    #[test]
    fn gap_line_shows_the_known_line_count_for_a_between_hunks_gap() {
        let app = app_with_a_gap(10);
        let line = gap_line(&app, 0, 0, 40, false);
        assert_eq!(
            line_text(&line).trim(),
            "\u{00b7}\u{00b7}\u{00b7} 6 unchanged lines \u{00b7}\u{00b7}\u{00b7}"
        );
    }

    #[test]
    fn gap_line_uses_the_singular_for_exactly_one_hidden_line() {
        // A one-line gap: hunk 0 ends at new 4 (exclusive), hunk 1 starts
        // at new 5.
        let app = app_with_a_gap(5);
        let line = gap_line(&app, 0, 0, 40, false);
        assert_eq!(
            line_text(&line).trim(),
            "\u{00b7}\u{00b7}\u{00b7} 1 unchanged line \u{00b7}\u{00b7}\u{00b7}"
        );
    }

    #[test]
    fn gap_line_omits_a_count_for_an_unbounded_trailing_gap() {
        let app = app_with_a_gap(10);
        // gap_idx 1 is the trailing gap — file_gaps orders Between before
        // Trailing, see `file_gaps`'s docs.
        let line = gap_line(&app, 0, 1, 40, false);
        assert_eq!(
            line_text(&line).trim(),
            "\u{00b7}\u{00b7}\u{00b7} unchanged lines \u{00b7}\u{00b7}\u{00b7}"
        );
    }

    #[test]
    fn gap_line_pads_to_the_exact_requested_width() {
        let app = app_with_a_gap(10);
        let line = gap_line(&app, 0, 0, 50, false);
        assert_eq!(display_width(&line_text(&line)), 50);
    }

    #[test]
    fn render_row_never_produces_more_than_one_visual_row_for_a_gap() {
        let app = app_with_a_gap(10);
        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let comments = CommentIndex::default();
        let lines = render_row(
            &app,
            RenderRow::Gap {
                file_idx: 0,
                gap_idx: 0,
            },
            5, // flat_idx — irrelevant here, a Gap row never reads it
            false,
            10, // deliberately narrow — a gap row must truncate, never wrap
            &mut highlighter,
            &diagnostics,
            &comments,
            true,
        );
        assert_eq!(lines.len(), 1);
    }

    // -- Issue #5: search-match rendering -------------------------------

    #[test]
    fn content_line_marks_two_matches_with_the_current_one_styled_differently() {
        let mut app = app_with_plain_row("alpha needle beta needle gamma");
        let matches = search::compute_matches(&app.files, &app.rows, "needle");
        assert_eq!(
            matches.len(),
            2,
            "sanity: exactly two occurrences of \"needle\""
        );
        app.search = Some(search::SearchHighlight {
            query: "needle".to_owned(),
            matches,
            current: 1, // the *second* occurrence is current
            highlight_visible: true,
        });

        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let lines = content_line(
            &app,
            0,
            0,
            0,
            2,   // flat_idx — see `app_with_rows`'s docs: 0 FileHeader, 1 HunkHeader, 2 this row
            200, // wide enough that this line never wraps
            false,
            &mut highlighter,
            &diagnostics,
            &[],
            true,
        );
        assert_eq!(lines.len(), 1);

        let is_current = |s: &Span| {
            s.style.add_modifier.contains(Modifier::BOLD) && s.style.bg == Some(Color::Yellow)
        };
        let is_other = |s: &Span| {
            s.style.add_modifier.contains(Modifier::DIM) && s.style.bg != Some(Color::Yellow)
        };
        let current_spans: Vec<&str> = lines[0]
            .spans
            .iter()
            .filter(|s| s.content.as_ref() == "needle" && is_current(s))
            .map(|s| s.content.as_ref())
            .collect();
        let other_spans: Vec<&str> = lines[0]
            .spans
            .iter()
            .filter(|s| s.content.as_ref() == "needle" && is_other(s))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(
            current_spans.len(),
            1,
            "exactly the current match gets the bold/yellow style; spans: {:?}",
            lines[0].spans
        );
        assert_eq!(
            other_spans.len(),
            1,
            "exactly the other match gets the dim style; spans: {:?}",
            lines[0].spans
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|s| !s.style.add_modifier.contains(Modifier::UNDERLINED)),
            "search marks must never collide with the active-symbol's UNDERLINED modifier"
        );
    }

    /// `:noh` (`App::clear_search`) sets `highlight_visible: false` while
    /// leaving `matches`/`current` untouched (see that field's docs) — so
    /// nothing should be drawn on screen at all despite the highlight still
    /// carrying two real matches, until `n`/`N` re-enables it.
    #[test]
    fn content_line_draws_no_marks_when_the_highlight_is_suppressed() {
        let mut app = app_with_plain_row("alpha needle beta needle gamma");
        let matches = search::compute_matches(&app.files, &app.rows, "needle");
        assert_eq!(
            matches.len(),
            2,
            "sanity: exactly two occurrences of \"needle\""
        );
        app.search = Some(search::SearchHighlight {
            query: "needle".to_owned(),
            matches,
            current: 1,
            highlight_visible: false,
        });

        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let lines = content_line(
            &app,
            0,
            0,
            0,
            2,
            200,
            false,
            &mut highlighter,
            &diagnostics,
            &[],
            true,
        );
        assert_eq!(lines.len(), 1);

        let is_current = |s: &Span| {
            s.style.add_modifier.contains(Modifier::BOLD) && s.style.bg == Some(Color::Yellow)
        };
        let is_other = |s: &Span| {
            s.style.add_modifier.contains(Modifier::DIM) && s.style.bg != Some(Color::Yellow)
        };
        let marked: Vec<&str> = lines[0]
            .spans
            .iter()
            .filter(|s| s.content.as_ref() == "needle" && (is_current(s) || is_other(s)))
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            marked.is_empty(),
            "a suppressed highlight must draw no search marks at all; spans: {:?}",
            lines[0].spans
        );
    }

    #[test]
    fn content_line_search_mark_survives_a_wrap_boundary() {
        // "needle" (6 columns) starts at column 20 — straddling a wrap
        // point set at content-column 22, so its first two columns land on
        // the first visual row and the remaining four on the second.
        let text = format!("{}needle{}", "x".repeat(20), "y".repeat(20));
        let mut app = app_with_plain_row(&text);
        let matches = search::compute_matches(&app.files, &app.rows, "needle");
        assert_eq!(matches.len(), 1);
        app.search = Some(search::SearchHighlight {
            query: "needle".to_owned(),
            matches,
            current: 0,
            highlight_visible: true,
        });

        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let content_width = 22;
        let lines = content_line(
            &app,
            0,
            0,
            0,
            2, // flat_idx
            content_width + gutter_width(),
            false,
            &mut highlighter,
            &diagnostics,
            &[],
            true,
        );
        assert!(
            lines.len() >= 2,
            "the match itself straddles the wrap point, so this needs at least two visual rows"
        );

        let carries_current_mark = |line: &Line| {
            line.spans.iter().any(|s| {
                s.style.add_modifier.contains(Modifier::BOLD) && s.style.bg == Some(Color::Yellow)
            })
        };
        assert!(
            carries_current_mark(&lines[0]),
            "the first visual row must carry the match's leading half; spans: {:?}",
            lines[0].spans
        );
        assert!(
            carries_current_mark(&lines[1]),
            "the second visual row must carry the match's trailing half; spans: {:?}",
            lines[1].spans
        );
    }

    #[test]
    fn side_by_side_row_line_marks_a_search_match_on_the_new_side_only() {
        let mut app = app_with_plain_row("alpha needle beta");
        let matches = search::compute_matches(&app.files, &app.rows, "needle");
        assert_eq!(matches.len(), 1);
        app.search = Some(search::SearchHighlight {
            query: "needle".to_owned(),
            matches,
            current: 0,
            highlight_visible: true,
        });

        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let comments = CommentIndex::default();

        // app.rows: 0 FileHeader, 1 HunkHeader, 2 the one content row —
        // placed on the *new* (right) side of a paired row, old side empty,
        // the same shape a pure addition renders as in side-by-side.
        let pair = SideBySideRow::Paired {
            old: SideCell::Empty,
            new: SideCell::Line { flat_idx: 2 },
        };
        // Wide enough on the new side that "alpha needle beta" never wraps
        // (`gutter_width()` alone eats a fixed chunk of whatever width is
        // requested — see that function's docs).
        let lines = side_by_side_row_line(
            &app,
            pair,
            20,
            gutter_width() + 40,
            &mut highlighter,
            &diagnostics,
            &comments,
            true,
        );
        assert_eq!(lines.len(), 1);

        let divider_idx = lines[0]
            .spans
            .iter()
            .position(|s| s.content.as_ref() == "\u{2502}")
            .expect("every paired row carries the old/new divider");
        let is_marked = |s: &Span| s.style.bg == Some(Color::Yellow);
        assert!(
            lines[0].spans[divider_idx + 1..].iter().any(is_marked),
            "the match must be marked on the new (right) side"
        );
        assert!(
            !lines[0].spans[..divider_idx].iter().any(is_marked),
            "the empty old side has nothing of its own to mark"
        );
    }

    // -- Issue #16: visual-line selection rendering ---------------------

    fn is_visual_selected(s: &Span) -> bool {
        s.style.bg == Some(Color::Rgb(25, 25, 70))
    }

    #[test]
    fn content_line_marks_every_visual_row_of_a_selected_wrapped_line() {
        let app_rows = vec![row(
            DiffLineKind::Context,
            &"x".repeat(50),
            Some(1),
            Some(1),
        )];
        let mut app = app_with_rows(app_rows);
        app.cursor = 2; // the one content row — see `app_with_rows`'s flat-idx convention
        app.toggle_visual();
        assert!(
            app.visual_active(),
            "sanity: V on a Line row must start a selection"
        );

        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let lines = content_line(
            &app,
            0,
            0,
            0,
            2,
            30,
            false,
            &mut highlighter,
            &diagnostics,
            &[],
            true,
        );
        assert!(lines.len() > 1, "sanity: this line must wrap at width 30");
        for (i, line) in lines.iter().enumerate() {
            assert!(
                line.spans.iter().any(is_visual_selected),
                "visual row {i} of a selected wrapped line must carry the selection \
                 background; spans: {:?}",
                line.spans
            );
        }
    }

    #[test]
    fn content_line_paints_a_selected_blank_line_and_the_trailing_padding() {
        let app_rows = vec![
            row(DiffLineKind::Context, "fn a() {}", Some(1), Some(1)),
            row(DiffLineKind::Context, "", Some(2), Some(2)),
        ];
        let mut app = app_with_rows(app_rows);
        app.cursor = 2; // first content row
        app.toggle_visual();
        app.cursor = 3; // extend across the blank row

        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();

        // The blank row's only visual row is entirely trailing padding
        // (`row_width == 0`, so `mark_range` has nothing to mark) — the
        // padding itself must carry the selection background, or a
        // contiguous selection shows a hole in the middle (req 7).
        let blank = content_line(
            &app,
            0,
            0,
            1,
            3,
            30,
            false,
            &mut highlighter,
            &diagnostics,
            &[],
            true,
        );
        assert_eq!(blank.len(), 1, "an empty line renders one visual row");
        assert!(
            blank[0].spans.iter().any(is_visual_selected),
            "a selected blank line must still show the selection; spans: {:?}",
            blank[0].spans
        );

        // A short selected row: the fill past its last character continues
        // the selection bar to the pane edge instead of cutting it off.
        let short = content_line(
            &app,
            0,
            0,
            0,
            2,
            30,
            false,
            &mut highlighter,
            &diagnostics,
            &[],
            true,
        );
        let pad = short[0].spans.last().expect("trailing pad span exists");
        assert!(
            is_visual_selected(pad),
            "trailing padding must extend the selection bar; spans: {:?}",
            short[0].spans
        );
    }

    /// Precedence test (see `visual_selection_style`'s doc): a search match
    /// on a selected row still shows its own style exactly where it
    /// matches, while the rest of the selected row keeps the selection
    /// background — the "partial override" property.
    #[test]
    fn content_line_selection_yields_to_a_search_match_only_inside_the_match() {
        let mut app = app_with_plain_row("alpha needle beta");
        let matches = search::compute_matches(&app.files, &app.rows, "needle");
        assert_eq!(matches.len(), 1);
        app.search = Some(search::SearchHighlight {
            query: "needle".to_owned(),
            matches,
            current: 0,
            highlight_visible: true,
        });
        app.cursor = 2; // the one content row
        app.toggle_visual();
        assert!(app.visual_active());

        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let lines = content_line(
            &app,
            0,
            0,
            0,
            2,
            200,
            false,
            &mut highlighter,
            &diagnostics,
            &[],
            true,
        );
        assert_eq!(lines.len(), 1);

        let needle_span = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "needle")
            .expect("needle span present");
        assert_eq!(
            needle_span.style.bg,
            Some(Color::Yellow),
            "the match itself must show the search style, not the selection background"
        );
        let beta_span = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("beta"))
            .expect("beta span present");
        assert_eq!(
            beta_span.style.bg,
            Some(Color::Rgb(25, 25, 70)),
            "text outside the match keeps the selection background"
        );
    }

    #[test]
    fn content_line_cursor_reverses_a_selected_row_too() {
        let mut app = app_with_plain_row("selected and also the cursor");
        app.cursor = 2; // the one content row
        app.toggle_visual();
        assert!(app.visual_active());

        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let lines = content_line(
            &app,
            0,
            0,
            0,
            2,
            200,
            true, // is_cursor
            &mut highlighter,
            &diagnostics,
            &[],
            true,
        );
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].style.add_modifier.contains(Modifier::REVERSED),
            "the cursor's own row must still reverse-video even while selected — \
             req 9's deterministic precedence puts the cursor last, always winning"
        );
    }

    #[test]
    fn side_by_side_selection_marks_only_the_selected_del_cell_not_its_paired_add() {
        let app_rows = vec![
            row(DiffLineKind::Del, "removed", Some(1), None),
            row(DiffLineKind::Add, "added", None, Some(1)),
        ];
        let mut app = app_with_rows(app_rows);
        // app.rows: 0 FileHeader, 1 HunkHeader, 2 the Del row, 3 the Add row.
        app.cursor = 2;
        app.toggle_visual(); // selects only the Del row (anchor == cursor == 2)
        assert!(app.visual_active());

        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let comments = CommentIndex::default();
        let pair = SideBySideRow::Paired {
            old: SideCell::Line { flat_idx: 2 },
            new: SideCell::Line { flat_idx: 3 },
        };
        // Wide enough on both sides that neither "removed" nor "added" wraps
        // — `gutter_width()` alone eats a fixed chunk of whatever width is
        // requested (see `side_by_side_row_line_marks_a_search_match_on_the_new_side_only`'s
        // own comment for the same margin).
        let lines = side_by_side_row_line(
            &app,
            pair,
            gutter_width() + 20,
            gutter_width() + 20,
            &mut highlighter,
            &diagnostics,
            &comments,
            true,
        );
        assert_eq!(lines.len(), 1);
        let divider_idx = lines[0]
            .spans
            .iter()
            .position(|s| s.content.as_ref() == "\u{2502}")
            .expect("every paired row carries the old/new divider");
        assert!(
            lines[0].spans[..divider_idx].iter().any(is_visual_selected),
            "the selected Del row must mark the old cell; spans: {:?}",
            lines[0].spans
        );
        assert!(
            !lines[0].spans[divider_idx + 1..]
                .iter()
                .any(is_visual_selected),
            "the Add row's new cell shares no `flat_idx` with the selected Del row and \
             must stay unmarked; spans: {:?}",
            lines[0].spans
        );
    }

    #[test]
    fn side_by_side_selection_marks_both_cells_of_a_selected_context_row() {
        let app_rows = vec![row(
            DiffLineKind::Context,
            "same both sides",
            Some(1),
            Some(1),
        )];
        let mut app = app_with_rows(app_rows);
        app.cursor = 2; // the one content row
        app.toggle_visual();
        assert!(app.visual_active());

        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let comments = CommentIndex::default();
        // A `Context` row's old and new cells share the same `flat_idx`
        // (see `flatten_side_by_side`) — the same logical line rendered
        // twice, so selecting it must mark both.
        let pair = SideBySideRow::Paired {
            old: SideCell::Line { flat_idx: 2 },
            new: SideCell::Line { flat_idx: 2 },
        };
        let lines = side_by_side_row_line(
            &app,
            pair,
            gutter_width() + 20,
            gutter_width() + 20,
            &mut highlighter,
            &diagnostics,
            &comments,
            true,
        );
        assert_eq!(lines.len(), 1);
        let divider_idx = lines[0]
            .spans
            .iter()
            .position(|s| s.content.as_ref() == "\u{2502}")
            .expect("every paired row carries the old/new divider");
        assert!(
            lines[0].spans[..divider_idx].iter().any(is_visual_selected),
            "a selected context row must mark the old cell; spans: {:?}",
            lines[0].spans
        );
        assert!(
            lines[0].spans[divider_idx + 1..]
                .iter()
                .any(is_visual_selected),
            "a selected context row must mark the new cell too; spans: {:?}",
            lines[0].spans
        );
    }
}
