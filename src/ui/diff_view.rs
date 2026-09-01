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
use crate::keymap::{Action, Keymap};
use crate::lsp::DiagnosticsStore;
use crate::ui::app::{App, Layout};
use crate::ui::mouse::{HitRow, LineHit};
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
use ratatui::layout::{Alignment, Rect};
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
    // Discards the `HitRow`s `render_content` hands back — issue #22's
    // click hit-testing is scoped to the top-level `View::Diff`/`View::File`
    // panes only (see `FrameGeometry::diff_content`'s docs on that
    // assumption); `TimelineView`'s nested diff pane, the only caller that
    // reaches this plain (non-focusable) `render` rather than
    // `render_focusable`, has no seam to record them into.
    let _ = render_content(
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
#[allow(clippy::too_many_arguments)]
// mirrors `render`'s own shape plus
// the focus/hint/keymap parameters `PaneChrome`/[`render_empty_state`] need;
// splitting this into a struct would just move the same fields one level
// down.
///
/// Returns one [`HitRow`] per rendered terminal row of the content pane, in
/// the same viewport-clamped order [`render_content`] pushed the matching
/// `Line`s — `ui::mod::draw`'s `View::Diff` arm pairs this with the same
/// content rect [`pane::inner_rect`] recomputes from `area` and hands both
/// to [`crate::ui::mouse::FrameGeometry::record_diff_content`], so issue
/// #22's click resolution shares this exact render pass rather than a
/// second wrap/layout computation (see `crate::ui::mouse`'s own doc
/// comment).
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
    keymap: &Keymap,
) -> Vec<HitRow> {
    let block = PaneChrome::new(" diff ", area.width)
        .focused(focused)
        .hints(hints)
        .block();
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // A parsed diff with zero files (the working tree is clean, a revision
    // has no changes, …) used to fall straight through to `render_content`,
    // which for an empty `app.rows` renders nothing at all — a bare
    // bordered pane indistinguishable from one still loading. Only this
    // root pane special-cases it (not the plain [`render`] [`TimelineView`]
    // nests): the hint line below names `Action::OpenScopeMenu`, which only
    // `View::Diff` actually intercepts (see `ui::mod::handle_action`'s
    // `OpenScopeMenu` arm) — showing it over a historical snapshot's empty
    // diff would name a key that does nothing there.
    if app.rows.is_empty() {
        render_empty_state(frame, inner, app, keymap);
        return Vec::new();
    }
    render_content(
        frame,
        inner,
        app,
        highlighter,
        layout,
        diagnostics,
        comments,
    )
}

/// [`render_focusable`]'s placeholder for an empty `app.rows`: a short,
/// dimmed headline plus a dimmed hint line, both centered in `inner` —
/// unlike every other row this pane draws, this isn't gutter/line-numbered
/// content read top-down, it's a single status message, so it earns the one
/// spot in this file that centers rather than left-aligns. Wording is
/// scope-aware: [`App::disk_is_new_side`] is `true` only for the plain
/// working-tree scope (see its own docs) — every other scope (staged, a
/// git/jj revision or range, a PR) gets the more general phrasing, since
/// "working tree" would be actively wrong for those. The hint line's keys
/// are read off `keymap`, not hardcoded `"o"`/`"q"` — a `[keys]` rebind or
/// the emacs preset must still name the keys that actually work, same
/// reasoning as [`crate::ui::hints::HintItem::for_actions`]. A third hint
/// part — "review this branch against `<base>` (+N commits)" — joins the
/// other two only on a clean working tree with something detected ahead of
/// it (`app.disk_is_new_side && app.branch_vs_base_hint`'s `ahead > 0`):
/// dirty-tree sessions and every non-working-tree scope simply don't show
/// it, matching [`App::branch_vs_base_hint`]'s own "only for the plain
/// working-tree scope" scope.
fn render_empty_state(frame: &mut Frame, inner: Rect, app: &App, keymap: &Keymap) {
    let headline = if app.disk_is_new_side {
        "working tree clean \u{2014} nothing to review"
    } else {
        "no changes in this scope"
    };
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    let mut lines = vec![Line::from(Span::styled(headline, dim))];

    let hint_parts: Vec<String> = [
        keymap
            .binding_for(Action::OpenScopeMenu)
            .map(|seq| format!("{} opens the scope menu", seq.compact_notation())),
        keymap
            .binding_for(Action::Quit)
            .map(|seq| format!("{} quits", seq.compact_notation())),
        app.disk_is_new_side
            .then_some(app.branch_vs_base_hint.as_ref())
            .flatten()
            .filter(|hint| hint.ahead > 0)
            .and_then(|hint| {
                keymap.binding_for(Action::ReviewBranchVsBase).map(|seq| {
                    format!(
                        "{} review this branch against {} (+{} commits)",
                        seq.compact_notation(),
                        hint.base,
                        hint.ahead,
                    )
                })
            }),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !hint_parts.is_empty() {
        lines.push(Line::from(Span::styled(hint_parts.join(" \u{b7} "), dim)));
    }

    // Vertically centered by shrinking the render area to just the text's
    // own height and shifting its top down, rather than padding the
    // `Vec<Line>` itself with blank entries — `Paragraph`'s own
    // `Alignment::Center` already handles the horizontal half.
    let block_height = (lines.len() as u16).min(inner.height);
    let top_pad = inner.height.saturating_sub(block_height) / 2;
    let area = Rect {
        x: inner.x,
        y: inner.y + top_pad,
        width: inner.width,
        height: block_height,
    };
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

fn render_content(
    frame: &mut Frame,
    inner: Rect,
    app: &App,
    highlighter: &mut LineHighlighter,
    layout: Layout,
    diagnostics: &DiagnosticsStore,
    comments: &CommentIndex,
) -> Vec<HitRow> {
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

/// As [`comments_for_row`], but the comments whose relocated (or, if
/// detached, original) *start* is `row`'s line — issue #19's fix for the
/// pre-#19 bug where a range's body block rendered once per line it
/// covered, via [`comments_for_row`]'s `.at()` fan-out, instead of once at
/// the range's first line. The gutter marker (drawn via [`comments_for_row`]
/// in [`render_row`]) is deliberately unaffected: every covered line still
/// needs its own marker, only the body block needs to render once.
fn comments_starting_at_row<'a>(
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
        Some(line) => comments.starting_at(file.display_path(), line),
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
/// Tags one [`render_row`] output line with its structural [`HitRow`] kind
/// for the *unified* layout — the only layout where a
/// [`crate::diff::RenderRow::Line`] produces a row of its own rather than
/// being folded into a [`crate::diff::SideBySideRow::Paired`] cell (see
/// `flatten_side_by_side`'s docs); a header/gap row maps the same way
/// whichever layout it renders under, since `flatten_side_by_side` always
/// puts those behind `SideBySideRow::Full` (see [`side_by_side_row_line`]'s
/// `Full` arm, which reuses this too).
fn hit_row_for(row: RenderRow, line: LineHit) -> HitRow {
    match row {
        RenderRow::Line { .. } => HitRow::Unified(line),
        RenderRow::Gap { .. } => HitRow::Gap {
            flat_idx: line.row_idx,
        },
        RenderRow::FileHeader { .. }
        | RenderRow::BinaryNotice { .. }
        | RenderRow::HunkHeader { .. } => HitRow::Structural {
            flat_idx: line.row_idx,
        },
    }
}

fn render_unified(
    frame: &mut Frame,
    inner: Rect,
    app: &App,
    highlighter: &mut LineHighlighter,
    diagnostics: &DiagnosticsStore,
    comments: &CommentIndex,
) -> Vec<HitRow> {
    let content_width = inner.width as usize;
    let viewport_height = inner.height as usize;
    let wrap = crate::config::wrap_enabled();
    let mut lines: Vec<Line> = Vec::with_capacity(viewport_height);
    let mut hits: Vec<HitRow> = Vec::with_capacity(viewport_height);

    let mut idx = app.scroll_offset;
    while lines.len() < viewport_height && idx < app.rows.len() {
        let row = app.rows[idx];
        for (line, line_hit) in render_row(
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
            hits.push(hit_row_for(row, line_hit));
        }
        if app.comments_visible {
            for (block_line, hit) in comment_block_lines(
                comments_starting_at_row(app, row, comments),
                content_width,
                idx,
            ) {
                if lines.len() >= viewport_height {
                    break;
                }
                lines.push(block_line);
                hits.push(hit);
            }
        }
        idx += 1;
    }

    frame.render_widget(Paragraph::new(lines), inner);
    hits
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
) -> Vec<HitRow> {
    let width = inner.width as usize;
    let divider_width = 1;
    let left_width = width.saturating_sub(divider_width) / 2;
    let right_width = width.saturating_sub(divider_width + left_width);
    let viewport_height = inner.height as usize;

    let start = side_by_side_scroll_start(&app.side_by_side_rows, app.scroll_offset);
    let wrap = crate::config::wrap_enabled();
    let mut lines: Vec<Line> = Vec::with_capacity(viewport_height);
    let mut hits: Vec<HitRow> = Vec::with_capacity(viewport_height);

    let mut idx = start;
    while lines.len() < viewport_height && idx < app.side_by_side_rows.len() {
        let row = app.side_by_side_rows[idx];
        for (line, hit) in side_by_side_row_line(
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
            hits.push(hit);
        }
        if app.comments_visible {
            let (anchor_flat_idx, annotations) =
                side_by_side_row_comments_starting_at(app, row, comments);
            for (block_line, hit) in comment_block_lines(annotations, width, anchor_flat_idx) {
                if lines.len() >= viewport_height {
                    break;
                }
                lines.push(block_line);
                hits.push(hit);
            }
        }
        idx += 1;
    }

    frame.render_widget(Paragraph::new(lines), inner);
    hits
}

/// As [`comments_starting_at_row`], for one [`SideBySideRow`] — the
/// side-by-side layout's own body-block call site, needing the same
/// once-per-range fix [`comments_starting_at_row`]'s own docs describe for
/// unified. Only the *new* side carries comment anchors (comments are only
/// ever left on a `Context`/`Add` row, which always has a `new_line` — see
/// `App::comment_target`), so a `Paired` row's old-side cell is never
/// consulted here — req 8's "new-cell-only" rule falls out of this for free,
/// the same way it already does for [`render_row`]'s marker (called through
/// `comments_for_row`, unaffected by this function).
///
/// Also returns the resolved `flat_idx` alongside the slice (`0` when
/// there's nothing to anchor to — never read in that case, since an empty
/// slice makes `comment_block_lines` a no-op regardless): issue #22's
/// `HitRow::CommentBody` needs the same anchor row `comment_block_lines` is
/// about to render a body block for, and re-deriving it a second time at
/// the call site would just repeat this match.
fn side_by_side_row_comments_starting_at<'a>(
    app: &App,
    row: SideBySideRow,
    comments: &'a CommentIndex,
) -> (usize, &'a [CommentAnnotation]) {
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
        Some(flat_idx) => (
            flat_idx,
            comments_starting_at_row(app, app.rows[flat_idx], comments),
        ),
        None => (0, &[]),
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
) -> Vec<(Line<'static>, HitRow)> {
    match row {
        // `flatten_side_by_side` never puts a `RenderRow::Line` behind
        // `Full` (see its own docs) — `render_row`'s output here is always
        // a header/gap, so `hit_row_for` (shared with `render_unified`)
        // never actually reaches its `Unified` arm from this call site, but
        // it's still the correct tag if that invariant ever changed.
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
        )
        .into_iter()
        .map(|(line, line_hit)| (line, hit_row_for(app.rows[flat_idx], line_hit)))
        .collect(),
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
            let blank_left = (Line::from(Span::raw(" ".repeat(left_width))), None);
            let blank_right = (Line::from(Span::raw(" ".repeat(right_width))), None);
            (0..pair_height)
                .map(|i| {
                    let (left_line, old_hit) = left_lines.get(i).unwrap_or(&blank_left);
                    let (right_line, new_hit) = right_lines.get(i).unwrap_or(&blank_right);
                    let mut spans = left_line.spans.clone();
                    spans.push(Span::styled(
                        "\u{2502}",
                        Style::default().fg(Color::DarkGray),
                    ));
                    spans.extend(right_line.spans.clone());
                    let hit = HitRow::SideBySide {
                        left_width,
                        old: *old_hit,
                        new: *new_hit,
                    };
                    (Line::from(spans), hit)
                })
                .collect()
        }
    }
}

/// One column's worth of visual rows for a [`SideCell`]: the same
/// rendering [`render_row`] already produces for that flat row when
/// populated (one row, or more if it wrapped), each paired with its own
/// [`LineHit`], or a single blank filler line at `width` paired with `None`
/// (nothing to hit-test — see [`HitRow::SideBySide`]'s docs) when the other
/// side has no counterpart here. Pairing this up against the other side's
/// row count is [`side_by_side_row_line`]'s job, not this function's.
#[allow(clippy::too_many_arguments)] // see `content_line`'s comment
fn side_cell_lines(
    app: &App,
    cell: SideCell,
    width: usize,
    highlighter: &mut LineHighlighter,
    diagnostics: &DiagnosticsStore,
    comments: &CommentIndex,
    wrap: bool,
) -> Vec<(Line<'static>, Option<LineHit>)> {
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
        )
        .into_iter()
        .map(|(line, hit)| (line, Some(hit)))
        .collect(),
        SideCell::Empty => vec![(Line::from(Span::raw(" ".repeat(width))), None)],
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
/// Every non-`Line` row is exactly one visual row whose whole width is a
/// single rendered line with nothing to sub-divide by column — its
/// [`LineHit`] is always `content_start_col: 0` (there's no wrap to
/// accumulate an offset across). Callers (`render_unified`'s loop,
/// `side_by_side_row_line`'s `Full` arm) turn this into the right
/// structural [`HitRow`] via [`hit_row_for`], which is what actually cares
/// which `RenderRow` variant this was — this function stays a plain
/// `LineHit` producer regardless.
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
) -> Vec<(Line<'static>, LineHit)> {
    let single = |line: Line<'static>| {
        vec![(
            line,
            LineHit {
                row_idx: flat_idx,
                content_start_col: 0,
            },
        )]
    };
    match row {
        RenderRow::FileHeader { file_idx } => {
            single(file_header_line(app, file_idx, width, is_cursor))
        }
        RenderRow::BinaryNotice { file_idx } => {
            single(binary_notice_line(app, file_idx, width, is_cursor))
        }
        RenderRow::HunkHeader { file_idx, hunk_idx } => {
            single(hunk_header_line(app, file_idx, hunk_idx, width, is_cursor))
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
            single(gap_line(app, file_idx, gap_idx, width, is_cursor))
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
/// `[detached]`, issue #19's `start-end` when the annotation is a range
/// (`start != end` — nothing extra for a single-line one, so this is a
/// no-op on every pre-#19 comment), and a short id prefix, so `ktmr comments
/// resolve <id>`'s argument is visible without leaving the TUI) followed by
/// its word-wrapped body, indented so it reads as subordinate to the diff
/// line above it rather than another row of the diff itself. A resolved
/// comment's body renders struck through and dimmed; an open one in plain
/// (but still slightly indented) text. Callers pass this
/// [`comments_starting_at_row`]'s output, never [`comments_for_row`]'s —
/// see that function's docs — so `annotations` here is never more than one
/// row's worth of *range starts*, not every line a range happens to cover.
///
/// Every line this produces is tagged [`HitRow::CommentBody`] with
/// `anchor_flat_idx` — issue #22: clicking anywhere in the block (header or
/// wrapped body text alike) positions the cursor on the row the comment is
/// anchored to, never a column within the block itself (there's no
/// `symbols::scan`-addressable source line here to resolve one against).
fn comment_block_lines(
    annotations: &[CommentAnnotation],
    width: usize,
    anchor_flat_idx: usize,
) -> Vec<(Line<'static>, HitRow)> {
    const INDENT: &str = "      ";
    let body_width = width.saturating_sub(display_width(INDENT));
    let mut out: Vec<Line<'static>> = Vec::new();

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
        let header_text = if annotation.start == annotation.end {
            format!("{INDENT}[{label} {id_prefix}]")
        } else {
            format!(
                "{INDENT}[{label} {}-{} {id_prefix}]",
                annotation.start, annotation.end
            )
        };
        out.push(Line::from(Span::styled(header_text, header_style)));

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
    out.into_iter()
        .map(|line| (line, HitRow::CommentBody { anchor_flat_idx }))
        .collect()
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
pub(crate) fn gutter_width() -> usize {
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
) -> Vec<(Line<'static>, LineHit)> {
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
        out.push((
            line,
            LineHit {
                row_idx: flat_idx,
                content_start_col: col_offset,
            },
        ));
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
    use crate::comments::{self, Comment, CommentIndex};
    use crate::diff::{DiffFile, DiffHunk, DiffLineKind, DiffRow, SideBySideRow, SideCell};
    use crate::keymap::{KeySeq, vim_preset};
    use crate::lsp::DiagnosticsStore;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use std::path::{Path, PathBuf};

    /// A plain vim-preset [`Keymap`] for tests that need one only because
    /// [`render_focusable`]'s signature requires it — every binding this
    /// file's own tests actually assert against text for (`o`/`q`) is the
    /// same under either built-in preset, so which one is beside the point.
    fn test_keymap() -> Keymap {
        Keymap::from_bindings(&vim_preset(false))
    }

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

    /// As [`app_with_rows`], but rooted at a real `repo_root` (a tempdir
    /// standing in for the reviewed repo) instead of the fixed, nonexistent
    /// `/repo` — issue #19's comment-render tests need `comments::build_index`
    /// to actually read a file, which `/repo` can never satisfy.
    fn app_with_rows_in(repo_root: &Path, filename: &str, rows: Vec<DiffRow>) -> App {
        let file = DiffFile {
            old_path: Some(filename.to_owned()),
            new_path: Some(filename.to_owned()),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: rows.len() as u32,
                new_start: 1,
                new_lines: rows.len() as u32,
                header: String::new(),
                known_eof: true,
                rows,
            }],
            ..Default::default()
        };
        App::new("repo".to_owned(), repo_root.to_owned(), vec![file])
    }

    /// A `Comment` (single-line when `start == end`, a range otherwise),
    /// anchored against `lines` — the same tempdir+`build_index` pattern
    /// `comments::index`'s own tests use, so a render test's
    /// `CommentAnnotation`s come from the real relocation path rather than
    /// being hand-assembled with fields a real build could never produce
    /// together.
    fn range_comment(
        id: &str,
        file: &str,
        lines: &[&str],
        start: u32,
        end: u32,
        body: &str,
        status: CommentStatus,
    ) -> Comment {
        Comment {
            id: id.to_owned(),
            created_at: 0,
            file: file.to_owned(),
            anchor: comments::anchor_for(lines, start).unwrap(),
            end_anchor: if start == end {
                None
            } else {
                Some(comments::anchor_for(lines, end).unwrap())
            },
            body: body.to_owned(),
            status,
            resolved_at: None,
        }
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Strips issue #22's `HitRow`/`LineHit` half off `content_line`/
    /// `render_row`/`side_by_side_row_line`/`comment_block_lines`'s output
    /// so every render test written before #22 keeps asserting on plain
    /// `Vec<Line>` exactly as it did — the hit-row half gets its own
    /// dedicated tests (below) rather than every existing rendering
    /// assertion needing to destructure a tuple it doesn't care about.
    fn lines_only<T>(pairs: Vec<(Line<'static>, T)>) -> Vec<Line<'static>> {
        pairs.into_iter().map(|(line, _)| line).collect()
    }

    /// Renders `app` through `render_unified`/`render_side_by_side`
    /// directly (bypassing `render`/`render_focusable`'s border chrome, so
    /// `width`/`height` map onto content rows one-to-one) into an offscreen
    /// `width`x`height` buffer.
    fn draw_unified(app: &App, comments: &CommentIndex, width: u16, height: u16) -> Buffer {
        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_unified(
                    frame,
                    frame.area(),
                    app,
                    &mut highlighter,
                    &diagnostics,
                    comments,
                );
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// As [`draw_unified`], through `render_side_by_side` instead.
    fn draw_side_by_side(app: &App, comments: &CommentIndex, width: u16, height: u16) -> Buffer {
        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_side_by_side(
                    frame,
                    frame.area(),
                    app,
                    &mut highlighter,
                    &diagnostics,
                    comments,
                );
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// The whole buffer's cells, concatenated in row-major order — for a
    /// whole-output substring count (e.g. "this text appears exactly
    /// once"), the same shape `sidebar`'s own `buffer_text` helper uses.
    fn buffer_text(buffer: &Buffer) -> String {
        buffer.content.iter().map(|cell| cell.symbol()).collect()
    }

    /// One row's cells, concatenated left to right — for asserting what's
    /// (or isn't) on a *specific* screen row, which whole-buffer
    /// concatenation alone can't distinguish (it drops row boundaries).
    fn row_text(buffer: &Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buffer.cell((x, y)).map_or(" ", |c| c.symbol()))
            .collect()
    }

    /// The first screen row (top to bottom) whose text contains `needle` —
    /// for locating a row by its known content rather than a hand-counted
    /// `y`, which shifts depending on how many lines an earlier row's
    /// comment body block consumed.
    fn find_row_containing(buffer: &Buffer, height: u16, width: u16, needle: &str) -> String {
        (0..height)
            .map(|y| row_text(buffer, y, width))
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no row contains {needle:?}"))
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
                    &test_keymap(),
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
                    &test_keymap(),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let corner = buffer.cell((0, 0)).expect("top-left corner");
        assert_ne!(corner.fg, Color::Cyan);
    }

    /// As [`draw_unified`]/[`draw_side_by_side`], but through
    /// `render_focusable` — the empty-state tests below need the real
    /// `PaneChrome` border (rather than `render_unified`'s bare content),
    /// since a centering assertion has to know exactly where that border
    /// puts the content rect.
    fn draw_focusable(app: &App, width: u16, height: u16) -> Buffer {
        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let comments = CommentIndex::default();
        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_focusable(
                    frame,
                    frame.area(),
                    app,
                    &mut highlighter,
                    Layout::Unified,
                    &diagnostics,
                    &comments,
                    false,
                    &[],
                    &test_keymap(),
                );
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// An `App` with a parsed diff of zero files — exactly what `git
    /// diff`/`jj diff` produces for a clean working tree or a
    /// no-op scope, and the trigger for [`render_empty_state`].
    fn app_with_no_files() -> App {
        App::new("repo".to_owned(), PathBuf::from("/repo"), Vec::new())
    }

    #[test]
    fn render_focusable_shows_the_working_tree_placeholder_when_rows_are_empty() {
        let mut app = app_with_no_files();
        app.disk_is_new_side = true; // the plain working-tree scope
        assert!(app.rows.is_empty(), "sanity: a fileless diff has no rows");

        let buffer = draw_focusable(&app, 60, 10);
        let text = buffer_text(&buffer);
        assert!(
            text.contains("working tree clean") && text.contains("nothing to review"),
            "expected the working-tree wording, got:\n{text}"
        );
        assert!(
            !text.contains("no changes in this scope"),
            "the working-tree scope shouldn't also show the generic scope wording:\n{text}"
        );
    }

    /// `disk_is_new_side` is `false` for every non-working-tree scope
    /// (staged, a git/jj revision or range, a PR — see its own docs), so a
    /// staged empty diff exercises the same "other scopes" branch this test
    /// is really about without needing one test per scope kind.
    #[test]
    fn render_focusable_shows_the_generic_placeholder_for_a_non_working_tree_empty_scope() {
        let mut app = app_with_no_files();
        app.disk_is_new_side = false;
        app.scope_label = Some("r: HEAD".to_owned());

        let buffer = draw_focusable(&app, 60, 10);
        let text = buffer_text(&buffer);
        assert!(
            text.contains("no changes in this scope"),
            "expected the scope-generic wording, got:\n{text}"
        );
        assert!(
            !text.contains("working tree clean"),
            "a non-working-tree empty scope shouldn't claim the working tree is clean:\n{text}"
        );
    }

    /// The empty-state hint must read `Action::OpenScopeMenu`/`Action::Quit`
    /// off the live `Keymap`, the same way every other on-screen hint does
    /// (see `hints::HintItem::for_actions`'s docs) — not hardcode `"o"`/`"q"`,
    /// which would go stale under a `[keys]` rebind. Proven by rebinding
    /// the scope-menu action to something neither built-in preset uses and
    /// checking the rendered hint follows it.
    #[test]
    fn render_focusable_empty_placeholder_reads_the_live_keymap_binding() {
        let mut app = app_with_no_files();
        app.disk_is_new_side = true;

        let mut bindings = vim_preset(false);
        let slot = bindings
            .iter_mut()
            .find(|(_, a)| *a == Action::OpenScopeMenu)
            .unwrap();
        slot.0 = KeySeq::parse("C-o");
        let keymap = Keymap::from_bindings(&bindings);

        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let comments = CommentIndex::default();
        let backend = TestBackend::new(60, 10);
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
                    &keymap,
                );
            })
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("C-o"),
            "expected the rebound scope-menu key in the hint, got:\n{text}"
        );
    }

    #[test]
    fn render_focusable_empty_placeholder_is_horizontally_centered() {
        let mut app = app_with_no_files();
        app.disk_is_new_side = true;
        let (width, height) = (60, 10);

        let buffer = draw_focusable(&app, width, height);
        let row = find_row_containing(&buffer, height, width, "working tree clean");
        // `PaneChrome`'s border owns column 0 and `width - 1` — measuring
        // padding only inside that, the same content rect
        // `pane::inner_rect` gives `render_focusable`'s own `block.inner`.
        let inner = pane::inner_rect(width, Rect::new(0, 0, width, height));
        // `.chars()`, not a byte-index slice: `row` also holds the
        // border's multi-byte `'\u{2502}'`, so a byte offset would land
        // mid-character rather than at column `inner.x`.
        let content: String = row
            .chars()
            .skip(inner.x as usize)
            .take(inner.width as usize)
            .collect();
        let left_pad = content.chars().count() - content.trim_start().chars().count();
        let right_pad = content.chars().count() - content.trim_end().chars().count();
        assert!(
            left_pad.abs_diff(right_pad) <= 1,
            "expected roughly equal left/right padding for a centered line, \
             got {left_pad} vs {right_pad} in {content:?}"
        );
    }

    #[test]
    fn render_focusable_empty_placeholder_is_vertically_centered() {
        let mut app = app_with_no_files();
        app.disk_is_new_side = true;
        let (width, height) = (60, 21);

        let buffer = draw_focusable(&app, width, height);
        let inner = pane::inner_rect(width, Rect::new(0, 0, width, height));
        let headline_row = (0..height)
            .find(|&y| row_text(&buffer, y, width).contains("working tree clean"))
            .expect("headline row not found");
        // Two lines of content (headline + hint) centered in a much taller
        // pane land well clear of both the top and bottom border, not
        // pinned to either.
        assert!(
            (inner.y + 2..inner.y + inner.height - 2).contains(&headline_row),
            "expected the headline vertically centered in a {height}-row pane, landed on row {headline_row}"
        );
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
        let lines = lines_only(content_line(
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
        ));
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
        let lines = lines_only(content_line(
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
        ));
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
        let lines = lines_only(content_line(
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
        ));
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
        let lines = lines_only(side_by_side_row_line(
            &app,
            pair,
            left_width,
            right_width,
            &mut highlighter,
            &diagnostics,
            &comments,
            true,
        ));

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

        let lines = lines_only(side_by_side_row_line(
            &app,
            SideBySideRow::Full { flat_idx: 0 }, // the file header row
            20,
            20,
            &mut highlighter,
            &diagnostics,
            &comments,
            true,
        ));
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
        let lines = lines_only(render_row(
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
        ));
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
        let lines = lines_only(content_line(
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
        ));
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
        let lines = lines_only(content_line(
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
        ));
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
        let lines = lines_only(content_line(
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
        ));
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
        let lines = lines_only(side_by_side_row_line(
            &app,
            pair,
            20,
            gutter_width() + 40,
            &mut highlighter,
            &diagnostics,
            &comments,
            true,
        ));
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
    fn content_line_emits_one_hit_per_wrap_row_with_accumulated_start_columns() {
        // The real renderer's LineHit output, not a hand-built fixture:
        // `content_start_col` must be the display column each wrap row
        // *starts* at (captured before the width increment), or every
        // continuation-row click resolves one row's width off.
        let app_rows = vec![row(
            DiffLineKind::Context,
            &"x".repeat(50),
            Some(1),
            Some(1),
        )];
        let app = app_with_rows(app_rows);
        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let width = 30;
        let content_width = width - gutter_width();

        let pairs = content_line(
            &app,
            0,
            0,
            0,
            2,
            width,
            false,
            &mut highlighter,
            &diagnostics,
            &[],
            true,
        );
        assert!(pairs.len() >= 2, "50 columns must wrap at width {width}");
        for (i, (_, hit)) in pairs.iter().enumerate() {
            assert_eq!(hit.row_idx, 2, "every wrap row belongs to flat row 2");
            assert_eq!(
                hit.content_start_col,
                i * content_width,
                "wrap row {i} starts where the previous row's width ended"
            );
        }
    }

    #[test]
    fn side_by_side_unequal_wrap_leaves_the_exhausted_side_without_a_hit() {
        // Real `side_by_side_row_line` output for a Paired row whose old
        // side wraps taller than its new side: the new cell's overflow
        // rows must carry `None` (blank filler is non-actionable), while
        // the old cell keeps its per-row hits.
        let app_rows = vec![
            row(DiffLineKind::Del, &"o".repeat(45), Some(1), None),
            row(DiffLineKind::Add, "short", None, Some(1)),
        ];
        let app = app_with_rows(app_rows);
        let mut highlighter = LineHighlighter::new();
        let diagnostics = DiagnosticsStore::new();
        let comments = CommentIndex::default();

        let pairs = side_by_side_row_line(
            &app,
            SideBySideRow::Paired {
                old: crate::diff::SideCell::Line { flat_idx: 2 },
                new: crate::diff::SideCell::Line { flat_idx: 3 },
            },
            30,
            30,
            &mut highlighter,
            &diagnostics,
            &comments,
            true,
        );
        assert!(pairs.len() >= 2, "the old side must wrap at width 30");
        let HitRow::SideBySide { old, new, .. } = &pairs[0].1 else {
            panic!("paired rows emit SideBySide hits, got {:?}", pairs[0].1);
        };
        assert!(old.is_some() && new.is_some(), "row 0 has both cells");
        let HitRow::SideBySide { old, new, .. } = &pairs[1].1 else {
            panic!("paired rows emit SideBySide hits, got {:?}", pairs[1].1);
        };
        assert!(old.is_some(), "the taller old side still has content");
        assert!(
            new.is_none(),
            "the exhausted new side's filler row must be non-actionable"
        );
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
        let lines = lines_only(content_line(
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
        ));
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
        let blank = lines_only(content_line(
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
        ));
        assert_eq!(blank.len(), 1, "an empty line renders one visual row");
        assert!(
            blank[0].spans.iter().any(is_visual_selected),
            "a selected blank line must still show the selection; spans: {:?}",
            blank[0].spans
        );

        // A short selected row: the fill past its last character continues
        // the selection bar to the pane edge instead of cutting it off.
        let short = lines_only(content_line(
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
        ));
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
        let lines = lines_only(content_line(
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
        ));
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
        let lines = lines_only(content_line(
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
        ));
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
        let lines = lines_only(side_by_side_row_line(
            &app,
            pair,
            gutter_width() + 20,
            gutter_width() + 20,
            &mut highlighter,
            &diagnostics,
            &comments,
            true,
        ));
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
        let lines = lines_only(side_by_side_row_line(
            &app,
            pair,
            gutter_width() + 20,
            gutter_width() + 20,
            &mut highlighter,
            &diagnostics,
            &comments,
            true,
        ));
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

    // -- Issue #19: range comment rendering -------------------------------

    /// The gutter marker fans out to every covered line (unchanged `.at()`
    /// behavior — req 7's first bullet), but the body block itself renders
    /// exactly once, at the range's first line — the `.at()` -> `.starting_at()`
    /// core fix this issue makes (see `comments_starting_at_row`'s docs for
    /// the pre-#19 bug this replaces).
    #[test]
    fn unified_range_marks_every_covered_row_but_renders_the_body_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.rs"), "one\ntwo\nthree\nfour\n").unwrap();
        let lines = ["one", "two", "three", "four"];
        let rows = vec![
            row(DiffLineKind::Context, "one", Some(1), Some(1)),
            row(DiffLineKind::Context, "two", Some(2), Some(2)),
            row(DiffLineKind::Context, "three", Some(3), Some(3)),
            row(DiffLineKind::Context, "four", Some(4), Some(4)),
        ];
        let app = app_with_rows_in(dir.path(), "f.rs", rows);
        let comment = range_comment(
            "rangeid1",
            "f.rs",
            &lines,
            2,
            3,
            "check this range",
            CommentStatus::Open,
        );
        let index = comments::build_index(dir.path(), std::slice::from_ref(&comment));

        let buffer = draw_unified(&app, &index, 80, 20);

        let marker = '\u{25C6}'; // open, still-anchored marker
        let one_row = find_row_containing(&buffer, 20, 80, "one");
        assert!(
            !one_row.contains(marker),
            "row before the range: {one_row:?}"
        );
        let two_row = find_row_containing(&buffer, 20, 80, "two");
        assert!(two_row.contains(marker), "range start row: {two_row:?}");
        let three_row = find_row_containing(&buffer, 20, 80, "three");
        assert!(three_row.contains(marker), "range end row: {three_row:?}");
        let four_row = find_row_containing(&buffer, 20, 80, "four");
        assert!(
            !four_row.contains(marker),
            "row after the range: {four_row:?}"
        );

        let whole = buffer_text(&buffer);
        assert_eq!(
            whole.matches("check this range").count(),
            1,
            "the body must render exactly once, not once per covered line"
        );
    }

    /// The `.at()` → `.starting_at()` body-block switch through the *full*
    /// unified render path with a genuinely single-line comment
    /// (`end_anchor: None`) — the acceptance criterion "existing
    /// single-line comments render unchanged" pinned by execution, not by
    /// the inference that `start == end` makes the two lookups agree.
    #[test]
    fn unified_single_line_comment_still_renders_marker_and_body_at_its_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.rs"), "one\ntwo\nthree\n").unwrap();
        let lines = ["one", "two", "three"];
        let rows = vec![
            row(DiffLineKind::Context, "one", Some(1), Some(1)),
            row(DiffLineKind::Context, "two", Some(2), Some(2)),
            row(DiffLineKind::Context, "three", Some(3), Some(3)),
        ];
        let app = app_with_rows_in(dir.path(), "f.rs", rows);
        // start == end → `range_comment` builds `end_anchor: None`.
        let comment = range_comment(
            "single01",
            "f.rs",
            &lines,
            2,
            2,
            "just this line",
            CommentStatus::Open,
        );
        let index = comments::build_index(dir.path(), std::slice::from_ref(&comment));

        let buffer = draw_unified(&app, &index, 80, 20);

        let marker = '\u{25C6}';
        let two_row = find_row_containing(&buffer, 20, 80, "two");
        assert!(two_row.contains(marker), "anchored row: {two_row:?}");
        let one_row = find_row_containing(&buffer, 20, 80, "one");
        assert!(!one_row.contains(marker), "row above: {one_row:?}");
        let three_row = find_row_containing(&buffer, 20, 80, "three");
        assert!(!three_row.contains(marker), "row below: {three_row:?}");

        let whole = buffer_text(&buffer);
        assert_eq!(whole.matches("just this line").count(), 1);
        assert!(
            !whole.contains("2-2"),
            "a single-line header never shows a range extent"
        );
    }

    /// As the unified test above, but through the side-by-side layout and
    /// specifically covering a paired del/add row's *new* cell as the
    /// range's start — the body block must still render exactly once
    /// (issue #19's fix applies per `SideBySideRow`, not per old/new cell),
    /// and only in association with the new-side content, never the old.
    #[test]
    fn side_by_side_range_starting_on_a_paired_rows_new_cell_renders_the_body_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.rs"), "one\nadded\nthree\n").unwrap();
        let lines = ["one", "added", "three"];
        let rows = vec![
            row(DiffLineKind::Context, "one", Some(1), Some(1)),
            row(DiffLineKind::Del, "removed", Some(2), None),
            row(DiffLineKind::Add, "added", None, Some(2)),
            row(DiffLineKind::Context, "three", Some(3), Some(3)),
        ];
        let app = app_with_rows_in(dir.path(), "f.rs", rows);
        // Range covers new_line 2 ("added") through 3 ("three") — starts on
        // the paired del/add row's new cell.
        let comment = range_comment(
            "rangeid2",
            "f.rs",
            &lines,
            2,
            3,
            "side by side range body",
            CommentStatus::Open,
        );
        let index = comments::build_index(dir.path(), std::slice::from_ref(&comment));

        let buffer = draw_side_by_side(&app, &index, 80, 20);

        let whole = buffer_text(&buffer);
        assert_eq!(
            whole.matches("side by side range body").count(),
            1,
            "the body must render exactly once across the whole side-by-side pane"
        );
        let added_row = find_row_containing(&buffer, 20, 80, "added");
        assert!(
            added_row.contains('\u{25C6}'),
            "the new cell's own row must still carry the marker: {added_row:?}"
        );
    }

    /// The header line issue #19 adds a `start-end` suffix to — present for
    /// a range (`start != end`), absent for a single-line comment (`start
    /// == end`), each built through the real `comments::build_index`
    /// relocation path.
    #[test]
    fn comment_block_lines_header_shows_the_range_only_when_start_and_end_differ() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.rs"), "one\ntwo\nthree\nfour\n").unwrap();
        let lines = ["one", "two", "three", "four"];
        let range = range_comment(
            "rangeid3",
            "f.rs",
            &lines,
            2,
            4,
            "range body",
            CommentStatus::Open,
        );
        let single = range_comment(
            "singleid",
            "f.rs",
            &lines,
            2,
            2,
            "single body",
            CommentStatus::Open,
        );
        let index = comments::build_index(dir.path(), &[range, single]);

        let range_annotations = index.starting_at("f.rs", 2);
        // Both comments start at line 2 — creation order preserves `range`
        // (pushed first) ahead of `single`.
        assert_eq!(range_annotations.len(), 2);
        let rendered = lines_only(comment_block_lines(range_annotations, 80, 0));
        let header_texts: Vec<String> = rendered.iter().map(line_text).collect();
        let range_header = header_texts
            .iter()
            .find(|t| t.contains("rangeid3"))
            .expect("range comment's header line");
        assert!(range_header.contains("2-4"), "{range_header:?}");
        let single_header = header_texts
            .iter()
            .find(|t| t.contains("singleid"))
            .expect("single-line comment's header line");
        assert!(
            !single_header.contains('-'),
            "a single-line comment's header must carry no range suffix: {single_header:?}"
        );
    }

    /// A resolved range renders struck through and dimmed — the same
    /// semantics a resolved single-line comment already had, unaffected by
    /// issue #19's `.starting_at()` switch (this checks the annotation
    /// that switch now feeds `comment_block_lines`, not a new dimming rule).
    #[test]
    fn comment_block_lines_dims_and_strikes_through_a_resolved_range() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.rs"), "one\ntwo\nthree\n").unwrap();
        let lines = ["one", "two", "three"];
        let comment = range_comment(
            "resolved1",
            "f.rs",
            &lines,
            2,
            3,
            "already handled",
            CommentStatus::Resolved,
        );
        let index = comments::build_index(dir.path(), std::slice::from_ref(&comment));

        let annotations = index.starting_at("f.rs", 2);
        assert_eq!(annotations.len(), 1);
        let rendered = lines_only(comment_block_lines(annotations, 80, 0));

        let header = &rendered[0];
        assert!(line_text(header).contains("resolved"));
        let body_line = rendered
            .iter()
            .find(|l| line_text(l).contains("already handled"))
            .expect("body line");
        let body_span = &body_line.spans[0];
        assert!(
            body_span.style.add_modifier.contains(Modifier::CROSSED_OUT),
            "a resolved comment's body must render struck through: {:?}",
            body_span.style
        );
    }

    /// A range whose endpoints can no longer both be relocated renders once,
    /// at its *original* start line, labeled `detached` — issue #19's
    /// `.starting_at()` fix is keyed by `CommentAnnotation::start`, which
    /// [`comments::build_index`] leaves at the original stored anchor for a
    /// detached range (never reordered or dropped — see
    /// `comments::RelocatedRange::detached`'s docs), so this falls out of
    /// the same mechanism the attached-range tests above exercise, not a
    /// separate code path.
    #[test]
    fn unified_detached_range_renders_once_at_its_original_start() {
        let dir = tempfile::tempdir().unwrap();
        // Disk content shares nothing with the lines the comment was
        // anchored against below — both endpoints fail to relocate.
        std::fs::write(dir.path().join("f.rs"), "unrelated content\n").unwrap();
        let anchor_lines = ["one", "two", "three", "four"];
        let rows = vec![
            row(DiffLineKind::Context, "one", Some(1), Some(1)),
            row(DiffLineKind::Context, "two", Some(2), Some(2)),
            row(DiffLineKind::Context, "three", Some(3), Some(3)),
            row(DiffLineKind::Context, "four", Some(4), Some(4)),
        ];
        let app = app_with_rows_in(dir.path(), "f.rs", rows);
        let comment = range_comment(
            "detached1",
            "f.rs",
            &anchor_lines,
            2,
            3,
            "orphaned range body",
            CommentStatus::Open,
        );
        let index = comments::build_index(dir.path(), std::slice::from_ref(&comment));

        let buffer = draw_unified(&app, &index, 80, 20);
        let whole = buffer_text(&buffer);
        assert_eq!(
            whole.matches("orphaned range body").count(),
            1,
            "a detached range's body must still render exactly once"
        );

        let two_row = find_row_containing(&buffer, 20, 80, "two");
        assert!(
            two_row.contains('\u{25C7}'),
            "a detached-only annotation shows the dim marker at its original start: {two_row:?}"
        );
        let three_row = find_row_containing(&buffer, 20, 80, "three");
        assert!(
            !three_row.contains("orphaned range body"),
            "the body must not also render at the range's original end row"
        );
    }

    /// Two comments whose ranges both start on the same line render in
    /// creation order — `comments::build_index` never sorts, and
    /// `comment_block_lines` renders `annotations` in whatever order it's
    /// given, so this is really pinning that neither of those quietly
    /// starts reordering by id, status, or range length.
    #[test]
    fn overlapping_comments_starting_on_the_same_line_render_in_creation_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.rs"), "one\ntwo\nthree\n").unwrap();
        let lines = ["one", "two", "three"];
        let first = range_comment(
            "firstid1",
            "f.rs",
            &lines,
            2,
            2,
            "first comment",
            CommentStatus::Open,
        );
        let second = range_comment(
            "secondid",
            "f.rs",
            &lines,
            2,
            3,
            "second comment",
            CommentStatus::Open,
        );
        let index = comments::build_index(dir.path(), &[first, second]);

        let annotations = index.starting_at("f.rs", 2);
        assert_eq!(annotations.len(), 2);
        assert_eq!(annotations[0].id, "firstid1");
        assert_eq!(annotations[1].id, "secondid");

        let rendered = lines_only(comment_block_lines(annotations, 80, 0));
        let texts: Vec<String> = rendered.iter().map(line_text).collect();
        let first_idx = texts
            .iter()
            .position(|t| t.contains("first comment"))
            .expect("first comment's body line");
        let second_idx = texts
            .iter()
            .position(|t| t.contains("second comment"))
            .expect("second comment's body line");
        assert!(
            first_idx < second_idx,
            "overlapping comments must render in creation order: {texts:?}"
        );
    }
}
