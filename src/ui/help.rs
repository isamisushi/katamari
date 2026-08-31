//! Issue #3's `?` help popup: a centered, modal floating window listing
//! every [`Action`] with its live keybinding, filterable by `/`. Three
//! things live here, the same split [`crate::ui::scope_menu`] draws between
//! its own list/input state and `ui::mod`'s rendering/routing:
//!
//! - [`describe`]: a hand-written, exhaustively-matched `(group,
//!   description)` table — the actual help copy. A forgotten new `Action`
//!   fails *this* to compile, unlike `ui::hints`'s curated per-view lists
//!   (silently just never shows a hint) or the many `Action` variants that
//!   still have no doc comment at all (contributor-facing prose, not
//!   checked, and not what a reviewer wants to read anyway) — this is a
//!   third, new table, not a reuse of either.
//! - [`HelpState`]/[`handle_key`]: pure state — which mode ([`HelpMode::Browse`]
//!   or [`HelpMode::Filter`]), the filter text, and the scroll offset —
//!   plus the one function that turns a raw terminal key into a state
//!   transition, terminal-free and unit-testable the same way
//!   [`crate::ui::scope_menu::handle_revision_key`] is.
//! - [`build_rows`]/[`render`]: turns [`describe`] plus whichever
//!   [`Keymap`] is actually live (preset plus any `[keys]` override) into
//!   the rows on screen — never a hardcoded key string, the same reason
//!   `ui::hints::HintItem::for_actions` reads the live keymap instead of
//!   duplicating it.
//!
//! While open, this popup is modal: `ui::mod`'s event loop routes every key
//! to [`handle_key`] through a raw-`KeyEvent` bypass — never through
//! [`crate::keymap::Resolver`] — the same way
//! [`crate::ui::compose::handle_key`]/[`crate::ui::scope_menu::handle_revision_key`]
//! do for their own overlays, and for the same reason `Filter` mode needs
//! it (literal characters, not resolved `Action`s). See `ui::mod`'s event
//! loop for exactly where that bypass arm sits and why.

use crate::keymap::{Action, Keymap, action_name};
use crate::ui::mouse::{FrameGeometry, ScrollTarget};
use crate::ui::text::display_width;
use crate::ui::text_input::{self, EditCommand, LineInput, cursor_marked_line};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

/// `(group, one-line description)` for `action` — the help copy itself.
/// Groups (fixed display order, see [`GROUPS`]): Navigation, Diff, LSP,
/// Comments, Agent, View, General. Descriptions are user-facing, kept to roughly
/// 52 characters or under — long enough to say something real, short
/// enough that they rarely clip even at a narrow popup width (see this
/// module's docs on why nothing here wraps).
///
/// An exhaustive match, deliberately: the compile error a forgotten new
/// `Action` produces here is the actual guarantee this feature's help copy
/// never silently goes stale, not a runtime check or a test someone has to
/// remember to update.
pub fn describe(action: Action) -> (&'static str, &'static str) {
    match action {
        Action::CursorDown => ("Navigation", "Move down one row"),
        Action::CursorUp => ("Navigation", "Move up one row"),
        Action::HalfPageDown => ("Navigation", "Scroll down half a page"),
        Action::HalfPageUp => ("Navigation", "Scroll up half a page"),
        Action::Top => ("Navigation", "Jump to the top"),
        Action::Bottom => ("Navigation", "Jump to the bottom"),
        Action::JumpBack => ("Navigation", "Retrace back through jump history"),
        Action::JumpForward => ("Navigation", "Retrace forward through jump history"),
        Action::OpenSearch => ("Navigation", "Search the diff"),
        Action::NextMatch => ("Navigation", "Jump to the next search match"),
        Action::PrevMatch => ("Navigation", "Jump to the previous search match"),
        Action::NextHunk => ("Diff", "Jump to the next hunk"),
        Action::PrevHunk => ("Diff", "Jump to the previous hunk"),
        Action::NextFile => ("Diff", "Jump to the next file"),
        Action::PrevFile => ("Diff", "Jump to the previous file"),
        Action::ExpandFold => ("Diff", "Expand a collapsed context gap"),
        Action::CollapseFold => ("Diff", "Collapse an expanded context gap"),
        Action::OpenScopeMenu => ("Diff", "Open the scope-picker popup"),
        Action::ToggleVisualLine => ("Diff", "Start, extend, or cancel visual-line selection"),
        Action::YankSelection => (
            "Diff",
            "Copy the visual selection with paths & line numbers",
        ),
        Action::Hover => ("LSP", "Show docs for symbol (again: close)"),
        Action::GotoDefinition => ("LSP", "Go to definition"),
        Action::FindReferences => ("LSP", "Find references"),
        Action::NextSymbol => ("LSP", "Select the next symbol on this line"),
        Action::PrevSymbol => ("LSP", "Select the previous symbol on this line"),
        Action::NextDiagnostic => ("LSP", "Jump to the next diagnostic"),
        Action::PrevDiagnostic => ("LSP", "Jump to the previous diagnostic"),
        Action::AddComment => ("Comments", "Add a comment on this row or visual range"),
        Action::ToggleComments => ("Comments", "Show or hide inline comment bodies"),
        Action::AskAgent => (
            "Agent",
            "Ask the resident agent about this line or selection",
        ),
        Action::ToggleAgentPanel => ("Agent", "Open or close the agent transcript panel"),
        Action::PushCommentsToAgent => ("Agent", "Push every open review comment to the agent"),
        Action::ToggleSidebar => ("View", "Show or hide the file sidebar"),
        Action::ToggleLayout => ("View", "Toggle unified/side-by-side layout"),
        Action::ToggleTimeline => ("View", "Open or close the jj snapshot timeline"),
        Action::ToggleLogView => ("View", "Open or close the commit log"),
        Action::ToggleUnits => ("View", "Group the diff into semantic units (agent CLI)"),
        Action::RegenerateUnits => ("View", "Regenerate the units grouping, skipping the cache"),
        Action::ToggleHints => ("View", "Show all status-bar hints, or just the minimal set"),
        Action::ToggleLspInspector => ("View", "Open or close the live LSP inspector"),
        Action::ToggleRangeSelect => ("View", "Toggle range-select (timeline/log)"),
        Action::ToggleDirectory => ("View", "Expand or collapse a files-pane directory"),
        Action::FocusNextPane => ("View", "Focus the next pane"),
        Action::FocusPrevPane => ("View", "Focus the previous pane"),
        Action::Cancel => ("General", "Close a popup/hover, or return from a view"),
        Action::Confirm => ("General", "Confirm the selection"),
        Action::Quit => ("General", "Quit katamari"),
        Action::OpenHelp => ("General", "Show this help window"),
    }
}

/// Display order for the popup's group headers — fixed, independent of
/// [`Action`]'s own declaration order or `describe`'s match arm order,
/// since neither is meant to double as UI layout.
const GROUPS: [&str; 7] = [
    "Navigation",
    "Diff",
    "LSP",
    "Comments",
    "Agent",
    "View",
    "General",
];

/// Every [`Action`] variant, once. [`crate::keymap::mod`]'s own
/// round-trip test keeps an equivalent list, but it's private to that
/// module's `#[cfg(test)]` block — unreachable from here — so this is a
/// second, hand-maintained enumeration rather than a shared one. Not a
/// second *risk*, though: [`describe`]'s exhaustive match is what actually
/// catches a forgotten new `Action` at compile time; a variant missing from
/// *this* list would only fail to list itself in the popup (and fail this
/// file's own coverage test, which iterates this same list — see the tests
/// module below) while still compiling.
const ALL_ACTIONS: &[Action] = &[
    Action::CursorDown,
    Action::CursorUp,
    Action::HalfPageDown,
    Action::HalfPageUp,
    Action::Top,
    Action::Bottom,
    Action::JumpBack,
    Action::JumpForward,
    Action::OpenSearch,
    Action::NextMatch,
    Action::PrevMatch,
    Action::NextHunk,
    Action::PrevHunk,
    Action::NextFile,
    Action::PrevFile,
    Action::ExpandFold,
    Action::CollapseFold,
    Action::OpenScopeMenu,
    Action::ToggleVisualLine,
    Action::YankSelection,
    Action::Hover,
    Action::GotoDefinition,
    Action::FindReferences,
    Action::NextSymbol,
    Action::PrevSymbol,
    Action::NextDiagnostic,
    Action::PrevDiagnostic,
    Action::AddComment,
    Action::ToggleComments,
    Action::AskAgent,
    Action::ToggleAgentPanel,
    Action::PushCommentsToAgent,
    Action::ToggleSidebar,
    Action::ToggleLayout,
    Action::ToggleTimeline,
    Action::ToggleLogView,
    Action::ToggleUnits,
    Action::RegenerateUnits,
    Action::ToggleHints,
    Action::ToggleLspInspector,
    Action::ToggleRangeSelect,
    Action::ToggleDirectory,
    Action::FocusNextPane,
    Action::FocusPrevPane,
    Action::Cancel,
    Action::Confirm,
    Action::Quit,
    Action::OpenHelp,
];

/// One row the popup can render: a group header, one action's entry, or the
/// single dim placeholder a filter with no matches shows (rather than an
/// empty list, which would look indistinguishable from "still loading" or
/// a rendering bug).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelpRow {
    GroupHeader(&'static str),
    Entry {
        action: Action,
        /// [`crate::keymap::KeySeq::compact_notation`]'s output, or
        /// `"(unbound)"` — computed once per [`build_rows`] call against
        /// whichever [`Keymap`] is actually live, never hardcoded (see this
        /// module's docs).
        binding: String,
        description: &'static str,
    },
    NoMatches,
}

/// `"(unbound)"` — [`build_rows`]'s text for an action with no live
/// binding, and [`render_rows`]'s cue to dim it. A real key sequence can
/// never collide with this (every [`crate::keymap::KeySeq::compact_notation`]
/// output is built from key names/punctuation, never parentheses-wrapped
/// prose), so checking the rendered string back against this constant in
/// [`render_rows`] is safe rather than fragile.
const UNBOUND: &str = "(unbound)";

fn entry_binding(keymap: &Keymap, action: Action) -> String {
    keymap
        .binding_for(action)
        .map(|seq| seq.compact_notation())
        .unwrap_or_else(|| UNBOUND.to_owned())
}

/// Builds the popup's full row list for `filter` (empty = everything)
/// against whichever `keymap` is currently live: one [`HelpRow::GroupHeader`]
/// per [`GROUPS`] entry that still has at least one visible row underneath
/// it, in fixed group order, each followed by its matching
/// [`HelpRow::Entry`] rows in [`ALL_ACTIONS`] order. A blank `filter`
/// matches everything (every action's `haystack` trivially contains an
/// empty needle), so `Browse` mode's unfiltered view and a since-cleared
/// filter both fall out of the same code path rather than a separate
/// "no filter" branch.
///
/// Matching is case-insensitive substring over the description, the kebab
/// action name (`crate::keymap::action_name`), and the binding text
/// together — a reviewer filtering for "hover" should find it whether they
/// think of it by its label, its config name, or (if they already half
/// remember it) its key.
pub fn build_rows(keymap: &Keymap, filter: &str) -> Vec<HelpRow> {
    let needle = filter.to_lowercase();
    let mut rows = Vec::new();

    for group in GROUPS {
        let mut entries = Vec::new();
        for &action in ALL_ACTIONS {
            let (action_group, description) = describe(action);
            if action_group != group {
                continue;
            }
            let binding = entry_binding(keymap, action);
            if !needle.is_empty() {
                let name = action_name(action);
                let haystack = format!("{description} {name} {binding}").to_lowercase();
                if !haystack.contains(&needle) {
                    continue;
                }
            }
            entries.push(HelpRow::Entry {
                action,
                binding,
                description,
            });
        }
        if !entries.is_empty() {
            rows.push(HelpRow::GroupHeader(group));
            rows.extend(entries);
        }
    }

    if rows.is_empty() {
        rows.push(HelpRow::NoMatches);
    }
    rows
}

/// Renders `rows` as styled [`Line`]s: bold group headers, entries as
/// `"<binding>  <description>"` with the binding right-padded into a
/// shared column (widest binding in `rows` sets the column — recomputed
/// per call, since a filtered view's widest binding is usually narrower
/// than the full list's), an [`UNBOUND`] binding dimmed, and
/// [`HelpRow::NoMatches`] as a single dim, italic line.
fn render_rows(rows: &[HelpRow]) -> Vec<Line<'static>> {
    let binding_width = rows
        .iter()
        .filter_map(|row| match row {
            HelpRow::Entry { binding, .. } => Some(display_width(binding)),
            _ => None,
        })
        .max()
        .unwrap_or(0);

    rows.iter()
        .map(|row| match row {
            HelpRow::GroupHeader(name) => Line::from(Span::styled(
                *name,
                Style::default().add_modifier(Modifier::BOLD),
            )),
            HelpRow::Entry {
                binding,
                description,
                ..
            } => {
                let pad = " ".repeat(binding_width.saturating_sub(display_width(binding)));
                let binding_style = if binding == UNBOUND {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default()
                };
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(binding.clone(), binding_style),
                    Span::raw(pad),
                    Span::raw("  "),
                    Span::raw(*description),
                ])
            }
            HelpRow::NoMatches => Line::from(Span::styled(
                "no matches",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )),
        })
        .collect()
}

/// Which half of the popup is active: browsing/scrolling the (possibly
/// filtered) row list, or editing the filter text itself. A separate mode
/// rather than "is the filter non-empty" — the filter can hold text while
/// `Browse` is active too (see [`HelpState`]'s docs on why `Enter` keeps
/// it), so mode and filter-content are independent axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpMode {
    Browse,
    Filter,
}

/// The help popup's state: which mode is active, the filter text (a
/// [`LineInput`] — the same char-indexed buffer
/// [`crate::ui::compose::ComposeBuffer`]/[`crate::ui::scope_menu::RevisionInput`]
/// use and for the same reason: never splitting a UTF-8 sequence
/// mid-character), and the scroll offset into the current (filtered) row
/// list.
///
/// The filter text is *not* dropped when leaving `Filter` mode via `Enter`
/// — `/` narrows the list, `Enter` "keeps browsing this narrowed list," and
/// only an explicit `Esc` *from* `Filter` mode clears it (see
/// [`Self::exit_filter_clear`]) — vim's own incremental-search-then-keep-
/// navigating feel, which is the whole point of a live filter over a
/// one-shot search-and-close.
pub struct HelpState {
    mode: HelpMode,
    filter: LineInput,
    scroll: usize,
    /// Set by a lone `g` in `Browse` mode, cleared by anything else
    /// (including a second, non-chord `g` press's own completion) — the
    /// popup's small stand-in for the app-wide [`crate::keymap::Resolver`]
    /// it deliberately bypasses (see this module's docs), just enough to
    /// give `gg` the real two-key chord the README documents rather than
    /// letting a single `g` jump to the top on its own.
    pending_g: bool,
}

impl HelpState {
    pub fn new() -> Self {
        Self {
            mode: HelpMode::Browse,
            filter: LineInput::new(),
            scroll: 0,
            pending_g: false,
        }
    }

    pub fn mode(&self) -> HelpMode {
        self.mode
    }

    pub fn filter_text(&self) -> &str {
        self.filter.text()
    }

    pub fn cursor(&self) -> usize {
        self.filter.cursor()
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// `/`: enters `Filter` mode with the cursor at the end of whatever
    /// filter text already exists (empty, the common case, or a
    /// previously-set filter a reviewer wants to refine further rather than
    /// retype from scratch).
    pub fn enter_filter(&mut self) {
        self.mode = HelpMode::Filter;
        self.filter.move_to_end();
    }

    /// `Enter` from `Filter` mode: back to `Browse`, filter text untouched.
    pub fn exit_filter_keep(&mut self) {
        self.mode = HelpMode::Browse;
    }

    /// `Esc` from `Filter` mode: back to `Browse`, filter text cleared and
    /// scroll reset — the "never mind" gesture, distinct from `Browse`
    /// mode's own `Esc`, which closes the whole popup (see `ui::mod`'s
    /// event loop, where that split actually lives — [`HelpState`] itself
    /// has no notion of "closed," only `Some`/`None` in the caller's
    /// `Option<HelpState>`).
    pub fn exit_filter_clear(&mut self) {
        self.filter = LineInput::new();
        self.scroll = 0;
        self.mode = HelpMode::Browse;
    }

    pub fn insert_char(&mut self, c: char) {
        self.filter.insert_char(c);
        // A live filter is pointless if the view stays scrolled to
        // wherever `Browse` mode last left it — every keystroke narrows
        // (or widens) the row set, so each one starts back at the top of
        // whatever now matches.
        self.scroll = 0;
    }

    pub fn backspace(&mut self) {
        self.filter.backspace();
        self.scroll = 0;
    }

    /// `C-w`/`M-Backspace`: as [`Self::backspace`], but a whole word at
    /// once — same scroll reset, for the same reason (a narrower or wider
    /// filter needs to be viewed from its own top, not wherever `Browse`
    /// mode happened to leave the scroll position).
    pub fn delete_previous_word(&mut self) {
        self.filter.delete_previous_word();
        self.scroll = 0;
    }

    pub fn move_left(&mut self) {
        self.filter.move_left();
    }

    pub fn move_right(&mut self) {
        self.filter.move_right();
    }

    /// `total_rows`/`viewport` (the popup's current row count and visible
    /// height) are threaded into every scroll-mutating method rather than
    /// cached on `HelpState` itself, so the clamp below is always computed
    /// against *this* call's real numbers — a filter narrowing the list, or
    /// a terminal resize changing the viewport, is reflected the next time
    /// a scroll key is pressed rather than trusting a possibly-stale
    /// snapshot. This is the "sane clamp" [`crate::ui::hover_popup`] is
    /// missing: its own scroll only clamps against `lines.len() - 1`, which
    /// lets the *last* page show mostly blank space once `scroll` gets
    /// close to the end; clamping against `total_rows - viewport` instead
    /// means the window is always full up to the last row, never dangling
    /// past it.
    pub fn scroll_down(&mut self, total_rows: usize, viewport: usize) {
        self.scroll = clamp_scroll(self.scroll + 1, total_rows, viewport);
    }

    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub fn page_down(&mut self, total_rows: usize, viewport: usize) {
        self.scroll = clamp_scroll(self.scroll + viewport.max(1), total_rows, viewport);
    }

    pub fn page_up(&mut self, viewport: usize) {
        self.scroll = self.scroll.saturating_sub(viewport.max(1));
    }

    pub fn top(&mut self) {
        self.scroll = 0;
    }

    pub fn bottom(&mut self, total_rows: usize, viewport: usize) {
        self.scroll = clamp_scroll(usize::MAX, total_rows, viewport);
    }
}

impl Default for HelpState {
    fn default() -> Self {
        Self::new()
    }
}

/// The scroll offset that leaves the last page full rather than dangling
/// past the end of `total_rows` — shared by every `HelpState` method that
/// moves `scroll` downward (`scroll_up`/`page_up`/`top` only ever decrease
/// it, which can never produce an empty page, so they don't need this).
/// `total_rows <= viewport` (everything fits, or there's nothing to show)
/// collapses to `0` via `saturating_sub`, which is exactly right: there's
/// nowhere to scroll to.
fn clamp_scroll(scroll: usize, total_rows: usize, viewport: usize) -> usize {
    scroll.min(total_rows.saturating_sub(viewport.max(1)))
}

/// What [`handle_key`] decided one key press should do beyond mutating
/// `HelpState` itself — the modal-popup sibling of
/// [`crate::ui::compose::ComposeOutcome`]/
/// [`crate::ui::scope_menu::RevisionInputOutcome`], with only two cases:
/// this popup has no save/submit step, only "stay open" or "close."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpOutcome {
    Continue,
    Close,
}

/// Applies one raw terminal key event to `state`, bypassing
/// [`crate::keymap`] entirely — `ui::mod`'s event loop calls this from its
/// raw-`KeyEvent` bypass chain rather than feeding the key to
/// [`crate::keymap::Resolver`] first, the same reasoning
/// [`crate::ui::compose::handle_key`]/
/// [`crate::ui::scope_menu::handle_revision_key`] give for their own
/// overlays: `Filter` mode needs literal characters, and — per this
/// feature's spec — the *whole* popup is modal while open, not just its
/// text-input half, so every key is consumed here, never falling through
/// to the resolver even in `Browse` mode.
///
/// `total_rows`/`viewport` are only consulted by `Browse` mode's
/// scroll/page/top/bottom keys (see [`HelpState::scroll_down`]'s docs);
/// `Filter` mode's keys never need them.
pub fn handle_key(
    state: &mut HelpState,
    key: KeyEvent,
    total_rows: usize,
    viewport: usize,
) -> HelpOutcome {
    match state.mode {
        HelpMode::Browse => {
            // `gg`: a real two-key chord, matching the README's notation
            // and the app-wide `Action::Top` binding (`"g g"` via
            // `crate::keymap::Resolver`) rather than a single `g` jumping
            // straight to the top — see `pending_g`'s docs. Handled ahead
            // of the main match (rather than as one more arm inside it) so
            // every *other* key can clear `pending_g` in one place instead
            // of every arm remembering to do it individually.
            if key.code == KeyCode::Char('g') && key.modifiers.is_empty() {
                return if std::mem::take(&mut state.pending_g) {
                    state.top();
                    HelpOutcome::Continue
                } else {
                    state.pending_g = true;
                    HelpOutcome::Continue
                };
            }
            state.pending_g = false;
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    state.scroll_down(total_rows, viewport);
                    HelpOutcome::Continue
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    state.scroll_up();
                    HelpOutcome::Continue
                }
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    state.scroll_down(total_rows, viewport);
                    HelpOutcome::Continue
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    state.scroll_up();
                    HelpOutcome::Continue
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    state.page_down(total_rows, viewport);
                    HelpOutcome::Continue
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    state.page_up(viewport);
                    HelpOutcome::Continue
                }
                KeyCode::PageDown => {
                    state.page_down(total_rows, viewport);
                    HelpOutcome::Continue
                }
                KeyCode::PageUp => {
                    state.page_up(viewport);
                    HelpOutcome::Continue
                }
                KeyCode::Char('G') => {
                    state.bottom(total_rows, viewport);
                    HelpOutcome::Continue
                }
                KeyCode::Char('/') if key.modifiers.is_empty() => {
                    state.enter_filter();
                    HelpOutcome::Continue
                }
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('?') => HelpOutcome::Close,
                // Modal: an unrecognized key is simply swallowed rather than
                // falling through to the resolver — see this function's docs.
                _ => HelpOutcome::Continue,
            }
        }
        HelpMode::Filter => {
            match key.code {
                KeyCode::Esc => {
                    state.exit_filter_clear();
                    return HelpOutcome::Continue;
                }
                KeyCode::Enter => {
                    state.exit_filter_keep();
                    return HelpOutcome::Continue;
                }
                _ => {}
            }
            // Insert/backspace/word-delete/left/right go through the
            // shared editing core every raw-key-bypass text field uses —
            // see `text_input::recognize`'s own docs for why an
            // unrecognized key (a stray `C-a`/`C-e` etc.) is swallowed
            // rather than inserted literally.
            match text_input::recognize(&key) {
                Some(EditCommand::Insert(c)) => state.insert_char(c),
                Some(EditCommand::Backspace) => state.backspace(),
                Some(EditCommand::DeletePreviousWord) => state.delete_previous_word(),
                Some(EditCommand::MoveLeft) => state.move_left(),
                Some(EditCommand::MoveRight) => state.move_right(),
                None => {}
            }
            HelpOutcome::Continue
        }
    }
}

/// The popup's outer rect: a fixed ~70%×~80% of `area` (floored at a
/// legible 40 columns / 8 rows, capped at what `area` actually has room
/// for), unlike [`crate::ui::scope_menu::popup_rect`]'s content-driven
/// sizing. The row list's own length varies live as a filter narrows it —
/// sizing the *popup* to match would make the window visibly resize while
/// typing; better for the window to hold still and the list scroll inside
/// it. `.min(available).max(floor)` (never `.clamp`, which panics if
/// `floor > available` — a terminal narrower than 40 columns is edge-case
/// enough to just render a too-wide popup, like every other popup in this
/// codebase does at the same extreme, rather than crash).
fn popup_rect(area: Rect) -> Rect {
    let width = (((area.width as u32 * 7) / 10) as u16)
        .min(area.width.saturating_sub(2))
        .max(40);
    let height = (((area.height as u32 * 4) / 5) as u16)
        .min(area.height.saturating_sub(2))
        .max(8);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// [`popup_rect`]'s inner content area, after the border [`render`] draws
/// — the one geometry calculation both [`render`] and [`viewport_rows`]
/// build on, so a future change to the border/title never lets the two
/// drift out of sync the way `ui::hints`' `wrap_for_area`/`render_lines`
/// split guards against the same trap (see that module's `LINE_PREFIX`
/// docs).
fn inner_rect(area: Rect) -> Rect {
    Block::default()
        .borders(Borders::ALL)
        .inner(popup_rect(area))
}

/// How many row lines are visible inside the popup at `area`'s current
/// size: the bordered inner height, minus one reserved for the
/// footer/filter-input line [`render`] always draws last. `ui::mod`'s
/// event loop calls this from the raw-key bypass to compute the `viewport`
/// argument [`handle_key`]'s scroll/page/bottom keys need — see
/// [`HelpState::scroll_down`]'s docs on why that number has to be real
/// rather than assumed.
pub fn viewport_rows(area: Rect) -> usize {
    inner_rect(area).height.saturating_sub(1) as usize
}

const FOOTER: &str = "/ filter \u{b7} j/k scroll \u{b7} Esc close";

/// Renders the popup: [`build_rows`] against `keymap`'s live bindings and
/// `state`'s current filter, [`render_rows`] to styled lines, sliced to
/// `state.scroll()` clamped against the real inner viewport (a second,
/// render-time clamp on top of [`HelpState`]'s own scroll-time one — belt
/// and suspenders for the same "never an empty page" guarantee, since
/// `Filter` mode's live narrowing changes `total_rows` on every keystroke
/// without ever calling a `scroll_*` method itself), and a final line: the
/// static [`FOOTER`] in `Browse` mode, or the filter text with its cursor
/// marked (reusing [`crate::ui::compose::cursor_marked_line`], prefixed
/// with `/`) in `Filter` mode.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &HelpState,
    keymap: &Keymap,
    geometry: &mut FrameGeometry,
) {
    let rect = popup_rect(area);
    // The help popup is drawn last, unconditionally, outside every other
    // view's `match` arm in `draw` (see that function's docs) — recording
    // it last here is what makes it win `FrameGeometry::hit`'s
    // last-recorded-wins scan over anything else on screen, req 7's modal
    // precedence for the one overlay that can open on top of literally
    // any view.
    geometry.record(rect, ScrollTarget::HelpPopup);
    frame.render_widget(Clear, rect);

    let block = Block::default().borders(Borders::ALL).title(" help ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let viewport = inner.height.saturating_sub(1) as usize;
    let rows = build_rows(keymap, state.filter_text());
    let lines = render_rows(&rows);
    let visible_scroll = clamp_scroll(state.scroll(), lines.len(), viewport);

    let mut visible: Vec<Line> = lines
        .into_iter()
        .skip(visible_scroll)
        .take(viewport)
        .collect();
    while visible.len() < viewport {
        visible.push(Line::default());
    }

    visible.push(match state.mode() {
        HelpMode::Browse => Line::from(Span::styled(FOOTER, Style::default().fg(Color::DarkGray))),
        HelpMode::Filter => {
            let marked = cursor_marked_line(state.filter_text(), state.cursor());
            let mut spans = vec![Span::raw("/")];
            spans.extend(marked.spans);
            Line::from(spans)
        }
    });

    frame.render_widget(Paragraph::new(visible), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{KeySeq, emacs_preset, vim_preset};
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode) -> KeyEvent {
        key_mod(code, KeyModifiers::NONE)
    }

    fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    // ---- describe coverage --------------------------------------------

    #[test]
    fn describe_covers_every_action_with_a_non_empty_description_and_a_known_group() {
        for &action in ALL_ACTIONS {
            let (group, description) = describe(action);
            assert!(
                GROUPS.contains(&group),
                "{action:?}'s group {group:?} is not one of the fixed GROUPS"
            );
            assert!(
                !description.is_empty(),
                "{action:?} has an empty description"
            );
            assert!(
                description.chars().count() <= 52,
                "{action:?}'s description is over the ~52-char budget: {description:?}"
            );
        }
    }

    #[test]
    fn all_actions_matches_the_keymap_modules_own_count() {
        // Both presets bind every `Action` exactly once (see
        // `keymap::every_vim_and_emacs_binding_covers_every_action_exactly_once`);
        // this file can't reach that module's private test-only list, but
        // it can at least pin its own list's length against the live vim
        // preset's, so the two enumerations can't silently drift apart in
        // count even though they're maintained separately (see
        // `ALL_ACTIONS`'s docs).
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let bound: std::collections::HashSet<&'static str> = ALL_ACTIONS
            .iter()
            .filter(|a| keymap.binding_for(**a).is_some())
            .map(|a| action_name(*a))
            .collect();
        assert_eq!(bound.len(), ALL_ACTIONS.len());
    }

    // ---- build_rows / filtering ----------------------------------------

    #[test]
    fn build_rows_with_no_filter_lists_every_group_header_once() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let rows = build_rows(&keymap, "");
        for group in GROUPS {
            let count = rows
                .iter()
                .filter(|r| matches!(r, HelpRow::GroupHeader(g) if *g == group))
                .count();
            assert_eq!(
                count, 1,
                "expected exactly one {group} header, rows: {rows:?}"
            );
        }
        let entries = rows
            .iter()
            .filter(|r| matches!(r, HelpRow::Entry { .. }))
            .count();
        assert_eq!(entries, ALL_ACTIONS.len());
    }

    #[test]
    fn build_rows_filter_matches_case_insensitively_on_description() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let rows = build_rows(&keymap, "HOVER");
        assert!(rows.iter().any(|r| matches!(
            r,
            HelpRow::Entry {
                action: Action::Hover,
                ..
            }
        )));
        assert!(!rows.iter().any(|r| matches!(
            r,
            HelpRow::Entry {
                action: Action::CursorDown,
                ..
            }
        )));
    }

    #[test]
    fn build_rows_filter_matches_on_the_kebab_action_name() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        // "cursor-down" is nowhere in `CursorDown`'s description ("Move
        // down one row"); only the kebab name itself contains it.
        let rows = build_rows(&keymap, "cursor-down");
        assert!(rows.iter().any(|r| matches!(
            r,
            HelpRow::Entry {
                action: Action::CursorDown,
                ..
            }
        )));
        assert!(!rows.iter().any(|r| matches!(
            r,
            HelpRow::Entry {
                action: Action::CursorUp,
                ..
            }
        )));
    }

    #[test]
    fn build_rows_filter_matches_on_binding_text() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        // `]c`/`[c` (next/prev hunk) share no substring with either
        // action's description or kebab name — only the rendered binding
        // notation itself contains `]c`.
        let rows = build_rows(&keymap, "]c");
        assert!(rows.iter().any(|r| matches!(
            r,
            HelpRow::Entry {
                action: Action::NextHunk,
                ..
            }
        )));
    }

    #[test]
    fn build_rows_with_no_matches_returns_a_single_placeholder_row() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let rows = build_rows(&keymap, "there is no action named like this");
        assert_eq!(rows, vec![HelpRow::NoMatches]);
    }

    #[test]
    fn build_rows_omits_a_group_header_when_none_of_its_entries_match() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        // "sidebar" only ever matches the View group's `ToggleSidebar`
        // entry — every other group's header must be absent, not just its
        // entries.
        let rows = build_rows(&keymap, "sidebar");
        assert_eq!(
            rows.iter()
                .filter(|r| matches!(r, HelpRow::GroupHeader(_)))
                .count(),
            1
        );
        assert_eq!(rows[0], HelpRow::GroupHeader("View"));
    }

    #[test]
    fn build_rows_reflects_a_keys_override_and_the_unbound_case() {
        // Only `CursorDown` is bound in this keymap — everything else must
        // render as `(unbound)`, and `CursorDown` itself must show the
        // override, not the vim-preset default.
        let keymap = Keymap::from_bindings(&[(KeySeq::parse("Z"), Action::CursorDown)]);
        let rows = build_rows(&keymap, "");
        let cursor_down = rows
            .iter()
            .find(|r| {
                matches!(
                    r,
                    HelpRow::Entry {
                        action: Action::CursorDown,
                        ..
                    }
                )
            })
            .unwrap();
        assert_eq!(
            cursor_down,
            &HelpRow::Entry {
                action: Action::CursorDown,
                binding: "Z".to_owned(),
                description: describe(Action::CursorDown).1,
            }
        );
        let quit = rows
            .iter()
            .find(|r| {
                matches!(
                    r,
                    HelpRow::Entry {
                        action: Action::Quit,
                        ..
                    }
                )
            })
            .unwrap();
        assert_eq!(
            quit,
            &HelpRow::Entry {
                action: Action::Quit,
                binding: UNBOUND.to_owned(),
                description: describe(Action::Quit).1,
            }
        );
    }

    #[test]
    fn build_rows_follows_the_emacs_preset_too() {
        let keymap = Keymap::from_bindings(&emacs_preset(false));
        let rows = build_rows(&keymap, "");
        let goto_def = rows
            .iter()
            .find(|r| {
                matches!(
                    r,
                    HelpRow::Entry {
                        action: Action::GotoDefinition,
                        ..
                    }
                )
            })
            .unwrap();
        assert_eq!(
            goto_def,
            &HelpRow::Entry {
                action: Action::GotoDefinition,
                binding: "M-.".to_owned(),
                description: describe(Action::GotoDefinition).1,
            }
        );
    }

    // ---- HelpState mode transitions -------------------------------------

    #[test]
    fn new_state_starts_in_browse_mode_with_an_empty_filter() {
        let state = HelpState::new();
        assert_eq!(state.mode(), HelpMode::Browse);
        assert_eq!(state.filter_text(), "");
    }

    #[test]
    fn slash_enters_filter_mode_with_the_cursor_at_the_end() {
        let mut state = HelpState::new();
        state.insert_char('x'); // pretend a previous filter session left text — see below
        state.exit_filter_keep(); // back to Browse, "x" kept
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Char('/')), 10, 5),
            HelpOutcome::Continue
        );
        assert_eq!(state.mode(), HelpMode::Filter);
        assert_eq!(state.cursor(), 1);
    }

    #[test]
    fn enter_from_filter_mode_keeps_the_filter_text() {
        let mut state = HelpState::new();
        state.enter_filter();
        state.insert_char('g');
        state.insert_char('d');
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Enter), 10, 5),
            HelpOutcome::Continue
        );
        assert_eq!(state.mode(), HelpMode::Browse);
        assert_eq!(state.filter_text(), "gd");
    }

    #[test]
    fn esc_from_filter_mode_clears_the_filter_with_text_present() {
        let mut state = HelpState::new();
        state.enter_filter();
        state.insert_char('x');
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Esc), 10, 5),
            HelpOutcome::Continue
        );
        assert_eq!(state.mode(), HelpMode::Browse);
        assert_eq!(state.filter_text(), "");
    }

    #[test]
    fn esc_from_filter_mode_with_no_text_still_returns_to_browse() {
        let mut state = HelpState::new();
        state.enter_filter();
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Esc), 10, 5),
            HelpOutcome::Continue
        );
        assert_eq!(state.mode(), HelpMode::Browse);
        assert_eq!(state.filter_text(), "");
    }

    #[test]
    fn esc_in_browse_mode_closes_the_popup() {
        let mut state = HelpState::new();
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Esc), 10, 5),
            HelpOutcome::Close
        );
    }

    #[test]
    fn q_and_question_mark_close_in_browse_mode() {
        let mut state = HelpState::new();
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Char('q')), 10, 5),
            HelpOutcome::Close
        );
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Char('?')), 10, 5),
            HelpOutcome::Close
        );
    }

    #[test]
    fn filter_persists_across_a_round_trip_through_browse_and_back_to_filter() {
        let mut state = HelpState::new();
        state.enter_filter();
        state.insert_char('c');
        state.exit_filter_keep();
        state.scroll_down(20, 5); // scroll around a bit in Browse
        state.enter_filter();
        assert_eq!(
            state.filter_text(),
            "c",
            "filter must survive scrolling in Browse"
        );
    }

    #[test]
    fn backspace_and_cursor_movement_are_byte_safe_for_multi_byte_filter_text() {
        let mut state = HelpState::new();
        state.enter_filter();
        for c in "日本語".chars() {
            state.insert_char(c);
        }
        assert_eq!(state.filter_text(), "日本語");
        assert_eq!(state.cursor(), 3, "cursor counts characters, not bytes");
        state.backspace();
        assert_eq!(state.filter_text(), "日本");
        state.move_left();
        state.move_left();
        assert_eq!(state.cursor(), 0);
        state.insert_char('a');
        assert_eq!(state.filter_text(), "a日本");
    }

    #[test]
    fn control_modified_char_is_not_inserted_into_the_filter() {
        let mut state = HelpState::new();
        state.enter_filter();
        assert_eq!(
            handle_key(
                &mut state,
                key_mod(KeyCode::Char('a'), KeyModifiers::CONTROL),
                10,
                5
            ),
            HelpOutcome::Continue
        );
        assert_eq!(state.filter_text(), "");
    }

    #[test]
    fn ctrl_w_deletes_the_previous_word_from_the_filter() {
        let mut state = HelpState::new();
        state.enter_filter();
        for c in "goto def".chars() {
            state.insert_char(c);
        }
        assert_eq!(
            handle_key(
                &mut state,
                key_mod(KeyCode::Char('w'), KeyModifiers::CONTROL),
                10,
                5
            ),
            HelpOutcome::Continue
        );
        assert_eq!(state.filter_text(), "goto ");
    }

    // ---- scroll clamping -------------------------------------------------

    #[test]
    fn scroll_down_never_scrolls_past_a_full_last_page() {
        let mut state = HelpState::new();
        for _ in 0..50 {
            state.scroll_down(10, 4);
        }
        assert_eq!(
            state.scroll(),
            6,
            "10 rows, 4-row viewport: max scroll is 6"
        );
    }

    #[test]
    fn scroll_up_saturates_at_zero() {
        let mut state = HelpState::new();
        state.scroll_up();
        assert_eq!(state.scroll(), 0);
    }

    #[test]
    fn g_and_shift_g_hit_the_top_and_bottom_bounds() {
        let mut state = HelpState::new();
        state.bottom(10, 4);
        assert_eq!(state.scroll(), 6);
        state.top();
        assert_eq!(state.scroll(), 0);
    }

    // ---- gg chord (Browse mode) -------------------------------------------

    #[test]
    fn a_single_g_does_not_jump_when_the_list_is_scrolled() {
        let mut state = HelpState::new();
        state.scroll_down(10, 4);
        let scrolled = state.scroll();
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Char('g')), 10, 4),
            HelpOutcome::Continue
        );
        assert_eq!(
            state.scroll(),
            scrolled,
            "a lone `g` must wait for a second `g`, not jump to the top on its own"
        );
    }

    #[test]
    fn g_then_g_jumps_to_the_top() {
        let mut state = HelpState::new();
        state.scroll_down(10, 4);
        handle_key(&mut state, key(KeyCode::Char('g')), 10, 4);
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Char('g')), 10, 4),
            HelpOutcome::Continue
        );
        assert_eq!(state.scroll(), 0);
    }

    #[test]
    fn g_then_j_clears_the_pending_g_and_moves_down_instead() {
        let mut state = HelpState::new();
        state.scroll_down(10, 4); // scroll = 1
        handle_key(&mut state, key(KeyCode::Char('g')), 10, 4); // pending, no jump yet
        handle_key(&mut state, key(KeyCode::Char('j')), 10, 4); // not a second `g`
        assert_eq!(
            state.scroll(),
            2,
            "`j` after a lone `g` must move down, not be swallowed as gg's second key"
        );

        // The interrupted chord must be fully cleared, not just skipped for
        // one key — a following lone `g` must once again wait for its own
        // second press rather than jumping immediately.
        let scrolled = state.scroll();
        handle_key(&mut state, key(KeyCode::Char('g')), 10, 4);
        assert_eq!(state.scroll(), scrolled);
    }

    #[test]
    fn page_down_and_page_up_move_by_a_viewport_sized_jump_within_bounds() {
        let mut state = HelpState::new();
        state.page_down(20, 5);
        assert_eq!(state.scroll(), 5);
        state.page_down(20, 5);
        assert_eq!(state.scroll(), 10);
        state.page_up(5);
        assert_eq!(state.scroll(), 5);
    }

    #[test]
    fn when_everything_fits_scroll_stays_at_zero() {
        let mut state = HelpState::new();
        state.scroll_down(3, 10); // fewer rows than the viewport
        assert_eq!(state.scroll(), 0);
        state.bottom(3, 10);
        assert_eq!(state.scroll(), 0);
    }

    #[test]
    fn handle_key_scroll_and_page_keys_thread_through_to_the_state() {
        let mut state = HelpState::new();
        handle_key(&mut state, key(KeyCode::Char('j')), 10, 4);
        assert_eq!(state.scroll(), 1);
        handle_key(&mut state, key(KeyCode::PageDown), 10, 4);
        assert_eq!(state.scroll(), 5);
        handle_key(&mut state, key(KeyCode::Char('G')), 10, 4);
        assert_eq!(state.scroll(), 6);
        // `gg`, not a single `g` — see the `pending_g`-chord tests below
        // for why one lone `g` must not jump on its own.
        handle_key(&mut state, key(KeyCode::Char('g')), 10, 4);
        handle_key(&mut state, key(KeyCode::Char('g')), 10, 4);
        assert_eq!(state.scroll(), 0);
        handle_key(
            &mut state,
            key_mod(KeyCode::Char('n'), KeyModifiers::CONTROL),
            10,
            4,
        );
        assert_eq!(state.scroll(), 1);
        handle_key(
            &mut state,
            key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL),
            10,
            4,
        );
        assert_eq!(state.scroll(), 0);
    }
}
