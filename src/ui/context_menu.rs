//! Issue #23: the right-click context menu — a compact, target-specific
//! action menu so mouse users can discover and invoke the same actions
//! their keyboard bindings already reach. Two things live here, mirroring
//! `scope_menu`'s own split between pure state and `ui::mod`'s dispatch:
//!
//! - Derivation ([`diff_row_entries`]/[`tree_dir_entries`]/[`tree_file_entries`]/
//!   [`file_view_symbol_entries`]): pure functions from already-resolved
//!   facts (an LSP readiness peek, a comment-target result, ...) to a
//!   bounded, never-empty `Vec<MenuEntry>` — no `LspManager`/`App`/terminal
//!   access of their own, so every combination is testable by hand-building
//!   the facts. Gathering those facts is `ui::mod`'s job (it owns
//!   `LspManager` and re-derives them every frame — see [`MenuEntry`]'s
//!   docs on why readiness text has to stay live) — the same split
//!   `scope_menu::available_entries` and `ui::mod::apply_scope_swap` draw
//!   between "what the menu could show" and "what actually resolving a
//!   choice does."
//! - [`ContextMenuState`]/[`render`]/[`entry_at`]: the popup's own
//!   navigation/selection state and its ratatui rendering — terminal-facing,
//!   but still no idea what invoking an entry actually *does* (that's
//!   [`MenuCommand`], dispatched entirely by `ui::mod::handle_action`'s
//!   interception block; see req 8 — this module never issues an LSP
//!   request, saves a comment, or touches the clipboard/tree itself).
//!
//! Every entry wraps an existing [`Action`] or one of the two genuinely new
//! tree-bulk operations ([`MenuCommand::ExpandAllDescendants`]/
//! [`MenuCommand::CollapseAllDescendants`]) — this menu is a second way to
//! *reach* an action, never a second implementation of one.

use crate::keymap::Action;
use crate::ui::app::{CommentTarget, CommentTargetError};
use crate::ui::mouse::FrameGeometry;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

/// What invoking a [`MenuEntry`] actually does. `Action(Action)` covers
/// every entry this menu can express in the keymap's own vocabulary —
/// hover/definition/references, add comment, start/cancel visual, yank —
/// *and*, less obviously, a files-tree row's open-file/toggle-directory
/// entries too: [`Action::Confirm`] already does exactly the right thing
/// for either row kind once `files_selection`/`focus` point at it (see
/// `App::confirm_row`'s own dispatch on [`crate::ui::file_tree::VisibleKind`]),
/// which is precisely the state `App::select_files_row` leaves behind at
/// menu-open time — so neither "open file" nor "toggle directory" needs a
/// command variant of its own. Only bulk descendant expand/collapse is
/// genuinely new: no keybinding reaches it today, and it's bounded by the
/// in-memory tree ([`crate::ui::file_tree::descendant_dir_paths`]) rather
/// than a filesystem walk — the one place this menu is a second mutation
/// path, not a second implementation of an existing one (req 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuCommand {
    Action(Action),
    ExpandAllDescendants,
    CollapseAllDescendants,
}

/// One selectable row: a label, whether it's currently invokable (`Ok`) or
/// disabled with the reason to show (`Err`, req 5 — e.g. "Go to definition
/// — LSP: rust-analyzer is starting"), and the command invoking it runs.
/// Rebuilt wholesale every frame the menu is open (see
/// [`ContextMenuState::set_entries`]) rather than mutated in place — an LSP
/// readiness reason is the one thing about an entry that can change while
/// the menu just sits there (nothing else can: the cursor/selection this
/// menu targets is frozen the instant it opens, since it — and every other
/// overlay — owns every keystroke and consumes every other click while
/// open).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    pub label: String,
    pub enabled: Result<(), String>,
    pub command: MenuCommand,
}

impl MenuEntry {
    fn new(label: impl Into<String>, enabled: Result<(), String>, command: MenuCommand) -> Self {
        Self {
            label: label.into(),
            enabled,
            command,
        }
    }
}

/// What a menu is showing entries *for* — captured at open time so a
/// live-recomputed frame (readiness text can flip while the menu sits open)
/// re-derives against the same target rather than re-resolving the click.
/// `TreeDir` names the directory by its repo-relative path (what
/// `App::set_descendants_collapsed`/`toggle_directory` and re-derivation's
/// own `visible_rows` lookup both key on); `TreeFile`/`DiffRow`/
/// `FileViewRow` need no payload — a files-tree file row always resolves to
/// the same one entry regardless of which file it is, and `DiffRow`/
/// `FileViewRow` read live off whatever `App`/`FileView`'s own cursor/visual
/// state already is (set once, at open time, by
/// `App::position_cursor_from_click`/`FileView::position_cursor_from_click`
/// — see `ui::mouse::handle_right_click`'s docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuTarget {
    TreeDir { path: String },
    TreeFile,
    DiffRow,
    FileViewRow,
}

/// The three LSP peeks a diff-row/file-view-row menu needs when a symbol
/// sits under the cursor — `None` means ready to invoke right now,
/// `Some(reason)` is `ui::mod::peek_action_readiness`'s verbatim disabled-
/// reason text (the exact same wording the keyboard path's own status line
/// would show). Grouped into one struct, rather than three loose
/// `Option<String>` parameters, so [`diff_row_entries`]/[`file_view_symbol_entries`]'s
/// signatures read as "the symbol triad," matching how the keyboard
/// bindings themselves group hover/gd/gr.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SymbolReadiness {
    pub hover: Option<String>,
    pub goto_definition: Option<String>,
    pub find_references: Option<String>,
}

fn readiness_result(peek: &Option<String>) -> Result<(), String> {
    match peek {
        None => Ok(()),
        Some(reason) => Err(reason.clone()),
    }
}

fn symbol_triad_entries(symbol: &SymbolReadiness) -> Vec<MenuEntry> {
    vec![
        MenuEntry::new(
            "Hover documentation",
            readiness_result(&symbol.hover),
            MenuCommand::Action(Action::Hover),
        ),
        MenuEntry::new(
            "Go to definition",
            readiness_result(&symbol.goto_definition),
            MenuCommand::Action(Action::GotoDefinition),
        ),
        MenuEntry::new(
            "Find references",
            readiness_result(&symbol.find_references),
            MenuCommand::Action(Action::FindReferences),
        ),
    ]
}

fn comment_label(target: &Result<CommentTarget, CommentTargetError>) -> String {
    match target {
        Ok(CommentTarget::Range { start, end, .. }) => {
            format!("Add comment ({} lines)", end - start + 1)
        }
        _ => "Add comment".to_owned(),
    }
}

fn visual_entry(visual_active: bool, can_start_visual: bool) -> MenuEntry {
    if visual_active {
        MenuEntry::new(
            "Cancel visual selection",
            Ok(()),
            MenuCommand::Action(Action::ToggleVisualLine),
        )
    } else {
        let enabled = if can_start_visual {
            Ok(())
        } else {
            Err("visual: no selectable source line here".to_owned())
        };
        MenuEntry::new(
            "Start visual selection",
            enabled,
            MenuCommand::Action(Action::ToggleVisualLine),
        )
    }
}

/// A diff-pane row's context-menu entries (req 2/3). `symbol` is `Some`
/// only when `App::hover_query` resolved a target at all — req 5's "omit
/// actions that make no conceptual sense," applied to the whole LSP triad
/// at once rather than disabling each individually when there's no symbol
/// under the cursor to begin with (a `Del` row, a header, plain
/// whitespace). `comment`/`visual_active`/`can_start_visual` are exactly
/// `App::comment_target()`/`App::visual_active()`/"is the cursor row a
/// `RenderRow::Line`" — see `ui::mod`'s one call site for how they're
/// gathered. Bounded 4..8 entries (comment, the visual toggle, and the two
/// resident-agent entries are always present), never empty.
pub fn diff_row_entries(
    symbol: Option<SymbolReadiness>,
    comment: Result<CommentTarget, CommentTargetError>,
    visual_active: bool,
    can_start_visual: bool,
) -> Vec<MenuEntry> {
    let mut entries = Vec::with_capacity(6);
    if let Some(symbol) = &symbol {
        entries.extend(symbol_triad_entries(symbol));
    }
    entries.push(MenuEntry::new(
        comment_label(&comment),
        comment
            .as_ref()
            .map(|_| ())
            .map_err(|e| e.message().to_owned()),
        MenuCommand::Action(Action::AddComment),
    ));
    entries.push(visual_entry(visual_active, can_start_visual));
    if visual_active {
        entries.push(MenuEntry::new(
            "Yank selection",
            Ok(()),
            MenuCommand::Action(Action::YankSelection),
        ));
    }
    // The resident-agent pair (see `crate::acp::session`): eligibility
    // mirrors the visual-selection entry just above rather than `comment`'s
    // — `Action::AskAgent`'s own docs explain why asking is looser than
    // commenting (any selectable line, on any diff, no re-anchoring
    // concern). "Push open comments" is unconditionally offered, same as
    // the comment/visual entries above — whether there's anything open to
    // push is a fact the agent handle, not this pure function, would need
    // to answer, so `ui::mod::handle_action`'s own `PushCommentsToAgent`
    // arm is where an empty comment list turns into a status note instead.
    let ask_enabled = if visual_active || can_start_visual {
        Ok(())
    } else {
        Err("ask: no selectable source line here".to_owned())
    };
    entries.push(MenuEntry::new(
        "Ask agent about this",
        ask_enabled,
        MenuCommand::Action(Action::AskAgent),
    ));
    entries.push(MenuEntry::new(
        "Push open comments to agent",
        Ok(()),
        MenuCommand::Action(Action::PushCommentsToAgent),
    ));
    entries
}

/// A `View::File` row's context-menu entries (req 2) — the symbol triad
/// only, since `FileView` has no comment/visual-selection concept at all
/// (see `FileView::update`'s own `ToggleVisualLine` no-op arm). Only ever
/// called when a symbol actually resolved — see `ui::mod`'s call site,
/// which treats "no symbol here" as a degenerate empty menu (nothing else
/// this target could conceptually show) rather than calling this at all.
pub fn file_view_symbol_entries(symbol: SymbolReadiness) -> Vec<MenuEntry> {
    symbol_triad_entries(&symbol)
}

/// A files-tree directory row's context-menu entries (req 4): a toggle
/// (mirrors `Space`/`Enter` on the row — always available, always `Ok`,
/// see [`MenuCommand::Action`]'s docs on why it's plain `Action::Confirm`),
/// then bulk expand/collapse of every nested directory, omitted entirely
/// when there are none (`descendant_dir_count == 0` — a leaf directory has
/// nothing to bulk-act on; req 4's own "if it can be added without
/// unbounded traversal surprises," satisfied by
/// `file_tree::descendant_dir_paths`'s in-memory bound — this is the
/// zR/zM analogy's "nothing to do" case, req 5's omit-don't-disable rule
/// rather than a disabled pair of entries nobody could ever use).
pub fn tree_dir_entries(expanded: bool, descendant_dir_count: usize) -> Vec<MenuEntry> {
    let toggle_label = if expanded {
        "Collapse directory"
    } else {
        "Expand directory"
    };
    let mut entries = vec![MenuEntry::new(
        toggle_label,
        Ok(()),
        MenuCommand::Action(Action::Confirm),
    )];
    if descendant_dir_count > 0 {
        entries.push(MenuEntry::new(
            "Expand all descendants",
            Ok(()),
            MenuCommand::ExpandAllDescendants,
        ));
        entries.push(MenuEntry::new(
            "Collapse all descendants",
            Ok(()),
            MenuCommand::CollapseAllDescendants,
        ));
    }
    entries
}

/// A files-tree file row's context-menu entries (req 4): one entry,
/// "Open file" — `Action::Confirm` again (see [`MenuCommand::Action`]'s
/// docs), always invokable.
pub fn tree_file_entries() -> Vec<MenuEntry> {
    vec![MenuEntry::new(
        "Open file",
        Ok(()),
        MenuCommand::Action(Action::Confirm),
    )]
}

/// One open context menu's navigation/selection state and the target it
/// was derived for. `entries` is never empty — callers gate on that before
/// ever constructing one (`ui::mouse::handle_right_click`'s degenerate case
/// reports "nothing to do here" as a status note instead of opening a
/// menu with zero rows — see `ui::mod`'s open-flow call site).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMenuState {
    pub target: MenuTarget,
    entries: Vec<MenuEntry>,
    selected: usize,
    /// The `(col, row)` the menu opened near — [`menu_rect`]'s anchor,
    /// fixed for the menu's whole lifetime even though entries themselves
    /// get replaced every frame (see [`Self::set_entries`]): the popup's
    /// on-screen position must never drift out from under a reviewer who's
    /// mid-decision just because a readiness reason's text changed length.
    anchor: (u16, u16),
}

impl ContextMenuState {
    /// Panics on an empty `entries` rather than silently rendering a menu
    /// with nothing in it and no key that could ever act on it — every real
    /// caller gates on non-empty first (see this type's own docs).
    pub fn new(target: MenuTarget, entries: Vec<MenuEntry>, anchor: (u16, u16)) -> Self {
        assert!(
            !entries.is_empty(),
            "a context menu must never open with zero entries"
        );
        Self {
            target,
            entries,
            selected: 0,
            anchor,
        }
    }

    pub fn entries(&self) -> &[MenuEntry] {
        &self.entries
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_entry(&self) -> &MenuEntry {
        &self.entries[self.selected]
    }

    pub fn anchor(&self) -> (u16, u16) {
        self.anchor
    }

    /// Live re-derivation (req 5: a readiness reason must flip in place
    /// while the menu sits open, not just on the frame it opened) —
    /// replaces `entries` wholesale and clamps `selected` against the new
    /// length. `ui::mod` closes the menu instead of calling this at all
    /// once a re-derivation comes back empty (the target stopped making
    /// sense entirely) — see that module's `refresh_context_menu`.
    pub fn set_entries(&mut self, entries: Vec<MenuEntry>) {
        assert!(!entries.is_empty(), "see ContextMenuState::new's own docs");
        self.selected = self.selected.min(entries.len() - 1);
        self.entries = entries;
    }

    /// A mouse click landing directly on entry `idx` (via [`entry_at`])
    /// selects it before `ui::mod` runs the exact same confirm dispatch
    /// `Action::Confirm` (keyboard Enter) would — one dispatch path for
    /// both input methods (req: "keyboard and mouse selection invoke the
    /// same underlying actions"), not two.
    pub fn set_selected(&mut self, idx: usize) {
        self.selected = idx.min(self.entries.len() - 1);
    }

    pub fn move_down(&mut self) {
        self.selected = (self.selected + 1).min(self.entries.len() - 1);
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_top(&mut self) {
        self.selected = 0;
    }

    pub fn move_bottom(&mut self) {
        self.selected = self.entries.len() - 1;
    }
}

/// `value` clamped into `[min, max]`, except `max` itself is first clamped
/// up to at least `1` and `min` down to at most `max` — the guard
/// [`Rect::clamp`]-style arithmetic elsewhere in this codebase doesn't need
/// (a terminal is never narrower than its own popup's *minimum* width in
/// practice), but [`menu_rect`] must never panic even against a
/// pathologically small `area` (req: "the menu never draws outside narrow/
/// short terminals") — plain `u16::clamp` panics whenever `min > max`,
/// exactly the case a pathological `area` (`width`/`height` below the
/// popup's own floor) would otherwise hit.
fn clamp_dim(value: u16, min: u16, max: u16) -> u16 {
    let max = max.max(1);
    let min = min.min(max);
    value.clamp(min, max)
}

fn entry_display_width(entry: &MenuEntry) -> usize {
    let label_len = entry.label.chars().count();
    match &entry.enabled {
        Ok(()) => label_len,
        Err(reason) => label_len + 3 + reason.chars().count(), // " — "
    }
}

/// The popup's own rect for `entries`, anchored near `anchor` and clamped
/// entirely inside `area` — req: "the menu never draws outside narrow/short
/// terminals." Width is the longest rendered label (padded 2 columns each
/// side for the border), clamped to at least 12 columns wide when `area`
/// has room; height is `entries.len() + 2` (one row of border top/bottom).
/// Opens below-and-right of `anchor` by default, flipping left when it
/// would run off the right edge and flipping above when it would run off
/// the bottom — the same "prefer the natural direction, flip only when it
/// wouldn't fit" rule [`crate::ui::hover_popup::popup_rect`] already uses
/// for the hover popup, just with two axes to flip instead of one (a menu,
/// unlike the hover popup, is never forced to open flush against `area`'s
/// own top when *both* directions run out of room — a menu is at most a
/// handful of rows tall, so on any terminal large enough to run `ktmr` at
/// all, one of the two directions always has room).
pub fn menu_rect(area: Rect, anchor: (u16, u16), entries: &[MenuEntry]) -> Rect {
    let max_label = entries.iter().map(entry_display_width).max().unwrap_or(0) as u16;
    let width = clamp_dim(max_label.saturating_add(4), 12, area.width);
    let height = clamp_dim(entries.len() as u16 + 2, 3, area.height);
    let (anchor_x, anchor_y) = anchor;

    let right_edge = area.x.saturating_add(area.width);
    let x = if anchor_x.saturating_add(width) <= right_edge {
        anchor_x.max(area.x)
    } else {
        right_edge.saturating_sub(width)
    };

    let bottom_edge = area.y.saturating_add(area.height);
    let below_fits = anchor_y.saturating_add(1).saturating_add(height) <= bottom_edge;
    let y = if below_fits {
        anchor_y.saturating_add(1)
    } else {
        anchor_y.saturating_sub(height).max(area.y)
    };

    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Renders the menu — `Clear` then a bordered `" menu "` block, selected
/// row bold+reversed, disabled rows dimmed with their reason appended
/// (req 5). Records its popup rect via [`FrameGeometry::record_context_menu`]
/// — deliberately *not* through the ordinary [`FrameGeometry::record`]/
/// [`crate::ui::mouse::ScrollTarget`] path every other overlay in this codebase uses: those
/// exist to name what a wheel tick or a stray click over a whole pane
/// should do, but a right-click missing this popup must still resolve
/// against whatever's *underneath* it (`DiffFiles`/`DiffPane`/`FilePane` —
/// the open flow's retarget rule, req 7), which a coarse "this rect blocks
/// the whole pane" recording (the shape the fully-modal `*Modal` variants
/// use) would defeat — see [`FrameGeometry::context_menu_rect`]'s own docs
/// for the full reasoning, and `ui::mouse::handle_right_click`/`ui::mod`'s
/// event loop for the two places that actually consult this field.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &ContextMenuState,
    geometry: &mut FrameGeometry,
) {
    let rect = menu_rect(area, state.anchor(), state.entries());
    geometry.record_context_menu(rect);

    frame.render_widget(Clear, rect);
    let block = Block::default().borders(Borders::ALL).title(" menu ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let lines: Vec<Line> = state
        .entries()
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let mut style = Style::default();
            if entry.enabled.is_err() {
                style = style.fg(Color::DarkGray);
            }
            if idx == state.selected() {
                style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
            }
            let mut text = entry.label.clone();
            if let Err(reason) = &entry.enabled {
                text.push_str(" \u{2014} "); // " — "
                text.push_str(reason);
            }
            Line::from(Span::styled(text, style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Which entry index (if any) sits at `(col, row)` within `rect` — the
/// popup's own border-inclusive rect, exactly as [`menu_rect`] returns it
/// (a fixed one-cell border on every side, so a plain +1/-2 is the whole
/// computation — no second border-geometry helper needed for a rect this
/// module fully owns the shape of). `None` on the border/title row/column
/// or past the last entry — a click there is non-actionable, letting
/// `ui::mod`'s mouse-while-open handling treat it as a captured no-op
/// rather than a miss that could fall through to content underneath
/// (mirrors [`crate::ui::mouse::resolve_hit`]'s divider/gutter "nothing
/// here" cases).
pub fn entry_at(rect: Rect, entry_count: usize, col: u16, row: u16) -> Option<usize> {
    if !rect.contains(Position { x: col, y: row }) {
        return None;
    }
    let left = rect.x + 1;
    let right = rect.x + rect.width.saturating_sub(1); // the right border column itself
    if col < left || col >= right {
        return None;
    }
    let top = rect.y + 1;
    let bottom = rect.y + rect.height.saturating_sub(1); // the bottom border column itself
    if row < top || row >= bottom {
        return None;
    }
    let idx = (row - top) as usize;
    (idx < entry_count).then_some(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_entry(label: &str) -> MenuEntry {
        MenuEntry::new(label, Ok(()), MenuCommand::Action(Action::Hover))
    }

    fn err_entry(label: &str, reason: &str) -> MenuEntry {
        MenuEntry::new(
            label,
            Err(reason.to_owned()),
            MenuCommand::Action(Action::Hover),
        )
    }

    // ---- diff_row_entries ---------------------------------------------

    #[test]
    fn diff_row_entries_with_no_symbol_omits_the_lsp_triad_entirely() {
        let entries = diff_row_entries(
            None,
            Err(CommentTargetError::NoSelectableLine),
            false,
            false,
        );
        assert!(
            entries
                .iter()
                .all(|e| e.command != MenuCommand::Action(Action::Hover))
        );
        assert!(
            entries
                .iter()
                .all(|e| e.command != MenuCommand::Action(Action::GotoDefinition))
        );
        assert_eq!(
            entries.len(),
            4,
            "comment + visual-toggle + ask-agent + push-comments-to-agent"
        );
    }

    #[test]
    fn diff_row_entries_with_a_symbol_includes_the_full_triad_in_order() {
        let symbol = SymbolReadiness {
            hover: None,
            goto_definition: Some("LSP: rust-analyzer is starting".to_owned()),
            find_references: None,
        };
        let entries = diff_row_entries(
            Some(symbol),
            Err(CommentTargetError::NoSelectableLine),
            false,
            false,
        );
        assert_eq!(entries[0].label, "Hover documentation");
        assert!(entries[0].enabled.is_ok());
        assert_eq!(entries[1].label, "Go to definition");
        assert_eq!(
            entries[1].enabled,
            Err("LSP: rust-analyzer is starting".to_owned())
        );
        assert_eq!(entries[2].label, "Find references");
        assert!(entries[2].enabled.is_ok());
    }

    #[test]
    fn diff_row_entries_comment_disabled_carries_the_exact_error_message() {
        let entries = diff_row_entries(
            None,
            Err(CommentTargetError::ContainsDeletion),
            false,
            false,
        );
        let comment = entries
            .iter()
            .find(|e| e.command == MenuCommand::Action(Action::AddComment))
            .unwrap();
        assert_eq!(
            comment.enabled,
            Err(CommentTargetError::ContainsDeletion.message().to_owned())
        );
    }

    #[test]
    fn diff_row_entries_single_line_comment_target_uses_the_plain_label() {
        let target = CommentTarget::Single {
            file: "a.rs".to_owned(),
            line: 3,
        };
        let entries = diff_row_entries(None, Ok(target), false, false);
        let comment = entries
            .iter()
            .find(|e| e.command == MenuCommand::Action(Action::AddComment))
            .unwrap();
        assert_eq!(comment.label, "Add comment");
        assert!(comment.enabled.is_ok());
    }

    #[test]
    fn diff_row_entries_range_comment_target_reports_the_line_count() {
        let target = CommentTarget::Range {
            file: "a.rs".to_owned(),
            start: 10,
            end: 13,
        };
        let entries = diff_row_entries(None, Ok(target), false, false);
        let comment = entries
            .iter()
            .find(|e| e.command == MenuCommand::Action(Action::AddComment))
            .unwrap();
        assert_eq!(comment.label, "Add comment (4 lines)");
    }

    #[test]
    fn diff_row_entries_visual_inactive_and_startable_offers_start_enabled() {
        let entries =
            diff_row_entries(None, Err(CommentTargetError::NoSelectableLine), false, true);
        let visual = entries
            .iter()
            .find(|e| e.command == MenuCommand::Action(Action::ToggleVisualLine))
            .unwrap();
        assert_eq!(visual.label, "Start visual selection");
        assert!(visual.enabled.is_ok());
        assert!(
            !entries
                .iter()
                .any(|e| e.command == MenuCommand::Action(Action::YankSelection)),
            "yank never appears without an active selection"
        );
    }

    #[test]
    fn diff_row_entries_visual_inactive_and_not_startable_is_disabled_with_a_reason() {
        let entries = diff_row_entries(
            None,
            Err(CommentTargetError::NoSelectableLine),
            false,
            false,
        );
        let visual = entries
            .iter()
            .find(|e| e.command == MenuCommand::Action(Action::ToggleVisualLine))
            .unwrap();
        assert_eq!(visual.label, "Start visual selection");
        assert_eq!(
            visual.enabled,
            Err("visual: no selectable source line here".to_owned())
        );
    }

    #[test]
    fn diff_row_entries_visual_active_offers_cancel_and_yank() {
        let entries = diff_row_entries(None, Err(CommentTargetError::NoSelectableLine), true, true);
        let visual = entries
            .iter()
            .find(|e| e.command == MenuCommand::Action(Action::ToggleVisualLine))
            .unwrap();
        assert_eq!(visual.label, "Cancel visual selection");
        assert!(visual.enabled.is_ok());
        let yank = entries
            .iter()
            .find(|e| e.command == MenuCommand::Action(Action::YankSelection))
            .expect("yank must appear while a selection is active");
        assert!(yank.enabled.is_ok());
    }

    #[test]
    fn diff_row_entries_is_never_empty_across_every_combination() {
        for symbol in [None, Some(SymbolReadiness::default())] {
            for comment in [
                Ok(CommentTarget::Single {
                    file: "a.rs".to_owned(),
                    line: 1,
                }),
                Err(CommentTargetError::NoSelectableLine),
            ] {
                for visual_active in [false, true] {
                    for can_start_visual in [false, true] {
                        let entries = diff_row_entries(
                            symbol.clone(),
                            comment.clone(),
                            visual_active,
                            can_start_visual,
                        );
                        assert!(!entries.is_empty());
                        assert!(entries.len() <= 8, "{}", entries.len());
                        assert!(entries.len() >= 4, "{}", entries.len());
                    }
                }
            }
        }
    }

    // ---- file_view_symbol_entries ---------------------------------------

    #[test]
    fn file_view_symbol_entries_is_always_exactly_the_triad() {
        let entries = file_view_symbol_entries(SymbolReadiness::default());
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries.iter().map(|e| e.label.as_str()).collect::<Vec<_>>(),
            vec!["Hover documentation", "Go to definition", "Find references"]
        );
    }

    // ---- tree_dir_entries / tree_file_entries ----------------------------

    #[test]
    fn tree_dir_entries_leaf_directory_omits_the_bulk_pair() {
        let entries = tree_dir_entries(true, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "Collapse directory");
    }

    #[test]
    fn tree_dir_entries_with_nested_directories_includes_the_bulk_pair() {
        let entries = tree_dir_entries(false, 3);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].label, "Expand directory");
        assert_eq!(entries[1].label, "Expand all descendants");
        assert_eq!(entries[1].command, MenuCommand::ExpandAllDescendants);
        assert_eq!(entries[2].label, "Collapse all descendants");
        assert_eq!(entries[2].command, MenuCommand::CollapseAllDescendants);
    }

    #[test]
    fn tree_dir_entries_every_entry_is_always_enabled() {
        for (expanded, count) in [(true, 0), (false, 0), (true, 5), (false, 5)] {
            for entry in tree_dir_entries(expanded, count) {
                assert!(entry.enabled.is_ok());
            }
        }
    }

    #[test]
    fn tree_file_entries_is_one_always_enabled_entry() {
        let entries = tree_file_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "Open file");
        assert!(entries[0].enabled.is_ok());
        assert_eq!(entries[0].command, MenuCommand::Action(Action::Confirm));
    }

    // ---- ContextMenuState nav clamps -------------------------------------

    fn three_entries() -> Vec<MenuEntry> {
        vec![ok_entry("a"), ok_entry("b"), ok_entry("c")]
    }

    #[test]
    fn move_down_and_up_clamp_at_the_entry_bounds() {
        let mut state = ContextMenuState::new(MenuTarget::DiffRow, three_entries(), (0, 0));
        state.move_up(); // already at 0
        assert_eq!(state.selected(), 0);
        for _ in 0..10 {
            state.move_down();
        }
        assert_eq!(state.selected(), 2);
        for _ in 0..10 {
            state.move_up();
        }
        assert_eq!(state.selected(), 0);
    }

    #[test]
    fn move_top_and_bottom_jump_to_the_ends() {
        let mut state = ContextMenuState::new(MenuTarget::DiffRow, three_entries(), (0, 0));
        state.move_bottom();
        assert_eq!(state.selected(), 2);
        state.move_top();
        assert_eq!(state.selected(), 0);
    }

    #[test]
    fn set_entries_clamps_selected_when_the_new_list_is_shorter() {
        let mut state = ContextMenuState::new(MenuTarget::DiffRow, three_entries(), (0, 0));
        state.move_bottom();
        assert_eq!(state.selected(), 2);
        state.set_entries(vec![ok_entry("only")]);
        assert_eq!(state.selected(), 0);
    }

    #[test]
    fn set_selected_clamps_to_the_last_entry() {
        let mut state = ContextMenuState::new(MenuTarget::DiffRow, three_entries(), (0, 0));
        state.set_selected(99);
        assert_eq!(state.selected(), 2);
        state.set_selected(1);
        assert_eq!(state.selected(), 1);
    }

    #[test]
    #[should_panic(expected = "zero entries")]
    fn new_panics_on_empty_entries() {
        ContextMenuState::new(MenuTarget::DiffRow, Vec::new(), (0, 0));
    }

    // ---- menu_rect: 4-corner clamping/flipping ---------------------------

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 24,
    };

    #[test]
    fn menu_rect_near_the_top_left_opens_below_and_right_unflipped() {
        let rect = menu_rect(AREA, (2, 2), &three_entries());
        assert_eq!(rect.x, 2);
        assert_eq!(rect.y, 3); // anchor row + 1
    }

    #[test]
    fn menu_rect_near_the_right_edge_flips_left() {
        let rect = menu_rect(AREA, (78, 2), &three_entries());
        assert!(
            rect.x + rect.width <= AREA.x + AREA.width,
            "must never draw past the right edge: x={} width={}",
            rect.x,
            rect.width
        );
        assert!(rect.x < 78, "must flip left of the anchor column");
    }

    #[test]
    fn menu_rect_near_the_bottom_edge_flips_above() {
        let rect = menu_rect(AREA, (2, 22), &three_entries());
        assert!(
            rect.y + rect.height <= AREA.y + AREA.height,
            "must never draw past the bottom edge: y={} height={}",
            rect.y,
            rect.height
        );
        assert!(rect.y < 22, "must flip above the anchor row");
    }

    #[test]
    fn menu_rect_near_the_bottom_right_corner_flips_both_axes() {
        let rect = menu_rect(AREA, (78, 22), &three_entries());
        assert!(rect.x + rect.width <= AREA.x + AREA.width);
        assert!(rect.y + rect.height <= AREA.y + AREA.height);
    }

    #[test]
    fn menu_rect_never_panics_on_a_pathologically_small_area() {
        let tiny = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 3,
        };
        let rect = menu_rect(tiny, (1, 1), &three_entries());
        assert!(rect.width <= tiny.width.max(1));
        assert!(rect.height <= tiny.height.max(1));
    }

    #[test]
    fn menu_rect_width_grows_with_the_longest_label_and_disabled_reason() {
        let short = vec![ok_entry("x")];
        let long = vec![err_entry(
            "Go to definition",
            "LSP: rust-analyzer is starting",
        )];
        let short_rect = menu_rect(AREA, (0, 0), &short);
        let long_rect = menu_rect(AREA, (0, 0), &long);
        assert!(long_rect.width > short_rect.width);
    }

    // ---- entry_at ---------------------------------------------------------

    #[test]
    fn entry_at_resolves_each_interior_row_in_order() {
        let rect = menu_rect(AREA, (0, 0), &three_entries());
        for i in 0..3 {
            let row = rect.y + 1 + i as u16;
            assert_eq!(entry_at(rect, 3, rect.x + 1, row), Some(i));
        }
    }

    #[test]
    fn entry_at_the_border_and_title_row_is_none() {
        let rect = menu_rect(AREA, (0, 0), &three_entries());
        assert_eq!(entry_at(rect, 3, rect.x, rect.y + 1), None, "left border");
        assert_eq!(
            entry_at(rect, 3, rect.x + rect.width - 1, rect.y + 1),
            None,
            "right border"
        );
        assert_eq!(entry_at(rect, 3, rect.x + 1, rect.y), None, "title row");
        assert_eq!(
            entry_at(rect, 3, rect.x + 1, rect.y + rect.height - 1),
            None,
            "bottom border"
        );
    }

    #[test]
    fn entry_at_outside_the_rect_entirely_is_none() {
        let rect = menu_rect(AREA, (10, 10), &three_entries());
        assert_eq!(entry_at(rect, 3, 0, 0), None);
    }

    #[test]
    fn entry_at_past_the_last_entry_but_still_in_the_border_box_is_none() {
        // `menu_rect` sizes the rect exactly to `entries.len()`, so asking
        // with a smaller `entry_count` than the rect was built for exercises
        // the "interior row, but past `entry_count`" branch directly.
        let rect = menu_rect(AREA, (0, 0), &three_entries());
        assert_eq!(entry_at(rect, 1, rect.x + 1, rect.y + 2), None);
    }

    // ---- render (real ratatui buffer) --------------------------------------

    #[test]
    fn render_marks_the_selected_row_reversed_and_a_disabled_row_dark_gray() {
        use ratatui::backend::TestBackend;

        let entries = vec![
            ok_entry("Enabled action"),
            err_entry("Disabled action", "not ready"),
        ];
        let mut state = ContextMenuState::new(MenuTarget::DiffRow, entries, (2, 2));
        state.move_down(); // select the disabled entry

        let backend = TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut geometry = FrameGeometry::new();
        terminal
            .draw(|frame| {
                render(frame, frame.area(), &state, &mut geometry);
            })
            .unwrap();

        let rect = geometry
            .context_menu_rect()
            .expect("render must record its own precise popup rect");
        let buffer = terminal.backend().buffer();

        let enabled_cell = buffer.cell((rect.x + 1, rect.y + 1)).unwrap();
        assert_ne!(
            enabled_cell.fg,
            Color::DarkGray,
            "an enabled, unselected row must render in the plain style"
        );
        assert!(!enabled_cell.modifier.contains(Modifier::REVERSED));

        let disabled_selected_cell = buffer.cell((rect.x + 1, rect.y + 2)).unwrap();
        assert_eq!(
            disabled_selected_cell.fg,
            Color::DarkGray,
            "a disabled row is dimmed regardless of selection"
        );
        assert!(
            disabled_selected_cell
                .modifier
                .contains(Modifier::BOLD | Modifier::REVERSED),
            "the selected row is bold+reversed even while also disabled"
        );
    }

    #[test]
    fn render_near_the_bottom_right_corner_stays_inside_the_screen() {
        use ratatui::backend::TestBackend;

        let state = ContextMenuState::new(MenuTarget::DiffRow, vec![ok_entry("only")], (39, 9));
        let backend = TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut geometry = FrameGeometry::new();
        terminal
            .draw(|frame| {
                render(frame, frame.area(), &state, &mut geometry);
            })
            .unwrap();

        let rect = geometry.context_menu_rect().unwrap();
        assert!(rect.x + rect.width <= 40, "{rect:?}");
        assert!(rect.y + rect.height <= 10, "{rect:?}");
    }

    #[test]
    fn render_records_a_precise_popup_rect_that_is_a_real_subset_of_area() {
        use ratatui::backend::TestBackend;

        let state = ContextMenuState::new(MenuTarget::DiffRow, vec![ok_entry("only")], (2, 2));
        let backend = TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut geometry = FrameGeometry::new();
        let area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        };
        terminal
            .draw(|frame| {
                render(frame, area, &state, &mut geometry);
            })
            .unwrap();

        // Deliberately *not* recorded as an ordinary `ScrollTarget::hit`-able
        // rect at all — see `FrameGeometry::context_menu_rect`'s docs on
        // why a right-click missing this rect must still see past it to
        // whatever's underneath.
        assert_eq!(geometry.hit(0, 0), None);
        let precise = geometry.context_menu_rect().unwrap();
        assert!(precise.width < area.width || precise.height < area.height);
    }
}
