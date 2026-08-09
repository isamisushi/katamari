//! Application state and its pure state transitions. Deliberately free of
//! any terminal or ratatui dependency: `App` can be constructed and driven
//! entirely with parsed diff data and [`Action`]s, which is what makes it
//! testable without a real terminal.

use crate::diff::{
    ColumnMap, DiffFile, Gap, RenderRow, SideBySideRow, context_rows_for_gap, file_gaps, flatten,
    flatten_side_by_side, gap_boundary_matches, lsp_target, splice_gap,
};
use crate::keymap::Action;
use crate::lsp::DiagnosticsStore;
use crate::ui::hover_popup::HoverQuery;
use crate::ui::refresh;
use crate::ui::scroll;
use crate::ui::search;
use crate::ui::symbols;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One previously expanded gap, recorded so a watch refresh can reapply it
/// without touching disk again (`lines` is the whole splice, cached) and so
/// `z c` knows exactly which rows to fold back — keyed in
/// [`App::expanded_folds`] by [`DiffFile::display_path`] rather than
/// `file_idx`, since a file's index can shift across a refresh/scope swap
/// in a way its path never does.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FoldRange {
    /// 1-based, inclusive new-side line range this fold covers — always
    /// exactly one [`Gap`]'s full extent at the moment it was expanded (`z
    /// o` always expands a whole gap, never a part of one).
    new_range: (u32, u32),
    /// The expanded lines' raw text, in order — re-numbered through
    /// whatever offset the gap resolves to on every [`App::rederive`] call,
    /// never stored as `DiffRow`s directly, so a rederive after a scope
    /// swap or a hunk merge elsewhere in the same file can't leave stale
    /// line numbers behind.
    lines: Vec<String>,
}

/// What `z o` actually did to the gap under the cursor — [`App::expand_gap`]'s
/// three-way result. A plain `bool` can't distinguish "revealed real lines"
/// from "confirmed there's nothing left to reveal," and `ui::mod`'s
/// status-bar message needs to tell those apart without relying on where
/// the cursor ended up afterward — see [`App::expand_gap`]'s own docs for
/// why that heuristic isn't reliable (it silently fails for a diff's last
/// file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandOutcome {
    /// The gap's on-disk content was read and spliced in as real `Line`
    /// rows.
    Revealed,
    /// An unbounded trailing gap was probed and genuinely has nothing left
    /// past it — recorded so this fold row stops reappearing (see
    /// `App`'s `trailing_probed_empty`), but there was no content to
    /// reveal.
    ProbedEmpty,
    /// `gap_idx` no longer resolves, or the disk no longer matches what the
    /// diff expects at this gap's boundary — nothing changed.
    Rejected,
}

/// Which of the two diff layouts the diff pane renders. Toggled by
/// [`Action::ToggleLayout`]; [`crate::ui::diff_view`] may still render
/// unified even when this is [`Layout::SideBySide`] if the pane is too
/// narrow to show two columns (see `diff_view::effective_layout`) — that
/// fallback is a rendering concern, so it doesn't change this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    #[default]
    Unified,
    SideBySide,
}

impl Layout {
    fn toggled(self) -> Self {
        match self {
            Layout::Unified => Layout::SideBySide,
            Layout::SideBySide => Layout::Unified,
        }
    }
}

/// All state the UI needs to render a frame and respond to input.
pub struct App {
    pub repo_name: String,
    /// Absolute repository root. Every path in `files` is relative to this
    /// — it's what turns a diff's `new_path` into a file `Action::Hover`
    /// can actually open, and it's the boundary
    /// [`crate::lsp::adapter::workspace_root`] searches within when it
    /// walks up from that file looking for a `Cargo.toml`.
    pub repo_root: PathBuf,
    /// The last parsed model, untouched by any fold — the source
    /// [`Self::rederive`] clones on every derivation. `files`/`rows`/
    /// `side_by_side_rows` are the *derived* view (pristine plus whatever
    /// `expanded_folds` currently records spliced in); nothing outside this
    /// module ever mutates them directly, which is what lets a scope swap
    /// or a fold toggle stay a single, always-consistent recomputation
    /// rather than an ad hoc patch to whatever was already on screen.
    pristine_files: Vec<DiffFile>,
    pub files: Vec<DiffFile>,
    pub rows: Vec<RenderRow>,
    /// Del/add-run pairing of `rows` for the side-by-side layout, computed
    /// once at load time alongside `rows` rather than per frame — see
    /// [`crate::diff::flatten_side_by_side`].
    pub side_by_side_rows: Vec<SideBySideRow>,
    /// `file_gaps(&files[i])`, one entry per file, recomputed alongside
    /// `files`/`rows` on every [`Self::rederive`] rather than per frame —
    /// [`crate::ui::diff_view::gap_line`] (called once per visible fold row
    /// on every redraw) reads a specific `(file_idx, gap_idx)` entry
    /// straight out of here instead of re-walking a whole file's hunks and
    /// discarding every gap but one, over and over, for as long as that row
    /// stays on screen.
    pub gap_cache: Vec<Vec<Gap>>,
    pub cursor: usize,
    pub scroll_offset: usize,
    /// Index into the current row's [`symbols::scan`] output — which
    /// identifier-like token `Action::Hover` would target. Reset to `0`
    /// whenever the cursor moves to a different row; cycled by
    /// `Action::NextSymbol`/`PrevSymbol` without moving the cursor.
    pub active_symbol: usize,
    pub sidebar_visible: bool,
    pub layout: Layout,
    pub pending_keys: String,
    pub should_quit: bool,
    /// Whether this session is running `ktmr diff --watch` — purely a
    /// display flag for the status bar's "⦿ watch" indicator; `ui::mod`'s
    /// event loop decides independently (via whether it was handed a
    /// [`crate::ui::PreRefreshHook`]) whether to actually spawn a watcher,
    /// so this field can never drift into claiming watch mode is on when
    /// nothing is watching.
    pub watch_mode: bool,
    /// Whether a commented row's body renders as an inline block underneath
    /// it — `Action::ToggleComments`, default on. The gutter marker itself
    /// is unaffected by this: it's compact enough to always show, so
    /// turning inline bodies off is purely about screen real estate on a
    /// heavily annotated diff, not about hiding that comments exist at all.
    pub comments_visible: bool,
    /// Whether this diff's content is trusted to match what's live on disk
    /// right now — `true` for everything M1 through M10 ever produced (the
    /// working tree, staged changes, a git range/single-revision diff), and
    /// the default. `false` for M11's revision diffs opened via `ktmr diff
    /// -r`/`--from`/`--to` or from [`crate::ui::log_view::LogView`] (every
    /// row except its git-only "local changes" one): an arbitrary jj
    /// revset or historical commit can point at content from long before
    /// the file's current on-disk state, so a hover/goto-definition/
    /// find-references request against it would be asking a language
    /// server about a position that may no longer mean anything — the same
    /// reasoning [`crate::ui::timeline_view::TimelineView::hover_query`]
    /// already applies to every snapshot in the jj op-log timeline,
    /// generalized here into a per-`App` flag so a revision diff can reuse
    /// it without also reusing `TimelineView`'s nested-pane shape (see
    /// [`Self::hover_query`]).
    pub interactive: bool,
    /// Whether the diff's *new* side is the live working tree right now —
    /// gates `z o`: expanding a fold reads the file straight off disk (see
    /// `ui::mod::handle_action`'s `ExpandFold` arm), which is only ever
    /// safe to compare against a diff whose new side genuinely is that same
    /// disk content. Deliberately a *separate* flag from [`Self::interactive`]
    /// rather than reusing it — `interactive` is `true` for `--staged` too
    /// (its new side is the *index*, not disk; see
    /// `ui::mod::apply_scope_swap`), which would silently corrupt an
    /// expand's line numbers against whatever the working tree currently
    /// happens to hold. Default `false`; set `true` only at the working-tree
    /// construction sites (`main.rs`'s plain `ktmr diff`, the scope-menu's
    /// "Working tree" choice) — everything else (staged, a git range/
    /// revision diff, jj revisions, `LogView`-opened diffs, `TimelineView`'s
    /// nested diff) leaves it at the default, which is exactly the
    /// "historical or otherwise not-disk" set `z o` must refuse.
    pub disk_is_new_side: bool,
    /// A short, human-readable description of what's being diffed, shown in
    /// the status bar next to the repo name — `None` for the ordinary
    /// working-tree/staged/range diffs that need no explanation beyond the
    /// repo they're in, `Some("r: <id>")`/`Some("<from>..<to>")` for a
    /// revision diff, where it's the only thing on screen that says which
    /// revision(s) are being compared.
    pub scope_label: Option<String>,
    /// Every gap a `z o` has expanded and not yet folded back, keyed by
    /// [`DiffFile::display_path`] — see [`FoldRange`]'s docs for why path
    /// rather than `file_idx`, and [`Self::rederive`] for how this actually
    /// gets spliced back into `files` on every derivation.
    expanded_folds: HashMap<String, Vec<FoldRange>>,
    /// Files whose trailing gap `z o` has probed and found genuinely empty
    /// (the working tree has no more lines past the last hunk) — fed into
    /// [`Self::rederive`] as an effective `known_eof` override so the same
    /// trailing fold row doesn't reappear every time the diff re-derives.
    /// Representation-free by design (a set of paths, not a splice): there
    /// is nothing to cache, since an empty gap has no lines.
    trailing_probed_empty: HashSet<String>,
    viewport_height: usize,
    /// The unified layout's per-row wrap width — display columns left for a
    /// content row's highlighted text after the pane border and gutter are
    /// subtracted (see [`crate::ui::diff_view::unified_content_width`]),
    /// refreshed every frame by [`Self::set_content_width`] alongside
    /// [`Self::set_viewport_height`]. Drives [`Self::row_visual_height`],
    /// which every wrap-aware scroll computation in [`Self::update`]/
    /// [`Self::jump_cursor_to`]/[`Self::jump_to_diagnostic`] reads.
    ///
    /// Deliberately *not* layout-aware: side-by-side's columns are each
    /// narrower than the unified pane and wrap independently per side (see
    /// `diff_view::render_side_by_side`), so a row's true visual height
    /// there can differ from what this field implies. Scroll math uses this
    /// single, unified-width-based estimate regardless of which layout is
    /// actually on screen — an intentional scope simplification (see the
    /// M13 milestone notes): side-by-side's own rendering still wraps and
    /// pairs correctly, it just isn't what `ensure-cursor-visible`/
    /// half-page math measures against.
    content_width: usize,
    /// Issue #5's active search: `Some` both while the `/` prompt is open
    /// (live-updated every keystroke — see [`Self::recompute_search_live`])
    /// and after Enter confirms it (see [`Self::confirm_search`]), `None`
    /// when there's genuinely nothing to repeat (never opened, cancelled
    /// via Esc while typing, or a scope swap — see
    /// [`Self::apply_scope_swap`]). A bare `Esc` in the *normal* view
    /// (vim's `:noh`) does *not* clear this — see [`Self::clear_search`] —
    /// it only flips [`search::SearchHighlight::highlight_visible`] off, so
    /// `n`/`N` keep working on a "cleared" search the same way they do in
    /// real vim. `pub` so [`crate::ui::diff_view`] reads it directly to
    /// mark match ranges, the same way it already reads `active_symbol`.
    /// Kept alive across a rows rebuild by [`Self::recompute_search`]
    /// (called from every method that funnels through [`Self::rederive`]
    /// and has a settled `cursor` to recompute against — see that method's
    /// own docs for exactly which ones and why); a scope swap clears it
    /// outright instead, since a swap's new diff has no relationship to the
    /// old one for a match to persist against.
    pub search: Option<search::SearchHighlight>,
}

impl App {
    pub fn new(repo_name: String, repo_root: PathBuf, files: Vec<DiffFile>) -> Self {
        let mut app = Self {
            repo_name,
            repo_root,
            pristine_files: files,
            files: Vec::new(),
            rows: Vec::new(),
            side_by_side_rows: Vec::new(),
            gap_cache: Vec::new(),
            cursor: 0,
            scroll_offset: 0,
            active_symbol: 0,
            sidebar_visible: true,
            layout: Layout::default(),
            pending_keys: String::new(),
            should_quit: false,
            watch_mode: false,
            comments_visible: true,
            interactive: true,
            disk_is_new_side: false,
            scope_label: None,
            expanded_folds: HashMap::new(),
            trailing_probed_empty: HashSet::new(),
            viewport_height: 1,
            // Unbounded until the first frame reports a real pane width
            // (see `set_content_width`) — `row_visual_height` then treats
            // every row as exactly one visual row, matching `wrap = false`
            // behavior, rather than wrapping against a width of `0`.
            content_width: usize::MAX,
            search: None,
        };
        // No folds exist yet, so this just moves `pristine_files` in
        // verbatim — reusing the one derivation path rather than
        // duplicating its move-then-flatten shape here.
        app.rederive();
        app
    }

    /// Rebuilds `files`/`rows`/`side_by_side_rows`/`gap_cache` from
    /// `pristine_files`. Two very different costs depending on whether
    /// there's any fold state to account for:
    ///
    /// - **No folds** (`expanded_folds` and `trailing_probed_empty` both
    ///   empty — watch mode's steady state for a reviewer who never
    ///   presses `z o`): the derived view is `pristine_files` verbatim, so
    ///   this *moves* it into `files` (`std::mem::take`) instead of paying
    ///   for a full deep clone just to hand back an identical copy of what
    ///   was already there. `pristine_files` is left empty afterward
    ///   rather than restored — nothing reads it again until a fold
    ///   actually needs it (see the other branch), so keeping it populated
    ///   here would be pure busywork on every refresh that never folds
    ///   anything, which is the whole cost this branch exists to avoid.
    /// - **Some fold state**: splice every recorded [`FoldRange`] back in
    ///   (applied last-position-first within each file so splicing a later
    ///   gap never invalidates an earlier one's still-pending hunk index —
    ///   see the loop below) and apply `trailing_probed_empty`'s
    ///   `known_eof` override, which needs a real, independent clone of
    ///   `pristine_files` to splice into without disturbing the pristine
    ///   baseline itself. If the *previous* call took the no-folds branch
    ///   above, `pristine_files` is sitting empty — `files` (kept current
    ///   by every call regardless of branch) is exactly what a fresh clone
    ///   of `pristine_files` would have contained anyway at that point (no
    ///   fold existed yet to make them differ), so this resyncs from
    ///   `files` first. That resync clone is the one case this module
    ///   accepts paying full price for: it only ever happens once, exactly
    ///   on the `z o` keypress that creates a diff's *first* fold, not on
    ///   every subsequent one.
    ///
    /// The single mechanism [`Self::new`]/[`Self::expand_gap`]/
    /// [`Self::collapse_fold_at_cursor`]/[`Self::apply_refresh`]/
    /// [`Self::apply_scope_swap`] all funnel through — a fold toggle is
    /// never a patch to the previous `files`, always a fresh, structurally
    /// consistent recomputation from the one source of truth.
    fn rederive(&mut self) {
        let has_fold_state =
            !self.expanded_folds.is_empty() || !self.trailing_probed_empty.is_empty();
        self.files = if has_fold_state {
            if self.pristine_files.is_empty() && !self.files.is_empty() {
                self.pristine_files = self.files.clone();
            }
            self.splice_folds()
        } else {
            std::mem::take(&mut self.pristine_files)
        };
        self.rows = flatten(&self.files);
        self.side_by_side_rows = flatten_side_by_side(&self.files);
        self.gap_cache = self.files.iter().map(file_gaps).collect();
    }

    /// [`Self::rederive`]'s slow path: clones `pristine_files` and splices
    /// every recorded [`FoldRange`]/`trailing_probed_empty` entry into the
    /// clone, returning it as the new `files`. Split out of `rederive`
    /// itself only so that function's fast/slow branch reads as one
    /// decision rather than this whole loop nested inside an `if`.
    fn splice_folds(&mut self) -> Vec<DiffFile> {
        let mut files = self.pristine_files.clone();
        let mut orphaned: Vec<(String, u32)> = Vec::new();
        for file in &mut files {
            let display_path = file.display_path().to_owned();
            if self.trailing_probed_empty.contains(&display_path)
                && let Some(last) = file.hunks.last_mut()
            {
                last.known_eof = true;
            }
            let Some(ranges) = self.expanded_folds.get(&display_path) else {
                continue;
            };
            // Descending by start: splicing a `Between`/`Trailing` gap
            // removes a hunk from the file, shifting every later hunk's
            // index down by one. Working from the end means every gap
            // still to come this pass sits *before* whatever index just
            // shifted, so its own recorded position never goes stale mid-
            // loop.
            let mut ordered: Vec<&FoldRange> = ranges.iter().collect();
            ordered.sort_by_key(|r| std::cmp::Reverse(r.new_range.0));
            for fold in ordered {
                let gaps = file_gaps(file);
                let Some(gap) = gaps.iter().find(|g| g.new_start == fold.new_range.0) else {
                    orphaned.push((display_path.clone(), fold.new_range.0));
                    continue;
                };
                let Some(rows) = context_rows_for_gap(gap, &fold.lines) else {
                    orphaned.push((display_path.clone(), fold.new_range.0));
                    continue;
                };
                splice_gap(file, gap, rows);
            }
        }
        // A recorded fold that no longer maps onto any gap can never be
        // reapplied *or* collapsed again — leaving it in the map would just
        // be invisible state drifting further from the display every
        // rederive. Dropping it keeps `expanded_folds` an exact record of
        // what is actually spliced in right now.
        for (path, start) in orphaned {
            if let Some(ranges) = self.expanded_folds.get_mut(&path) {
                ranges.retain(|r| r.new_range.0 != start);
                if ranges.is_empty() {
                    self.expanded_folds.remove(&path);
                }
            }
        }
        files
    }

    /// The raw text of the row at `cursor`, for [`symbols::scan`] — `None`
    /// for a file/hunk header or binary notice, which have no line content
    /// to scan.
    fn cursor_row_text(&self) -> Option<&str> {
        match self.rows.get(self.cursor)? {
            RenderRow::Line {
                file_idx,
                hunk_idx,
                row_idx,
            } => Some(
                self.files[*file_idx].hunks[*hunk_idx].rows[*row_idx]
                    .text
                    .as_str(),
            ),
            _ => None,
        }
    }

    fn cycle_symbol(&mut self, delta: isize) {
        let Some(text) = self.cursor_row_text() else {
            return;
        };
        let symbols = symbols::scan(text);
        if symbols.is_empty() {
            return;
        }
        let len = symbols.len() as isize;
        let next = (self.active_symbol as isize + delta).rem_euclid(len);
        self.active_symbol = next as usize;
    }

    /// What `Action::Hover` should ask a language server about right now:
    /// the file and 0-based line the cursor's row targets (via
    /// [`lsp_target`]), the line's text (needed to convert the active
    /// symbol's display column into the server's encoding), and that
    /// column itself. `None` when the cursor isn't on an eligible row (a
    /// header, a `Del` line, a deleted/binary file) or the row has no
    /// identifier-like token to target.
    pub fn hover_query(&self) -> Option<HoverQuery> {
        if !self.interactive {
            return None;
        }
        let RenderRow::Line {
            file_idx,
            hunk_idx,
            row_idx,
        } = self.rows.get(self.cursor)?
        else {
            return None;
        };
        let file = &self.files[*file_idx];
        let row = &file.hunks[*hunk_idx].rows[*row_idx];
        let (relative_path, line) = lsp_target(row, file)?;
        let symbol = symbols::scan(&row.text).get(self.active_symbol).copied()?;
        Some(HoverQuery {
            file: self.repo_root.join(relative_path),
            git_root: self.repo_root.clone(),
            line,
            line_text: row.text.clone(),
            display_col: symbol.display_start,
        })
    }

    /// The file (repo-relative, exactly as it appears in the diff) and
    /// 1-based working-tree line `Action::AddComment` would anchor a new
    /// comment to: the cursor's current row, when it's eligible the same
    /// way [`Self::hover_query`]'s target is — a `Context`/`Add` row on a
    /// file that's still present on disk (see [`lsp_target`]'s docs for the
    /// exact rule). `None` on a header row, a `Del` row, or a
    /// deleted/binary file, none of which have a current line for a
    /// comment to be *about*.
    pub fn comment_target(&self) -> Option<(String, u32)> {
        let RenderRow::Line {
            file_idx,
            hunk_idx,
            row_idx,
        } = self.rows.get(self.cursor)?
        else {
            return None;
        };
        let file = &self.files[*file_idx];
        let row = &file.hunks[*hunk_idx].rows[*row_idx];
        let (relative_path, line0) = lsp_target(row, file)?;
        Some((relative_path.to_string_lossy().into_owned(), line0 + 1))
    }

    /// The diff pane's visible row count changes on terminal resize; the
    /// event loop reports it before each frame so half-page scrolling and
    /// scroll-to-cursor clamping use an up-to-date value.
    pub fn set_viewport_height(&mut self, height: usize) {
        self.viewport_height = height.max(1);
        self.clamp_scroll();
    }

    /// The diff pane's content width changes on resize the same way its
    /// height does — reported every frame alongside [`Self::set_viewport_height`]
    /// so [`Self::row_visual_height`]'s wrap-width stays current. See
    /// [`Self::content_width`]'s docs for exactly what width this expects
    /// (the unified layout's, regardless of which layout is actually
    /// showing) and [`crate::ui::diff_view::unified_content_width`], which
    /// every caller derives it through.
    pub fn set_content_width(&mut self, width: usize) {
        self.content_width = width.max(1);
        self.clamp_scroll();
    }

    /// How many visual rows `self.rows[idx]` occupies on screen right now —
    /// `1` for every header/binary-notice row (headers never wrap, see
    /// `diff_view::render_row`), `1` for a content row when `[ui] wrap` is
    /// off, and otherwise however many rows [`crate::ui::text::wrapped_row_count`]
    /// says its text soft-wraps into at [`Self::content_width`]. The single
    /// row-height oracle every wrap-aware call into `ui::scroll` in this
    /// module reads.
    fn row_visual_height(&self, idx: usize) -> usize {
        if !crate::config::wrap_enabled() {
            return 1;
        }
        let Some(RenderRow::Line {
            file_idx,
            hunk_idx,
            row_idx,
        }) = self.rows.get(idx)
        else {
            return 1;
        };
        let text = &self.files[*file_idx].hunks[*hunk_idx].rows[*row_idx].text;
        crate::ui::text::wrapped_row_count(text, crate::config::tab_width(), self.content_width)
    }

    /// Swaps in a freshly re-parsed diff — watch mode's refresh pipeline
    /// calls this after every debounced batch of filesystem changes (see
    /// `ui::mod`'s `handle_watch_refresh`). Preserves the cursor's logical
    /// position via [`refresh::capture_anchor`]/[`refresh::restore_anchor`]
    /// rather than reusing the old flat index blindly, which after an edit
    /// anywhere else in the diff would typically now point at unrelated
    /// content; the scroll offset is restored the same way, keeping the
    /// cursor's on-screen row stable rather than re-centering on every
    /// refresh.
    ///
    /// Returns whether the cursor's landed-on row is *exactly* the row it
    /// was anchored to before the refresh (same file, line, and text) —
    /// `ui::mod` uses this to decide whether an open hover/references
    /// overlay, always anchored to whatever's under the cursor, should
    /// survive the swap or close.
    pub fn apply_refresh(&mut self, files: Vec<DiffFile>) -> bool {
        let anchor =
            refresh::capture_anchor(&self.files, &self.rows, self.cursor, self.scroll_offset);

        self.prune_stale_folds(&files);
        self.pristine_files = files;
        self.rederive();
        self.active_symbol = 0;

        if self.rows.is_empty() {
            self.cursor = 0;
            self.scroll_offset = 0;
            self.recompute_search();
            return false;
        }

        let restored = refresh::restore_anchor(&self.files, &self.rows, &anchor);
        self.cursor = restored.row_index;
        self.scroll_offset = restored
            .row_index
            .saturating_sub(refresh::scroll_delta(&anchor));
        self.clamp_scroll();
        self.recompute_search();
        restored.overlay_survives
    }

    /// [`Self::apply_refresh`]'s pruning pass, run against the *old*
    /// `pristine_files` (still in place — this runs before they're
    /// replaced) and the incoming `new_files`: drops any recorded
    /// [`FoldRange`]/probed-empty-trailing entry that a re-diff has
    /// invalidated, so [`Self::rederive`] never tries to splice a fold back
    /// into content that no longer has a matching gap for it. A fold
    /// survives only if the new parse still has a gap starting at exactly
    /// the fold's recorded start with exactly the fold's recorded end (a
    /// gap that grew or shrank around that position — e.g. a new hunk now
    /// sitting where the fold's tail used to be — no longer "falls entirely
    /// inside exactly one gap," so it's dropped rather than partially
    /// re-spliced) *and* the boundary rows on either side read the same in
    /// both parses (see [`crate::diff::gap_adjacent_line`]) — a gap that
    /// kept the same numeric bounds but whose neighboring content actually
    /// changed underneath it is exactly the drift this second check is for.
    /// A trailing probe survives only if the file's last hunk kept the same
    /// four boundary fields, the same reasoning applied to "is there
    /// nothing past here" rather than to a specific cached splice.
    fn prune_stale_folds(&mut self, new_files: &[DiffFile]) {
        let old_files = &self.pristine_files;
        self.expanded_folds.retain(|display_path, ranges| {
            let (Some(old_file), Some(new_file)) = (
                find_file(old_files, display_path),
                find_file(new_files, display_path),
            ) else {
                return false;
            };
            ranges.retain(|fold| fold_still_fits(old_file, new_file, fold));
            !ranges.is_empty()
        });
        self.trailing_probed_empty.retain(|display_path| {
            let (Some(old_file), Some(new_file)) = (
                find_file(old_files, display_path),
                find_file(new_files, display_path),
            ) else {
                return false;
            };
            match (old_file.hunks.last(), new_file.hunks.last()) {
                (Some(o), Some(n)) => {
                    o.old_start == n.old_start
                        && o.old_lines == n.old_lines
                        && o.new_start == n.new_start
                        && o.new_lines == n.new_lines
                }
                _ => false,
            }
        });
    }

    /// Swaps in a completely different diff — the M12 scope-menu popup's
    /// "Working tree" / "Staged" / "Revision…" selections. Unlike
    /// [`Self::apply_refresh`] (a same-scope re-diff after a watched file
    /// changed, where preserving the cursor's logical position across a
    /// small edit is the whole point), a scope swap has nothing meaningful
    /// to preserve: the new diff isn't a later version of the old one, it's
    /// an unrelated review surface, so anchor restoration would just land
    /// the cursor somewhere arbitrary. Always resets to the top instead.
    /// `interactive`/`disk_is_new_side`/`scope_label` are set by the caller
    /// (see `crate::ui::mod::apply_scope_swap`), which is the one place
    /// that knows which scope this diff came from. Every fold is dropped
    /// unconditionally rather than pruned — unlike [`Self::apply_refresh`],
    /// a scope swap's new diff has no relationship to the old one at all,
    /// so "does this fold still fit" isn't even a meaningful question to
    /// ask of it.
    pub fn apply_scope_swap(
        &mut self,
        files: Vec<DiffFile>,
        interactive: bool,
        disk_is_new_side: bool,
        scope_label: Option<String>,
    ) {
        self.pristine_files = files;
        self.expanded_folds.clear();
        self.trailing_probed_empty.clear();
        self.rederive();
        self.cursor = 0;
        self.scroll_offset = 0;
        self.active_symbol = 0;
        self.interactive = interactive;
        self.disk_is_new_side = disk_is_new_side;
        self.scope_label = scope_label;
        // Cleared outright, matching fold state's own reset above, rather
        // than recomputed via `Self::recompute_search`: a scope swap's new
        // diff isn't a later version of the old one (see this method's own
        // docs), so a query that matched the *previous* scope has no
        // meaningful relationship to this one for a match to persist
        // against — unlike `Self::apply_refresh`, which re-diffs the same
        // scope and so keeps a confirmed search alive across it.
        self.search = None;
        self.clamp_scroll();
    }

    /// Index into `files`/`rows` for whichever file the cursor currently
    /// sits within, for sidebar highlighting.
    pub fn selected_file(&self) -> usize {
        match self.rows.get(self.cursor) {
            Some(RenderRow::FileHeader { file_idx }) => *file_idx,
            Some(RenderRow::BinaryNotice { file_idx }) => *file_idx,
            Some(RenderRow::HunkHeader { file_idx, .. }) => *file_idx,
            Some(RenderRow::Line { file_idx, .. }) => *file_idx,
            // A fold row is cursor-addressable (see `RenderRow::Gap`'s
            // docs) and belongs to a file just as much as any other row —
            // sidebar highlighting must follow the cursor onto it, not fall
            // back to file 0.
            Some(RenderRow::Gap { file_idx, .. }) => *file_idx,
            None => 0,
        }
    }

    pub fn update(&mut self, action: Action) {
        if self.rows.is_empty() {
            self.should_quit |= action == Action::Quit;
            return;
        }
        let last = self.rows.len() - 1;
        let cursor_before = self.cursor;
        match action {
            Action::CursorDown => self.cursor = (self.cursor + 1).min(last),
            Action::CursorUp => self.cursor = self.cursor.saturating_sub(1),
            Action::HalfPageDown => {
                self.cursor =
                    scroll::half_page_down(self.cursor, last, self.viewport_height, |i| {
                        self.row_visual_height(i)
                    });
            }
            Action::HalfPageUp => {
                self.cursor = scroll::half_page_up(self.cursor, self.viewport_height, |i| {
                    self.row_visual_height(i)
                });
            }
            Action::Top => self.cursor = 0,
            Action::Bottom => self.cursor = last,
            Action::NextHunk => self.jump_to(|row| matches!(row, RenderRow::HunkHeader { .. })),
            Action::PrevHunk => {
                self.jump_to_prev(|row| matches!(row, RenderRow::HunkHeader { .. }))
            }
            Action::NextFile => self.jump_to(|row| matches!(row, RenderRow::FileHeader { .. })),
            Action::PrevFile => {
                self.jump_to_prev(|row| matches!(row, RenderRow::FileHeader { .. }))
            }
            Action::ToggleSidebar => self.sidebar_visible = !self.sidebar_visible,
            Action::ToggleLayout => self.layout = self.layout.toggled(),
            Action::ToggleComments => self.comments_visible = !self.comments_visible,
            Action::NextSymbol => self.cycle_symbol(1),
            Action::PrevSymbol => self.cycle_symbol(-1),
            // `ui::mod`'s event loop intercepts all of these before they
            // reach here — each needs either the LSP manager, the
            // diagnostics store, the jump stack, or (for `AddComment`) the
            // repo root and comment store, none of which `App` owns — so
            // they're no-ops from `App`'s own point of view.
            // `ExpandFold`/`CollapseFold` join this bucket for the same
            // reason `AddComment` does: `ui::mod::handle_action` intercepts
            // both inside a `View::Diff`-only match before an action ever
            // reaches `App::update` (expanding needs `std::fs::read_to_string`,
            // which `App` doesn't do) — `App::expand_gap`/
            // `collapse_fold_at_cursor` are the real pure implementations,
            // called directly from there instead.
            //
            // `OpenSearch`/`NextMatch`/`PrevMatch` join it too, but for a
            // slightly different reason than the I/O ones above: opening
            // the prompt is ordinary in-memory state, and `next_match`/
            // `prev_match` are real `App` methods (unlike `expand_gap`'s
            // I/O) — but `ui::mod::handle_action` still needs to report a
            // "search wrapped" status note, which `App::update`'s `()`
            // return type has no way to carry, and to construct the prompt
            // overlay it owns, which `App` doesn't. Staying no-ops here
            // also means `TimelineView`'s nested `diff_app.update(action)`
            // fallthrough (see `timeline_view::TimelineView::update`) can
            // never leak real search navigation into its embedded diff
            // pane even if it were ever reached for these actions — see
            // `ui::mod::handle_action`'s `NextMatch`/`PrevMatch` arm for
            // the primary guard (a `View::Diff`-only gate) this backs up.
            Action::Hover
            | Action::Cancel
            | Action::GotoDefinition
            | Action::FindReferences
            | Action::NextDiagnostic
            | Action::PrevDiagnostic
            | Action::JumpBack
            | Action::JumpForward
            | Action::AddComment
            | Action::ExpandFold
            | Action::CollapseFold
            | Action::OpenSearch
            | Action::NextMatch
            | Action::PrevMatch
            | Action::Confirm => {}
            // `ui::mod` intercepts `ToggleTimeline`/`ToggleLogView`/
            // `OpenScopeMenu` before they reach here (constructing/tearing
            // down a `TimelineView`/`LogView`/the scope-menu popup isn't a
            // pure state transition); `ToggleRangeSelect` only means
            // something inside `TimelineView`/`LogView` themselves.
            Action::ToggleTimeline
            | Action::ToggleLogView
            | Action::ToggleLspInspector
            | Action::OpenScopeMenu
            | Action::ToggleRangeSelect => {}
            // `ui::mod` intercepts this before it reaches here too, same
            // bucket as `ToggleTimeline`/`ToggleLogView`/`OpenScopeMenu`
            // above — opening `ui::help`'s popup needs the live `Keymap` to
            // build its row list, which `App` doesn't own, and (unlike
            // those three) it opens from *any* view rather than gating on
            // `View::Diff` — see `Action::OpenHelp`'s docs.
            Action::OpenHelp => {}
            Action::Quit => self.should_quit = true,
        }
        if self.cursor != cursor_before {
            self.active_symbol = 0;
        }
        self.clamp_scroll();
    }

    /// The row index whose `(file, new_line)` matches `target_file`/
    /// `target_line` (0-based), if this diff has one — how a
    /// go-to-definition/references jump decides to move the cursor within
    /// the diff already being reviewed instead of pushing a new `FileView`
    /// on top of it. `target_file` is compared as an absolute path, the
    /// same coordinate space [`Self::hover_query`] reports.
    pub fn row_for_target(&self, target_file: &Path, target_line: u32) -> Option<usize> {
        self.rows.iter().position(|row| {
            let RenderRow::Line {
                file_idx,
                hunk_idx,
                row_idx,
            } = row
            else {
                return false;
            };
            let file = &self.files[*file_idx];
            let line_row = &file.hunks[*hunk_idx].rows[*row_idx];
            match lsp_target(line_row, file) {
                Some((relative, line)) => {
                    line == target_line && self.repo_root.join(&relative) == target_file
                }
                None => false,
            }
        })
    }

    /// Moves the cursor to `row_idx` and, if the active symbol at that row
    /// covers `display_col`, selects it — the destination of a
    /// go-to-definition/references jump, or of `]d`/`[d`. Centers the
    /// scroll offset (rather than the minimal nudge ordinary cursor
    /// movement uses) so a jump's destination lands with surrounding
    /// context visible, not pinned to the viewport's edge.
    pub fn jump_cursor_to(&mut self, row_idx: usize, display_col: usize) {
        self.cursor = row_idx.min(self.rows.len().saturating_sub(1));
        self.active_symbol = self
            .cursor_row_text()
            .map(symbols::scan)
            .and_then(|syms| {
                syms.iter()
                    .position(|s| s.display_start <= display_col && display_col < s.display_end)
            })
            .unwrap_or(0);
        self.scroll_offset = scroll::center(self.cursor, self.viewport_height, |i| {
            self.row_visual_height(i)
        });
        self.clamp_scroll();
    }

    /// Moves the cursor to the next (`forward`) or previous row whose
    /// `(file, new_line)` carries a diagnostic, wrapping around the end of
    /// the row list — `Action::NextDiagnostic`/`PrevDiagnostic`. A no-op
    /// when no row in the diff has one.
    pub fn jump_to_diagnostic(&mut self, diagnostics: &DiagnosticsStore, forward: bool) {
        let Some(target) = self.find_diagnostic_row(diagnostics, forward) else {
            return;
        };
        self.cursor = target;
        self.active_symbol = 0;
        self.scroll_offset = scroll::center(self.cursor, self.viewport_height, |i| {
            self.row_visual_height(i)
        });
        self.clamp_scroll();
    }

    fn find_diagnostic_row(&self, diagnostics: &DiagnosticsStore, forward: bool) -> Option<usize> {
        if self.rows.is_empty() {
            return None;
        }
        let has_diagnostic = |idx: usize| -> bool {
            let RenderRow::Line {
                file_idx,
                hunk_idx,
                row_idx,
            } = self.rows[idx]
            else {
                return false;
            };
            let file = &self.files[file_idx];
            let row = &file.hunks[hunk_idx].rows[row_idx];
            let Some((relative, line)) = lsp_target(row, file) else {
                return false;
            };
            diagnostics
                .severity_at(&self.repo_root.join(relative), line)
                .is_some()
        };

        let len = self.rows.len();
        (1..len)
            .map(|step| {
                if forward {
                    (self.cursor + step) % len
                } else {
                    (self.cursor + len - step) % len
                }
            })
            .find(|idx| has_diagnostic(*idx))
    }

    fn jump_to(&mut self, is_target: impl Fn(&RenderRow) -> bool) {
        if let Some(pos) = self
            .rows
            .iter()
            .enumerate()
            .skip(self.cursor + 1)
            .find(|(_, r)| is_target(r))
            .map(|(i, _)| i)
        {
            self.cursor = pos;
        }
    }

    fn jump_to_prev(&mut self, is_target: impl Fn(&RenderRow) -> bool) {
        if let Some(pos) = self.rows[..self.cursor]
            .iter()
            .enumerate()
            .rev()
            .find(|(_, r)| is_target(r))
            .map(|(i, _)| i)
        {
            self.cursor = pos;
        }
    }

    fn clamp_scroll(&mut self) {
        self.scroll_offset =
            scroll::clamp_scroll(self.cursor, self.viewport_height, self.scroll_offset, |i| {
                self.row_visual_height(i)
            });
    }

    /// `z o`'s pure half: expands the gap at `(file_idx, gap_idx)` using
    /// `file_lines` (the new side's on-disk content, already split via
    /// `.lines()` by the caller — see `ui::mod::handle_action`'s
    /// `ExpandFold` arm for why that split has to match the parser's own
    /// convention exactly). Returns an [`ExpandOutcome`] — see its docs for
    /// what each variant means and why a `bool` wasn't enough.
    ///
    /// No `capture_anchor`/`restore_anchor` here (unlike a watch refresh):
    /// a `Gap` row's anchor has no `new_line` to key off, so that machinery
    /// would degrade straight to the file-header fallback. Instead the
    /// cursor policy is exact and needs no search at all — before this
    /// call, `self.cursor` already indexes the `Gap` row being expanded
    /// (the caller found it there), and splicing a gap replaces exactly
    /// that one row with the gap's spliced-in rows *in place*, so every row
    /// before it keeps the same flat index it always had and the first
    /// spliced row lands at the exact index the `Gap` row used to occupy —
    /// `self.cursor` is already correct and never needs to move.
    pub fn expand_gap(
        &mut self,
        file_idx: usize,
        gap_idx: usize,
        file_lines: &[&str],
    ) -> ExpandOutcome {
        let Some(file) = self.files.get(file_idx) else {
            return ExpandOutcome::Rejected;
        };
        let Some(gap) = file_gaps(file).into_iter().nth(gap_idx) else {
            return ExpandOutcome::Rejected;
        };

        let new_end = match gap.new_end {
            Some(end) => end,
            None => {
                let disk_len = file_lines.len() as u32;
                if disk_len < gap.new_start {
                    // A file shorter than the gap expects *looks* like
                    // "genuinely nothing past the last hunk" from length
                    // alone — but a stale diff over a file that also
                    // happens to have been shortened looks identical.
                    // Validate the boundary row above the gap against disk
                    // first (the same check an ordinary bounded expand
                    // always runs below) before trusting that reading;
                    // on a mismatch this is drift, not EOF, and must not
                    // be recorded as a probed-empty gap.
                    if !gap_boundary_matches(file, &gap, file_lines) {
                        return ExpandOutcome::Rejected;
                    }
                    // The live file genuinely has nothing past the last
                    // hunk — record it so this trailing gap stops
                    // reappearing, and let the fold row disappear.
                    let display_path = file.display_path().to_owned();
                    self.trailing_probed_empty.insert(display_path);
                    self.rederive();
                    self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
                    self.clamp_scroll();
                    self.recompute_search();
                    return ExpandOutcome::ProbedEmpty;
                }
                disk_len
            }
        };

        if !gap_boundary_matches(file, &gap, file_lines) {
            return ExpandOutcome::Rejected;
        }

        let Some(texts) = (gap.new_start..=new_end)
            .map(|line| file_lines.get((line - 1) as usize).map(|s| (*s).to_owned()))
            .collect::<Option<Vec<String>>>()
        else {
            return ExpandOutcome::Rejected; // disk is shorter than the gap expects — stale
        };
        if context_rows_for_gap(&gap, &texts).is_none() {
            return ExpandOutcome::Rejected; // malformed offset — defensive, shouldn't happen
        }

        let display_path = file.display_path().to_owned();
        self.expanded_folds
            .entry(display_path)
            .or_default()
            .push(FoldRange {
                new_range: (gap.new_start, new_end),
                lines: texts,
            });
        self.rederive();
        self.clamp_scroll();
        self.recompute_search();
        ExpandOutcome::Revealed
    }

    /// `z c`'s pure half: folds back whichever expanded range the cursor
    /// currently sits inside, if any. `false` when the cursor isn't on a
    /// `Line` row belonging to one — including a row with no `new_line` at
    /// all (a `Del` row can appear inside a hunk an expand merged into, but
    /// never inside the spliced `Context` rows themselves, so this is
    /// defensive rather than reachable in practice).
    ///
    /// Cursor policy mirrors [`Self::expand_gap`]'s but the other direction:
    /// the reappearing `Gap` row lands at the flat index the fold's *first*
    /// line currently occupies (found before mutating anything below,
    /// since folding collapses the whole recorded range down to that one
    /// row) — not necessarily `self.cursor` itself, since the cursor can
    /// sit anywhere within an expanded range, not just its first row.
    pub fn collapse_fold_at_cursor(&mut self) -> bool {
        let Some(RenderRow::Line {
            file_idx,
            hunk_idx,
            row_idx,
        }) = self.rows.get(self.cursor).copied()
        else {
            return false;
        };
        let file = &self.files[file_idx];
        let Some(new_line) = file.hunks[hunk_idx].rows[row_idx].new_line else {
            return false;
        };
        let display_path = file.display_path().to_owned();
        let Some(ranges) = self.expanded_folds.get_mut(&display_path) else {
            return false;
        };
        let Some(pos) = ranges
            .iter()
            .position(|r| r.new_range.0 <= new_line && new_line <= r.new_range.1)
        else {
            return false;
        };
        let fold = ranges.remove(pos);
        if ranges.is_empty() {
            self.expanded_folds.remove(&display_path);
        }

        let fold_start = fold.new_range.0;
        // Same "find the flat row index whose new-side line matches a
        // target" search `refresh::restore_anchor` needs after a watch
        // refresh — `display_path` (already computed above) rather than
        // `file_idx` is an immaterial difference here, since both name the
        // same file.
        let target = refresh::find_exact_line(&self.files, &self.rows, &display_path, fold_start);

        self.rederive();
        if let Some(idx) = target {
            self.cursor = idx.min(self.rows.len().saturating_sub(1));
        }
        self.clamp_scroll();
        self.recompute_search();
        true
    }

    // ---- Issue #5: search ------------------------------------------------

    /// The live incremental prompt's per-keystroke recomputation: reruns
    /// [`search::compute_search`] for `query` against the rows as they
    /// stand *right now*, with `origin` (captured once, when `/` was first
    /// pressed — see `search::SearchPromptState`'s docs) resolved fresh via
    /// [`refresh::restore_anchor`] rather than trusted as a fixed row index,
    /// since a watch refresh can rebuild `rows` mid-prompt (see
    /// `ui::mod::handle_watch_refresh`'s `live_search` parameter). Jumps the
    /// cursor to whichever match the recompute selected as current (via
    /// [`Self::jump_cursor_to`], the exact function every search jump in
    /// this module funnels through) so the incremental preview updates live
    /// as the reviewer types; when there's nothing to jump to (an empty
    /// query, or a query that currently matches nothing), restores the
    /// cursor/scroll to `origin` instead — real vim's incsearch snaps the
    /// view back to the position `/` was pressed from the instant a pattern
    /// stops matching, rather than sticking wherever a previous,
    /// less-narrow prefix's preview happened to land the cursor (see
    /// [`Self::cancel_search`], which restores the same way for the
    /// "give up on the whole prompt" case, and [`search::compute_search`]'s
    /// own doc comment for the general claim this makes true even in the
    /// zero-match case).
    pub fn recompute_search_live(&mut self, query: &str, origin: &refresh::Anchor) {
        let origin_row = refresh::restore_anchor(&self.files, &self.rows, origin).row_index;
        self.search = search::compute_search(&self.files, &self.rows, query, origin_row);
        match self
            .search
            .as_ref()
            .and_then(|h| h.matches.get(h.current).copied())
        {
            Some(m) => {
                let display_col = self.display_col_for_match(m.row_idx, m.start);
                self.jump_cursor_to(m.row_idx, display_col);
            }
            None => {
                self.cursor = origin_row;
                self.scroll_offset = origin_row.saturating_sub(refresh::scroll_delta(origin));
                self.active_symbol = 0;
                self.clamp_scroll();
            }
        }
    }

    /// Esc in the prompt: restores the cursor/scroll to exactly where `/`
    /// was pressed (via `origin`, the same [`refresh::Anchor`]
    /// [`Self::recompute_search_live`] resolves against — see
    /// [`refresh::restore_anchor`]/[`refresh::scroll_delta`], the identical
    /// pair [`Self::apply_refresh`] uses to restore a watch refresh's
    /// cursor) and clears the highlight entirely — vim's own "cancel a
    /// search in progress" behavior, distinct from `Esc` in the *normal*
    /// view afterward, which only clears an already-*confirmed* search
    /// (see `ui::mod::handle_action`'s `Action::Cancel` handling, which
    /// reaches `App::update`'s ordinary cursor-movement path — clearing a
    /// confirmed search on plain `Esc` is `Self::clear_search`, not this).
    pub fn cancel_search(&mut self, origin: &refresh::Anchor) {
        let restored = refresh::restore_anchor(&self.files, &self.rows, origin);
        self.cursor = restored.row_index;
        self.scroll_offset = restored
            .row_index
            .saturating_sub(refresh::scroll_delta(origin));
        self.active_symbol = 0;
        self.clamp_scroll();
        self.search = None;
    }

    /// Enter in the prompt: locks the current live preview in as the
    /// confirmed search (already sitting in `self.search`, current match
    /// already jumped to by the last [`Self::recompute_search_live`] call —
    /// there's nothing more to *do* here beyond deciding what to report).
    /// `query` is the prompt's own current text, checked directly rather
    /// than inferred from `self.search.is_none()`: `self.search` only
    /// becomes `None` via a `recompute_search_live("")` call, which never
    /// happens for a prompt confirmed the instant it opens (Enter with
    /// nothing typed goes straight from `SearchPromptOutcome::Confirm` to
    /// here with no intervening `Continue`) — so if a search was already
    /// confirmed *before* this `/` press, `self.search` would still hold
    /// that stale query, and checking it instead of `query` would silently
    /// reconfirm the stale search rather than cancelling the empty prompt
    /// the way `Esc` would. An empty `query` confirms into the same outcome
    /// `Esc` would, restoring the origin rather than leaving a query-less
    /// "confirmed search" — or a stale prior one — that doesn't reflect
    /// anything the reviewer just typed. Returns the status-bar note the
    /// caller should show: `Some("no matches: …")` for a real, zero-hit
    /// query, `None` otherwise (matches found and already highlighted, or
    /// nothing was typed at all).
    pub fn confirm_search(&mut self, query: &str, origin: &refresh::Anchor) -> Option<String> {
        if query.is_empty() {
            self.cancel_search(origin);
            return None;
        }
        let Some(highlight) = &self.search else {
            return None;
        };
        if highlight.matches.is_empty() {
            return Some(format!("no matches: {}", highlight.query));
        }
        None
    }

    /// `n`/`N`'s real navigation (`ui::mod::handle_action`'s
    /// `Action::NextMatch`/`PrevMatch` arm calls these — see its docs on
    /// why the logic lives here and not in [`Self::update`]). `None` when
    /// there's no confirmed search *or* it currently has zero matches (see
    /// [`Self::search_status_note`] for the status-bar text either case
    /// gets); `Some(wrapped)` otherwise, where `wrapped` is whether this
    /// step crossed either end of the match list — see [`search::step`]'s
    /// docs on why a single-match search always reports `true`.
    pub fn next_match(&mut self) -> Option<bool> {
        self.jump_to_match(true)
    }

    pub fn prev_match(&mut self) -> Option<bool> {
        self.jump_to_match(false)
    }

    fn jump_to_match(&mut self, forward: bool) -> Option<bool> {
        let (len, current) = {
            let highlight = self.search.as_ref()?;
            (highlight.matches.len(), highlight.current)
        };
        let (next, wrapped) = search::step(current, len, forward)?;
        let m = self.search.as_ref().unwrap().matches[next];
        let highlight = self.search.as_mut().unwrap();
        highlight.current = next;
        // Real vim re-enables `:nohlsearch`-suppressed highlighting the
        // instant you search again — `n`/`N` count as "searching again"
        // just as much as a fresh `/` does, so a step here un-suppresses
        // it too (see `SearchHighlight::highlight_visible`'s docs and
        // `Self::clear_search`, the only place that ever sets it `false`).
        highlight.highlight_visible = true;
        let display_col = self.display_col_for_match(m.row_idx, m.start);
        self.jump_cursor_to(m.row_idx, display_col);
        Some(wrapped)
    }

    /// The status-bar note `n`/`N` show when [`Self::next_match`]/
    /// [`Self::prev_match`] return `None`: distinguishes "there's no
    /// confirmed search to repeat at all" from "there's a confirmed query,
    /// but it currently matches nothing" (the case
    /// `Self::recompute_search`'s docs describe — a fold toggle or refresh
    /// can shrink a confirmed search's matches down to zero without
    /// clearing the query itself).
    pub fn search_status_note(&self) -> String {
        match &self.search {
            Some(highlight) => format!("no matches: {}", highlight.query),
            None => "search: nothing to repeat".to_owned(),
        }
    }

    /// `Esc` in the *normal* diff view (not the prompt — see
    /// [`Self::cancel_search`]): vim's `:noh`, which only *suppresses* the
    /// active search's highlight without moving the cursor — the confirmed
    /// query, its matches, and `current` all stay exactly as they were (see
    /// [`search::SearchHighlight::highlight_visible`]'s docs), so `n`/`N`
    /// keep working immediately afterward, matching real vim's
    /// `:nohlsearch` (the pattern lives on in the search register; only the
    /// drawing stops). [`Self::jump_to_match`] flips the flag back to
    /// `true` the moment `n`/`N` jump again, since vim re-enables
    /// highlighting the instant you search again too. A no-op with nothing
    /// active.
    pub fn clear_search(&mut self) {
        if let Some(highlight) = &mut self.search {
            highlight.highlight_visible = false;
        }
    }

    /// The display column `byte_offset` (a [`search::Match`]'s `start`/`end`
    /// — into `row_idx`'s raw, unwrapped text) converts to, via
    /// [`ColumnMap::utf8_to_display`] — the byte→display leg of the
    /// conversion pipeline described in [`search::Match`]'s own docs.
    /// `0` for a `row_idx` that isn't a [`RenderRow::Line`] — shouldn't
    /// happen, since every [`search::Match`] is only ever built from one
    /// (see [`search::compute_matches`]), but this is cursor-jump code, not
    /// a place to panic over a stale index.
    fn display_col_for_match(&self, row_idx: usize, byte_offset: usize) -> usize {
        let Some(RenderRow::Line {
            file_idx,
            hunk_idx,
            row_idx: line_idx,
        }) = self.rows.get(row_idx).copied()
        else {
            return 0;
        };
        let text = &self.files[file_idx].hunks[hunk_idx].rows[line_idx].text;
        ColumnMap::new(text).utf8_to_display(byte_offset)
    }

    /// Recomputes a *confirmed* search's matches against `self.files`/
    /// `self.rows` as they stand right now, keeping the same `query` and
    /// moving `current` to the nearest match at-or-after `self.cursor` (via
    /// [`search::nearest_match_index`] — the same "which match is closest"
    /// rule [`search::compute_search`]'s origin selection uses). Called at
    /// the end of every method that rebuilds rows out from under a possibly-
    /// active confirmed search — [`Self::apply_refresh`], [`Self::expand_gap`]
    /// (both outcomes that actually rederive), [`Self::collapse_fold_at_cursor`]
    /// — each *after* `self.cursor` has already landed at its own final
    /// position for that operation, so "nearest to the cursor" means the
    /// cursor a reviewer actually sees once the redraw happens, not a stale
    /// pre-rebuild one. Deliberately does *not* move the cursor itself (unlike
    /// [`Self::recompute_search_live`]) — a fold toggle or a background watch
    /// refresh recomputing which match `n`/`N` would land on next shouldn't
    /// also yank the cursor there uninvited; the reviewer sees the updated
    /// highlight set on the next redraw and drives `n`/`N` themselves.
    /// [`Self::apply_scope_swap`] doesn't call this — it clears `self.search`
    /// outright instead (see its own docs). Carries `highlight_visible`
    /// over from the existing highlight unchanged: a fold toggle or watch
    /// refresh that happens to land mid-`:noh` (see [`Self::clear_search`])
    /// shouldn't quietly re-show a highlight the reviewer explicitly
    /// suppressed. A no-op with no confirmed search active.
    fn recompute_search(&mut self) {
        let Some(existing) = &self.search else {
            return;
        };
        let query = existing.query.clone();
        let highlight_visible = existing.highlight_visible;
        let matches = search::compute_matches(&self.files, &self.rows, &query);
        let current = search::nearest_match_index(&matches, self.cursor);
        self.search = Some(search::SearchHighlight {
            query,
            matches,
            current,
            highlight_visible,
        });
    }
}

/// The file whose [`DiffFile::display_path`] is `display_path`, if any —
/// shared by [`App::prune_stale_folds`]'s two retain passes so both look a
/// path up the same way.
fn find_file<'a>(files: &'a [DiffFile], display_path: &str) -> Option<&'a DiffFile> {
    files.iter().find(|f| f.display_path() == display_path)
}

/// Whether a previously recorded [`FoldRange`] still safely reapplies
/// against a freshly re-parsed `new_file`, per [`App::prune_stale_folds`]'s
/// docs: the new parse must still have a gap starting at exactly the fold's
/// recorded start and ending at exactly its recorded end (an unbounded
/// trailing gap always accepts, regardless of the fold's recorded end,
/// since any previously-expanded trailing length still fits an "at least
/// this far and possibly further" gap), and the boundary rows on whichever
/// side(s) the gap has one must read identically in `old_file` and
/// `new_file`.
fn fold_still_fits(old_file: &DiffFile, new_file: &DiffFile, fold: &FoldRange) -> bool {
    let Some(old_gap) = file_gaps(old_file)
        .into_iter()
        .find(|g| g.new_start == fold.new_range.0)
    else {
        return false;
    };
    let Some(new_gap) = file_gaps(new_file)
        .into_iter()
        .find(|g| g.new_start == fold.new_range.0)
    else {
        return false;
    };
    let bounds_match = match new_gap.new_end {
        Some(end) => end == fold.new_range.1,
        None => true,
    };
    bounds_match && gap_boundaries_equal(old_file, &old_gap, new_file, &new_gap)
}

/// Compares a [`Gap`]'s boundary rows (see
/// [`crate::diff::gap_adjacent_line`]) between two parses of the same file
/// — `old_gap`'s neighbors in `old_file` against `new_gap`'s neighbors in
/// `new_file`. Equal (including "neither side has a boundary row") means
/// the content immediately around the gap didn't actually change, so
/// reapplying a fold recorded against `old_file` there is still safe;
/// anything else — a boundary row's text changed, or one parse has a
/// boundary row the other doesn't — is treated as drift.
fn gap_boundaries_equal(
    old_file: &DiffFile,
    old_gap: &Gap,
    new_file: &DiffFile,
    new_gap: &Gap,
) -> bool {
    use crate::diff::{GapSide, gap_adjacent_line};
    [GapSide::Above, GapSide::Below].into_iter().all(|side| {
        let old = gap_adjacent_line(old_file, old_gap.position, side);
        let new = gap_adjacent_line(new_file, new_gap.position, side);
        match (old, new) {
            (Some((_, ot)), Some((_, nt))) => ot == nt,
            (None, None) => true,
            _ => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse_unified_diff;

    const FIXTURE: &str = include_str!("../diff/fixtures/multi_file.diff");

    fn test_app() -> App {
        let files = parse_unified_diff(FIXTURE);
        let mut app = App::new("test-repo".to_owned(), PathBuf::from("/repo"), files);
        app.set_viewport_height(10);
        app
    }

    #[test]
    fn cursor_down_and_up_stay_in_bounds() {
        let mut app = test_app();
        app.update(Action::CursorUp);
        assert_eq!(app.cursor, 0, "cannot go above the first row");
        app.update(Action::Bottom);
        let last = app.rows.len() - 1;
        app.update(Action::CursorDown);
        assert_eq!(app.cursor, last, "cannot go below the last row");
    }

    #[test]
    fn top_and_bottom_jump_to_extremes() {
        let mut app = test_app();
        app.update(Action::Bottom);
        assert_eq!(app.cursor, app.rows.len() - 1);
        app.update(Action::Top);
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn next_file_visits_each_file_header_in_order() {
        let mut app = test_app();
        let mut visited_files = Vec::new();
        loop {
            let before = app.cursor;
            app.update(Action::NextFile);
            if app.cursor == before {
                break; // no more files ahead; NextFile is a no-op at the last file
            }
            match app.rows[app.cursor] {
                RenderRow::FileHeader { file_idx } => visited_files.push(file_idx),
                _ => break,
            }
        }
        assert_eq!(visited_files, vec![1, 2, 3, 4]);
    }

    #[test]
    fn toggle_sidebar_flips_visibility() {
        let mut app = test_app();
        assert!(app.sidebar_visible);
        app.update(Action::ToggleSidebar);
        assert!(!app.sidebar_visible);
    }

    #[test]
    fn toggle_layout_flips_between_unified_and_side_by_side() {
        let mut app = test_app();
        assert_eq!(app.layout, Layout::Unified);
        app.update(Action::ToggleLayout);
        assert_eq!(app.layout, Layout::SideBySide);
        app.update(Action::ToggleLayout);
        assert_eq!(app.layout, Layout::Unified);
    }

    #[test]
    fn quit_sets_should_quit() {
        let mut app = test_app();
        assert!(!app.should_quit);
        app.update(Action::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn next_symbol_cycles_and_wraps_and_resets_when_cursor_moves() {
        let mut app = test_app();
        app.update(Action::Top);
        app.update(Action::CursorDown); // row 1: the hunk header
        app.update(Action::CursorDown); // row 2: "fn helper() {}" — two symbols

        assert_eq!(app.active_symbol, 0);
        app.update(Action::NextSymbol);
        assert_eq!(app.active_symbol, 1);
        app.update(Action::NextSymbol);
        assert_eq!(
            app.active_symbol, 0,
            "cycling past the last symbol wraps around"
        );
        app.update(Action::PrevSymbol);
        assert_eq!(
            app.active_symbol, 1,
            "cycling before the first symbol wraps backward"
        );

        app.update(Action::CursorDown);
        assert_eq!(
            app.active_symbol, 0,
            "moving to a new row resets the active symbol"
        );
    }

    #[test]
    fn hover_query_targets_the_active_symbol_on_an_add_row() {
        let mut app = test_app();
        app.update(Action::Top);
        for _ in 0..4 {
            app.update(Action::CursorDown);
        }
        // Row 4 is the "+fn new_name() {}" add line in src/lib.rs's hunk.
        let query = app.hover_query().expect("add row is hover-eligible");
        assert_eq!(query.file, PathBuf::from("/repo/src/lib.rs"));
        assert_eq!(query.line, 1); // new_line 2, 0-based
        assert_eq!(query.line_text, "fn new_name() {}");
        assert_eq!(query.display_col, 0); // "fn", the first symbol

        app.update(Action::NextSymbol);
        let query = app.hover_query().expect("still on the same row");
        assert_eq!(query.display_col, 3); // "new_name"
    }

    #[test]
    fn hover_query_is_none_on_a_del_row() {
        let mut app = test_app();
        app.update(Action::Top);
        for _ in 0..3 {
            app.update(Action::CursorDown);
        }
        // Row 3 is the "-fn old_name() {}" del line — nothing to hover.
        assert_eq!(app.hover_query(), None);
    }

    #[test]
    fn hover_query_is_none_on_a_non_interactive_app_even_on_an_eligible_row() {
        let mut app = test_app();
        app.interactive = false;
        app.update(Action::Top);
        for _ in 0..4 {
            app.update(Action::CursorDown);
        }
        // Same otherwise-hover-eligible add row as
        // `hover_query_targets_the_active_symbol_on_an_add_row` — only
        // `interactive` differs.
        assert_eq!(app.hover_query(), None);
    }

    #[test]
    fn scroll_follows_cursor_past_viewport_bottom() {
        let mut app = test_app();
        app.set_viewport_height(3);
        app.update(Action::Bottom);
        assert!(app.cursor >= app.scroll_offset);
        assert!(app.cursor < app.scroll_offset + 3);
    }

    /// A one-hunk file whose middle line is much longer than the others —
    /// the fixture the wrap-aware scroll tests below build an `App` from,
    /// since [`FIXTURE`] has no line wide enough to ever wrap.
    fn app_with_a_wrapping_line(long_line_len: usize) -> App {
        use crate::diff::{DiffFile, DiffHunk, DiffLineKind, DiffRow};

        let row = |text: &str, n: u32| DiffRow {
            kind: DiffLineKind::Context,
            text: text.to_owned(),
            old_line: Some(n),
            new_line: Some(n),
        };
        let file = DiffFile {
            old_path: Some("f.rs".to_owned()),
            new_path: Some("f.rs".to_owned()),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 3,
                new_start: 1,
                new_lines: 3,
                header: String::new(),
                // Not testing fold rows here — no trailing gap, so wrap/
                // scroll math tests below don't have to account for one.
                known_eof: true,
                rows: vec![
                    row("short one", 1),
                    row(&"x".repeat(long_line_len), 2),
                    row("short two", 3),
                ],
            }],
            ..Default::default()
        };
        App::new("repo".to_owned(), PathBuf::from("/repo"), vec![file])
    }

    #[test]
    fn row_visual_height_reflects_wrapping_at_the_current_content_width() {
        let mut app = app_with_a_wrapping_line(100);
        app.set_viewport_height(10);
        app.set_content_width(40);

        // Row 0 is the file header, row 1 the hunk header, row 2 "short
        // one", row 3 the 100-column line, row 4 "short two".
        assert_eq!(app.row_visual_height(0), 1, "headers never wrap");
        assert_eq!(app.row_visual_height(2), 1);
        assert_eq!(
            app.row_visual_height(3),
            3,
            "100 columns at a width of 40 wraps into 3 visual rows"
        );
        assert_eq!(app.row_visual_height(4), 1);
    }

    #[test]
    fn row_visual_height_is_always_one_when_content_width_was_never_set() {
        // Before the first frame reports a real pane width, `content_width`
        // defaults to effectively unbounded — matching plain, unwrapped
        // rendering rather than wrapping against a width of 0.
        let app = app_with_a_wrapping_line(500);
        assert_eq!(app.row_visual_height(3), 1);
    }

    #[test]
    fn clamp_scroll_pulls_the_offset_forward_to_keep_a_wrapped_cursor_row_visible() {
        let mut app = app_with_a_wrapping_line(100);
        app.set_viewport_height(3);
        app.set_content_width(40); // the long row wraps into 3 visual rows
        app.update(Action::Top);

        // Rows 0-2 (file header, hunk header, "short one") are 1 visual row
        // each — the offset has no reason to move while the cursor is
        // still among them, even in a 3-row viewport.
        app.update(Action::CursorDown);
        app.update(Action::CursorDown);
        assert_eq!(app.cursor, 2);
        assert_eq!(
            app.scroll_offset, 0,
            "three ordinary 1-row lines fit a 3-row viewport with no scrolling"
        );

        // Row 3 (the 100-column line) wraps into exactly 3 visual rows —
        // as wide as the whole viewport on its own. A uniform-height
        // `clamp_scroll` would have left the offset at 0 (only 3 *logical*
        // rows precede the cursor); the wrap-aware version must instead
        // scroll all the way to the cursor's own row, since rows 0-2 no
        // longer fit alongside it.
        app.update(Action::CursorDown);
        assert_eq!(app.cursor, 3);
        assert_eq!(app.scroll_offset, 3);
    }

    #[test]
    fn apply_refresh_with_an_identical_diff_keeps_the_cursor_in_place_and_reports_survival() {
        let mut app = test_app();
        app.update(Action::Top);
        for _ in 0..4 {
            app.update(Action::CursorDown);
        }
        let cursor_before = app.cursor;

        let same_files = parse_unified_diff(FIXTURE);
        let survived = app.apply_refresh(same_files);

        assert!(
            survived,
            "identical content at the same position must survive"
        );
        assert_eq!(app.cursor, cursor_before);
    }

    #[test]
    fn apply_scope_swap_resets_cursor_to_top_and_sets_interactive_and_label() {
        let mut app = test_app();
        app.update(Action::Bottom);
        assert_ne!(app.cursor, 0, "sanity: cursor moved away from the top");

        let files = parse_unified_diff(FIXTURE);
        app.apply_scope_swap(files, false, false, Some("r: deadbeef".to_owned()));

        assert_eq!(app.cursor, 0, "a scope swap always resets to the top");
        assert_eq!(app.scroll_offset, 0);
        assert!(!app.interactive);
        assert_eq!(app.scope_label.as_deref(), Some("r: deadbeef"));
    }

    #[test]
    fn apply_scope_swap_to_working_tree_clears_interactive_and_label() {
        let mut app = test_app();
        app.interactive = false;
        app.scope_label = Some("r: old".to_owned());

        let files = parse_unified_diff(FIXTURE);
        app.apply_scope_swap(files, true, true, None);

        assert!(app.interactive);
        assert_eq!(app.scope_label, None);
    }

    #[test]
    fn apply_scope_swap_on_an_empty_diff_resets_cursor_and_scroll_without_panicking() {
        let mut app = test_app();
        app.update(Action::Bottom);
        app.apply_scope_swap(Vec::new(), true, true, None);
        assert_eq!(app.cursor, 0);
        assert_eq!(app.scroll_offset, 0);
        assert!(app.rows.is_empty());
    }

    #[test]
    fn apply_refresh_on_an_empty_diff_resets_cursor_and_scroll_without_panicking() {
        let mut app = test_app();
        app.update(Action::Bottom);
        let survived = app.apply_refresh(Vec::new());
        assert!(!survived);
        assert_eq!(app.cursor, 0);
        assert_eq!(app.scroll_offset, 0);
        assert!(app.rows.is_empty());
    }

    // -- Issue #5: search ---------------------------------------------------

    #[test]
    fn clear_search_suppresses_the_highlight_without_discarding_the_query_vims_noh() {
        let mut app = test_app();
        let origin = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);
        // "fn " matches several rows — enough for `next_match` below to
        // have somewhere real to jump.
        app.recompute_search_live("fn ", &origin);
        assert!(app.search.is_some(), "sanity: a search is active");
        let matches_before = app.search.as_ref().unwrap().matches.clone();
        let current_before = app.search.as_ref().unwrap().current;

        app.clear_search();
        let highlight = app.search.as_ref().expect(
            "a bare Esc's :noh must only suppress the highlight, not discard the confirmed query",
        );
        assert!(!highlight.highlight_visible);
        assert_eq!(highlight.matches, matches_before);
        assert_eq!(highlight.current, current_before);

        // Real vim's `n`/`N` keep working right after a bare `:noh` — the
        // pattern stays live in the search register — and re-show the
        // highlight the instant you search again.
        let cursor_before = app.cursor;
        assert!(
            app.next_match().is_some(),
            "n must still work after :noh, unlike an actually-cleared search"
        );
        assert_ne!(app.cursor, cursor_before, "n must actually jump");
        assert!(
            app.search.as_ref().unwrap().highlight_visible,
            "n must re-enable the highlight, mirroring vim's auto-restore-on-repeat-search"
        );
    }

    #[test]
    fn clear_search_is_a_no_op_with_nothing_active() {
        let mut app = test_app();
        app.clear_search(); // must not panic
        assert!(app.search.is_none());
    }

    #[test]
    fn apply_scope_swap_clears_a_noh_suppressed_search_too() {
        let mut app = test_app();
        let origin = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);
        app.recompute_search_live("new_name", &origin);
        app.clear_search();
        assert!(
            !app.search.as_ref().unwrap().highlight_visible,
            "sanity: the search is confirmed but its highlight is suppressed"
        );

        let files = parse_unified_diff(FIXTURE);
        app.apply_scope_swap(files, true, true, None);

        assert!(
            app.search.is_none(),
            "a scope swap clears a :noh-suppressed search just as completely as a visible one"
        );
    }

    #[test]
    fn recompute_search_live_jumps_to_the_first_match_at_or_after_the_origin() {
        let mut app = test_app();
        app.update(Action::Bottom); // origin near the end of the diff
        let origin = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);

        // "added_one"/"added_two" (src/new_module.rs) sit *before* the
        // bottom of the diff — with the origin at the very end, every match
        // is before it, so the incremental jump wraps to the first one.
        app.recompute_search_live("added_", &origin);
        let highlight = app.search.as_ref().expect("query is non-empty");
        assert_eq!(highlight.matches.len(), 2);
        assert_eq!(highlight.current, 0);
        assert_eq!(app.cursor, highlight.matches[0].row_idx);
    }

    /// vim incsearch parity: once a query narrows to zero matches, the
    /// cursor/scroll must snap back to exactly where `/` was pressed, not
    /// stay wherever a previous, less-narrow prefix's preview happened to
    /// land them (see `Self::recompute_search_live`'s docs).
    #[test]
    fn recompute_search_live_with_zero_matches_restores_the_cursor_and_scroll_to_the_origin() {
        let mut app = test_app();
        app.update(Action::Top);
        let cursor_before = app.cursor;
        let scroll_before = app.scroll_offset;
        let origin = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);

        // A shorter prefix matches and jumps the cursor away from the
        // origin first — the exact stale position the zero-match narrowing
        // below must undo.
        app.recompute_search_live("new_name", &origin);
        assert_ne!(
            app.cursor, cursor_before,
            "sanity: the incremental jump actually moved the cursor"
        );

        app.recompute_search_live("new_name_that_matches_nothing", &origin);
        let highlight = app
            .search
            .as_ref()
            .expect("a real, zero-hit query still returns a highlight");
        assert!(highlight.matches.is_empty());
        assert_eq!(
            app.cursor, cursor_before,
            "zero matches must snap the cursor back to the origin, not leave it at the stale jump"
        );
        assert_eq!(app.scroll_offset, scroll_before);
    }

    #[test]
    fn recompute_search_live_with_an_empty_query_clears_the_search() {
        let mut app = test_app();
        let origin = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);
        app.recompute_search_live("new_name", &origin);
        assert!(app.search.is_some());

        app.recompute_search_live("", &origin);
        assert!(app.search.is_none());
    }

    #[test]
    fn cancel_search_restores_the_pre_search_cursor_and_scroll_and_clears_the_highlight() {
        let mut app = test_app();
        app.update(Action::Top);
        let cursor_before = app.cursor;
        let scroll_before = app.scroll_offset;
        let origin = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);

        // Typing narrows the query and jumps the cursor away from the
        // origin — the exact thing `cancel_search` has to undo.
        app.recompute_search_live("added_one", &origin);
        assert_ne!(
            app.cursor, cursor_before,
            "sanity: the incremental jump actually moved the cursor"
        );

        app.cancel_search(&origin);
        assert_eq!(app.cursor, cursor_before);
        assert_eq!(app.scroll_offset, scroll_before);
        assert!(app.search.is_none());
    }

    #[test]
    fn confirm_search_reports_no_matches_for_a_query_present_nowhere() {
        let mut app = test_app();
        let origin = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);
        app.recompute_search_live("xyz_not_anywhere", &origin);

        let note = app.confirm_search("xyz_not_anywhere", &origin);
        assert_eq!(note.as_deref(), Some("no matches: xyz_not_anywhere"));
        // The query stays confirmed (so a later fold/refresh recompute has
        // something to re-check) even with zero matches right now.
        assert!(app.search.is_some());
    }

    #[test]
    fn confirm_search_with_matches_reports_no_note_and_keeps_the_highlight() {
        let mut app = test_app();
        let origin = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);
        app.recompute_search_live("new_name", &origin);

        let note = app.confirm_search("new_name", &origin);
        assert_eq!(note, None);
        assert!(app.search.is_some());
    }

    #[test]
    fn confirm_search_with_an_empty_query_cancels_instead_of_confirming() {
        let mut app = test_app();
        app.update(Action::Top);
        let cursor_before = app.cursor;
        let origin = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);

        let note = app.confirm_search("", &origin);
        assert_eq!(note, None);
        assert!(app.search.is_none());
        assert_eq!(app.cursor, cursor_before);
    }

    /// The regression this signature is for: `confirm_search` must decide
    /// "was anything typed this prompt session" from the prompt's own text,
    /// not from `self.search.is_none()` — reopening `/` and hitting `Enter`
    /// immediately (nothing typed) must cancel, exactly like `Esc`, even
    /// though `self.search` still holds a completely unrelated *prior*
    /// confirmed search from before this `/` press (see
    /// `Self::confirm_search`'s docs for why `self.search.is_none()` alone
    /// can't tell the two cases apart).
    #[test]
    fn confirm_search_with_an_empty_query_cancels_even_with_a_prior_confirmed_search() {
        let mut app = test_app();
        app.update(Action::Top);
        let origin1 = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);
        app.recompute_search_live("new_name", &origin1);
        app.confirm_search("new_name", &origin1);
        assert!(app.search.is_some(), "sanity: a search is confirmed");

        // Reopening `/` and pressing Enter with nothing typed this time —
        // `self.search` is still `Some("new_name")` at this point, exactly
        // as it would be through the real event loop (see
        // `ui::mod`'s `Action::OpenSearch`/`SearchPromptOutcome::Confirm`
        // handling, which never touches `app.search` before Enter).
        let cursor_before = app.cursor;
        let origin2 = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);
        let note = app.confirm_search("", &origin2);

        assert_eq!(note, None);
        assert!(
            app.search.is_none(),
            "an empty query must cancel, not silently reconfirm the stale prior search"
        );
        assert_eq!(app.cursor, cursor_before);
    }

    #[test]
    fn next_and_prev_match_cycle_and_wrap_moving_the_cursor_each_time() {
        let mut app = test_app();
        let origin = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);
        // "fn " matches every function definition in the fixture — several
        // hits across several files, plenty to exercise wraparound.
        app.recompute_search_live("fn ", &origin);
        let total = app.search.as_ref().unwrap().matches.len();
        assert!(total > 2, "sanity: enough matches to exercise wraparound");
        assert_eq!(
            app.search.as_ref().unwrap().current,
            0,
            "sanity: origin at the top lands on the first match"
        );

        for step in 0..total - 1 {
            let wrapped = app.next_match();
            assert_eq!(wrapped, Some(false), "step {step} should not wrap yet");
            assert_eq!(app.search.as_ref().unwrap().current, step + 1);
        }
        // One more step returns to the first match, wrapping — and the
        // cursor jumps right along with it.
        assert_eq!(app.next_match(), Some(true));
        assert_eq!(app.search.as_ref().unwrap().current, 0);
        let first_match_row = app.search.as_ref().unwrap().matches[0].row_idx;
        assert_eq!(app.cursor, first_match_row);

        // `prev_match` immediately after wraps the other way, back to the
        // last match.
        assert_eq!(app.prev_match(), Some(true));
        assert_eq!(app.search.as_ref().unwrap().current, total - 1);
    }

    #[test]
    fn next_match_with_no_confirmed_search_is_a_no_op() {
        let mut app = test_app();
        assert_eq!(app.next_match(), None);
        assert_eq!(app.prev_match(), None);
    }

    #[test]
    fn search_status_note_distinguishes_no_search_from_zero_matches() {
        let mut app = test_app();
        assert_eq!(app.search_status_note(), "search: nothing to repeat");

        let origin = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);
        app.recompute_search_live("xyz_not_anywhere", &origin);
        assert_eq!(app.search_status_note(), "no matches: xyz_not_anywhere");
    }

    #[test]
    fn apply_scope_swap_clears_an_active_confirmed_search() {
        let mut app = test_app();
        let origin = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);
        app.recompute_search_live("new_name", &origin);
        assert!(app.search.is_some(), "sanity: a search is active");

        let files = parse_unified_diff(FIXTURE);
        app.apply_scope_swap(files, true, true, None);

        assert!(
            app.search.is_none(),
            "a scope swap has no relationship to the old scope's search"
        );
    }

    #[test]
    fn apply_refresh_recomputes_a_confirmed_searchs_matches_against_the_new_rows() {
        let mut app = test_app();
        let origin = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);
        app.recompute_search_live("new_name", &origin);
        let before = app.search.as_ref().unwrap().matches.len();
        assert_eq!(before, 2, "sanity: \"new_name\" and \"new_name2\"'s prefix");

        // A refreshed diff that adds a third occurrence — the confirmed
        // query survives the rebuild and its match count grows to match.
        // Continuation lines start at column 0 (not indented to match the
        // surrounding Rust code): a `\`-newline strips leading whitespace
        // on the following line too, which would otherwise eat a context
        // line's own leading-space marker — see `main.rs`'s `GAP_FIXTURE`
        // for the same convention.
        const REFRESHED: &str = "diff --git a/src/lib.rs b/src/lib.rs\n\
index 1111111..2222222 100644\n\
--- a/src/lib.rs\n\
+++ b/src/lib.rs\n\
@@ -1,4 +1,6 @@\n\
 fn helper() {}\n\
-fn old_name() {}\n\
+fn new_name() {}\n\
+fn new_name2() {}\n\
+fn new_name3() {}\n\
 fn tail() {}\n\
 fn unchanged() {}\n";
        app.apply_refresh(parse_unified_diff(REFRESHED));

        let after = app.search.as_ref().expect("query survives a refresh");
        assert_eq!(after.query, "new_name");
        assert_eq!(after.matches.len(), 3);
    }

    // -- Fold rows: expand/collapse, validation, refresh reapply --------

    /// One file, two hunks (new 1..=3, new 10..=12), every row `Context`
    /// with matching old/new line numbers (offset 0 throughout) — a gap
    /// between them (new 4..=9, 6 lines) and an unknown-size trailing gap
    /// after the second hunk. Every fold test below expands/collapses
    /// against this same shape.
    fn gap_fixture_file() -> DiffFile {
        use crate::diff::{DiffHunk, DiffLineKind, DiffRow};
        let mk_hunk = |start: u32, lines: u32| DiffHunk {
            old_start: start,
            old_lines: lines,
            new_start: start,
            new_lines: lines,
            header: String::new(),
            known_eof: false,
            rows: (0..lines)
                .map(|i| DiffRow {
                    kind: DiffLineKind::Context,
                    text: format!("line {}", start + i),
                    old_line: Some(start + i),
                    new_line: Some(start + i),
                })
                .collect(),
        };
        DiffFile {
            old_path: Some("f.txt".to_owned()),
            new_path: Some("f.txt".to_owned()),
            hunks: vec![mk_hunk(1, 3), mk_hunk(10, 3)],
            ..Default::default()
        }
    }

    /// The full 12-line file `gap_fixture_file`'s diff only partially
    /// shows — what `expand_gap` would read off "disk."
    fn gap_fixture_disk_lines() -> Vec<String> {
        (1..=12).map(|n| format!("line {n}")).collect()
    }

    /// `gap_fixture_file` as a live `App`: rows are `[FileHeader,
    /// HunkHeader, line1, line2, line3, Gap(between), HunkHeader, line10,
    /// line11, line12, Gap(trailing)]` — cursor `5` is the between-hunks
    /// gap, `10` the trailing one. `disk_is_new_side` is on, matching the
    /// only scope `z o` is ever reachable from.
    fn gap_fixture_app() -> App {
        let mut app = App::new(
            "repo".to_owned(),
            PathBuf::from("/repo"),
            vec![gap_fixture_file()],
        );
        app.set_viewport_height(30);
        app.disk_is_new_side = true;
        app
    }

    fn line_text_at(app: &App, idx: usize) -> &str {
        match app.rows[idx] {
            RenderRow::Line {
                file_idx,
                hunk_idx,
                row_idx,
            } => &app.files[file_idx].hunks[hunk_idx].rows[row_idx].text,
            other => panic!("expected a Line row at {idx}, got {other:?}"),
        }
    }

    #[test]
    fn expand_then_collapse_round_trips_the_cursor_back_to_the_gap_row() {
        let mut app = gap_fixture_app();
        app.cursor = 5; // the Between gap
        let disk = gap_fixture_disk_lines();
        let refs: Vec<&str> = disk.iter().map(String::as_str).collect();
        let rows_before = app.rows.len();

        assert_eq!(app.expand_gap(0, 0, &refs), ExpandOutcome::Revealed);
        // +6 new content lines, -1 for the consumed `Gap` row, -1 for the
        // second hunk's own `HunkHeader` (the two hunks merge into one —
        // see `splice_gap`'s `Between` arm).
        assert_eq!(app.rows.len(), rows_before + 4);
        assert_eq!(
            app.cursor, 5,
            "cursor stays exactly where the gap row stood"
        );
        assert_eq!(line_text_at(&app, app.cursor), "line 4");
        assert_eq!(line_text_at(&app, app.cursor + 5), "line 9");

        assert!(app.collapse_fold_at_cursor());
        assert_eq!(
            app.rows.len(),
            rows_before,
            "folds back to the original row count"
        );
        assert_eq!(app.cursor, 5, "cursor lands back on the reappeared gap row");
        assert!(matches!(app.rows[app.cursor], RenderRow::Gap { .. }));
    }

    /// Issue #5's fold interplay: a confirmed search over content that only
    /// exists inside a *collapsed* fold finds nothing until `z o` reveals
    /// it — `App::expand_gap`'s own [`Self::recompute_search`] call is what
    /// makes the match appear with no need to search again. Mirrors the
    /// e2e suite's `tests/e2e/search.rs` negative-assertion test, at the
    /// `App` level.
    #[test]
    fn expand_gap_recomputes_a_confirmed_search_over_the_newly_revealed_rows() {
        let mut app = gap_fixture_app();
        // "line 5" only exists inside the collapsed gap (new 4..=9) —
        // nowhere in the three visible rows on either side of it.
        let origin = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);
        app.recompute_search_live("line 5", &origin);
        assert!(app.search.as_ref().unwrap().matches.is_empty());

        app.cursor = 5; // the Between gap
        let disk = gap_fixture_disk_lines();
        let refs: Vec<&str> = disk.iter().map(String::as_str).collect();
        assert_eq!(app.expand_gap(0, 0, &refs), ExpandOutcome::Revealed);

        let highlight = app.search.as_ref().expect("the query is still confirmed");
        assert_eq!(highlight.query, "line 5");
        assert_eq!(highlight.matches.len(), 1);
        assert_eq!(line_text_at(&app, highlight.matches[0].row_idx), "line 5");
    }

    #[test]
    fn collapsing_from_the_middle_of_an_expanded_fold_still_lands_on_the_gap_row() {
        let mut app = gap_fixture_app();
        app.cursor = 5;
        let disk = gap_fixture_disk_lines();
        let refs: Vec<&str> = disk.iter().map(String::as_str).collect();
        assert_eq!(app.expand_gap(0, 0, &refs), ExpandOutcome::Revealed);

        app.cursor += 3; // somewhere inside the expanded range, not its first row
        assert!(app.collapse_fold_at_cursor());
        assert_eq!(
            app.cursor, 5,
            "collapse always finds the fold's own start, not the cursor's"
        );
        assert!(matches!(app.rows[app.cursor], RenderRow::Gap { .. }));
    }

    #[test]
    fn expand_gap_rejects_when_disk_content_has_drifted_from_the_hunk_edge() {
        let mut app = gap_fixture_app();
        app.cursor = 5;
        let mut disk = gap_fixture_disk_lines();
        disk[2] = "line 3 EDITED ON DISK".to_owned(); // drifted from the diff's own boundary row
        let refs: Vec<&str> = disk.iter().map(String::as_str).collect();
        let rows_before = app.rows.len();

        assert_eq!(app.expand_gap(0, 0, &refs), ExpandOutcome::Rejected);
        assert_eq!(
            app.rows.len(),
            rows_before,
            "a rejected expand must not touch rows"
        );
        assert_eq!(app.cursor, 5, "and must not move the cursor either");
    }

    #[test]
    fn expand_gap_without_disk_is_new_side_still_validates_normally() {
        // `disk_is_new_side` is a gate `ui::mod::handle_action` enforces
        // before ever calling `expand_gap` — the pure method itself has no
        // opinion about it, so this documents that boundary rather than
        // asserting anything `expand_gap` would refuse on its own.
        let mut app = gap_fixture_app();
        app.disk_is_new_side = false;
        app.cursor = 5;
        let disk = gap_fixture_disk_lines();
        let refs: Vec<&str> = disk.iter().map(String::as_str).collect();
        assert_eq!(app.expand_gap(0, 0, &refs), ExpandOutcome::Revealed);
    }

    #[test]
    fn expand_gap_on_an_exhausted_trailing_gap_records_probed_empty_and_removes_the_row() {
        let mut app = gap_fixture_app();
        let trailing_idx = app.rows.len() - 1;
        assert!(matches!(app.rows[trailing_idx], RenderRow::Gap { .. }));
        app.cursor = trailing_idx;

        let disk = gap_fixture_disk_lines(); // exactly 12 lines — nothing past hunk 1's own end
        let refs: Vec<&str> = disk.iter().map(String::as_str).collect();
        let rows_before = app.rows.len();

        assert_eq!(app.expand_gap(0, 1, &refs), ExpandOutcome::ProbedEmpty);
        assert_eq!(
            app.rows.len(),
            rows_before - 1,
            "the exhausted trailing gap row disappears entirely"
        );
        assert!(
            !matches!(app.rows.last(), Some(RenderRow::Gap { .. })),
            "no trailing gap row remains once probed empty"
        );
    }

    #[test]
    fn expanding_a_non_empty_trailing_gap_leaves_no_dangling_gap_row_behind() {
        // Regression for the silent-content-loss bug: a trailing expand
        // that revealed real lines used to leave a fresh unbounded `···`
        // row right below them, and a second `z o` on that dangling row
        // took the empty-probe path — orphaning the recorded fold and
        // reverting the file to its unexpanded shape with no feedback.
        let mut app = gap_fixture_app();
        let trailing_idx = app.rows.len() - 1;
        assert!(matches!(app.rows[trailing_idx], RenderRow::Gap { .. }));
        app.cursor = trailing_idx;

        // Disk has four real lines past the last hunk's end.
        let disk: Vec<String> = (1..=16).map(|n| format!("line {n}")).collect();
        let refs: Vec<&str> = disk.iter().map(String::as_str).collect();
        let rows_before = app.rows.len();

        assert_eq!(app.expand_gap(0, 1, &refs), ExpandOutcome::Revealed);
        // +4 revealed lines, -1 consumed gap row.
        assert_eq!(app.rows.len(), rows_before + 3);
        assert_eq!(line_text_at(&app, app.cursor), "line 13");
        assert!(
            !matches!(app.rows.last(), Some(RenderRow::Gap { .. })),
            "the revealed content must not be followed by a dangling gap row"
        );

        // The revealed lines must survive every later rederive — collapse
        // and re-expand the *between* gap and re-check the tail.
        let between_idx = app
            .rows
            .iter()
            .position(|r| matches!(r, RenderRow::Gap { .. }))
            .expect("the between gap is still collapsed");
        app.cursor = between_idx;
        assert_eq!(app.expand_gap(0, 0, &refs), ExpandOutcome::Revealed);
        assert!(app.collapse_fold_at_cursor());
        assert_eq!(line_text_at(&app, app.rows.len() - 1), "line 16");
        assert!(
            !matches!(app.rows.last(), Some(RenderRow::Gap { .. })),
            "the trailing expansion survives unrelated rederives"
        );
    }

    #[test]
    fn expand_gap_leaves_the_between_gap_alone_when_only_the_trailing_gap_is_probed_empty() {
        let mut app = gap_fixture_app();
        let trailing_idx = app.rows.len() - 1;
        app.cursor = trailing_idx;
        let disk = gap_fixture_disk_lines();
        let refs: Vec<&str> = disk.iter().map(String::as_str).collect();

        assert_eq!(app.expand_gap(0, 1, &refs), ExpandOutcome::ProbedEmpty);
        let gap_rows: Vec<_> = app
            .rows
            .iter()
            .filter(|r| matches!(r, RenderRow::Gap { .. }))
            .collect();
        assert_eq!(
            gap_rows.len(),
            1,
            "the between gap must still be there, untouched"
        );
    }

    #[test]
    fn derived_rows_and_side_by_side_rows_stay_in_lockstep_after_expand() {
        let mut app = gap_fixture_app();
        app.cursor = 5;
        let disk = gap_fixture_disk_lines();
        let refs: Vec<&str> = disk.iter().map(String::as_str).collect();
        assert_eq!(app.expand_gap(0, 0, &refs), ExpandOutcome::Revealed);

        // Every row here is a header, a Gap, or a Context line — none of
        // which `flatten_side_by_side` ever pairs into a multi-cell run —
        // so side-by-side must have exactly one row per flat row, and every
        // flat index it references must resolve inside the freshly derived
        // `rows` rather than a stale, pre-expand length.
        assert_eq!(app.side_by_side_rows.len(), app.rows.len());
        for row in &app.side_by_side_rows {
            let max_idx = match row {
                SideBySideRow::Full { flat_idx } => *flat_idx,
                SideBySideRow::Paired { old, new } => {
                    use crate::diff::SideCell;
                    [old, new]
                        .into_iter()
                        .filter_map(|c| match c {
                            SideCell::Line { flat_idx } => Some(*flat_idx),
                            SideCell::Empty => None,
                        })
                        .max()
                        .unwrap_or(0)
                }
            };
            assert!(max_idx < app.rows.len());
        }
    }

    #[test]
    fn apply_refresh_reapplies_a_fold_whose_gap_survives_unchanged() {
        let mut app = gap_fixture_app();
        app.cursor = 5;
        let disk = gap_fixture_disk_lines();
        let refs: Vec<&str> = disk.iter().map(String::as_str).collect();
        assert_eq!(app.expand_gap(0, 0, &refs), ExpandOutcome::Revealed);
        let expanded_len = app.rows.len();

        // A refresh whose new parse has this file's exact same shape (the
        // triggering edit happened somewhere else entirely) — clean
        // containment, no disk access needed to reapply.
        app.apply_refresh(vec![gap_fixture_file()]);

        assert_eq!(
            app.rows.len(),
            expanded_len,
            "the fold reapplies after refresh"
        );
        assert_eq!(
            line_text_at(&app, 5),
            "line 4",
            "reapplied from cached lines, not re-read"
        );
    }

    #[test]
    fn apply_refresh_drops_a_fold_when_a_new_hunk_splits_its_gap() {
        use crate::diff::{DiffHunk, DiffLineKind, DiffRow};
        let mut app = gap_fixture_app();
        app.cursor = 5;
        let disk = gap_fixture_disk_lines();
        let refs: Vec<&str> = disk.iter().map(String::as_str).collect();
        assert_eq!(app.expand_gap(0, 0, &refs), ExpandOutcome::Revealed);

        // The new parse has a genuine edit landing right in the middle of
        // what used to be the fold's hidden range (new line 6) — splitting
        // the old new-4..=9 gap into new-4..=5 and new-7..=9, neither of
        // which is "exactly one gap" matching the fold's recorded bounds.
        let mut new_file = gap_fixture_file();
        new_file.hunks.insert(
            1,
            DiffHunk {
                old_start: 6,
                old_lines: 1,
                new_start: 6,
                new_lines: 1,
                header: String::new(),
                known_eof: false,
                rows: vec![
                    DiffRow {
                        kind: DiffLineKind::Del,
                        text: "line 6".to_owned(),
                        old_line: Some(6),
                        new_line: None,
                    },
                    DiffRow {
                        kind: DiffLineKind::Add,
                        text: "line 6 EDITED".to_owned(),
                        old_line: None,
                        new_line: Some(6),
                    },
                ],
            },
        );
        app.apply_refresh(vec![new_file]);

        assert!(
            app.rows.iter().any(|r| matches!(r, RenderRow::Gap { .. })),
            "the split gap(s) must reappear rather than staying merged into the old splice"
        );
        assert!(
            !app.rows.iter().any(
                |r| matches!(r, RenderRow::Line { file_idx, hunk_idx, row_idx }
                if app.files[*file_idx].hunks[*hunk_idx].rows[*row_idx].text == "line 4")
            ),
            "the dropped fold's cached content must not silently reappear"
        );
    }

    #[test]
    fn apply_scope_swap_clears_all_fold_state() {
        let mut app = gap_fixture_app();
        app.cursor = 5;
        let disk = gap_fixture_disk_lines();
        let refs: Vec<&str> = disk.iter().map(String::as_str).collect();
        assert_eq!(app.expand_gap(0, 0, &refs), ExpandOutcome::Revealed);
        assert!(
            !app.expanded_folds.is_empty(),
            "sanity: a fold was recorded"
        );

        app.apply_scope_swap(vec![gap_fixture_file()], true, false, None);

        assert!(app.expanded_folds.is_empty());
        assert!(app.trailing_probed_empty.is_empty());
        assert!(
            app.rows.iter().any(|r| matches!(r, RenderRow::Gap { .. })),
            "the gap row is back — the previously expanded content is gone"
        );
        assert!(
            !app.disk_is_new_side,
            "threaded through from the swap's own argument"
        );
    }

    #[test]
    fn collapse_fold_at_cursor_is_false_off_a_line_that_belongs_to_no_fold() {
        let mut app = gap_fixture_app();
        app.cursor = 2; // an ordinary Line row, never part of any fold
        assert!(!app.collapse_fold_at_cursor());
    }

    #[test]
    fn expand_gap_is_rejected_for_an_out_of_range_gap_idx() {
        let mut app = gap_fixture_app();
        assert_eq!(app.expand_gap(0, 99, &[]), ExpandOutcome::Rejected);
    }

    #[test]
    fn expand_gap_rejects_a_short_trailing_probe_when_the_boundary_row_has_drifted() {
        // Regression: the trailing-gap empty-probe branch used to trust
        // `disk_len < gap.new_start` alone to mean "genuinely at EOF,"
        // without ever checking whether the boundary row right above the
        // gap still matches disk — so a file that got *both* shortened
        // *and* edited right at its new tail looked identical to one that
        // simply ended there, and got silently recorded as probed-empty.
        let mut app = gap_fixture_app();
        let trailing_idx = app.rows.len() - 1;
        app.cursor = trailing_idx;

        // 12 lines — short enough to take the "probably EOF" branch (the
        // trailing gap starts at new line 13) — but line 12, the boundary
        // row `gap_boundary_matches` checks, no longer reads what the diff
        // expects there.
        let mut disk = gap_fixture_disk_lines();
        disk[11] = "line 12 EDITED ON DISK".to_owned();
        let refs: Vec<&str> = disk.iter().map(String::as_str).collect();
        let rows_before = app.rows.len();

        assert_eq!(app.expand_gap(0, 1, &refs), ExpandOutcome::Rejected);
        assert_eq!(
            app.rows.len(),
            rows_before,
            "a rejected probe must not touch rows"
        );
        assert!(
            matches!(app.rows[app.cursor], RenderRow::Gap { .. }),
            "the trailing gap row must still be there"
        );
        assert!(
            !app.trailing_probed_empty.contains("f.txt"),
            "drifted boundary content must not be recorded as probed-empty"
        );
    }
}
