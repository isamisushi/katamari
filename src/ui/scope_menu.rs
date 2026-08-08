//! M12's scope-picker popup: `o` from a live [`crate::ui::view::View::Diff`]
//! session opens a small menu for switching what's being reviewed — working
//! tree, staged, a free-form revision, or straight into `L`/`t`'s own views
//! — without restarting `ktmr diff` with CLI flags. Two things live here:
//!
//! - [`ScopeMenuList`]/[`available_entries`]: the menu itself, a pure,
//!   terminal-free state machine (which entries show, which is selected) —
//!   matching every other view/overlay's pattern in this codebase of
//!   keeping the state transitions testable without a real terminal.
//! - [`RevisionInput`]/[`handle_revision_key`]: the one-line text field
//!   "Revision…" opens, reusing [`crate::ui::compose`]'s char-indexed
//!   buffer conventions (see [`RevisionInput`]'s docs) since it's the same
//!   kind of "a user is typing text" state, just single-line and
//!   submit-on-Enter rather than newline-on-Enter.
//!
//! Neither type talks to git/jj or `ratatui::Frame` state directly.
//! Resolving a menu selection into an actual diff (a real subprocess call
//! that can fail) and rendering the popup are `crate::ui::mod`'s job — the
//! same split [`crate::ui::log_view::LogView`] draws between its own list
//! and `ui::mod::handle_action`'s `Action::Confirm` handling for it.

use crate::ui::compose::cursor_marked_line;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

/// One selectable row in the scope-menu popup. `Log`/`Timeline` don't swap
/// the current diff's content the way the other three do — selecting
/// either just opens the same view `L`/`t` already would (see
/// `crate::ui::mod::confirm_scope_menu_selection`) — but they're listed
/// here anyway for discoverability, per the milestone spec: a reviewer
/// browsing "what can I look at" shouldn't have to already know `L`/`t`
/// exist to find them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeMenuEntry {
    WorkingTree,
    Staged,
    Log,
    Timeline,
    Revision,
}

impl ScopeMenuEntry {
    pub fn label(self) -> &'static str {
        match self {
            ScopeMenuEntry::WorkingTree => "Working tree",
            ScopeMenuEntry::Staged => "Staged",
            ScopeMenuEntry::Log => "Log — browse commits/changes",
            ScopeMenuEntry::Timeline => "Timeline (jj)",
            ScopeMenuEntry::Revision => "Revision…",
        }
    }
}

/// Which entries the popup shows, in display order. `Working tree`/
/// `Staged`/`Log`/`Revision…` are always available — every repository
/// `ktmr` runs in is a git repository first, and staged/working-tree/a
/// revision or range are all plain `git` operations regardless of whether
/// jj is colocated. `Timeline (jj)` is the one entry that's actually
/// conditional, gated on the exact same detection [`crate::keymap::Action::ToggleTimeline`]
/// (`t`) already uses — see `crate::ui::mod::detect_jj_repo`.
pub fn available_entries(jj_available: bool) -> Vec<ScopeMenuEntry> {
    let mut entries = vec![
        ScopeMenuEntry::WorkingTree,
        ScopeMenuEntry::Staged,
        ScopeMenuEntry::Log,
    ];
    if jj_available {
        entries.push(ScopeMenuEntry::Timeline);
    }
    entries.push(ScopeMenuEntry::Revision);
    entries
}

/// The popup's list state: which entries show (fixed at construction —
/// jj's availability doesn't change mid-session) and which one is
/// highlighted. `j`/`k`/arrow-bound `Action::CursorDown`/`CursorUp` move
/// [`Self::selected`]; `Action::Confirm` reads it via
/// [`Self::selected_entry`] — see `crate::ui::mod::handle_action`'s
/// scope-menu interception.
pub struct ScopeMenuList {
    entries: Vec<ScopeMenuEntry>,
    selected: usize,
}

impl ScopeMenuList {
    pub fn new(jj_available: bool) -> Self {
        Self {
            entries: available_entries(jj_available),
            selected: 0,
        }
    }

    pub fn entries(&self) -> &[ScopeMenuEntry] {
        &self.entries
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// The highlighted entry. Never panics: [`available_entries`] always
    /// returns at least `WorkingTree`/`Staged`/`Log`/`Revision`, so the
    /// list this indexes is never empty.
    pub fn selected_entry(&self) -> ScopeMenuEntry {
        self.entries[self.selected]
    }

    pub fn move_down(&mut self) {
        self.selected = (self.selected + 1).min(self.entries.len() - 1);
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
}

/// A single-line text buffer for the "Revision…" entry's free-form input —
/// [`crate::ui::compose::LineInput`] under its own name here rather than a
/// redefinition: see that type's docs for why the buffer itself (char, not
/// byte, indexed — safe against ever splitting a UTF-8 sequence
/// mid-character) lives in `compose` and is shared with
/// [`crate::ui::search::SearchInput`] rather than each prompt keeping its
/// own copy. Deliberately not [`crate::ui::compose::ComposeBuffer`]: that
/// type is multi-line, with `Enter` meaning "insert a newline" — exactly
/// wrong for a field where `Enter` means "submit this one line" (see
/// [`handle_revision_key`]) and there is no second line to navigate to.
pub type RevisionInput = crate::ui::compose::LineInput;

/// What [`handle_revision_key`] decided one key press should do, beyond
/// editing the buffer itself — the single-line sibling of
/// [`crate::ui::compose::ComposeOutcome`], with `Back` in place of `Cancel`:
/// Esc here returns to [`ScopeMenuList`] rather than closing the whole
/// popup, since the reviewer most likely just wants to reconsider which
/// scope to pick, not back out of the menu entirely (see
/// `crate::ui::mod`'s scope-menu key handling for where that distinction
/// matters — the popup as a whole still closes on Esc from the list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionInputOutcome {
    Continue,
    /// `Enter`: submit the buffer's current text (trimmed by the caller —
    /// see `crate::ui::mod::apply_scope_swap`) as a revision/revset.
    Submit(String),
    Back,
}

/// Applies one raw terminal key event to `input`, bypassing
/// [`crate::keymap`] entirely — mirrors [`crate::ui::compose::handle_key`]'s
/// reasoning exactly: a revset like `main..feature` or `@-` can contain
/// characters (`.`, `@`, `-`) that would otherwise resolve to unrelated
/// [`crate::keymap::Action`]s if routed through the keymap resolver first.
pub fn handle_revision_key(input: &mut RevisionInput, key: KeyEvent) -> RevisionInputOutcome {
    match key.code {
        KeyCode::Esc => RevisionInputOutcome::Back,
        KeyCode::Enter => RevisionInputOutcome::Submit(input.text().to_owned()),
        KeyCode::Backspace => {
            input.backspace();
            RevisionInputOutcome::Continue
        }
        KeyCode::Left => {
            input.move_left();
            RevisionInputOutcome::Continue
        }
        KeyCode::Right => {
            input.move_right();
            RevisionInputOutcome::Continue
        }
        // As `compose::handle_key`: a stray control/alt-modified char
        // (habitual `C-a`/`C-e` etc.) is left alone rather than inserted
        // literally.
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            input.insert_char(c);
            RevisionInputOutcome::Continue
        }
        _ => RevisionInputOutcome::Continue,
    }
}

/// What the scope-menu popup is currently showing: the entry list, or (once
/// "Revision…" is selected) the one-line input for it. Owned by
/// `crate::ui::mod`'s event loop alongside `hover_state`/`compose`/
/// `refs_panel` — every other transient overlay this codebase has lives the
/// same way, outside the `View` stack, since none of them are a screen a
/// reviewer navigates *to* so much as a temporary layer on top of whichever
/// `View::Diff` is already on screen.
pub enum ScopeMenuState {
    List(ScopeMenuList),
    Revision(RevisionInput),
}

impl ScopeMenuState {
    pub fn new_list(jj_available: bool) -> Self {
        ScopeMenuState::List(ScopeMenuList::new(jj_available))
    }

    pub fn new_revision_input() -> Self {
        ScopeMenuState::Revision(RevisionInput::new())
    }
}

/// Roughly centered in `area`, sized to the content rather than a fixed
/// fraction of the screen the way [`crate::ui::hover_popup`]/
/// [`crate::ui::compose`]'s row-anchored popups are — a scope change isn't
/// about any particular row under the cursor, so there's no cursor row to
/// anchor near, and a small centered box reads more like the command
/// palette this menu actually is.
fn popup_rect(area: Rect, content_height: u16) -> Rect {
    let width = 46u16.min(area.width.saturating_sub(2)).max(20);
    let height = content_height.min(area.height.saturating_sub(2)).max(3);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Renders whichever half of [`ScopeMenuState`] is current. `jj_available`
/// controls only the revision input's placeholder text (a jj revset's
/// syntax vs. a git rev/range) — the list itself already baked its
/// `Timeline` availability in at construction (see [`ScopeMenuList::new`]).
pub fn render(frame: &mut Frame, area: Rect, state: &ScopeMenuState, jj_available: bool) {
    match state {
        ScopeMenuState::List(list) => render_list(frame, area, list),
        ScopeMenuState::Revision(input) => render_revision_input(frame, area, input, jj_available),
    }
}

fn render_list(frame: &mut Frame, area: Rect, list: &ScopeMenuList) {
    let rect = popup_rect(area, list.entries().len() as u16 + 2);
    frame.render_widget(Clear, rect);

    let block = Block::default().borders(Borders::ALL).title(" scope ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let lines: Vec<Line> = list
        .entries()
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let mut style = Style::default();
            if idx == list.selected_index() {
                style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
            }
            Line::from(Span::styled(entry.label(), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The revision input's prompt line, naming exactly what's accepted —
/// different in a colocated jj repo (a jj revset, with jj's own `..`/`@-`
/// operators) versus a git-only one (a git rev, or an `A..B`/`A...B`
/// range) — see `crate::ui::mod::apply_scope_swap`'s docs for why the two
/// modes are resolved so differently underneath this one text field.
fn prompt(jj_available: bool) -> &'static str {
    if jj_available {
        "jj revset — e.g. @, @-, main..feature"
    } else {
        "git rev, or A..B / A...B range"
    }
}

const REVISION_HINT: &str = "Enter diff · Esc back";

fn render_revision_input(frame: &mut Frame, area: Rect, input: &RevisionInput, jj_available: bool) {
    let rect = popup_rect(area, 5);
    frame.render_widget(Clear, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" scope: revision ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let lines = vec![
        Line::from(Span::styled(
            prompt(jj_available),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )),
        cursor_marked_line(input.text(), input.cursor()),
        Line::from(Span::styled(
            REVISION_HINT,
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    // ---- available_entries / ScopeMenuList --------------------------------

    #[test]
    fn available_entries_without_jj_excludes_timeline() {
        let entries = available_entries(false);
        assert!(!entries.contains(&ScopeMenuEntry::Timeline), "{entries:?}");
        assert_eq!(
            entries,
            vec![
                ScopeMenuEntry::WorkingTree,
                ScopeMenuEntry::Staged,
                ScopeMenuEntry::Log,
                ScopeMenuEntry::Revision,
            ]
        );
    }

    #[test]
    fn available_entries_with_jj_includes_timeline_before_revision() {
        let entries = available_entries(true);
        assert_eq!(
            entries,
            vec![
                ScopeMenuEntry::WorkingTree,
                ScopeMenuEntry::Staged,
                ScopeMenuEntry::Log,
                ScopeMenuEntry::Timeline,
                ScopeMenuEntry::Revision,
            ]
        );
    }

    #[test]
    fn move_down_and_up_clamp_at_the_list_bounds() {
        let mut list = ScopeMenuList::new(false);
        list.move_up(); // already at 0
        assert_eq!(list.selected_index(), 0);

        for _ in 0..10 {
            list.move_down();
        }
        assert_eq!(list.selected_index(), list.entries().len() - 1);
    }

    #[test]
    fn selected_entry_tracks_the_selected_index() {
        let mut list = ScopeMenuList::new(true);
        assert_eq!(list.selected_entry(), ScopeMenuEntry::WorkingTree);
        list.move_down();
        assert_eq!(list.selected_entry(), ScopeMenuEntry::Staged);
        list.move_down();
        list.move_down();
        assert_eq!(list.selected_entry(), ScopeMenuEntry::Timeline);
    }

    // ---- RevisionInput / handle_revision_key -------------------------------

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

    #[test]
    fn typed_characters_insert_at_the_cursor() {
        let mut input = RevisionInput::new();
        for c in "main..feature".chars() {
            input.insert_char(c);
        }
        assert_eq!(input.text(), "main..feature");
        assert_eq!(input.cursor(), 13);
    }

    #[test]
    fn backspace_deletes_the_preceding_character() {
        let mut input = RevisionInput::new();
        for c in "abc".chars() {
            input.insert_char(c);
        }
        input.backspace();
        assert_eq!(input.text(), "ab");
    }

    #[test]
    fn backspace_at_the_start_is_a_no_op() {
        let mut input = RevisionInput::new();
        input.backspace();
        assert_eq!(input.text(), "");
    }

    #[test]
    fn left_and_right_move_the_cursor_within_bounds() {
        let mut input = RevisionInput::new();
        for c in "ab".chars() {
            input.insert_char(c);
        }
        input.move_left();
        input.move_left();
        input.move_left(); // already at 0
        assert_eq!(input.cursor(), 0);
        input.insert_char('x');
        assert_eq!(input.text(), "xab");

        input.move_right();
        input.move_right();
        input.move_right();
        input.move_right(); // already at the end
        assert_eq!(input.cursor(), 3);
    }

    #[test]
    fn handle_key_enter_submits_the_current_text() {
        let mut input = RevisionInput::new();
        for c in "@-".chars() {
            input.insert_char(c);
        }
        assert_eq!(
            handle_revision_key(&mut input, key(KeyCode::Enter)),
            RevisionInputOutcome::Submit("@-".to_owned())
        );
    }

    #[test]
    fn handle_key_esc_goes_back_without_clearing_the_buffer() {
        let mut input = RevisionInput::new();
        input.insert_char('x');
        assert_eq!(
            handle_revision_key(&mut input, key(KeyCode::Esc)),
            RevisionInputOutcome::Back
        );
        // The buffer itself is left untouched by Esc — `crate::ui::mod`
        // discards it by dropping the whole `RevisionInput` when it goes
        // back to `ScopeMenuList`, not by clearing it in place.
        assert_eq!(input.text(), "x");
    }

    #[test]
    fn handle_key_plain_char_inserts_and_continues() {
        let mut input = RevisionInput::new();
        let outcome = handle_revision_key(&mut input, key(KeyCode::Char('@')));
        assert_eq!(outcome, RevisionInputOutcome::Continue);
        assert_eq!(input.text(), "@");
    }

    #[test]
    fn handle_key_control_modified_char_is_not_inserted() {
        let mut input = RevisionInput::new();
        let outcome = handle_revision_key(
            &mut input,
            key_mod(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        assert_eq!(outcome, RevisionInputOutcome::Continue);
        assert_eq!(input.text(), "");
    }
}
