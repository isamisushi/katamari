//! Application state and its pure state transitions. Deliberately free of
//! any terminal or ratatui dependency: `App` can be constructed and driven
//! entirely with parsed diff data and [`Action`]s, which is what makes it
//! testable without a real terminal.

use crate::diff::{
    ColumnMap, DiffFile, DiffLineKind, Gap, RenderRow, SideBySideRow, context_rows_for_gap,
    file_gaps, flatten, flatten_side_by_side, gap_boundary_matches, lsp_target, splice_gap,
};
use crate::keymap::Action;
use crate::lsp::DiagnosticsStore;
use crate::ui::file_tree::{self, FileTree, NodeId};
use crate::ui::hover_popup::HoverQuery;
use crate::ui::pane;
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

/// [`App::toggle_visual`]'s three-way result — issue #16's `V`, the same
/// "a plain `bool` can't distinguish the cases a caller needs to report"
/// shape [`ExpandOutcome`] already uses for `z o`.
/// `ui::mod::handle_action`'s `Action::ToggleVisualLine` arm turns each
/// variant into its own status-bar note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualToggleOutcome {
    /// A new selection started at the cursor's current logical row.
    Started,
    /// An active selection was cancelled, without moving the cursor.
    Cancelled,
    /// The cursor isn't on a [`RenderRow::Line`] — nothing here to start a
    /// selection from.
    NotSelectable,
}

/// What `Action::AddComment` ("c") would create a review comment about —
/// [`App::comment_target`]'s success type. `Single` is the pre-#19 shape
/// (one file, one line); `Range` is issue #19's addition, backed by the
/// same-named `end_anchor`-carrying half of [`crate::comments::Comment`].
/// Deliberately its own type rather than a tuple threaded through
/// `ui::mod`'s event loop and [`crate::ui::compose::ComposeState`] — the
/// issue's "State guidance": one value that names what it is, not a
/// `(String, u32)` a reader has to infer the meaning of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentTarget {
    Single {
        file: String,
        line: u32,
    },
    /// `start <= end` by construction: [`App::range_comment_target`] is the
    /// only place this variant is built, and it only ever builds one from
    /// an already-validated, strictly contiguous run of ascending new-side
    /// line numbers — nothing downstream needs to re-check the ordering.
    Range {
        file: String,
        start: u32,
        end: u32,
    },
}

impl CommentTarget {
    /// The repo-relative file this target anchors to, regardless of variant
    /// — `ui::mod::save_comment` reads this once before matching on the
    /// rest of the shape.
    pub fn file(&self) -> &str {
        match self {
            CommentTarget::Single { file, .. } | CommentTarget::Range { file, .. } => file,
        }
    }

    /// `path:line` for `Single`, `path:start-end` for `Range` (req 5) — the
    /// compose overlay's title, via [`crate::comments::location_label`],
    /// the same formatting `ktmr comments list`/`add`/export already use so
    /// a range reads identically wherever it's shown.
    pub fn location_label(&self) -> String {
        match self {
            CommentTarget::Single { file, line } => {
                crate::comments::location_label(file, *line, *line)
            }
            CommentTarget::Range { file, start, end } => {
                crate::comments::location_label(file, *start, *end)
            }
        }
    }
}

/// Why [`App::comment_target`] refused to produce a [`CommentTarget`] —
/// `ui::mod`'s `Action::AddComment` arm turns each variant into its own
/// status-bar note (req 3: report the exact reason a selection was
/// rejected, never a generic failure), replacing the pre-#19 `Option<...>`'s
/// single undifferentiated "nothing to annotate here."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentTargetError {
    /// The diff isn't [`App::interactive`] — a historical/read-only diff's
    /// on-disk content may not match what's on screen at all (the same
    /// class of risk [`App::disk_is_new_side`]'s docs describe for `z o`),
    /// so neither a single line nor a range should ever anchor against it.
    /// Checked ahead of both paths in [`App::comment_target`], closing the
    /// gap [`App::toggle_visual`]'s own docs used to note.
    NotInteractive,
    /// The cursor (single-line path) isn't on a row with a current
    /// working-tree line to comment about — a header row, a `Del` row, or a
    /// deleted/binary file.
    NoSelectableLine,
    /// The selection (range path) spans more than one [`DiffFile`].
    MultipleFiles,
    /// The selection (range path) is on a deleted file. Checked ahead of
    /// [`Self::ContainsDeletion`] so this more specific reason always wins
    /// — every row of a deleted file is itself a `Del` row (see
    /// [`DiffFile::status`]), so the reverse check order would make this
    /// variant unreachable.
    DeletedFile,
    /// The selection (range path) includes at least one `Del` row.
    ContainsDeletion,
    /// The selection (range path)'s new-side line numbers aren't strictly
    /// contiguous — it crosses a collapsed [`crate::diff::Gap`] or a hunk/
    /// file boundary with hidden context lines in between.
    Discontinuous,
}

impl CommentTargetError {
    /// The exact status-bar text `ui::mod`'s `Action::AddComment` arm shows
    /// for this rejection — kept here, next to the variant it describes,
    /// rather than duplicated at that call site, so the wording and the
    /// reason it names can never drift apart.
    pub fn message(self) -> &'static str {
        match self {
            CommentTargetError::NotInteractive => {
                "comment: not available on a historical/read-only diff"
            }
            CommentTargetError::NoSelectableLine => "comment: nothing to annotate on this row",
            CommentTargetError::MultipleFiles => "comment: selection spans more than one file",
            CommentTargetError::DeletedFile => "comment: can't comment on a deleted file",
            CommentTargetError::ContainsDeletion => {
                "comment: selection includes a deleted line — select added/context lines only"
            }
            CommentTargetError::Discontinuous => {
                "comment: selection isn't contiguous — it crosses a gap"
            }
        }
    }
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

/// Which of the main view's two panes has keyboard focus — issue #14.
/// Defaults to `Diff` (the pre-#14 behavior every action already assumed),
/// so an `App` nobody has pressed Tab in yet behaves exactly as it always
/// did. Reachable states are only ever what [`FOCUS_ORDER`]/
/// [`App::pane_visible`] allow: `Files` only while the sidebar is showing
/// (see `Action::ToggleSidebar`'s arm in [`App::update`], which moves focus
/// off it the instant that stops being true). Unlike the LSP inspector's
/// three-pane Servers/Detail/Journal split, the root diff view only ever
/// has these two panes — a file tree (issue #15) restructures what `Files`
/// shows, not how many panes there are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MainPaneFocus {
    Files,
    #[default]
    Diff,
}

/// The order [`pane::cycle_focus`] walks for `Action::FocusNextPane`/
/// `FocusPrevPane` in the root diff view — `Files` before `Diff` so a
/// reviewer whose sidebar is visible always reaches it on the very first
/// Tab, the pane this milestone just made keyboard-navigable at all.
const FOCUS_ORDER: [MainPaneFocus; 2] = [MainPaneFocus::Files, MainPaneFocus::Diff];

/// A semantic-unit scope on the diff — while set, [`App::rederive`] keeps
/// only the hunks whose content-hash ids (see
/// [`crate::groups::enumerate_hunks`]) appear in `hunk_ids`, so the whole
/// view (rows, sidebar, search, navigation) reads as if the unit were the
/// entire diff. Ids rather than `(file_idx, hunk_idx)` addresses because
/// the filter must survive rederivation and watch refreshes, both of which
/// can shift indices while leaving a hunk's *content* — and therefore its
/// id — alone.
#[derive(Debug, Clone)]
pub struct UnitFilter {
    pub label: String,
    /// The unit's one-sentence rationale, carried into the scope so the
    /// banner above the filtered diff (see
    /// [`crate::ui::units_panel::render_banner`]) can keep answering "why
    /// is this a unit" without the reviewer reopening the panel.
    pub description: String,
    /// 1-based position within the grouping, for the status bar's
    /// "unit 2/5" — navigation context a bare label can't provide.
    pub index: usize,
    pub total: usize,
    pub hunk_ids: HashSet<String>,
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
    /// Whether this session's root diff has live filesystem refresh enabled —
    /// purely a display flag for the status bar's "⦿ watch" indicator;
    /// `ui::mod`'s event loop decides independently (via whether it was
    /// handed a [`crate::ui::PreRefreshHook`]) whether to actually spawn a
    /// watcher, so this field can never drift into claiming watch mode is on
    /// when nothing is watching.
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
    /// The merged `[units]` config (agent CLI preference, model/effort
    /// tuning), carried on `App` the same way `layout`/`scope_label` are:
    /// set once by `main.rs` after construction, read whenever `u`/`U`
    /// spawns a grouping. Default (all-`None`) everywhere else an `App` is
    /// built — nested diff panes never trigger grouping.
    pub units_config: crate::config::UnitsConfig,
    /// Whether the first `u`/`U` that would spawn an agent CLI should
    /// open [`crate::ui::units_setup`]'s one-time picker instead — set by
    /// `main.rs` from [`crate::config::Config::units_configured`]'s
    /// negation, flipped off by the picker completing. Default `false`
    /// (never prompt): every `App` built outside `main.rs` — tests,
    /// nested diff panes — has no business prompting.
    pub units_prompt_needed: bool,
    /// Every gap a `z o` has expanded and not yet folded back, keyed by
    /// [`DiffFile::display_path`] — see [`FoldRange`]'s docs for why path
    /// rather than `file_idx`, and [`Self::rederive`] for how this actually
    /// gets spliced back into `files` on every derivation.
    expanded_folds: HashMap<String, Vec<FoldRange>>,
    /// The active semantic-unit scope, if any — see [`UnitFilter`].
    /// Private for the same reason `expanded_folds` is: changing it
    /// without a [`Self::rederive`] would desynchronize `files`/`rows`
    /// from the state they're supposed to be derived from, so the only
    /// doors in are [`Self::set_unit_filter`]/[`Self::clear_unit_filter`].
    unit_filter: Option<UnitFilter>,
    /// Files whose trailing gap `z o` has probed and found genuinely empty
    /// (the working tree has no more lines past the last hunk) — fed into
    /// [`Self::rederive`] as an effective `known_eof` override so the same
    /// trailing fold row doesn't reappear every time the diff re-derives.
    /// Representation-free by design (a set of paths, not a splice): there
    /// is nothing to cache, since an empty gap has no lines.
    trailing_probed_empty: HashSet<String>,
    /// Issue #16: the logical row [`Self::cursor`] sat on when `V` started
    /// visual-line selection, or `None` when no selection is active. Bare
    /// `RenderRow` index, not anything content-keyed — contrast
    /// `expanded_folds`, which keys off `display_path` precisely so it
    /// *can* survive a rederive: visual selection is explicitly out of
    /// scope for surviving one (see the issue's "Out of scope" list), so
    /// [`Self::rederive`] clears this unconditionally on every call rather
    /// than trying to re-anchor it the way `files_selection`/
    /// `expanded_folds` do. Private for the same reason those two are: the
    /// only doors in are [`Self::toggle_visual`]/[`Self::cancel_visual`],
    /// the only doors out the read-only [`Self::visual_active`]/
    /// [`Self::visual_bounds`]/[`Self::is_row_selected`]/
    /// [`Self::selected_rows`] queries — matching the issue's "State
    /// guidance": no terminal coordinates or duplicated line text, just
    /// this one logical index plus whatever `self.cursor` already is.
    visual_anchor: Option<usize>,
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
    /// Which of the files/diff panes currently receives keyboard actions —
    /// see [`MainPaneFocus`]. `pub` because [`crate::ui::sidebar`] and
    /// [`crate::ui::diff_view::render_focusable`] both read it directly to
    /// decide which border draws focused, the same way they already read
    /// `sidebar_visible`.
    pub focus: MainPaneFocus,
    /// The files pane's independently browsed selection — an index into
    /// [`Self::visible_rows`] (issue #15; a directory row has no `files`
    /// counterpart, so this can no longer index `files` directly the way
    /// #14 had it), distinct from [`Self::diff_file`] (derived from the
    /// diff cursor) the moment `Files` has its own focus and its own
    /// movement (see [`Self::update`]'s `MainPaneFocus::Files` arms). Kept
    /// in sync with `diff_file` only while `Diff` owns focus (see
    /// [`Self::sync_files_selection`]); re-anchored by [`NodeId`] across a
    /// refresh/scope-swap/unit-filter/directory-toggle change instead of
    /// just clamped (see [`Self::resolve_files_selection`]), since a stale
    /// index into a *different* `visible_rows` would otherwise point at an
    /// unrelated row or, if the list shrank, panic out of bounds.
    pub files_selection: usize,
    /// The files pane's own scroll offset — independent of `scroll_offset`
    /// (the diff pane's) for the same reason `files_selection` is
    /// independent of `cursor`.
    pub files_scroll_offset: usize,
    /// The files pane's visible row count, mirroring [`Self::viewport_height`]'s
    /// own docs: refreshed every frame by [`Self::set_files_viewport_height`]
    /// from the sidebar's real rendered inner height, so top/bottom/
    /// half-page movement in `Files` focus and its scroll clamping use an
    /// up-to-date value. Unlike `viewport_height`, every files-pane row is
    /// exactly one visual row tall (a path never soft-wraps) — every
    /// scroll computation over it passes `|_| 1` rather than a per-row
    /// height lookup.
    files_viewport_height: usize,
    /// The files pane's derived directory tree (issue #15) — rebuilt from
    /// `files` on every [`Self::rederive`], the same choke point every
    /// other derived-from-`files` field (`rows`/`side_by_side_rows`/
    /// `gap_cache`) already goes through. Private: nothing outside `App`
    /// addresses a [`file_tree::Node`] directly — only through
    /// `visible_rows`, [`Self::toggle_directory`], and whatever needs a
    /// file's own [`NodeId`] (see [`Self::file_node_id`]).
    tree: FileTree,
    /// Directory paths currently collapsed in the files-pane tree, keyed by
    /// path exactly the way `collapsed`'s callers in
    /// [`crate::ui::file_tree`] expect. Survives every `rederive` — a watch
    /// refresh's whole point is preserving a reviewer's place, and epic
    /// decision 5 extends that to a scope swap too (unlike `expanded_folds`/
    /// `unit_filter`, which a scope swap *does* clear — see
    /// [`Self::apply_scope_swap`]'s docs: a fold or a unit scope describes
    /// specific diff *content* that a swap's unrelated new diff has no
    /// relationship to, but "which directories this reviewer likes
    /// collapsed" is a browsing preference that plausibly carries over
    /// regardless of which diff is on screen). [`file_tree::prune_collapsed`]
    /// still drops whatever a rebuild's new tree no longer has a matching
    /// directory for, the same staleness cleanup `prune_stale_folds` performs
    /// for `expanded_folds`.
    collapsed_dirs: HashSet<String>,
    /// The files pane's flattened, indexable row sequence — [`Self::files_selection`]
    /// indexes into *this*, not `files` directly, as of issue #15 (a
    /// directory row has no `files` counterpart at all). Rebuilt alongside
    /// `tree` on every [`Self::rederive`]; `pub` because
    /// [`crate::ui::sidebar::render`] reads it directly, the same way it
    /// already reads `files_selection`/`files_scroll_offset`.
    pub visible_rows: Vec<file_tree::VisibleRow>,
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
            watch_mode: false,
            comments_visible: true,
            interactive: true,
            disk_is_new_side: false,
            scope_label: None,
            units_config: crate::config::UnitsConfig::default(),
            units_prompt_needed: false,
            expanded_folds: HashMap::new(),
            unit_filter: None,
            trailing_probed_empty: HashSet::new(),
            visual_anchor: None,
            viewport_height: 1,
            // Unbounded until the first frame reports a real pane width
            // (see `set_content_width`) — `row_visual_height` then treats
            // every row as exactly one visual row, matching `wrap = false`
            // behavior, rather than wrapping against a width of `0`.
            content_width: usize::MAX,
            search: None,
            focus: MainPaneFocus::default(),
            files_selection: 0,
            files_scroll_offset: 0,
            files_viewport_height: 1,
            tree: FileTree::default(),
            collapsed_dirs: HashSet::new(),
            visible_rows: Vec::new(),
        };
        // No folds exist yet, so this just moves `pristine_files` in
        // verbatim — reusing the one derivation path rather than
        // duplicating its move-then-flatten shape here.
        app.rederive();
        // Issue #15: `files_selection`'s struct-literal default (`0`) is no
        // longer guaranteed to mean "the diff cursor's own file" the moment
        // there's a tree — a root-level directory can sort ahead of every
        // file (e.g. a dotfile directory alphabetically before an ordinary
        // one), landing index `0` on a *directory* row instead. Pre-#15
        // this was harmless: every row *was* a file row in the same order
        // as `files` itself, so `files_selection == 0` and `diff_file() ==
        // 0` were simply the same fact stated twice.
        //
        // Resolved directly via `resolve_and_set_selection`, not the
        // `sync_files_selection`/`toggle_directory` wrapper that also
        // clamps scroll: `files_viewport_height` is still its harmless-only-
        // once-a-real-frame-has-run placeholder of `1` at this point (the
        // event loop's frame prep calls `Self::set_files_viewport_height`
        // with the real terminal height, but only once rendering actually
        // starts, well after construction) — clamping scroll against that
        // placeholder here would compute a bogus offset that a *later*,
        // correctly-sized `set_files_viewport_height` call can't necessarily
        // undo (`ui::scroll::clamp_scroll` only ever pulls an offset
        // *forward*, never resets one that's already sitting past where a
        // newly-widened viewport would put it). Leaving `files_scroll_offset`
        // at its own harmless default (`0`) instead means the first real
        // `set_files_viewport_height` call clamps correctly from a clean
        // slate.
        app.resolve_and_set_selection(None);
        app
    }

    /// Rebuilds `files`/`rows`/`side_by_side_rows`/`gap_cache` from
    /// `pristine_files`. Two very different costs depending on whether
    /// there's any fold state to account for:
    ///
    /// - **No derived state** (`expanded_folds`/`trailing_probed_empty`
    ///   empty and no [`UnitFilter`] active — watch mode's steady state
    ///   for a reviewer who never presses `z o` or scopes to a unit): the
    ///   derived view is `pristine_files` verbatim, so
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
        // Issue #16: every path into this function can shuffle, insert, or
        // remove flat row indices out from under a stored anchor (a watch
        // refresh, a scope swap, a unit-filter change, a fold toggle) — one
        // unconditional clear here, ahead of every other branch, covers
        // all of them at once rather than each of `Self::apply_refresh`/
        // `Self::apply_scope_swap`/`Self::set_unit_filter`/
        // `Self::clear_unit_filter`/`Self::expand_gap`/
        // `Self::collapse_fold_at_cursor` remembering to clear it
        // individually — see `Self::visual_anchor`'s own docs for why a
        // coarse, no-exceptions clear is the right call here (persisting
        // selection through any of these is explicitly out of scope).
        self.visual_anchor = None;
        let has_derived_state = !self.expanded_folds.is_empty()
            || !self.trailing_probed_empty.is_empty()
            || self.unit_filter.is_some();
        self.files = if has_derived_state {
            if self.pristine_files.is_empty() && !self.files.is_empty() {
                self.pristine_files = self.files.clone();
            }
            let mut files = self.splice_folds();
            if let Some(filter) = &self.unit_filter {
                apply_unit_filter(&mut files, &filter.hunk_ids);
                if files.is_empty() {
                    // Every id stopped resolving — a watch refresh rewrote
                    // the content this unit described. An empty diff view
                    // with no visible way back would read as data loss;
                    // silently widening back to the full diff is the
                    // behavior a reviewer can actually recover from.
                    self.unit_filter = None;
                    files = self.splice_folds();
                }
            }
            files
        } else {
            std::mem::take(&mut self.pristine_files)
        };
        self.rows = flatten(&self.files);
        self.side_by_side_rows = flatten_side_by_side(&self.files);
        self.gap_cache = self.files.iter().map(file_gaps).collect();
        // Issue #15: the files-pane tree is derived from `files` the same
        // way `rows`/`side_by_side_rows`/`gap_cache` are — one rebuild here
        // covers every caller of `rederive` (a watch refresh, a scope swap,
        // a unit-filter change, and even a plain `z o`/`z c` fold toggle,
        // which touches no path at all and so leaves `visible_rows`
        // structurally identical — see this method's own callers'
        // docs) rather than each duplicating the same three calls.
        // `files_selection` is deliberately left untouched here: every
        // caller that actually needs to re-anchor it after a rebuild does
        // so itself, afterward, with the *old* selection captured before
        // this call — see `Self::resolve_files_selection`'s docs.
        self.tree = file_tree::build(&self.files);
        file_tree::prune_collapsed(&self.tree, &mut self.collapsed_dirs);
        self.visible_rows = file_tree::flatten_visible(&self.tree, &self.collapsed_dirs);
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

    /// What `Action::AddComment` ("c") would create a review comment about
    /// right now: a single line at the cursor when no visual selection is
    /// active, or (issue #19) the active selection's contiguous new-side
    /// range when one is — dispatching to [`Self::single_comment_target`]/
    /// [`Self::range_comment_target`] respectively. Gated on
    /// [`Self::interactive`] at the very top, ahead of either path (see
    /// [`CommentTargetError::NotInteractive`]'s docs) — this is the gate
    /// [`Self::toggle_visual`]'s own docs used to note was missing.
    pub fn comment_target(&self) -> Result<CommentTarget, CommentTargetError> {
        if !self.interactive {
            return Err(CommentTargetError::NotInteractive);
        }
        if self.visual_active() {
            self.range_comment_target()
        } else {
            self.single_comment_target()
        }
    }

    /// The single-line case: the cursor's current row, when it's eligible
    /// the same way [`Self::hover_query`]'s target is — a `Context`/`Add`
    /// row on a file that's still present on disk (see [`lsp_target`]'s
    /// docs for the exact rule). [`CommentTargetError::NoSelectableLine`]
    /// on a header row, a `Del` row, or a deleted/binary file, none of
    /// which have a current line for a comment to be *about*.
    fn single_comment_target(&self) -> Result<CommentTarget, CommentTargetError> {
        let Some(RenderRow::Line {
            file_idx,
            hunk_idx,
            row_idx,
        }) = self.rows.get(self.cursor)
        else {
            return Err(CommentTargetError::NoSelectableLine);
        };
        let file = &self.files[*file_idx];
        let row = &file.hunks[*hunk_idx].rows[*row_idx];
        let Some((relative_path, line0)) = lsp_target(row, file) else {
            return Err(CommentTargetError::NoSelectableLine);
        };
        Ok(CommentTarget::Single {
            file: relative_path.to_string_lossy().into_owned(),
            line: line0 + 1,
        })
    }

    /// The range case (issue #19): every row [`Self::selected_rows`]
    /// currently covers must be a `Context`/`Add` row, in the same file, on
    /// a file that still exists, forming one strictly contiguous inclusive
    /// run of new-side line numbers (issue #10's decision 6). Checked in
    /// one ascending pass over `selected_rows()`: the first *row* that
    /// violates anything decides the reported reason, with the checks
    /// ordered per row as below — so a selection failing several
    /// conditions on *different* rows reports whichever the earliest bad
    /// row hit (e.g. a deletion crossed early masks a file boundary crossed
    /// later). Every reported reason is a true statement about the
    /// selection; no whole-selection priority ranking is promised.
    ///
    /// Within one row, the check order is:
    ///
    /// 1. [`CommentTargetError::MultipleFiles`] — the row must share the
    ///    first row's `file_idx`.
    /// 2. [`CommentTargetError::DeletedFile`] — checked *before* row kind:
    ///    every row of a deleted file is a `Del` row (see
    ///    [`DiffFile::status`]), so checking kind first would make this
    ///    variant unreachable, and "the file is gone" is the more useful
    ///    thing to tell a reviewer who selected across one.
    /// 3. [`CommentTargetError::ContainsDeletion`] — any remaining `Del`
    ///    row.
    ///
    /// [`CommentTargetError::Discontinuous`] runs after the pass, once
    /// every row's `new_line` is known good: adjacent selected rows'
    /// new-side line numbers must differ by exactly `1`. This one numeric
    /// check covers both a selection spanning a collapsed
    /// [`crate::diff::Gap`] (whose lines were never material to begin with
    /// — `RenderRow::Gap` never appears in `selected_rows()`) and one
    /// crossing a hunk/file boundary with hidden context in between,
    /// without needing to special-case either shape.
    ///
    /// A selection that clears every check collapses to
    /// [`CommentTarget::Single`] when it covers exactly one line — "a
    /// one-row visual selection may create a single-line record" (req 4) is
    /// this function's natural output, not a separate branch.
    fn range_comment_target(&self) -> Result<CommentTarget, CommentTargetError> {
        let rows = self.selected_rows();
        let Some(&first_idx) = rows.first() else {
            // Structurally unreachable while `visual_active()` —
            // `toggle_visual` only ever starts a selection on a
            // `RenderRow::Line`, which is always inside `visual_bounds()`
            // and so always present in `selected_rows()` — but reporting a
            // clean rejection here costs nothing and needs no unchecked
            // assumption about an invariant this function doesn't itself
            // enforce.
            return Err(CommentTargetError::NoSelectableLine);
        };
        let RenderRow::Line {
            file_idx: first_file_idx,
            ..
        } = self.rows[first_idx]
        else {
            return Err(CommentTargetError::NoSelectableLine);
        };

        let mut new_lines = Vec::with_capacity(rows.len());
        for &idx in &rows {
            let RenderRow::Line {
                file_idx,
                hunk_idx,
                row_idx,
            } = self.rows[idx]
            else {
                return Err(CommentTargetError::NoSelectableLine);
            };
            if file_idx != first_file_idx {
                return Err(CommentTargetError::MultipleFiles);
            }
            let file = &self.files[file_idx];
            if file.is_deleted {
                return Err(CommentTargetError::DeletedFile);
            }
            let row = &file.hunks[hunk_idx].rows[row_idx];
            if row.kind == DiffLineKind::Del {
                return Err(CommentTargetError::ContainsDeletion);
            }
            // A non-deleted file's Context/Add row always carries a
            // `new_line` (see `DiffRow`'s docs) — a binary file never
            // reaches this point at all (`toggle_visual` only ever starts a
            // selection on a `Line` row, and `flatten` never emits one for
            // a binary file), so there is no reachable case where this is
            // `None`.
            let Some(new_line) = row.new_line else {
                return Err(CommentTargetError::NoSelectableLine);
            };
            new_lines.push(new_line);
        }

        if !new_lines.windows(2).all(|w| w[1] == w[0] + 1) {
            return Err(CommentTargetError::Discontinuous);
        }

        let file = self.files[first_file_idx].display_path().to_owned();
        let start = new_lines[0];
        let end = *new_lines
            .last()
            .expect("selected_rows is non-empty whenever first_idx resolved");
        if start == end {
            Ok(CommentTarget::Single { file, line: start })
        } else {
            Ok(CommentTarget::Range { file, start, end })
        }
    }

    /// `V`'s effect. Cancel-first — checked via `.take()` *before* the
    /// row-eligibility check below — so a second `V` always cancels an
    /// active selection, even if the cursor has since wandered onto a
    /// header/gap/binary row a fresh `V` could never have started a
    /// selection from (req 3). Starting a *new* selection only happens on
    /// a [`RenderRow::Line`] (req 5); every other row reports
    /// `NotSelectable` and leaves `visual_anchor` untouched — already
    /// `None` at that point, since cancellation took priority above, so
    /// there's nothing to leave alone that wasn't already there.
    ///
    /// Deliberately does *not* gate on [`Self::interactive`]: a historical/
    /// read-only diff can still select and later copy lines (req 11) — the
    /// interactivity gate belongs only to what visual selection eventually
    /// feeds (hover stays gated at [`Self::hover_query`]; issue #19 gave
    /// [`Self::comment_target`] that same gate, at the top of both its
    /// single-line and range paths).
    pub fn toggle_visual(&mut self) -> VisualToggleOutcome {
        if self.visual_anchor.take().is_some() {
            return VisualToggleOutcome::Cancelled;
        }
        if !matches!(self.rows.get(self.cursor), Some(RenderRow::Line { .. })) {
            return VisualToggleOutcome::NotSelectable;
        }
        self.visual_anchor = Some(self.cursor);
        VisualToggleOutcome::Started
    }

    /// `Esc`'s half of visual cancellation — a `bool` outcome mirroring
    /// [`Self::clear_unit_filter`]'s shape, since `ui::mod`'s `Action::Cancel`
    /// handling only needs to know whether *something* was cancelled here,
    /// to decide whether to keep unwinding through the rest of the Esc
    /// precedence chain (see that arm's own docs). Unlike
    /// [`Self::toggle_visual`], never needs to check row eligibility —
    /// cancelling never starts anything, so there's nothing to validate
    /// beyond "was a selection active."
    pub fn cancel_visual(&mut self) -> bool {
        self.visual_anchor.take().is_some()
    }

    /// Whether visual-line selection is currently active.
    pub fn visual_active(&self) -> bool {
        self.visual_anchor.is_some()
    }

    /// The inclusive logical-row interval visual selection currently
    /// covers: `(min, max)` of the anchor and wherever the cursor has
    /// moved to since — recomputed fresh from `self.cursor` on every call
    /// rather than stored alongside the anchor at start time (req 4). That
    /// is what makes every cursor-moving action — paging, top/bottom,
    /// hunk/file jumps, search/diagnostic jumps, a future mouse click —
    /// extend the selection for free: none of those call sites need to
    /// know visual mode exists. `None` when no selection is active.
    pub fn visual_bounds(&self) -> Option<(usize, usize)> {
        self.visual_anchor.map(|anchor| {
            if anchor <= self.cursor {
                (anchor, self.cursor)
            } else {
                (self.cursor, anchor)
            }
        })
    }

    /// Whether `flat_idx` is a selected *source* line — inside
    /// [`Self::visual_bounds`] **and** a [`RenderRow::Line`] (req 6). A
    /// file/hunk header or fold row can sit inside the inclusive interval
    /// (the selection spans a hunk or file boundary) without itself being
    /// selectable content — `diff_view::render_row` only ever calls
    /// `diff_view::content_line` (the only renderer that consults this) for
    /// a `Line` row in the first place, so structural rows are excluded by
    /// construction as much as by this check.
    pub fn is_row_selected(&self, flat_idx: usize) -> bool {
        let Some((lo, hi)) = self.visual_bounds() else {
            return false;
        };
        lo <= flat_idx
            && flat_idx <= hi
            && matches!(self.rows.get(flat_idx), Some(RenderRow::Line { .. }))
    }

    /// Every selected content row's flat index, ascending screen order —
    /// the #17/#19 handoff: yanking and range-commenting both need the
    /// concrete row list, not just the interval, since structural rows
    /// inside [`Self::visual_bounds`] must be skipped rather than treated
    /// as selected. Indices only, never text or file/line numbers, per the
    /// issue's "State guidance" — a caller that needs more looks each index
    /// up in `self.rows`/`self.files` itself, which is exactly what
    /// `ui::clipboard::resolve_selection` does for issue #17's `y` and
    /// [`Self::range_comment_target`] does for issue #19's `c`.
    pub fn selected_rows(&self) -> Vec<usize> {
        let Some((lo, hi)) = self.visual_bounds() else {
            return Vec::new();
        };
        (lo..=hi)
            .filter(|&idx| matches!(self.rows.get(idx), Some(RenderRow::Line { .. })))
            .collect()
    }

    /// The diff pane's visible row count changes on terminal resize; the
    /// event loop reports it before each frame so half-page scrolling and
    /// scroll-to-cursor clamping use an up-to-date value.
    ///
    /// A no-op when `height` already matches — issue #20 made this matter
    /// for the first time: `ui::mod`'s event loop calls this every single
    /// iteration (not only on an actual resize), and before #20 that was
    /// always harmless, since `self.clamp_scroll()` re-deriving
    /// `scroll_offset` from `self.cursor` was idempotent — every path that
    /// ever moved the cursor already called it too, so a same-value resize
    /// call always recomputed the identical `scroll_offset`. `Self::scroll_by`
    /// broke that idempotence on purpose: it moves `scroll_offset` *without*
    /// moving `self.cursor`, and calling this every frame with the same
    /// unchanged height would otherwise re-run `clamp_scroll` and pull that
    /// wheel-scrolled offset right back to the cursor's row before the next
    /// draw ever showed it — the guard below is what lets a wheel scroll
    /// actually stay on screen.
    pub fn set_viewport_height(&mut self, height: usize) {
        let height = height.max(1);
        if height == self.viewport_height {
            return;
        }
        self.viewport_height = height;
        self.clamp_scroll();
    }

    /// The diff pane's content width changes on resize the same way its
    /// height does — reported every frame alongside [`Self::set_viewport_height`]
    /// so [`Self::row_visual_height`]'s wrap-width stays current. See
    /// [`Self::content_width`]'s docs for exactly what width this expects
    /// (the unified layout's, regardless of which layout is actually
    /// showing) and [`crate::ui::diff_view::unified_content_width`], which
    /// every caller derives it through. Skips the clamp when `width` is
    /// unchanged for the same reason [`Self::set_viewport_height`] does.
    pub fn set_content_width(&mut self, width: usize) {
        let width = width.max(1);
        if width == self.content_width {
            return;
        }
        self.content_width = width;
        self.clamp_scroll();
    }

    /// How many visual rows `self.rows[idx]` occupies on screen right now —
    /// `1` for every header/binary-notice row (headers never wrap, see
    /// `diff_view::render_row`), `1` for a content row when `[ui] wrap` is
    /// off, and otherwise however many rows [`crate::ui::text::wrapped_row_count`]
    /// says its text soft-wraps into at [`Self::content_width`]. The single
    /// row-height oracle every wrap-aware call into `ui::scroll` in this
    /// module reads.
    ///
    /// Never accounts for a comment's own inline body block (single-line or
    /// #19 range alike) the way it does a row's soft-wrap — that mismatch
    /// predates #19 and stays out of scope here too; see
    /// `diff_view::render_unified`'s doc for the accepted trade-off. #19's
    /// `.at()` -> `.starting_at()` render fix (`diff_view::comments_starting_at_row`)
    /// already bounds a range's worst-case contribution to that mismatch to
    /// exactly what a single-line comment always had, since a range's body
    /// now renders once regardless of how many lines it spans.
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
        // Captured against the *old* `visible_rows` before it's rebuilt
        // below — see `Self::resolve_files_selection`'s docs on why the
        // sidebar selection is re-anchored by `NodeId` rather than just
        // clamped.
        let selected_id = self
            .visible_rows
            .get(self.files_selection)
            .map(|r| r.id.clone());

        self.prune_stale_folds(&files);
        self.pristine_files = files;
        self.rederive();
        self.active_symbol = 0;

        if self.rows.is_empty() {
            self.cursor = 0;
            self.scroll_offset = 0;
            self.recompute_search();
            self.resolve_files_selection(selected_id);
            return false;
        }

        let restored = refresh::restore_anchor(&self.files, &self.rows, &anchor);
        self.cursor = restored.row_index;
        self.restore_scroll_from_delta(restored.row_index, refresh::scroll_delta(&anchor));
        self.recompute_search();
        self.resolve_files_selection(selected_id);
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
    /// The files-pane selection is the one exception (req 10 — see
    /// [`Self::resolve_files_selection`]): unlike the diff cursor, browsing
    /// position in the sidebar can still meaningfully carry over when the
    /// new scope happens to touch the same file.
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
        // Unlike the diff cursor below, the files-pane selection *is*
        // worth trying to preserve across a scope swap (req 10): swapping
        // working-tree <-> staged, say, very often touches the same files,
        // and there's no reason to lose a reviewer's place in the sidebar
        // just because the diff cursor itself has nothing meaningful to
        // restore. Captured before `files`/`visible_rows` is rebuilt below.
        // `collapsed_dirs` itself is *not* cleared here (unlike
        // `expanded_folds`/`unit_filter` just below) — see that field's own
        // docs for why a scope swap keeps it.
        let selected_id = self
            .visible_rows
            .get(self.files_selection)
            .map(|r| r.id.clone());
        self.pristine_files = files;
        self.expanded_folds.clear();
        self.trailing_probed_empty.clear();
        // Same reasoning as the fold state above: a unit grouping
        // describes the *previous* scope's diff; against an unrelated one
        // its ids are meaningless.
        self.unit_filter = None;
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
        self.resolve_files_selection(selected_id);
    }

    /// Scopes the whole diff view down to one semantic unit's hunks —
    /// `Enter` on [`crate::ui::units_panel`]'s selected unit. Cursor and
    /// scroll reset to the top rather than being anchor-restored: the
    /// filtered view is a different reading surface (the unit is meant to
    /// be read from its beginning), not a refresh of the same one — the
    /// same reasoning [`Self::apply_scope_swap`] applies to scope changes.
    /// The files-pane selection still tries to survive by path (req 10;
    /// also fixes a latent out-of-bounds risk: `files` can shrink to just
    /// the unit's own files, and a `files_selection` left pointing at its
    /// pre-filter index could land past the end of the new, shorter list).
    pub fn set_unit_filter(&mut self, filter: UnitFilter) {
        let selected_id = self
            .visible_rows
            .get(self.files_selection)
            .map(|r| r.id.clone());
        self.unit_filter = Some(filter);
        self.rederive();
        self.cursor = 0;
        self.scroll_offset = 0;
        self.active_symbol = 0;
        // A confirmed search's matches were computed against the full row
        // list; recomputing against the filtered one keeps `n`/`N` from
        // landing on rows that no longer exist.
        self.recompute_search();
        self.clamp_scroll();
        self.resolve_files_selection(selected_id);
    }

    /// Widens back to the full diff — plain `Esc`'s first meaning while a
    /// unit scope is active (see `ui::mod::handle_action`'s `Cancel` arm:
    /// filter first, then search highlight). Returns whether there was
    /// anything to clear, so that arm knows whether `Esc` is spent. Same
    /// files-pane re-anchoring as [`Self::set_unit_filter`], for the same
    /// out-of-bounds reason (widening back grows `files`, so the previously
    /// filtered-down selection's index is stale either way).
    pub fn clear_unit_filter(&mut self) -> bool {
        if self.unit_filter.is_none() {
            return false;
        }
        let selected_id = self
            .visible_rows
            .get(self.files_selection)
            .map(|r| r.id.clone());
        self.unit_filter = None;
        self.rederive();
        self.cursor = 0;
        self.scroll_offset = 0;
        self.active_symbol = 0;
        self.recompute_search();
        self.clamp_scroll();
        self.resolve_files_selection(selected_id);
        true
    }

    pub fn unit_filter(&self) -> Option<&UnitFilter> {
        self.unit_filter.as_ref()
    }

    /// The complete diff regardless of any active [`UnitFilter`] — what
    /// grouping-related lookups must key on. While any derived state is
    /// active, `pristine_files` holds the full parse (see
    /// [`Self::rederive`]'s slow path); in the steady state it was moved
    /// into `files` and sits there instead. Callers that want *what's on
    /// screen* keep reading `files`; this exists precisely for the
    /// unit-grouping paths, where hashing the filtered view would make the
    /// cache key describe the scope rather than the diff (a `u` pressed
    /// mid-scope would then miss the cache and re-spawn the agent CLI on a
    /// fragment of the diff).
    pub fn full_files(&self) -> &[DiffFile] {
        if self.pristine_files.is_empty() {
            &self.files
        } else {
            &self.pristine_files
        }
    }

    /// Index into `files`/`rows` for whichever file the diff cursor
    /// currently sits within. Pre-#14 this *was* the sidebar's highlight;
    /// now it's the "background" file [`crate::ui::sidebar::render`] marks
    /// distinctly from [`Self::files_selection`] when the two differ (see
    /// [`Self::sync_files_selection`]), and what
    /// [`Self::resolve_files_selection`] falls back to when a refresh/
    /// scope-swap can't find the previously selected path anymore. Renamed
    /// from `selected_file` in #14 so callers can't confuse this
    /// diff-cursor-derived value with the sidebar's own independently
    /// browsed selection.
    pub fn diff_file(&self) -> usize {
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
            return;
        }
        let last = self.rows.len() - 1;
        let cursor_before = self.cursor;
        match action {
            // Cursor/paging/top-bottom movement targets whichever pane owns
            // focus (issue #14) — `Files` moves `files_selection` over
            // `self.visible_rows` (issue #15's tree rows, not `self.files`
            // directly — a directory row has no `files` counterpart at all)
            // instead of the diff cursor, using the same `ui::scroll`
            // machinery with `|_| 1` row heights (a files-pane row never
            // wraps, see `Self::files_viewport_height`'s docs). `Diff`
            // behaves exactly as every milestone before #14 did.
            Action::CursorDown => match self.focus {
                MainPaneFocus::Diff => self.cursor = (self.cursor + 1).min(last),
                MainPaneFocus::Files => {
                    self.files_selection =
                        (self.files_selection + 1).min(self.visible_rows.len().saturating_sub(1));
                    self.clamp_files_scroll();
                }
            },
            Action::CursorUp => match self.focus {
                MainPaneFocus::Diff => self.cursor = self.cursor.saturating_sub(1),
                MainPaneFocus::Files => {
                    self.files_selection = self.files_selection.saturating_sub(1);
                    self.clamp_files_scroll();
                }
            },
            Action::HalfPageDown => match self.focus {
                MainPaneFocus::Diff => {
                    self.cursor =
                        scroll::half_page_down(self.cursor, last, self.viewport_height, |i| {
                            self.row_visual_height(i)
                        });
                }
                MainPaneFocus::Files => {
                    let last_row = self.visible_rows.len().saturating_sub(1);
                    self.files_selection = scroll::half_page_down(
                        self.files_selection,
                        last_row,
                        self.files_viewport_height,
                        |_| 1,
                    );
                    self.clamp_files_scroll();
                }
            },
            Action::HalfPageUp => match self.focus {
                MainPaneFocus::Diff => {
                    self.cursor = scroll::half_page_up(self.cursor, self.viewport_height, |i| {
                        self.row_visual_height(i)
                    });
                }
                MainPaneFocus::Files => {
                    self.files_selection = scroll::half_page_up(
                        self.files_selection,
                        self.files_viewport_height,
                        |_| 1,
                    );
                    self.clamp_files_scroll();
                }
            },
            Action::Top => match self.focus {
                MainPaneFocus::Diff => self.cursor = 0,
                MainPaneFocus::Files => {
                    self.files_selection = 0;
                    self.clamp_files_scroll();
                }
            },
            Action::Bottom => match self.focus {
                MainPaneFocus::Diff => self.cursor = last,
                MainPaneFocus::Files => {
                    self.files_selection = self.visible_rows.len().saturating_sub(1);
                    self.clamp_files_scroll();
                }
            },
            // Issue #15: `Space` toggles the selected directory row's
            // collapsed state — a no-op off `Files` focus or off a
            // directory row (a file row has nothing to toggle; `Confirm`/
            // Enter is what opens it). Handled entirely here, unlike most
            // `ui::mod`-intercepted actions, since there's no LSP/IO/
            // overlay concern — just `App`'s own tree/collapse state, the
            // same pure-state-flip shape `ToggleSidebar`/`ToggleComments`
            // already have.
            Action::ToggleDirectory => {
                if self.focus == MainPaneFocus::Files
                    && let Some(row) = self.visible_rows.get(self.files_selection)
                    && matches!(row.kind, file_tree::VisibleKind::Directory { .. })
                {
                    let path = row.id.path.clone();
                    self.toggle_directory(&path);
                }
            }
            // Hunk/file/symbol navigation is diff-content movement — while
            // `Files` is focused it stays a no-op rather than reaching into
            // a diff cursor the reviewer isn't looking at (req 5: "irrelevant
            // diff-only actions do not mutate the diff cursor"). Next/
            // PrevFile in particular could look tempting to repoint at
            // `files_selection` instead, but issue #14 explicitly keeps
            // that out of scope (see its "Out of scope" list) — the files
            // pane's own up/down already does that job.
            Action::NextHunk => {
                if self.focus == MainPaneFocus::Diff {
                    self.jump_to(|row| matches!(row, RenderRow::HunkHeader { .. }));
                }
            }
            Action::PrevHunk => {
                if self.focus == MainPaneFocus::Diff {
                    self.jump_to_prev(|row| matches!(row, RenderRow::HunkHeader { .. }));
                }
            }
            Action::NextFile => {
                if self.focus == MainPaneFocus::Diff {
                    self.jump_to(|row| matches!(row, RenderRow::FileHeader { .. }));
                }
            }
            Action::PrevFile => {
                if self.focus == MainPaneFocus::Diff {
                    self.jump_to_prev(|row| matches!(row, RenderRow::FileHeader { .. }));
                }
            }
            Action::ToggleSidebar => {
                self.sidebar_visible = !self.sidebar_visible;
                // Hiding the sidebar can never leave focus pointing at a
                // pane that's no longer drawn (req 3); showing it back
                // never steals focus away from wherever `Diff` already was.
                if !self.sidebar_visible && self.focus == MainPaneFocus::Files {
                    self.focus = MainPaneFocus::Diff;
                }
            }
            Action::ToggleLayout => self.layout = self.layout.toggled(),
            Action::ToggleComments => self.comments_visible = !self.comments_visible,
            Action::NextSymbol => {
                if self.focus == MainPaneFocus::Diff {
                    self.cycle_symbol(1);
                }
            }
            Action::PrevSymbol => {
                if self.focus == MainPaneFocus::Diff {
                    self.cycle_symbol(-1);
                }
            }
            // Issue #14: the root diff view now has two real panes to
            // cycle between. `pane::cycle_focus` is the same shared
            // mechanic the LSP inspector's Servers/Detail/Journal split and
            // the timeline's list/diff split already use — see its own
            // docs for the visibility-skipping/wrap/no-op-when-alone rules
            // this inherits for free. `TimelineView` never forwards these
            // to its nested `App` at all (its own `update` returns as soon
            // as it has cycled `self.focus` — see that method's docs), so
            // this arm's only real caller is a root
            // [`crate::ui::view::View::Diff`].
            Action::FocusNextPane => {
                self.focus =
                    pane::cycle_focus(&FOCUS_ORDER, self.focus, true, |p| self.pane_visible(p));
                // Issue #16's 7th invalidation trigger: a visual selection
                // makes no sense once the diff pane no longer has focus (a
                // reviewer browsing `Files` has no logical row on screen to
                // extend it from), so leaving `Diff` cancels it the same
                // way every `Self::rederive` call already does for the
                // other six. Entering `Diff` never needs the opposite
                // treatment — there's nothing stale to inherit, since a
                // selection can only ever have started while `Diff` already
                // had focus.
                if self.focus != MainPaneFocus::Diff {
                    self.visual_anchor = None;
                }
            }
            Action::FocusPrevPane => {
                self.focus =
                    pane::cycle_focus(&FOCUS_ORDER, self.focus, false, |p| self.pane_visible(p));
                if self.focus != MainPaneFocus::Diff {
                    self.visual_anchor = None;
                }
            }
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
            //
            // `ToggleVisualLine` joins the bucket for the same reason as
            // `OpenSearch`/`NextMatch`/`PrevMatch`: `Self::toggle_visual`
            // is a real, pure `App` method (no LSP/IO/overlay concern at
            // all), but `ui::mod::handle_action` needs to turn its
            // `VisualToggleOutcome` into a status-bar note this `()` return
            // type has no way to carry — see that arm's docs.
            //
            // `YankSelection` (issue #17) joins it too: `Self::selected_rows`
            // is a real, pure `App` query, but formatting the selection,
            // writing OSC 52, and reporting success/failure all need
            // `ui::clipboard` plus this `()` return type still can't carry
            // a status note — so, like `ToggleVisualLine`, the actual work
            // happens in `ui::mod::handle_action`'s own arm instead.
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
            | Action::ToggleVisualLine
            | Action::YankSelection
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
            | Action::ToggleUnits
            | Action::RegenerateUnits
            | Action::ToggleHints
            | Action::ToggleRangeSelect => {}
            // `ui::mod` intercepts this before it reaches here too, same
            // bucket as `ToggleTimeline`/`ToggleLogView`/`OpenScopeMenu`
            // above — opening `ui::help`'s popup needs the live `Keymap` to
            // build its row list, which `App` doesn't own, and (unlike
            // those three) it opens from *any* view rather than gating on
            // `View::Diff` — see `Action::OpenHelp`'s docs.
            Action::OpenHelp => {}
            // `ui::mod`'s event loop intercepts `q` at the keymap resolver,
            // before a matched action is ever dispatched anywhere (see
            // `ui::mod::event_loop`'s `StepResult::Matched(Action::Quit)`
            // arm) — global quit, not a per-view "close" — so this can never
            // actually reach here; kept as an explicit no-op arm (rather
            // than folded into `other` below) so `Action`'s exhaustive match
            // stays a compile-time reminder if that ever stops being true.
            Action::Quit => {}
        }
        if self.cursor != cursor_before {
            self.active_symbol = 0;
        }
        self.clamp_scroll();
        self.sync_files_selection();
    }

    /// Whether `pane` is currently a valid focus target — the predicate
    /// [`pane::cycle_focus`] skips past to land only somewhere actually on
    /// screen. `Diff` is always visible (there is no way to hide the diff
    /// pane itself); `Files` mirrors `sidebar_visible` exactly, so hiding
    /// the sidebar can never leave focus pointing at a pane no longer drawn
    /// — see `Action::ToggleSidebar`'s arm above for what happens to focus
    /// already resting there when that flips.
    fn pane_visible(&self, pane: MainPaneFocus) -> bool {
        match pane {
            MainPaneFocus::Files => self.sidebar_visible,
            MainPaneFocus::Diff => true,
        }
    }

    /// The files pane's visible row count changes on terminal resize the
    /// same way the diff pane's does (see [`Self::set_viewport_height`]) —
    /// reported every frame, from the sidebar's real rendered inner height,
    /// so `Files`-focused top/bottom/half-page movement and its scroll
    /// clamping use an up-to-date value. Skips the clamp when `height` is
    /// unchanged, for the same reason [`Self::set_viewport_height`] does:
    /// `Self::scroll_files_by` moves `files_scroll_offset` without moving
    /// `files_selection`, and an every-frame re-clamp against an unchanged
    /// height would otherwise snap a wheel-scrolled sidebar right back
    /// before the next draw ever showed it.
    pub fn set_files_viewport_height(&mut self, height: usize) {
        let height = height.max(1);
        if height == self.files_viewport_height {
            return;
        }
        self.files_viewport_height = height;
        self.clamp_files_scroll();
    }

    fn clamp_files_scroll(&mut self) {
        self.files_scroll_offset = scroll::clamp_scroll(
            self.files_selection,
            self.files_viewport_height,
            self.files_scroll_offset,
            |_| 1,
        );
    }

    /// Keeps `files_selection` following wherever the diff cursor currently
    /// sits, but only while `Diff` owns focus (req 6) — the sidebar reading
    /// "whatever file the reviewer is scrolled to" is exactly the pre-#14
    /// behavior `Self::diff_file` always provided. Once `Files` has its own
    /// focus and its own independently browsed position (req 5), letting
    /// the diff cursor keep overwriting it out from under an in-progress
    /// `j`/`k` in the sidebar would fight the reviewer's own input, so this
    /// is a no-op then. Called after every cursor-moving path that doesn't
    /// already funnel through [`Self::update`]'s own tail call to this —
    /// [`Self::jump_cursor_to`], [`Self::jump_to_diagnostic`],
    /// [`Self::cancel_search`], [`Self::recompute_search_live`].
    /// [`Self::resolve_files_selection`]'s callers (refresh/scope-swap/
    /// unit-filter) deliberately do *not* call this too — they resolve by
    /// `NodeId`/fallback instead, a stronger rule this would only interfere
    /// with. As of issue #15, "wherever the diff cursor sits" may resolve to
    /// the cursor's own file row, or (when a collapsed directory hides it)
    /// the nearest visible ancestor directory — see
    /// [`Self::resolve_and_set_selection`], the same resolution
    /// [`Self::confirm_files_selection`]'s `Toggled` outcome and
    /// [`Self::toggle_directory`] itself both go through.
    fn sync_files_selection(&mut self) {
        if self.focus == MainPaneFocus::Diff {
            self.resolve_and_set_selection(None);
            self.clamp_files_scroll();
        }
    }

    /// [`file_tree::NodeId`] for `files[file_idx]`'s own (never a directory)
    /// row — what [`Self::resolve_and_set_selection`] resolves against as a
    /// fallback candidate, and what [`Self::confirm_files_selection`] jumps
    /// to for a selected file row. `None` only for an out-of-range
    /// `file_idx` (an empty diff's `Self::diff_file`, which is always `0`
    /// and never a valid index into an empty `files`).
    fn file_node_id(&self, file_idx: usize) -> Option<NodeId> {
        self.files.get(file_idx).map(|f| NodeId {
            path: f.display_path().to_owned(),
            is_directory: false,
        })
    }

    /// Resolves `previous` (falling back to wherever the diff cursor's own
    /// file currently sits) against the current `visible_rows`, via
    /// [`file_tree::resolve_selection`], and sets `files_selection` to the
    /// resulting row's index — or `0` if nothing resolves at all (an empty
    /// `visible_rows`; `resolve_selection` itself already returns `None`
    /// then). Deliberately leaves scroll untouched: [`Self::resolve_files_selection`]
    /// (refresh/scope-swap/unit-filter) resets it to `0` afterward — those
    /// operations can shift *everything*, so re-centering is the right
    /// call — while [`Self::sync_files_selection`]/[`Self::toggle_directory`]
    /// only clamp, since neither should yank a deliberately-scrolled sidebar
    /// back to the top over what's typically a small, local change.
    fn resolve_and_set_selection(&mut self, previous: Option<NodeId>) {
        let fallback = self.file_node_id(self.diff_file());
        let resolved =
            file_tree::resolve_selection(&self.visible_rows, previous.as_ref(), fallback.as_ref());
        self.files_selection = resolved
            .and_then(|id| self.visible_rows.iter().position(|row| row.id == id))
            .unwrap_or(0);
    }

    /// Re-anchors `files_selection` (and resets/clamps its scroll) after an
    /// operation that just rebuilt `visible_rows` out from under it — a
    /// watch refresh, a scope swap, or a unit-filter toggle (req 10). Tries
    /// to keep browsing the *same node* first, identified by
    /// [`file_tree::NodeId`] and captured by the caller *before*
    /// `visible_rows` was rebuilt (the sidebar's whole point is a browsing
    /// position independent of the diff cursor, so an operation that keeps
    /// the node present — or at least a visible ancestor of it, once a
    /// collapsed directory can hide it — should keep the selection there
    /// too, even though a node's index can shift); falls back to wherever
    /// the diff cursor landed otherwise, via [`Self::resolve_and_set_selection`].
    fn resolve_files_selection(&mut self, previous: Option<NodeId>) {
        self.resolve_and_set_selection(previous);
        self.files_scroll_offset = 0;
        self.clamp_files_scroll();
    }

    /// The visible-row index that best represents wherever the diff cursor
    /// currently sits — an exact match for that file's own row when it's
    /// visible, or its nearest visible ancestor directory when a collapsed
    /// directory hides it. Computed independently of `files_selection`
    /// (which may have diverged from the diff cursor while `Files` owns its
    /// own focus — see that field's docs) so [`crate::ui::sidebar::render`]
    /// can still mark the diff's "background" file distinctly even then.
    /// `None` only on an empty diff (`visible_rows` itself empty).
    pub fn diff_file_visible_row(&self) -> Option<usize> {
        let fallback = self.file_node_id(self.diff_file())?;
        let resolved = file_tree::resolve_selection(&self.visible_rows, None, Some(&fallback));
        resolved.and_then(|id| self.visible_rows.iter().position(|row| row.id == id))
    }

    /// `Space`/`Enter`'s effect on a selected directory row (issue #15):
    /// flips its collapsed state, re-flattens `visible_rows` against the
    /// updated `collapsed_dirs`, and re-anchors `files_selection` the same
    /// way [`Self::resolve_files_selection`] does elsewhere — capturing the
    /// *current* selection as `previous` before the flip is what makes req
    /// 7 ("collapsing a directory that contains the selection moves
    /// selection to that directory") fall out for free: if the selection
    /// was a descendant that just got hidden, [`file_tree::resolve_selection`]'s
    /// ancestor-walk tier lands it on `path` itself (now the nearest
    /// visible ancestor) without this method needing to special-case that
    /// at all. Only clamps scroll rather than resetting it — see
    /// [`Self::resolve_and_set_selection`]'s docs on why.
    pub fn toggle_directory(&mut self, path: &str) {
        let previous = self
            .visible_rows
            .get(self.files_selection)
            .map(|row| row.id.clone());
        if !self.collapsed_dirs.remove(path) {
            self.collapsed_dirs.insert(path.to_owned());
        }
        self.visible_rows = file_tree::flatten_visible(&self.tree, &self.collapsed_dirs);
        self.resolve_and_set_selection(previous);
        self.clamp_files_scroll();
    }

    /// The row index a `(target_file, target_line)` jump should land on
    /// within this diff, if it has one — how a go-to-definition/references
    /// jump (or `Ctrl-o`/`Ctrl-i`) decides to move the cursor within the
    /// diff already being reviewed instead of pushing a new `FileView` on
    /// top of it. `target_file` is compared as an absolute path, the same
    /// coordinate space [`Self::hover_query`] reports; `target_line` is
    /// `None` for a structural target (a diff file header) — see
    /// [`crate::ui::navigation::JumpEntry::line`]'s docs.
    ///
    /// A thin wrapper around [`refresh::locate_in_diff`] /
    /// [`refresh::locate_exact_in_diff`], which do the actual row search in
    /// display-path space: strips `repo_root` off `target_file` (its
    /// absence — a target outside this repo entirely — simply can't match
    /// anything here, same as before) and hands the rest straight through.
    ///
    /// `drift_tolerant` picks the lookup: a history return (`Ctrl-o` back
    /// to a definition a few lines have since shifted away from) should
    /// still land close by via the nearest-line tier rather than silently
    /// push a redundant `FileView` over content already on screen; a fresh
    /// definition/reference jump must *not* get that tolerance — its target
    /// line was just resolved by the server, and if this diff doesn't
    /// render it, landing on the numerically-nearest rendered row (possibly
    /// an unrelated hunk far away) would silently show the reviewer the
    /// wrong code. `None` from the exact lookup makes
    /// [`crate::ui::navigation::navigate_to`] open the real file instead.
    pub fn row_for_target(
        &self,
        target_file: &Path,
        target_line: Option<u32>,
        drift_tolerant: bool,
    ) -> Option<usize> {
        let display_path = target_file.strip_prefix(&self.repo_root).ok()?;
        let display_path = display_path.display().to_string();
        if drift_tolerant {
            refresh::locate_in_diff(&self.files, &self.rows, &display_path, target_line)
        } else {
            refresh::locate_exact_in_diff(&self.files, &self.rows, &display_path, target_line)
        }
    }

    /// Which symbol on the cursor's *current* row (see `Self::cursor_row_text`)
    /// contains `display_col`, and whether one actually does — the shared
    /// lookup behind [`Self::jump_cursor_to`] (keyboard `gd`/`gr`/`]d`/`[d`
    /// landing) and [`Self::position_cursor_from_click`] (issue #22's mouse
    /// landing). Falls back to symbol `0` when nothing matches, the same
    /// fallback `jump_cursor_to` always used — a whitespace/gutter click (or
    /// a `Ctrl-o` return a few columns off from where the symbol used to be)
    /// still leaves *some* active symbol selected rather than an
    /// out-of-range index — but also reports whether the fallback fired,
    /// which `jump_cursor_to`'s callers never needed to know and a mouse
    /// click does: `hover_query` alone can't tell a click that actually
    /// landed on a symbol apart from `active_symbol` merely sitting on `0`
    /// by default (see `position_cursor_from_click`'s docs).
    fn resolve_active_symbol(&self, display_col: usize) -> (usize, bool) {
        let symbols = self
            .cursor_row_text()
            .map(symbols::scan)
            .unwrap_or_default();
        match symbols
            .iter()
            .position(|s| s.display_start <= display_col && display_col < s.display_end)
        {
            Some(idx) => (idx, true),
            None => (0, false),
        }
    }

    /// Moves the cursor to `row_idx` and, if the active symbol at that row
    /// covers `display_col`, selects it — the destination of a
    /// go-to-definition/references jump, or of `]d`/`[d`. Centers the
    /// scroll offset (rather than the minimal nudge ordinary cursor
    /// movement uses) so a jump's destination lands with surrounding
    /// context visible, not pinned to the viewport's edge.
    /// Moves the cursor to `row_idx`, and — issue #14's plan decision 3 —
    /// puts `Diff` in focus regardless of what it was before: every caller
    /// (go-to-definition/references, `Ctrl-o`/`Ctrl-i`, search, diagnostic
    /// stepping) is landing the cursor on a specific line of code, which
    /// only makes sense to look at with `Diff` focused. In practice every
    /// other caller can only ever run while `Diff` already is focused (the
    /// files-focus gate in `ui::mod::handle_action` blocks the actions that
    /// reach them), so this only has real work to do for a `Ctrl-o`/
    /// `Ctrl-i` history return landing back inside the diff while `Files`
    /// happened to be focused — but a single unconditional assignment here
    /// is simpler than threading that one case through every caller.
    pub fn jump_cursor_to(&mut self, row_idx: usize, display_col: usize) {
        self.focus = MainPaneFocus::Diff;
        self.cursor = row_idx.min(self.rows.len().saturating_sub(1));
        self.active_symbol = self.resolve_active_symbol(display_col).0;
        self.scroll_offset = scroll::center(self.cursor, self.viewport_height, |i| {
            self.row_visual_height(i)
        });
        self.clamp_scroll();
        self.sync_files_selection();
    }

    /// As [`Self::jump_cursor_to`], for a mouse click (issue #22) — same
    /// cursor/focus/active-symbol resolution and `sync_files_selection`
    /// call, but deliberately *without* `scroll::center`: the clicked row is
    /// already on screen (that's how the reviewer clicked it), so
    /// recentering would visibly yank it to mid-viewport for no reason —
    /// `clamp_scroll` alone still keeps a wrapped multi-row cursor fully
    /// visible without moving anything else already in view. This is a
    /// deliberate divergence from `jump_cursor_to`, pinned by
    /// `position_cursor_from_click_never_recenters_unlike_jump_cursor_to`
    /// below rather than left to bit-rot silently if a future refactor tries
    /// to unify the two.
    ///
    /// Returns whether the click actually landed *on* a symbol (see
    /// `Self::resolve_active_symbol`) — `ui::mouse`'s click orchestration
    /// uses this, not `Self::hover_query`, to decide whether an identifier
    /// click should chase go-to-definition: `hover_query` would happily
    /// return `Some` for a whitespace click that fell back to symbol `0`,
    /// as long as symbol `0` itself is LSP-eligible.
    pub fn position_cursor_from_click(&mut self, row_idx: usize, display_col: usize) -> bool {
        self.focus = MainPaneFocus::Diff;
        self.cursor = row_idx.min(self.rows.len().saturating_sub(1));
        let (active_symbol, matched) = self.resolve_active_symbol(display_col);
        self.active_symbol = active_symbol;
        self.clamp_scroll();
        self.sync_files_selection();
        matched
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
        self.sync_files_selection();
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

    /// Issue #20: the wheel's whole vocabulary for the diff pane — moves
    /// `scroll_offset` directly by `delta` visual rows without touching
    /// `self.cursor` at all, so scrolling the pane under the pointer can
    /// never disturb wherever the reviewer's own keyboard navigation left
    /// the cursor (req 5: works "regardless of which pane owns keyboard
    /// focus," and must not move a *different* pane). Deliberately doesn't
    /// call [`Self::clamp_scroll`] afterward — that function derives
    /// `scroll_offset` from `self.cursor`, which would just undo the wheel
    /// movement. The next cursor-moving action runs it anyway, pulling the
    /// offset back to wherever the cursor needs it; a wheel-only session is
    /// allowed to leave the cursor's row scrolled off screen in the
    /// meantime, the same as any ordinary scrollable document viewer.
    pub fn scroll_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last_row = self.rows.len() - 1;
        self.scroll_offset = scroll::scroll_by(
            self.scroll_offset,
            delta,
            last_row,
            self.viewport_height,
            |i| self.row_visual_height(i),
        );
    }

    /// Reapplies a captured [`refresh::scroll_delta`] once a restored
    /// cursor row is known — the shared tail of [`Self::apply_refresh`],
    /// [`Self::cancel_search`], and [`Self::recompute_search_live`]'s
    /// no-match branch. The clamp depends on the delta's sign: a
    /// non-negative delta is the ordinary cursor-coupled state and keeps
    /// the pre-existing [`Self::clamp_scroll`] (cursor stays visible); a
    /// negative one is the wheel-decoupled state ([`Self::scroll_by`] ran
    /// the viewport past the cursor), where cursor-clamping would snap the
    /// view back onto a cursor the reviewer deliberately scrolled away
    /// from — there only the content-bounds clamp applies, via
    /// `scroll_by(0)`.
    fn restore_scroll_from_delta(&mut self, row_index: usize, delta: isize) {
        self.scroll_offset = (row_index as isize - delta).max(0) as usize;
        if delta >= 0 {
            self.clamp_scroll();
        } else {
            self.scroll_by(0);
        }
    }

    /// As [`Self::scroll_by`], for the files pane: moves
    /// `files_scroll_offset` without touching `files_selection`, regardless
    /// of which pane currently owns keyboard focus and independent of it —
    /// a wheel over the sidebar scrolls the sidebar even while `Diff` owns
    /// focus, req 5's "wheel scrolls the pane under the pointer... does not
    /// steal keyboard focus" in one method. Files-pane rows never wrap (see
    /// [`Self::files_viewport_height`]'s docs), so every row is `1`.
    pub fn scroll_files_by(&mut self, delta: isize) {
        if self.visible_rows.is_empty() {
            return;
        }
        let last_row = self.visible_rows.len() - 1;
        self.files_scroll_offset = scroll::scroll_by(
            self.files_scroll_offset,
            delta,
            last_row,
            self.files_viewport_height,
            |_| 1,
        );
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
                self.restore_scroll_from_delta(origin_row, refresh::scroll_delta(origin));
                self.active_symbol = 0;
            }
        }
        self.sync_files_selection();
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
        self.restore_scroll_from_delta(restored.row_index, refresh::scroll_delta(origin));
        self.active_symbol = 0;
        self.search = None;
        self.sync_files_selection();
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
/// [`App::rederive`]'s unit-filter pass: keeps only hunks whose content id
/// is in `ids`, then drops files left with nothing to show. Ids are
/// recomputed against the *derived* (fold-spliced) files each time rather
/// than cached — [`crate::groups::enumerate_hunks`] hashes only changed
/// rows, so an expanded fold's extra context rows don't move a hunk's id,
/// while a hunk whose actual changes were edited correctly falls out of
/// the unit (its content is no longer what the grouping described). The
/// rare surprise this accepts: an expansion big enough to merge two hunks
/// concatenates their changed rows into one new id, which drops the merged
/// hunk from the unit until the filter is cleared.
fn apply_unit_filter(files: &mut Vec<DiffFile>, ids: &HashSet<String>) {
    let keep: HashSet<(usize, usize)> = crate::groups::enumerate_hunks(files)
        .into_iter()
        .filter(|meta| ids.contains(&meta.id))
        .map(|meta| (meta.file_idx, meta.hunk_idx))
        .collect();
    for (file_idx, file) in files.iter_mut().enumerate() {
        let mut hunk_idx = 0;
        file.hunks.retain(|_| {
            let kept = keep.contains(&(file_idx, hunk_idx));
            hunk_idx += 1;
            kept
        });
    }
    // Binary files carry no hunks, so they can never belong to a unit and
    // fall away here along with fully filtered-out text files.
    files.retain(|file| !file.hunks.is_empty());
}

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

    /// The display path of whichever `visible_rows` node `files_selection`
    /// currently points at — `None` for an out-of-range index (an empty
    /// tree). Issue #15 made `files_selection` an index into the *tree's*
    /// flattened rows rather than `files` directly (a directory row has no
    /// `files` counterpart at all), so tests that want to assert "the
    /// sidebar is browsing file X" read through this rather than
    /// `app.files[app.files_selection]`, which — now that a leading
    /// directory row can shift every file's row index — no longer names the
    /// same file `files_selection`'s value would suggest.
    fn selected_path(app: &App) -> Option<&str> {
        Some(app.visible_rows.get(app.files_selection)?.id.path.as_str())
    }

    /// `files_selection`'s value re-expressed as a `file_idx` into `files`,
    /// for tests that want to compare it against [`App::diff_file`] — the
    /// pre-#15 relationship "`files_selection == diff_file()`" only held
    /// because every row *was* a file row; #15's directory rows break that,
    /// so this looks the file up through `visible_rows` instead. `None` for
    /// a directory row or an out-of-range index.
    fn selected_file_idx(app: &App) -> Option<usize> {
        match app.visible_rows.get(app.files_selection)?.kind {
            file_tree::VisibleKind::File { file_idx } => Some(file_idx),
            file_tree::VisibleKind::Directory { .. } => None,
        }
    }

    #[test]
    fn unit_filter_scopes_rows_and_files_to_the_unit_and_clear_widens_back() {
        let mut app = test_app();
        let full_file_count = app.files.len();
        let full_row_count = app.rows.len();
        assert!(full_file_count >= 2, "fixture must span files");
        app.update(Action::CursorDown);
        app.update(Action::CursorDown);

        let first_hunk_id = crate::groups::enumerate_hunks(&app.files)[0].id.clone();
        app.set_unit_filter(UnitFilter {
            label: "first hunk only".to_owned(),
            description: String::new(),
            index: 1,
            total: 2,
            hunk_ids: HashSet::from([first_hunk_id]),
        });

        assert_eq!(app.files.len(), 1, "sidebar narrows with the scope");
        assert_eq!(app.files[0].hunks.len(), 1);
        assert!(app.rows.len() < full_row_count);
        assert_eq!(
            (app.cursor, app.scroll_offset),
            (0, 0),
            "a unit reads from its top"
        );
        assert_eq!(
            app.full_files().len(),
            full_file_count,
            "grouping lookups must still see the whole diff"
        );

        assert!(app.clear_unit_filter());
        assert_eq!(app.files.len(), full_file_count);
        assert_eq!(app.rows.len(), full_row_count);
        assert!(
            !app.clear_unit_filter(),
            "second clear has nothing left to do"
        );
    }

    #[test]
    fn a_unit_filter_matching_nothing_auto_clears_instead_of_blanking_the_view() {
        let mut app = test_app();
        let full_row_count = app.rows.len();
        app.set_unit_filter(UnitFilter {
            label: "stale".to_owned(),
            description: String::new(),
            index: 1,
            total: 1,
            hunk_ids: HashSet::from(["feedbeef00000000".to_owned()]),
        });
        assert!(
            app.unit_filter().is_none(),
            "unresolvable scope must self-clear"
        );
        assert_eq!(app.rows.len(), full_row_count);
    }

    #[test]
    fn a_scope_swap_drops_the_unit_filter() {
        let mut app = test_app();
        let id = crate::groups::enumerate_hunks(&app.files)[0].id.clone();
        app.set_unit_filter(UnitFilter {
            label: "unit".to_owned(),
            description: String::new(),
            index: 1,
            total: 1,
            hunk_ids: HashSet::from([id]),
        });
        assert!(app.unit_filter().is_some());

        app.apply_scope_swap(parse_unified_diff(FIXTURE), true, false, None);
        assert!(
            app.unit_filter().is_none(),
            "a new scope's diff is unrelated to the grouping's"
        );
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

    // ---- resolve_active_symbol / position_cursor_from_click (issue #22) --

    #[test]
    fn resolve_active_symbol_matches_the_symbol_a_column_falls_within() {
        let mut app = test_app();
        app.update(Action::Top);
        app.update(Action::CursorDown); // hunk header
        app.update(Action::CursorDown); // row 2: "fn helper() {}"

        // "fn" spans [0, 2); "helper" spans [3, 9) — pick a column inside
        // each, not just its start, to prove this is a real range match.
        assert_eq!(app.resolve_active_symbol(1), (0, true));
        assert_eq!(app.resolve_active_symbol(5), (1, true));
    }

    #[test]
    fn resolve_active_symbol_falls_back_to_symbol_zero_on_whitespace() {
        let mut app = test_app();
        app.update(Action::Top);
        app.update(Action::CursorDown);
        app.update(Action::CursorDown); // row 2: "fn helper() {}"

        // Column 2 is the space between "fn" and "helper" — no symbol
        // covers it, so this falls back to symbol 0 rather than matching,
        // and reports that it did (the `false` half of the pair) — the one
        // thing `hover_query` alone can't tell apart from a real match on
        // symbol 0 itself.
        assert_eq!(app.resolve_active_symbol(2), (0, false));
    }

    #[test]
    fn position_cursor_from_click_moves_the_cursor_and_reports_whether_it_matched() {
        let mut app = test_app();
        let matched = app.position_cursor_from_click(4, 3); // "new_name" on the add row
        assert!(matched);
        assert_eq!(app.cursor, 4);
        assert_eq!(app.active_symbol, 1);
        assert_eq!(app.focus, MainPaneFocus::Diff);

        let matched = app.position_cursor_from_click(4, 2); // whitespace before "new_name"
        assert!(!matched);
        assert_eq!(
            app.active_symbol, 0,
            "falls back to symbol 0, same as jump_cursor_to"
        );
    }

    #[test]
    fn position_cursor_from_click_never_recenters_unlike_jump_cursor_to() {
        // A target row with plenty of rows both above and below it, and a
        // small enough viewport, that `scroll::center` would actually
        // settle on a different offset than clamping alone would — proving
        // the divergence needs a case where the two methods visibly
        // disagree, not just one (e.g. the very first/last row) where
        // clamping happens to produce the same answer either way.
        let mut app = test_app();
        app.set_viewport_height(6);
        assert!(
            app.rows.len() >= 12,
            "fixture must be tall enough for a real mid-diff target"
        );
        let target = app.rows.len() / 2;

        let mut centered = App::new(
            app.repo_name.clone(),
            app.repo_root.clone(),
            app.files.clone(),
        );
        centered.set_viewport_height(6);
        centered.jump_cursor_to(target, 0);

        app.position_cursor_from_click(target, 0);

        assert_eq!(app.cursor, centered.cursor, "both land on the same row");
        assert_ne!(
            app.scroll_offset, centered.scroll_offset,
            "position_cursor_from_click must not recenter the way jump_cursor_to does"
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

    // ---- scroll_by / scroll_files_by (issue #20 wheel routing) --------

    #[test]
    fn scroll_by_moves_the_offset_without_touching_the_cursor() {
        let mut app = test_app();
        app.set_viewport_height(3);
        app.update(Action::Bottom);
        let cursor_before = app.cursor;
        let offset_before = app.scroll_offset;
        app.scroll_by(-1);
        assert_eq!(
            app.cursor, cursor_before,
            "wheel scroll never moves the cursor"
        );
        assert_eq!(app.scroll_offset, offset_before.saturating_sub(1));
    }

    #[test]
    fn scroll_by_clamps_at_the_top() {
        let mut app = test_app();
        app.set_viewport_height(3);
        assert_eq!(app.scroll_offset, 0);
        app.scroll_by(-5);
        assert_eq!(app.scroll_offset, 0, "can't scroll above the top");
    }

    #[test]
    fn scroll_by_clamps_at_the_last_useful_offset() {
        let mut app = test_app();
        app.set_viewport_height(3);
        app.scroll_by(1000);
        let maxed = app.scroll_offset;
        app.scroll_by(1000);
        assert_eq!(
            app.scroll_offset, maxed,
            "already at the furthest useful offset — nothing more to scroll"
        );
    }

    #[test]
    fn scroll_by_is_a_no_op_on_an_empty_diff() {
        let mut app = App::new("empty".to_owned(), PathBuf::from("/repo"), Vec::new());
        app.set_viewport_height(3);
        app.scroll_by(5);
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn a_wheel_scrolled_viewport_survives_a_refresh_of_unchanged_content() {
        // The wheel-decoupled state must round-trip through the anchor:
        // before the signed `scroll_delta`, `capture_anchor` saturated
        // `cursor - scroll_offset` to 0 here and a background watch
        // refresh pinned the cursor's row to the top of the viewport,
        // silently discarding the position the reviewer had wheeled to.
        let mut app = test_app();
        app.set_viewport_height(3);
        // Cursor stays near the top; wheel runs the viewport well past it.
        app.scroll_by(1000);
        let offset_before = app.scroll_offset;
        assert!(
            offset_before > app.cursor,
            "sanity: this test needs the decoupled state"
        );

        let same_files = parse_unified_diff(FIXTURE);
        app.apply_refresh(same_files);

        assert_eq!(
            app.scroll_offset, offset_before,
            "an unchanged-content refresh must not move a wheeled viewport"
        );
        assert!(app.scroll_offset > app.cursor, "still decoupled");
    }

    #[test]
    fn scroll_by_offset_self_heals_on_the_next_cursor_move() {
        let mut app = test_app();
        app.set_viewport_height(3);
        app.update(Action::Bottom);
        let settled_offset = app.scroll_offset;
        app.scroll_by(-3);
        assert_ne!(
            app.scroll_offset, settled_offset,
            "the wheel scroll actually moved the viewport"
        );
        // Already at the last row, so `CursorDown` doesn't move `cursor` —
        // but `Self::update` runs `clamp_scroll` unconditionally at its
        // tail, which pulls `scroll_offset` right back to the cursor's row.
        app.update(Action::CursorDown);
        assert_eq!(
            app.scroll_offset, settled_offset,
            "the next cursor-moving action self-heals the scroll offset"
        );
    }

    #[test]
    fn scroll_files_by_moves_the_files_offset_without_touching_selection() {
        let mut app = test_app();
        app.set_files_viewport_height(1);
        assert!(
            app.visible_rows.len() > 1,
            "fixture must have more file-tree rows than the viewport"
        );
        let selection_before = app.files_selection;
        // A one-row viewport already needed to scroll to keep whatever
        // `files_selection` started on visible — read the real starting
        // offset back rather than assuming zero, the same way
        // `scroll_by_moves_the_offset_without_touching_the_cursor` above
        // reads `scroll_offset` before asserting a relative delta.
        let offset_before = app.files_scroll_offset;
        app.scroll_files_by(1);
        assert_eq!(
            app.files_selection, selection_before,
            "wheel scroll never moves the files selection"
        );
        assert_eq!(app.files_scroll_offset, offset_before + 1);
    }

    #[test]
    fn scroll_files_by_clamps_at_the_top() {
        let mut app = test_app();
        app.set_files_viewport_height(1);
        app.scroll_files_by(-5);
        assert_eq!(app.files_scroll_offset, 0);
    }

    #[test]
    fn scroll_files_by_clamps_at_the_last_useful_offset() {
        let mut app = test_app();
        app.set_files_viewport_height(1);
        app.scroll_files_by(1000);
        let maxed = app.files_scroll_offset;
        app.scroll_files_by(1000);
        assert_eq!(app.files_scroll_offset, maxed);
    }

    #[test]
    fn scroll_files_by_is_a_no_op_on_an_empty_diff() {
        let mut app = App::new("empty".to_owned(), PathBuf::from("/repo"), Vec::new());
        app.set_files_viewport_height(1);
        app.scroll_files_by(5);
        assert_eq!(app.files_scroll_offset, 0);
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

    // ---- row_for_target ---------------------------------------------------

    #[test]
    fn row_for_target_finds_the_row_matching_file_and_line() {
        let app = test_app();
        // "fn new_name() {}" is src/lib.rs's new_line 2, 0-based line 1 —
        // see `hover_query_targets_the_active_symbol_on_an_add_row` above.
        let idx = app
            .row_for_target(Path::new("/repo/src/lib.rs"), Some(1), false)
            .expect("src/lib.rs line 1 is in this diff");
        let RenderRow::Line { file_idx, .. } = app.rows[idx] else {
            panic!("expected a line row");
        };
        assert_eq!(app.files[file_idx].display_path(), "src/lib.rs");
    }

    #[test]
    fn row_for_target_with_no_line_lands_on_the_files_header_row() {
        let app = test_app();
        let idx = app
            .row_for_target(Path::new("/repo/src/lib.rs"), None, false)
            .expect("src/lib.rs is in this diff");
        assert!(matches!(app.rows[idx], RenderRow::FileHeader { .. }));
    }

    #[test]
    fn row_for_target_outside_the_repo_root_or_diff_is_none() {
        let app = test_app();
        // Not under `/repo` at all — `strip_prefix` fails before
        // `refresh::locate_in_diff` is even reached.
        assert_eq!(
            app.row_for_target(Path::new("/elsewhere/src/lib.rs"), Some(0), true),
            None
        );
        // Under `/repo`, but not a path this diff touches.
        assert_eq!(
            app.row_for_target(Path::new("/repo/src/untouched.rs"), Some(0), true),
            None
        );
    }

    #[test]
    fn row_for_target_reserves_the_nearest_fallback_for_history_returns() {
        let app = test_app();
        // 0-based line 50 is nowhere in this diff's hunks: a fresh jump
        // must report None (and open the real file), while a history
        // return may settle for the nearest remaining row of the file.
        assert_eq!(
            app.row_for_target(Path::new("/repo/src/lib.rs"), Some(50), false),
            None
        );
        let idx = app
            .row_for_target(Path::new("/repo/src/lib.rs"), Some(50), true)
            .expect("drift-tolerant lookup lands on the nearest row");
        // The nearest row may be a `Line` or a fold `Gap` (a gap carries
        // its `new_start` — see `refresh::row_line_and_text`); what matters
        // is that it belongs to the requested file at all.
        let file_idx = match app.rows[idx] {
            RenderRow::Line { file_idx, .. } | RenderRow::Gap { file_idx, .. } => file_idx,
            other => panic!("expected a line or gap row, got {other:?}"),
        };
        assert_eq!(app.files[file_idx].display_path(), "src/lib.rs");
    }

    // ---- Issue #14: main-pane focus -------------------------------------

    fn one_file_app() -> App {
        let files = parse_unified_diff(
            "diff --git a/a.rs b/a.rs\n\
             index 1111111..2222222 100644\n\
             --- a/a.rs\n\
             +++ b/a.rs\n\
             @@ -1,2 +1,2 @@\n\
             -old\n\
             +new\n\
             \x20unchanged\n",
        );
        let mut app = App::new("repo".to_owned(), PathBuf::from("/repo"), files);
        app.set_viewport_height(10);
        app
    }

    #[test]
    fn focus_defaults_to_diff() {
        assert_eq!(test_app().focus, MainPaneFocus::Diff);
    }

    #[test]
    fn hiding_the_sidebar_while_files_focused_moves_focus_to_diff() {
        let mut app = test_app();
        app.focus = MainPaneFocus::Files;
        app.update(Action::ToggleSidebar);
        assert!(!app.sidebar_visible);
        assert_eq!(
            app.focus,
            MainPaneFocus::Diff,
            "hiding the sidebar can never leave focus on an invisible pane"
        );
    }

    #[test]
    fn showing_the_sidebar_never_steals_focus_from_diff() {
        let mut app = test_app();
        app.update(Action::ToggleSidebar); // hide it first
        assert_eq!(app.focus, MainPaneFocus::Diff);
        app.update(Action::ToggleSidebar); // show it again
        assert!(app.sidebar_visible);
        assert_eq!(
            app.focus,
            MainPaneFocus::Diff,
            "showing the sidebar must not grab focus away from Diff"
        );
    }

    #[test]
    fn focus_next_and_prev_pane_cycle_files_and_diff_and_wrap() {
        let mut app = test_app();
        assert_eq!(app.focus, MainPaneFocus::Diff);
        app.update(Action::FocusNextPane);
        assert_eq!(app.focus, MainPaneFocus::Files);
        app.update(Action::FocusNextPane);
        assert_eq!(app.focus, MainPaneFocus::Diff, "forward must wrap");
        app.update(Action::FocusPrevPane);
        assert_eq!(app.focus, MainPaneFocus::Files, "backward must wrap too");
        app.update(Action::FocusPrevPane);
        assert_eq!(app.focus, MainPaneFocus::Diff);
    }

    #[test]
    fn focus_cycling_is_a_no_op_when_the_sidebar_is_hidden() {
        let mut app = test_app();
        app.update(Action::ToggleSidebar);
        assert!(!app.sidebar_visible);
        app.update(Action::FocusNextPane);
        assert_eq!(
            app.focus,
            MainPaneFocus::Diff,
            "Files isn't visible, so there's nothing else to cycle to"
        );
    }

    #[test]
    fn focus_cycling_still_works_on_a_one_file_diff() {
        // req 9's no-op is specifically about a *hidden sidebar*, not a
        // short files list — a one-file diff's sidebar is still a real,
        // focusable pane.
        let mut app = one_file_app();
        assert!(app.sidebar_visible);
        app.update(Action::FocusNextPane);
        assert_eq!(app.focus, MainPaneFocus::Files);
    }

    #[test]
    fn an_empty_diff_never_panics_on_focus_actions_and_stays_on_diff() {
        // req 9: "empty diffs ... remain stable." `App::update` returns
        // before its own match on an empty `rows` (nothing to review at
        // all — see its own early-return docs), so `Files`/`Diff` never
        // actually differ in practice here; this pins down that the early
        // return doesn't panic reaching for `self.files.len() - 1` or
        // similar and that focus simply stays at its default.
        let mut app = App::new("repo".to_owned(), PathBuf::from("/repo"), Vec::new());
        assert!(app.rows.is_empty());
        app.update(Action::FocusNextPane);
        app.update(Action::FocusPrevPane);
        app.update(Action::ToggleSidebar);
        assert_eq!(app.focus, MainPaneFocus::Diff);
    }

    #[test]
    fn files_focused_movement_never_touches_the_diff_cursor() {
        let mut app = test_app();
        let cursor_before = app.cursor;
        app.focus = MainPaneFocus::Files;
        app.update(Action::CursorDown);
        app.update(Action::CursorDown);
        app.update(Action::Bottom);
        app.update(Action::Top);
        app.update(Action::HalfPageDown);
        app.update(Action::HalfPageUp);
        assert_eq!(
            app.cursor, cursor_before,
            "Files-focused movement must only move files_selection"
        );
    }

    #[test]
    fn files_focused_cursor_down_and_up_move_and_clamp_the_selection() {
        let mut app = test_app();
        app.focus = MainPaneFocus::Files;
        // One extra row over `files.len()` for the fixture's single "src"
        // directory row (issue #15) — every file in `FIXTURE` shares that
        // one top-level prefix.
        let last = app.visible_rows.len() - 1;
        app.update(Action::CursorUp); // already at 0: must not underflow
        assert_eq!(app.files_selection, 0);
        for _ in 0..app.visible_rows.len() + 2 {
            app.update(Action::CursorDown);
        }
        assert_eq!(app.files_selection, last, "must clamp at the last row");
        app.update(Action::Top);
        assert_eq!(app.files_selection, 0);
        app.update(Action::Bottom);
        assert_eq!(app.files_selection, last);
    }

    #[test]
    fn hunk_and_file_navigation_are_no_ops_while_files_is_focused() {
        let mut app = test_app();
        app.focus = MainPaneFocus::Files;
        let cursor_before = app.cursor;
        let selection_before = app.files_selection;
        for action in [
            Action::NextHunk,
            Action::PrevHunk,
            Action::NextFile,
            Action::PrevFile,
        ] {
            app.update(action);
        }
        assert_eq!(app.cursor, cursor_before);
        assert_eq!(app.files_selection, selection_before);
    }

    #[test]
    fn next_and_prev_symbol_are_no_ops_while_files_is_focused() {
        let mut app = test_app();
        app.focus = MainPaneFocus::Files;
        let symbol_before = app.active_symbol;
        app.update(Action::NextSymbol);
        app.update(Action::PrevSymbol);
        assert_eq!(app.active_symbol, symbol_before);
    }

    #[test]
    fn files_selection_follows_the_diff_cursor_while_diff_is_focused() {
        let mut app = test_app();
        assert_eq!(app.focus, MainPaneFocus::Diff);
        app.update(Action::NextFile);
        assert_eq!(
            selected_file_idx(&app),
            Some(app.diff_file()),
            "the sidebar must track wherever the diff cursor lands"
        );
        app.update(Action::NextFile);
        assert_eq!(selected_file_idx(&app), Some(app.diff_file()));
    }

    #[test]
    fn files_selection_diverges_from_diff_file_while_files_is_focused() {
        let mut app = test_app();
        app.update(Action::NextFile); // diff cursor -> file 1
        let diff_file_before = app.diff_file();
        app.focus = MainPaneFocus::Files;
        app.files_selection = diff_file_before; // start in sync
        app.update(Action::CursorDown); // moves only files_selection
        app.update(Action::CursorDown);
        assert_ne!(
            app.files_selection, diff_file_before,
            "browsing the sidebar must not drag the diff cursor's file along"
        );
        assert_eq!(
            app.diff_file(),
            diff_file_before,
            "the diff cursor itself never moved"
        );
    }

    #[test]
    fn set_files_viewport_height_clamps_a_scrolled_past_offset() {
        let mut app = test_app();
        app.focus = MainPaneFocus::Files;
        app.update(Action::Bottom); // files_selection = last file
        app.set_files_viewport_height(1);
        assert!(
            app.files_scroll_offset <= app.files_selection,
            "the scroll offset must never sit past the selection"
        );
    }

    #[test]
    fn refresh_preserves_the_files_selection_by_path_when_it_survives() {
        let mut app = test_app();
        app.files_selection = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "src/new_module.rs")
            .unwrap();
        let same_files = parse_unified_diff(FIXTURE);
        app.apply_refresh(same_files);
        assert_eq!(
            selected_path(&app),
            Some("src/new_module.rs"),
            "a refresh that keeps the file must keep browsing it"
        );
    }

    /// Issue #15's ancestor-walk tier (`file_tree::resolve_selection`) sits
    /// *ahead* of the plain fallback-to-diff-cursor tier — see that
    /// function's docs. So a vanished selection whose parent directory
    /// still exists (every other `FIXTURE` file shares the "src" prefix
    /// `old_module.rs` also had) lands on that directory, not on wherever
    /// the diff cursor happens to be. This supersedes #14's
    /// `refresh_falls_back_to_the_diff_file_when_the_selected_path_disappears`,
    /// whose "fall straight back to the diff cursor" expectation predates
    /// there being any ancestor to land on at all.
    #[test]
    fn refresh_falls_back_to_the_nearest_surviving_ancestor_directory_when_the_file_vanishes() {
        let mut app = test_app();
        app.files_selection = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "src/old_module.rs")
            .unwrap();
        let narrowed = parse_unified_diff(FIXTURE)
            .into_iter()
            .filter(|f| f.display_path() != "src/old_module.rs")
            .collect::<Vec<_>>();
        app.apply_refresh(narrowed);
        assert_eq!(
            selected_path(&app),
            Some("src"),
            "the vanished file's parent directory still exists, so the ancestor-walk \
             tier lands there ahead of falling back to the diff cursor's file"
        );
    }

    /// The plain fallback-to-diff-cursor tier only fires once the
    /// ancestor-walk tier has nothing left to land on either — a refresh
    /// whose new diff shares no path prefix with the vanished selection at
    /// all.
    #[test]
    fn refresh_falls_back_to_the_diff_file_when_no_ancestor_survives_either() {
        let mut app = test_app();
        app.files_selection = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "src/old_module.rs")
            .unwrap();
        let files = vec![DiffFile {
            new_path: Some("other.rs".to_owned()),
            old_path: Some("other.rs".to_owned()),
            ..Default::default()
        }];
        app.apply_refresh(files);
        assert_eq!(
            selected_file_idx(&app),
            Some(app.diff_file()),
            "with no ancestor to land on, the selection falls back to the diff cursor's file"
        );
    }

    #[test]
    fn scope_swap_preserves_the_files_selection_by_path_despite_resetting_the_cursor() {
        let mut app = test_app();
        app.files_selection = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "src/new_module.rs")
            .unwrap();
        app.apply_scope_swap(parse_unified_diff(FIXTURE), true, false, None);
        assert_eq!(app.cursor, 0, "the diff cursor always resets to the top");
        assert_eq!(
            selected_path(&app),
            Some("src/new_module.rs"),
            "but the files-pane selection tries to survive by path"
        );
    }

    #[test]
    fn unit_filter_re_resolves_the_files_selection_within_the_narrowed_list() {
        let mut app = test_app();
        let target_path = "src/new_module.rs".to_owned();
        app.files_selection = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == target_path)
            .unwrap();
        let hunk_id = crate::groups::enumerate_hunks(&app.files)
            .into_iter()
            .find(|meta| app.files[meta.file_idx].display_path() == target_path)
            .expect("the target file has a hunk")
            .id;
        app.set_unit_filter(UnitFilter {
            label: "one file".to_owned(),
            description: String::new(),
            index: 1,
            total: 1,
            hunk_ids: HashSet::from([hunk_id]),
        });
        assert!(
            app.files_selection < app.visible_rows.len(),
            "the selection must never point past the narrowed tree"
        );
        assert_eq!(selected_path(&app), Some(target_path.as_str()));

        assert!(app.clear_unit_filter());
        assert!(
            app.files_selection < app.visible_rows.len(),
            "widening back must also leave the selection in bounds"
        );
    }

    // ---- collapsible tree integration (issue #15) ------------------------

    /// A three-file, two-directory fixture for tests that need real
    /// collapse/expand behavior — `FIXTURE`'s files all share one `src`
    /// prefix, too flat to exercise nesting.
    fn nested_app() -> App {
        let files = vec![
            crate::diff::DiffFile {
                new_path: Some("src/a.rs".to_owned()),
                old_path: Some("src/a.rs".to_owned()),
                ..Default::default()
            },
            crate::diff::DiffFile {
                new_path: Some("src/nested/b.rs".to_owned()),
                old_path: Some("src/nested/b.rs".to_owned()),
                ..Default::default()
            },
            crate::diff::DiffFile {
                new_path: Some("other.rs".to_owned()),
                old_path: Some("other.rs".to_owned()),
                ..Default::default()
            },
        ];
        App::new("test-repo".to_owned(), PathBuf::from("/repo"), files)
    }

    #[test]
    fn toggle_directory_collapses_and_re_expands_a_row() {
        let mut app = nested_app();
        let src_idx = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "src" && r.id.is_directory)
            .unwrap();
        assert!(matches!(
            app.visible_rows[src_idx].kind,
            file_tree::VisibleKind::Directory { expanded: true, .. }
        ));
        let before_len = app.visible_rows.len();

        app.toggle_directory("src");
        assert!(matches!(
            app.visible_rows[src_idx].kind,
            file_tree::VisibleKind::Directory {
                expanded: false,
                ..
            }
        ));
        assert!(
            app.visible_rows.len() < before_len,
            "collapsing must hide src's descendants"
        );

        app.toggle_directory("src");
        assert_eq!(
            app.visible_rows.len(),
            before_len,
            "expanding must restore every descendant row"
        );
    }

    /// req 7: collapsing a directory that contains the current selection
    /// must move the selection onto that directory, not leave it pointing
    /// at a row that no longer exists.
    #[test]
    fn collapsing_a_directory_containing_the_selection_reselects_that_directory() {
        let mut app = nested_app();
        app.focus = MainPaneFocus::Files;
        let nested_file_idx = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "src/nested/b.rs")
            .unwrap();
        app.files_selection = nested_file_idx;

        app.toggle_directory("src/nested");

        assert_eq!(
            selected_path(&app),
            Some("src/nested"),
            "the selection must land on the collapsed directory"
        );
    }

    #[test]
    fn confirm_on_a_directory_row_toggles_without_moving_focus_or_the_diff_cursor() {
        let mut app = nested_app();
        app.focus = MainPaneFocus::Files;
        app.files_selection = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "src" && r.id.is_directory)
            .unwrap();
        let cursor_before = app.cursor;

        let outcome = app.confirm_files_selection();
        assert_eq!(outcome, crate::ui::navigation::FilesConfirmOutcome::Toggled);
        assert_eq!(app.focus, MainPaneFocus::Files);
        assert_eq!(app.cursor, cursor_before);
    }

    #[test]
    fn confirm_on_a_file_row_opens_it_and_focuses_diff() {
        let mut app = nested_app();
        app.focus = MainPaneFocus::Files;
        app.files_selection = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "other.rs")
            .unwrap();

        let outcome = app.confirm_files_selection();
        assert!(matches!(
            outcome,
            crate::ui::navigation::FilesConfirmOutcome::Opened(_)
        ));
        assert_eq!(app.focus, MainPaneFocus::Diff);
    }

    #[test]
    fn toggle_action_is_a_no_op_on_a_file_row_or_outside_files_focus() {
        let mut app = nested_app();
        app.focus = MainPaneFocus::Files;
        app.files_selection = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "other.rs")
            .unwrap();
        let before = app.visible_rows.len();
        app.update(Action::ToggleDirectory);
        assert_eq!(
            app.visible_rows.len(),
            before,
            "a file row has nothing to toggle"
        );

        app.focus = MainPaneFocus::Diff;
        app.files_selection = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "src" && r.id.is_directory)
            .unwrap();
        app.update(Action::ToggleDirectory);
        assert_eq!(
            app.visible_rows.len(),
            before,
            "ToggleDirectory must do nothing outside Files focus"
        );
    }

    // ---- click_files_row (issue #21) --------------------------------------

    #[test]
    fn click_files_row_on_a_directory_focuses_files_and_selects_the_clicked_row() {
        let mut app = nested_app();
        app.focus = MainPaneFocus::Diff;
        // A selection that has nothing to do with "src" — proves the click
        // moves selection to the *clicked* row, not wherever it already was.
        app.files_selection = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "other.rs")
            .unwrap();
        let src_idx = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "src" && r.id.is_directory)
            .unwrap();

        let outcome = app.click_files_row(src_idx);
        assert_eq!(outcome, crate::ui::navigation::FilesConfirmOutcome::Toggled);
        assert_eq!(app.focus, MainPaneFocus::Files);
        assert_eq!(app.files_selection, src_idx);
        assert!(matches!(
            app.visible_rows[src_idx].kind,
            file_tree::VisibleKind::Directory {
                expanded: false,
                ..
            }
        ));
    }

    /// The req-2 crux: unlike [`confirm_on_a_file_row_opens_it_and_focuses_diff`]
    /// (`Enter`, which hands focus to `Diff`), a click on a file row must
    /// leave `Files` focused so repeated clicks — or `j`/`k` right after
    /// one — keep browsing the tree without an intervening Tab.
    #[test]
    fn click_files_row_on_a_file_opens_it_and_keeps_files_focused() {
        let mut app = nested_app();
        app.focus = MainPaneFocus::Diff;
        let other_idx = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "other.rs")
            .unwrap();

        let outcome = app.click_files_row(other_idx);
        let crate::ui::navigation::FilesConfirmOutcome::Opened(_) = outcome else {
            panic!("a file row must open, not toggle or no-op");
        };
        assert_eq!(
            app.focus,
            MainPaneFocus::Files,
            "a click must not hand focus to Diff the way Enter does"
        );
        assert_eq!(app.files_selection, other_idx);
        let other_file_idx = app
            .files
            .iter()
            .position(|f| f.display_path() == "other.rs")
            .unwrap();
        assert!(
            matches!(app.rows[app.cursor], RenderRow::FileHeader { file_idx } if file_idx == other_file_idx),
            "the diff cursor must land on other.rs's own file header"
        );
    }

    /// req 4: clicking a directory that collapses the currently selected
    /// *descendant* must land the selection on the directory itself —
    /// exercised here through the real click entry point (not
    /// `toggle_directory` called directly, as
    /// [`collapsing_a_directory_containing_the_selection_reselects_that_directory`]
    /// does for the keyboard's `Space` path). Falls out for free: `click_files_row`
    /// writes `files_selection = idx` (the directory just clicked) *before*
    /// `toggle_directory` ever captures "previous" from it, so the
    /// ancestor-walk tier of `resolve_selection` doesn't even need to run —
    /// see `confirm_row`'s own docs.
    #[test]
    fn clicking_a_directory_containing_the_selection_reselects_that_directory() {
        let mut app = nested_app();
        app.focus = MainPaneFocus::Files;
        app.files_selection = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "src/nested/b.rs")
            .unwrap();
        let nested_dir_idx = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "src/nested" && r.id.is_directory)
            .unwrap();

        let outcome = app.click_files_row(nested_dir_idx);
        assert_eq!(outcome, crate::ui::navigation::FilesConfirmOutcome::Toggled);
        assert_eq!(
            selected_path(&app),
            Some("src/nested"),
            "the selection must land on the collapsed directory, not vanish"
        );
    }

    /// req 5/out-of-range: a click index past `visible_rows`' end (blank
    /// space below the last row) reports `NoSelection` and mutates nothing
    /// — `files_selection` in particular must stay wherever it already was,
    /// not snap to the invalid index.
    #[test]
    fn click_files_row_out_of_range_reports_no_selection_and_mutates_nothing() {
        let mut app = nested_app();
        app.focus = MainPaneFocus::Diff;
        let other_idx = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "other.rs")
            .unwrap();
        app.files_selection = other_idx;
        let cursor_before = app.cursor;
        let past_the_end = app.visible_rows.len() + 5;

        assert_eq!(
            app.click_files_row(past_the_end),
            crate::ui::navigation::FilesConfirmOutcome::NoSelection
        );
        assert_eq!(
            app.files_selection, other_idx,
            "an out-of-range click must not move the selection"
        );
        assert_eq!(app.cursor, cursor_before);
        assert_eq!(
            app.focus,
            MainPaneFocus::Diff,
            "an out-of-range click is a true no-op — focus included"
        );
    }

    /// req 7: a click resolves to the right file header regardless of what
    /// kind of change that file represents — new, deleted, renamed, or
    /// binary (which, having no hunks at all, stands in for "hunk-less"
    /// too; see `RenderRow::BinaryNotice`). `FIXTURE` (`multi_file.diff`)
    /// already covers modified/new/deleted/renamed; a synthetic binary file
    /// is appended for the one kind it has no real diff text for.
    #[test]
    fn click_files_row_jumps_to_the_right_header_for_every_kind_of_change() {
        let mut files = parse_unified_diff(FIXTURE);
        files.push(DiffFile {
            new_path: Some("assets/logo.png".to_owned()),
            old_path: Some("assets/logo.png".to_owned()),
            is_binary: true,
            ..Default::default()
        });
        let mut app = App::new("test-repo".to_owned(), PathBuf::from("/repo"), files);
        app.focus = MainPaneFocus::Diff;

        for display_path in [
            "src/lib.rs",        // modified
            "src/new_module.rs", // new
            "src/old_module.rs", // deleted
            "src/renamed_to.rs", // renamed
            "assets/logo.png",   // binary/hunk-less
        ] {
            let row_idx = app
                .visible_rows
                .iter()
                .position(|r| r.id.path == display_path)
                .unwrap_or_else(|| panic!("{display_path} has no tree row"));
            let file_idx = app
                .files
                .iter()
                .position(|f| f.display_path() == display_path)
                .unwrap();

            let outcome = app.click_files_row(row_idx);
            assert!(
                matches!(
                    outcome,
                    crate::ui::navigation::FilesConfirmOutcome::Opened(_)
                ),
                "{display_path} must open on click"
            );
            assert!(
                matches!(
                    app.rows[app.cursor],
                    RenderRow::FileHeader { file_idx: idx } | RenderRow::BinaryNotice { file_idx: idx }
                        if idx == file_idx
                ),
                "{display_path} must land on its own header/binary-notice row"
            );
            assert_eq!(app.focus, MainPaneFocus::Files);
        }
    }

    #[test]
    fn refresh_prunes_a_collapsed_directory_path_that_no_longer_exists() {
        let mut app = nested_app();
        app.toggle_directory("src/nested");
        assert!(app.visible_rows.iter().any(|r| r.id.path == "src/nested"
            && !matches!(
                r.kind,
                file_tree::VisibleKind::Directory { expanded: true, .. }
            )));

        // A refresh whose new diff no longer touches anything under
        // `src/nested` at all — the collapsed path must be pruned rather
        // than lingering as dead state forever.
        let files = vec![crate::diff::DiffFile {
            new_path: Some("other.rs".to_owned()),
            old_path: Some("other.rs".to_owned()),
            ..Default::default()
        }];
        app.apply_refresh(files);
        assert!(
            app.visible_rows.iter().all(|r| r.id.path != "src/nested"),
            "src/nested no longer exists in the refreshed tree"
        );

        // Rebuilding a tree that reintroduces `src/nested` must start
        // expanded again — pruning drops the stale collapse entry rather
        // than somehow resurrecting it later.
        let files_again = vec![crate::diff::DiffFile {
            new_path: Some("src/nested/b.rs".to_owned()),
            old_path: Some("src/nested/b.rs".to_owned()),
            ..Default::default()
        }];
        app.apply_refresh(files_again);
        let nested_row = app
            .visible_rows
            .iter()
            .find(|r| r.id.path == "src/nested")
            .expect("src/nested exists again");
        assert!(matches!(
            nested_row.kind,
            file_tree::VisibleKind::Directory { expanded: true, .. }
        ));
    }

    #[test]
    fn refresh_keeps_a_collapsed_directory_whose_files_survive() {
        // The other half of req 8 / epic decision 5, pinned on the
        // *live-watch* entry point specifically: `apply_refresh` is what
        // `ui::mod`'s watch path actually calls, and it is a different
        // function from `apply_scope_swap` (different anchor/search/fold
        // handling around the same rederive), so the scope-swap test below
        // can't stand in for it.
        let mut app = nested_app();
        app.toggle_directory("src/nested");

        app.apply_refresh(vec![
            crate::diff::DiffFile {
                new_path: Some("src/a.rs".to_owned()),
                old_path: Some("src/a.rs".to_owned()),
                ..Default::default()
            },
            crate::diff::DiffFile {
                new_path: Some("src/nested/b.rs".to_owned()),
                old_path: Some("src/nested/b.rs".to_owned()),
                ..Default::default()
            },
        ]);

        let nested_row = app
            .visible_rows
            .iter()
            .find(|r| r.id.path == "src/nested")
            .expect("src/nested still exists after the refresh");
        assert!(
            matches!(
                nested_row.kind,
                file_tree::VisibleKind::Directory {
                    expanded: false,
                    ..
                }
            ),
            "a refresh that keeps the directory must keep it collapsed"
        );
        assert!(
            app.visible_rows
                .iter()
                .all(|r| r.id.path != "src/nested/b.rs"),
            "descendants of the still-collapsed directory stay hidden"
        );
    }

    #[test]
    fn scope_swap_keeps_collapsed_directory_state() {
        // Epic decision 5 / plan-15 §5: unlike `expanded_folds`/
        // `unit_filter`, a scope swap deliberately does *not* clear
        // `collapsed_dirs` — a reviewer's "which directories I like folded"
        // preference plausibly carries over even into an unrelated diff.
        let mut app = nested_app();
        app.toggle_directory("src/nested");

        app.apply_scope_swap(
            vec![
                crate::diff::DiffFile {
                    new_path: Some("src/a.rs".to_owned()),
                    old_path: Some("src/a.rs".to_owned()),
                    ..Default::default()
                },
                crate::diff::DiffFile {
                    new_path: Some("src/nested/b.rs".to_owned()),
                    old_path: Some("src/nested/b.rs".to_owned()),
                    ..Default::default()
                },
            ],
            true,
            false,
            None,
        );

        let nested_row = app
            .visible_rows
            .iter()
            .find(|r| r.id.path == "src/nested")
            .expect("src/nested still exists after the swap");
        assert!(
            matches!(
                nested_row.kind,
                file_tree::VisibleKind::Directory {
                    expanded: false,
                    ..
                }
            ),
            "a scope swap must keep collapsed_dirs, unlike fold/unit state"
        );
    }

    #[test]
    fn rename_is_addressable_at_its_new_path() {
        let files = parse_unified_diff(FIXTURE);
        let app = App::new("test-repo".to_owned(), PathBuf::from("/repo"), files);
        let renamed = app
            .visible_rows
            .iter()
            .find(|r| r.id.path == "src/renamed_to.rs")
            .expect("the rename appears at its new path in the tree");
        assert!(matches!(renamed.kind, file_tree::VisibleKind::File { .. }));
        assert!(
            app.visible_rows
                .iter()
                .all(|r| r.id.path != "src/renamed_from.rs"),
            "the old path must not appear as its own tree row"
        );
    }

    #[test]
    fn confirm_opens_a_renamed_file_at_its_new_path_header() {
        // Beyond the row merely existing (the test above): the acceptance
        // criterion is that every visible file can be *opened* — drive the
        // rename through the same confirm path Enter uses and land on its
        // header row in the diff.
        let files = parse_unified_diff(FIXTURE);
        let mut app = App::new("test-repo".to_owned(), PathBuf::from("/repo"), files);
        app.focus = MainPaneFocus::Files;
        app.files_selection = app
            .visible_rows
            .iter()
            .position(|r| r.id.path == "src/renamed_to.rs")
            .expect("rename row present");

        let outcome = app.confirm_files_selection();
        let crate::ui::navigation::FilesConfirmOutcome::Opened(entry) = outcome else {
            panic!("expected Opened, got {outcome:?}");
        };
        assert_eq!(entry.line, None, "a tree jump targets the file header");
        let RenderRow::FileHeader { file_idx } = app.rows[app.cursor] else {
            panic!("cursor must land on the file header row");
        };
        assert_eq!(app.files[file_idx].display_path(), "src/renamed_to.rs");
        assert_eq!(app.focus, MainPaneFocus::Diff);
    }

    // -- Issue #16: visual-line selection --------------------------------

    /// Advances the cursor to the first `RenderRow::Line` in `app.rows`,
    /// via ordinary `CursorDown` movement — the same navigation a reviewer
    /// would actually use, rather than poking `app.cursor` directly, so
    /// these tests stay honest about starting from a state `App::update`
    /// itself could produce. `FIXTURE`'s first content row is `src/lib.rs`'s
    /// `"fn helper() {}"` context line, two rows past the file/hunk
    /// headers.
    fn move_cursor_to_first_line_row(app: &mut App) {
        app.update(Action::Top);
        while !matches!(app.rows[app.cursor], RenderRow::Line { .. }) {
            app.update(Action::CursorDown);
        }
    }

    #[test]
    fn v_on_a_line_row_starts_a_selection_at_the_cursor() {
        let mut app = test_app();
        move_cursor_to_first_line_row(&mut app);
        let anchor = app.cursor;

        assert!(!app.visual_active());
        assert_eq!(app.toggle_visual(), VisualToggleOutcome::Started);
        assert!(app.visual_active());
        assert_eq!(app.visual_bounds(), Some((anchor, anchor)));
        assert_eq!(
            app.cursor, anchor,
            "starting a selection never moves the cursor"
        );
    }

    #[test]
    fn v_off_a_header_gap_or_binary_row_reports_not_selectable() {
        // Header/gap rows, via the fold fixture's shape (see
        // `gap_fixture_app`'s docs): `[FileHeader, HunkHeader, .., Gap(5),
        // .., Gap(10)]`.
        let mut app = gap_fixture_app();
        for idx in [0usize, 1, 5, 10] {
            app.cursor = idx;
            assert!(
                !app.visual_active(),
                "sanity: no selection carried over between cases"
            );
            assert_eq!(
                app.toggle_visual(),
                VisualToggleOutcome::NotSelectable,
                "row {idx} ({:?}) must refuse to start a selection",
                app.rows[idx]
            );
            assert!(!app.visual_active());
        }

        // A binary file has only a `FileHeader`/`BinaryNotice` pair — no
        // `RenderRow::Line` at all to select.
        let binary_file = DiffFile {
            old_path: Some("image.png".to_owned()),
            new_path: Some("image.png".to_owned()),
            is_binary: true,
            ..Default::default()
        };
        let mut binary_app = App::new("repo".to_owned(), PathBuf::from("/repo"), vec![binary_file]);
        binary_app.cursor = 1; // the BinaryNotice row
        assert!(matches!(binary_app.rows[1], RenderRow::BinaryNotice { .. }));
        assert_eq!(
            binary_app.toggle_visual(),
            VisualToggleOutcome::NotSelectable
        );
    }

    #[test]
    fn v_again_cancels_the_selection_without_moving_the_cursor() {
        let mut app = test_app();
        move_cursor_to_first_line_row(&mut app);
        app.toggle_visual();
        assert!(app.visual_active());
        app.update(Action::CursorDown);
        app.update(Action::CursorDown);
        let cursor_before = app.cursor;

        assert_eq!(app.toggle_visual(), VisualToggleOutcome::Cancelled);
        assert!(!app.visual_active());
        assert_eq!(
            app.cursor, cursor_before,
            "cancelling a selection never moves the cursor"
        );
    }

    #[test]
    fn v_again_cancels_even_from_a_structural_row_the_cursor_wandered_onto() {
        // req 3: cancellation checks the anchor before row eligibility, so
        // a selection started on a `Line` row can still be cancelled after
        // the cursor moves onto a header/gap row a *fresh* `V` could never
        // have started from.
        let mut app = gap_fixture_app();
        app.cursor = 2; // "line 1" — a selectable Line row
        assert_eq!(app.toggle_visual(), VisualToggleOutcome::Started);
        app.cursor = 5; // the Between gap row
        assert!(matches!(app.rows[app.cursor], RenderRow::Gap { .. }));

        assert_eq!(app.toggle_visual(), VisualToggleOutcome::Cancelled);
        assert!(!app.visual_active());
    }

    #[test]
    fn cancel_visual_reports_whether_a_selection_was_active() {
        let mut app = test_app();
        assert!(!app.cancel_visual(), "nothing to cancel yet");

        move_cursor_to_first_line_row(&mut app);
        app.toggle_visual();
        assert!(app.cancel_visual());
        assert!(!app.visual_active());
        assert!(!app.cancel_visual(), "already cancelled once");
    }

    #[test]
    fn visual_bounds_are_the_inclusive_min_max_of_anchor_and_cursor_either_direction() {
        let mut app = test_app();
        move_cursor_to_first_line_row(&mut app);
        let anchor = app.cursor;
        app.toggle_visual();

        app.update(Action::CursorDown);
        app.update(Action::CursorDown);
        assert_eq!(app.visual_bounds(), Some((anchor, anchor + 2)));

        // Reverse direction: move the cursor back above the anchor.
        app.update(Action::CursorUp);
        app.update(Action::CursorUp);
        app.update(Action::CursorUp);
        assert_eq!(
            app.visual_bounds(),
            Some((anchor.saturating_sub(1), anchor))
        );
    }

    #[test]
    fn is_row_selected_skips_structural_rows_inside_the_bounds_but_the_cursor_still_crosses_them() {
        // The fold fixture's Between gap (row 5) sits inside a selection
        // spanning from "line 3" (row 4) to "line 10" (row 7) — the gap
        // itself must never read as selected, but the cursor must still be
        // able to land on it and extend the interval past it (req 6).
        let mut app = gap_fixture_app();
        app.cursor = 4; // "line 3"
        app.toggle_visual();
        app.cursor = 7; // "line 10", the other side of the gap
        assert!(matches!(app.rows[5], RenderRow::Gap { .. }));

        assert!(app.is_row_selected(4));
        assert!(
            !app.is_row_selected(5),
            "a Gap row is never itself selected"
        );
        assert!(app.is_row_selected(7));
        assert_eq!(app.visual_bounds(), Some((4, 7)));
    }

    #[test]
    fn selection_extends_across_hunks_and_files_in_screen_order() {
        let mut app = test_app();
        move_cursor_to_first_line_row(&mut app);
        let anchor = app.cursor;
        app.toggle_visual();
        // Walk to the far side of the multi-file fixture.
        app.update(Action::Bottom);

        let selected = app.selected_rows();
        assert!(!selected.is_empty());
        assert_eq!(
            selected.first().copied(),
            Some(anchor),
            "the first selected row is the anchor, screen order ascending"
        );
        // Every selected index is ascending and strictly increasing.
        assert!(selected.windows(2).all(|w| w[0] < w[1]));
        // Spans more than one file's worth of content — the fixture's
        // files are far enough apart that a same-hunk-only selection could
        // never reach `Bottom`.
        assert!(app.rows.len() - anchor > 10);
    }

    #[test]
    fn selected_rows_returns_only_lines_ascending() {
        let mut app = gap_fixture_app();
        app.cursor = 2; // "line 1"
        app.toggle_visual();
        app.cursor = 7; // "line 10" — spans the Between gap (row 5) and the
        // second hunk's own HunkHeader (row 6), both structural
        assert_eq!(app.selected_rows(), vec![2, 3, 4, 7]);
    }

    /// Every one of the seven [`App::rederive`] call sites must clear a
    /// live anchor — the single `self.visual_anchor = None;` line at the
    /// top of `rederive` covers all of them at once (see that method's
    /// docs), so this test drives each real caller in turn (not
    /// `rederive` directly, which is private) and asserts the anchor is
    /// gone afterward. `App::new` is the eighth call but isn't listed
    /// separately: a selection cannot exist before construction finishes,
    /// so there's nothing for it to clear.
    #[test]
    fn a_live_selection_is_cleared_by_every_rederive_call_site() {
        // 1. apply_refresh
        {
            let mut app = test_app();
            move_cursor_to_first_line_row(&mut app);
            app.toggle_visual();
            assert!(app.visual_active());
            app.apply_refresh(parse_unified_diff(FIXTURE));
            assert!(
                !app.visual_active(),
                "apply_refresh must clear a live selection"
            );
        }
        // 2. apply_scope_swap
        {
            let mut app = test_app();
            move_cursor_to_first_line_row(&mut app);
            app.toggle_visual();
            app.apply_scope_swap(parse_unified_diff(FIXTURE), true, false, None);
            assert!(
                !app.visual_active(),
                "apply_scope_swap must clear a live selection"
            );
        }
        // 3. set_unit_filter
        {
            let mut app = test_app();
            move_cursor_to_first_line_row(&mut app);
            app.toggle_visual();
            let id = crate::groups::enumerate_hunks(&app.files)[0].id.clone();
            app.set_unit_filter(UnitFilter {
                label: "unit".to_owned(),
                description: String::new(),
                index: 1,
                total: 1,
                hunk_ids: HashSet::from([id]),
            });
            assert!(
                !app.visual_active(),
                "set_unit_filter must clear a live selection"
            );
        }
        // 4. clear_unit_filter
        {
            let mut app = test_app();
            let id = crate::groups::enumerate_hunks(&app.files)[0].id.clone();
            app.set_unit_filter(UnitFilter {
                label: "unit".to_owned(),
                description: String::new(),
                index: 1,
                total: 1,
                hunk_ids: HashSet::from([id]),
            });
            move_cursor_to_first_line_row(&mut app);
            app.toggle_visual();
            assert!(app.clear_unit_filter());
            assert!(
                !app.visual_active(),
                "clear_unit_filter must clear a live selection"
            );
        }
        // 5. expand_gap
        {
            let mut app = gap_fixture_app();
            app.cursor = 2; // "line 1" — a selectable Line row
            app.toggle_visual();
            assert!(app.visual_active());
            let disk = gap_fixture_disk_lines();
            let refs: Vec<&str> = disk.iter().map(String::as_str).collect();
            assert_eq!(app.expand_gap(0, 0, &refs), ExpandOutcome::Revealed);
            assert!(
                !app.visual_active(),
                "expand_gap must clear a live selection"
            );
        }
        // 6. collapse_fold_at_cursor
        {
            let mut app = gap_fixture_app();
            app.cursor = 5; // the Between gap
            let disk = gap_fixture_disk_lines();
            let refs: Vec<&str> = disk.iter().map(String::as_str).collect();
            app.expand_gap(0, 0, &refs);
            // `expand_gap` leaves the cursor exactly where the gap row
            // stood (see `expand_then_collapse_round_trips_the_cursor_back_to_the_gap_row`) —
            // now one of the revealed `Line` rows, so `V` here starts a
            // selection `z c` can then fold back over.
            assert!(matches!(app.rows[app.cursor], RenderRow::Line { .. }));
            app.toggle_visual();
            assert!(app.visual_active());
            assert!(app.collapse_fold_at_cursor());
            assert!(
                !app.visual_active(),
                "collapse_fold_at_cursor must clear a live selection"
            );
        }
    }

    #[test]
    fn a_selection_survives_toggle_layout_and_a_resize() {
        let mut app = test_app();
        move_cursor_to_first_line_row(&mut app);
        app.toggle_visual();
        assert!(app.visual_active());

        app.update(Action::ToggleLayout);
        assert!(app.visual_active(), "ToggleLayout calls no rederive");
        app.set_viewport_height(40);
        app.set_content_width(120);
        assert!(app.visual_active(), "a resize calls no rederive");
    }

    #[test]
    fn visual_selection_works_the_same_on_a_non_interactive_historical_app() {
        let mut app = test_app();
        app.interactive = false;
        move_cursor_to_first_line_row(&mut app);
        let anchor = app.cursor;

        assert_eq!(app.toggle_visual(), VisualToggleOutcome::Started);
        app.update(Action::CursorDown);
        assert!(app.is_row_selected(anchor));
        assert!(!app.selected_rows().is_empty());
        assert!(
            app.cancel_visual(),
            "select-and-later-cancel/copy must work on a read-only historical diff"
        );
    }

    // -- Issue #19: range comment target ---------------------------------
    //
    // `test_app()` (`FIXTURE` = `multi_file.diff`) rows relevant below:
    // 0 FileHeader(src/lib.rs), 1 HunkHeader,
    // 2 ctx new1 "fn helper() {}", 3 del (old2, no new_line),
    // 4 add new2 "fn new_name() {}", 5 add new3 "fn new_name2() {}",
    // 6 ctx new4 "fn tail() {}", 7 ctx new5 "fn unchanged() {}",
    // 8 Gap, 9 FileHeader(src/new_module.rs), 10 HunkHeader,
    // 11 add new1, 12 add new2,
    // 13 FileHeader(src/old_module.rs, deleted), 14 HunkHeader,
    // 15 del old1 (no new_line), 16 del old2 (no new_line).

    #[test]
    fn single_line_comment_target_is_unchanged_by_the_result_type() {
        let mut app = test_app();
        app.cursor = 2; // ctx "fn helper() {}", new_line 1
        assert_eq!(
            app.comment_target(),
            Ok(CommentTarget::Single {
                file: "src/lib.rs".to_owned(),
                line: 1
            })
        );
    }

    #[test]
    fn comment_target_on_a_header_row_reports_no_selectable_line() {
        let mut app = test_app();
        app.cursor = 0; // FileHeader
        assert_eq!(
            app.comment_target(),
            Err(CommentTargetError::NoSelectableLine)
        );
    }

    #[test]
    fn comment_target_single_line_path_is_gated_on_interactive() {
        // The gap issue #19 closes: pre-#19, `comment_target` had no
        // `interactive` check at all — this is the regression guard.
        let mut app = test_app();
        app.interactive = false;
        app.cursor = 2;
        assert_eq!(
            app.comment_target(),
            Err(CommentTargetError::NotInteractive)
        );
    }

    #[test]
    fn a_forward_range_selection_targets_the_contiguous_new_line_span() {
        let mut app = test_app();
        app.cursor = 4; // add new2
        app.toggle_visual();
        app.cursor = 6; // ctx new4 — rows 4,5,6 are new_line 2,3,4
        assert_eq!(
            app.comment_target(),
            Ok(CommentTarget::Range {
                file: "src/lib.rs".to_owned(),
                start: 2,
                end: 4
            })
        );
    }

    #[test]
    fn a_reversed_range_selection_targets_the_same_span() {
        let mut app = test_app();
        app.cursor = 6; // ctx new4
        app.toggle_visual();
        app.cursor = 4; // add new2 — anchor above cursor now
        assert_eq!(
            app.comment_target(),
            Ok(CommentTarget::Range {
                file: "src/lib.rs".to_owned(),
                start: 2,
                end: 4
            })
        );
    }

    #[test]
    fn a_one_row_selection_collapses_to_single_rather_than_a_redundant_range() {
        let mut app = test_app();
        app.cursor = 4; // add new2
        app.toggle_visual();
        assert_eq!(
            app.comment_target(),
            Ok(CommentTarget::Single {
                file: "src/lib.rs".to_owned(),
                line: 2
            })
        );
    }

    #[test]
    fn a_selection_spanning_two_files_reports_multiple_files() {
        let mut app = test_app();
        app.cursor = 7; // src/lib.rs, ctx new5
        app.toggle_visual();
        app.cursor = 11; // src/new_module.rs, add new1
        assert_eq!(app.comment_target(), Err(CommentTargetError::MultipleFiles));
    }

    #[test]
    fn the_earliest_violating_row_decides_the_reported_reason() {
        // Rows 3..=11 both cross a deletion (row 3, src/lib.rs's del) and a
        // file boundary (row 11, src/new_module.rs). The per-row pass stops
        // at the first bad row — the deletion — even though a later row
        // violates a different condition: pins `range_comment_target`'s
        // documented first-bad-row rule (no whole-selection priority
        // ranking is promised).
        let mut app = test_app();
        app.cursor = 3; // src/lib.rs, del row
        app.toggle_visual();
        app.cursor = 11; // src/new_module.rs, add new1
        assert_eq!(
            app.comment_target(),
            Err(CommentTargetError::ContainsDeletion)
        );
    }

    #[test]
    fn a_selection_including_a_deletion_reports_contains_deletion() {
        let mut app = test_app();
        app.cursor = 3; // del
        app.toggle_visual();
        app.cursor = 4; // add new2
        assert_eq!(
            app.comment_target(),
            Err(CommentTargetError::ContainsDeletion)
        );
    }

    #[test]
    fn a_selection_on_a_deleted_file_reports_deleted_file_before_contains_deletion() {
        // Both rows of `src/old_module.rs` are `Del` — checking row kind
        // first would make `DeletedFile` unreachable (see
        // `App::range_comment_target`'s docs on the priority order).
        let mut app = test_app();
        app.cursor = 15; // src/old_module.rs, del old1
        app.toggle_visual();
        app.cursor = 16; // del old2
        assert_eq!(app.comment_target(), Err(CommentTargetError::DeletedFile));
    }

    #[test]
    fn a_selection_crossing_a_gap_reports_discontinuous() {
        let mut app = gap_fixture_app();
        app.cursor = 4; // "line 3", new_line 3
        app.toggle_visual();
        app.cursor = 7; // "line 10", new_line 10 — spans the Between gap and
        // the second hunk's own HunkHeader, both structural
        assert_eq!(app.selected_rows(), vec![4, 7]);
        assert_eq!(app.comment_target(), Err(CommentTargetError::Discontinuous));
    }

    #[test]
    fn a_valid_range_is_still_gated_on_interactive() {
        let mut app = test_app();
        app.interactive = false;
        app.cursor = 4;
        app.toggle_visual();
        app.cursor = 6;
        assert_eq!(
            app.comment_target(),
            Err(CommentTargetError::NotInteractive)
        );
    }

    #[test]
    fn comment_target_location_label_formats_single_and_range() {
        let single = CommentTarget::Single {
            file: "a.rs".to_owned(),
            line: 5,
        };
        assert_eq!(single.location_label(), "a.rs:5");

        let range = CommentTarget::Range {
            file: "a.rs".to_owned(),
            start: 5,
            end: 8,
        };
        assert_eq!(range.location_label(), "a.rs:5-8");
    }
}
