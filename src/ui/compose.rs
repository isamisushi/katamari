//! The comment-authoring overlay: a small multi-line text buffer for
//! `Action::AddComment`, plus the floating widget that draws it. Like
//! [`crate::ui::hover_popup::HoverState`], this lives outside `App` — saving
//! a finished comment needs the repo root and [`crate::comments::CommentStore`],
//! neither of which `App` owns, so `ui::mod`'s event loop is where the
//! overlay's lifecycle (open, edit, save-or-cancel) has to live.
//!
//! [`handle_key`] is deliberately free of any I/O or `ratatui` dependency —
//! it only ever mutates a [`ComposeBuffer`] and reports what should happen
//! next — so buffer-editing behavior (insert, newline, backspace, cursor
//! movement) is testable without a terminal, matching every other pure state
//! transition in this codebase.

use crate::keymap::{KeyChord, KeySeq};
use crate::ui::app::CommentTarget;
use crate::ui::text_input::{self, EditCommand, char_byte_index, cursor_marked_line};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::collections::HashMap;

/// A cursor-addressable multi-line text buffer, indexed by `char` (not byte
/// or grapheme) offsets within each line — simple enough for a short review
/// comment, and immune to ever splitting a UTF-8 sequence mid-character the
/// way byte indexing would. Always holds at least one (possibly empty)
/// line.
#[derive(Debug, Default)]
pub struct ComposeBuffer {
    lines: Vec<String>,
    row: usize,
    /// `char` index into `lines[row]` — not a byte offset.
    col: usize,
}

impl ComposeBuffer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    pub fn insert_char(&mut self, c: char) {
        let line = &mut self.lines[self.row];
        let byte_idx = char_byte_index(line, self.col);
        line.insert(byte_idx, c);
        self.col += 1;
    }

    pub fn newline(&mut self) {
        let line = &mut self.lines[self.row];
        let byte_idx = char_byte_index(line, self.col);
        let rest = line.split_off(byte_idx);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    /// Deletes the character before the cursor, or — at the start of a line
    /// past the first — merges this line into the end of the previous one,
    /// mirroring how backspace behaves in every ordinary text editor.
    pub fn backspace(&mut self) {
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let start = char_byte_index(line, self.col - 1);
            let end = char_byte_index(line, self.col);
            line.replace_range(start..end, "");
            self.col -= 1;
        } else {
            self.merge_into_previous_line();
        }
    }

    /// `C-w`/`M-Backspace`: deletes the word behind the cursor via
    /// [`text_input::word_start_before`] — readline's unix-word-rubout
    /// semantics, same as [`crate::ui::text_input::LineInput::delete_previous_word`]
    /// but scoped to the current line. At the start of a line past the
    /// first, there's no "word" to delete across the boundary, only a line
    /// to join — [`Self::merge_into_previous_line`], exactly
    /// [`Self::backspace`]'s own row-merge branch, not a no-op the way
    /// stopping at col 0 mid-buffer would be.
    pub fn delete_previous_word(&mut self) {
        if self.col > 0 {
            let chars: Vec<char> = self.lines[self.row].chars().collect();
            let start = text_input::word_start_before(&chars, self.col);
            let line = &mut self.lines[self.row];
            let start_byte = char_byte_index(line, start);
            let end_byte = char_byte_index(line, self.col);
            line.replace_range(start_byte..end_byte, "");
            self.col = start;
        } else {
            self.merge_into_previous_line();
        }
    }

    /// Removes the current line and appends its text to the end of the
    /// previous one, cursor landing at the join point — the shared tail of
    /// [`Self::backspace`] and [`Self::delete_previous_word`] when both hit
    /// column 0 past the first line. A no-op on the first line (`self.row
    /// == 0`): there's no previous line to merge into, so both callers'
    /// `self.col > 0` guard is what actually decides whether this runs, not
    /// a check in here.
    fn merge_into_previous_line(&mut self) {
        if self.row == 0 {
            return;
        }
        let current = self.lines.remove(self.row);
        self.row -= 1;
        self.col = self.lines[self.row].chars().count();
        self.lines[self.row].push_str(&current);
    }

    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
        }
    }

    pub fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
        }
    }

    /// The buffer's full text, lines joined with `\n` — what gets stored as
    /// a [`crate::comments::Comment::body`] on save.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Whether the buffer has nothing but whitespace in it — `ui::mod`
    /// refuses to save an empty comment rather than writing a blank body a
    /// reviewer almost certainly didn't mean to leave.
    pub fn is_blank(&self) -> bool {
        self.lines.iter().all(|l| l.trim().is_empty())
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// `(row, char-column)` — for the renderer to draw a cursor indicator.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }
}

/// The compose overlay's own two special keys, resolved once at startup
/// from `[keys.compose]` (issue #27) layered onto the `C-s`/`Esc` defaults.
/// Deliberately not `crate::keymap::Action`/`Keymap` entries the way every
/// other rebindable key in this app is: [`handle_key`]'s own docs explain
/// why compose bypasses the trie entirely (it needs literal characters,
/// including IME input, not the action vocabulary), and that reasoning
/// applies just as much to *how* save/cancel get rebound as to how the rest
/// of a keystroke is handled — a `KeyChord` compared directly against
/// [`KeyChord::from`] one raw event is all this overlay's single-event
/// dispatch can ever use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposeKeymap {
    save: KeyChord,
    cancel: KeyChord,
}

impl Default for ComposeKeymap {
    fn default() -> Self {
        Self {
            save: KeyChord::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            cancel: KeyChord::new(KeyCode::Esc, KeyModifiers::NONE),
        }
    }
}

impl ComposeKeymap {
    /// Applies `[keys.compose]`'s `save`/`cancel` entries onto
    /// [`Self::default`], failing on the first invalid one with a
    /// `"[keys.compose] <name> = <notation>: <why>"` message —
    /// [`crate::config::apply_key_overrides`]'s own error shape for the
    /// main `[keys]` table, so a bad entry in either table reads the same
    /// way in the startup error `ui::run` surfaces.
    ///
    /// This is deliberately *stricter* than that table: `Keymap::binding_for`'s
    /// docs (see its `binding_for_never_reports_an_entry_the_trie_resolves_to_a_different_action`
    /// test) walk through how a `[keys]` override is allowed to silently
    /// shadow another action's key, because the worst that does is leave
    /// that other action unbound and unmentioned in its own hint — a cost a
    /// reviewer discovers by reading `?`. A shadowed compose key is a
    /// different order of problem: it can make an ordinary typed character
    /// stop reaching the buffer at all (a bare-char binding — see the
    /// `Continue` outcome this function refuses before it ever ships), or
    /// make `Save`/`Cancel` indistinguishable (a `save == cancel`
    /// collision) — both silent at config-parse time and discovered only
    /// when a reviewer's comment doesn't save, or a letter they typed never
    /// showed up. So this validates and refuses outright instead of
    /// deferring to the same laxness — don't "fix" this divergence to match
    /// `[keys]`'s behavior; it's deliberate.
    pub fn resolve(overrides: &HashMap<String, String>) -> Result<Self, String> {
        let mut keymap = Self::default();
        for (name, notation) in overrides {
            if name != "save" && name != "cancel" {
                return Err(format!(
                    "[keys.compose]: unrecognized key `{name}` (expected `save` or `cancel`)"
                ));
            }
            let seq = KeySeq::try_parse(notation)
                .map_err(|e| format!("[keys.compose] {name} = {notation:?}: {e}"))?;
            let chord = seq.as_single_chord().ok_or_else(|| {
                format!(
                    "[keys.compose] {name} = {notation:?}: must be a single key, not a sequence"
                )
            })?;
            // A bare, unmodified character (`KeyChord::as_plain_char` —
            // includes `Space`, whose *notation* is a named key but whose
            // underlying chord is exactly as bare as any other unmodified
            // letter) would make that character untypeable — `Save`/
            // `Cancel` are checked ahead of the insert fallback in
            // `handle_key`, so it would never reach `ComposeBuffer` again.
            if let Some(c) = chord.as_plain_char() {
                return Err(format!(
                    "[keys.compose] {name} = {notation:?}: binding to {c:?} would stop that \
                     character from being typed into a comment"
                ));
            }
            match name.as_str() {
                "save" => keymap.save = chord,
                "cancel" => keymap.cancel = chord,
                _ => unreachable!("checked above"),
            }
        }
        if keymap.save == keymap.cancel {
            return Err(format!(
                "[keys.compose]: save and cancel must not be the same key (both {})",
                keymap.save_notation()
            ));
        }
        Ok(keymap)
    }

    fn save_notation(self) -> String {
        KeySeq::from_chords(vec![self.save]).compact_notation()
    }

    fn cancel_notation(self) -> String {
        KeySeq::from_chords(vec![self.cancel]).compact_notation()
    }
}

/// What [`handle_key`] decided one key press should do to the overlay as a
/// whole, beyond editing the buffer itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeOutcome {
    /// The buffer changed (or the key was a no-op); the overlay stays open.
    Continue,
    /// `keys.save` (`C-s` by default): the caller should persist the
    /// buffer's text as a comment and close the overlay.
    Save,
    /// `keys.cancel` (`Esc` by default): discard the buffer and close the
    /// overlay.
    Cancel,
}

/// Applies one raw terminal key event to `buffer`, bypassing
/// [`crate::keymap`]'s `Action`/`Keymap` trie entirely — the compose
/// overlay needs literal characters (including IME-composed Japanese input,
/// which crossterm delivers as ordinary per-character `KeyCode::Char`
/// events once the IME commits them), not the action vocabulary every other
/// view's input goes through. This is why `ui::mod`'s event loop checks for
/// an open overlay *before* feeding a key to the keymap resolver at all:
/// routing plain prose through the vim keymap first would fire
/// `Action::Quit` on a stray `q` instead of typing one. `keys` (issue #27)
/// is the one piece of `crate::keymap` this still leans on — a bare
/// [`KeyChord`] comparison, not the trie, since there's only ever one raw
/// event to match against at a time.
pub fn handle_key(
    buffer: &mut ComposeBuffer,
    key: KeyEvent,
    keys: &ComposeKeymap,
) -> ComposeOutcome {
    let chord = KeyChord::from(key);
    if chord == keys.cancel {
        return ComposeOutcome::Cancel;
    }
    if chord == keys.save {
        return ComposeOutcome::Save;
    }
    match key.code {
        KeyCode::Enter => {
            buffer.newline();
            return ComposeOutcome::Continue;
        }
        KeyCode::Up => {
            buffer.move_up();
            return ComposeOutcome::Continue;
        }
        KeyCode::Down => {
            buffer.move_down();
            return ComposeOutcome::Continue;
        }
        _ => {}
    }
    // Everything else — insert/backspace/word-delete/left/right — is the
    // shared editing core every raw-key-bypass text field in this codebase
    // dispatches through; see `text_input::recognize`'s own docs for why an
    // unrecognized key (a stray `C-a`/`C-e` etc.) is swallowed rather than
    // inserted literally.
    match text_input::recognize(&key) {
        Some(EditCommand::Insert(c)) => buffer.insert_char(c),
        Some(EditCommand::Backspace) => buffer.backspace(),
        Some(EditCommand::DeletePreviousWord) => buffer.delete_previous_word(),
        Some(EditCommand::MoveLeft) => buffer.move_left(),
        Some(EditCommand::MoveRight) => buffer.move_right(),
        None => {}
    }
    ComposeOutcome::Continue
}

/// State for one open compose overlay: what it was opened to comment on —
/// a single line or (issue #19) a validated visual-selection range, see
/// `App::comment_target` — and the buffer being edited.
pub struct ComposeState {
    pub target: CommentTarget,
    buffer: ComposeBuffer,
}

impl ComposeState {
    pub fn new(target: CommentTarget) -> Self {
        Self {
            target,
            buffer: ComposeBuffer::new(),
        }
    }

    pub fn buffer_mut(&mut self) -> &mut ComposeBuffer {
        &mut self.buffer
    }

    pub fn buffer(&self) -> &ComposeBuffer {
        &self.buffer
    }
}

/// The overlay's bottom-row hint, built from `keys`' live bindings rather
/// than a hardcoded string — the same "read the resolved keymap, don't
/// restate it" rule `ui::hints`/`ui::help` follow for every other action's
/// hint, now that `[keys.compose]` (issue #27) can move save/cancel off
/// their `C-s`/`Esc` defaults.
fn hint_text(keys: &ComposeKeymap) -> String {
    format!(
        "Enter newline · {} save · {} cancel",
        keys.save_notation(),
        keys.cancel_notation()
    )
}

/// Draws the overlay near `cursor_screen_row`, reusing
/// [`crate::ui::hover_popup`]'s positioning idea (below the cursor when
/// there's room, above it otherwise) so the two floating panels this
/// codebase has feel like one family rather than two independent designs.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    cursor_screen_row: u16,
    state: &ComposeState,
    keys: &ComposeKeymap,
) {
    let rect = popup_rect(area, cursor_screen_row);
    frame.render_widget(Clear, rect);

    let title = format!(" comment: {} ", state.target.location_label());
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let text_height = inner.height.saturating_sub(1) as usize; // last row is the hint
    let (cursor_row, cursor_col) = state.buffer.cursor();
    let mut lines: Vec<Line> = state
        .buffer
        .lines()
        .iter()
        .enumerate()
        .take(text_height)
        .map(|(idx, text)| {
            if idx == cursor_row {
                cursor_marked_line(text, cursor_col)
            } else {
                Line::from(text.clone())
            }
        })
        .collect();
    while lines.len() < text_height {
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        hint_text(keys),
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// As `hover_popup::popup_rect`: roughly 60% wide, 40% tall, anchored just
/// below the cursor's row (or above it, if there isn't room below).
fn popup_rect(area: Rect, cursor_screen_row: u16) -> Rect {
    let width = ((area.width as u32 * 3) / 5).clamp(20, area.width.max(20) as u32) as u16;
    let height = ((area.height as u32 * 2) / 5).clamp(5, area.height.max(5) as u32) as u16;
    let x = area.x + area.width.saturating_sub(width) / 2;

    let space_below = area
        .height
        .saturating_sub(cursor_screen_row.saturating_add(1));
    let y = if space_below >= height {
        area.y + cursor_screen_row + 1
    } else if cursor_screen_row >= height {
        area.y + cursor_screen_row - height
    } else {
        area.y
    };
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn typed_characters_insert_at_the_cursor() {
        let mut buffer = ComposeBuffer::new();
        for c in "hi".chars() {
            buffer.insert_char(c);
        }
        assert_eq!(buffer.text(), "hi");
        assert_eq!(buffer.cursor(), (0, 2));
    }

    #[test]
    fn enter_splits_the_line_at_the_cursor_into_two() {
        let mut buffer = ComposeBuffer::new();
        for c in "hello".chars() {
            buffer.insert_char(c);
        }
        buffer.move_left();
        buffer.move_left(); // cursor between "hel" and "lo"
        buffer.newline();
        assert_eq!(buffer.text(), "hel\nlo");
        assert_eq!(buffer.cursor(), (1, 0));
    }

    #[test]
    fn backspace_at_line_start_merges_into_the_previous_line() {
        let mut buffer = ComposeBuffer::new();
        buffer.insert_char('a');
        buffer.newline();
        buffer.insert_char('b');
        buffer.move_left(); // cursor at col 0 of the second line
        buffer.backspace();
        assert_eq!(buffer.text(), "ab");
        assert_eq!(buffer.cursor(), (0, 1));
    }

    #[test]
    fn backspace_within_a_line_deletes_the_preceding_character() {
        let mut buffer = ComposeBuffer::new();
        for c in "abc".chars() {
            buffer.insert_char(c);
        }
        buffer.backspace();
        assert_eq!(buffer.text(), "ab");
    }

    #[test]
    fn japanese_characters_insert_one_at_a_time_like_committed_ime_input() {
        let mut buffer = ComposeBuffer::new();
        for c in "日本語".chars() {
            buffer.insert_char(c);
        }
        assert_eq!(buffer.text(), "日本語");
        assert_eq!(
            buffer.cursor(),
            (0, 3),
            "cursor advances per character, not per byte"
        );
    }

    #[test]
    fn is_blank_is_true_only_for_whitespace_only_content() {
        let mut buffer = ComposeBuffer::new();
        assert!(buffer.is_blank());
        buffer.insert_char(' ');
        buffer.newline();
        assert!(buffer.is_blank());
        buffer.insert_char('x');
        assert!(!buffer.is_blank());
    }

    /// Every `handle_key` test that doesn't care about `[keys.compose]`
    /// itself uses the built-in `C-s`/`Esc` defaults, same as before this
    /// overlay had a configurable keymap at all.
    fn default_keys() -> ComposeKeymap {
        ComposeKeymap::default()
    }

    #[test]
    fn handle_key_esc_cancels() {
        let mut buffer = ComposeBuffer::new();
        assert_eq!(
            handle_key(&mut buffer, key(KeyCode::Esc), &default_keys()),
            ComposeOutcome::Cancel
        );
    }

    #[test]
    fn handle_key_ctrl_s_saves() {
        let mut buffer = ComposeBuffer::new();
        let outcome = handle_key(
            &mut buffer,
            key_mod(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &default_keys(),
        );
        assert_eq!(outcome, ComposeOutcome::Save);
    }

    #[test]
    fn handle_key_plain_char_inserts_and_continues() {
        let mut buffer = ComposeBuffer::new();
        let outcome = handle_key(&mut buffer, key(KeyCode::Char('x')), &default_keys());
        assert_eq!(outcome, ComposeOutcome::Continue);
        assert_eq!(buffer.text(), "x");
    }

    #[test]
    fn handle_key_enter_inserts_a_newline() {
        let mut buffer = ComposeBuffer::new();
        let keys = default_keys();
        handle_key(&mut buffer, key(KeyCode::Char('a')), &keys);
        handle_key(&mut buffer, key(KeyCode::Enter), &keys);
        handle_key(&mut buffer, key(KeyCode::Char('b')), &keys);
        assert_eq!(buffer.text(), "a\nb");
    }

    #[test]
    fn delete_previous_word_removes_the_word_behind_the_cursor_mid_line() {
        let mut buffer = ComposeBuffer::new();
        for c in "hello world".chars() {
            buffer.insert_char(c);
        }
        buffer.delete_previous_word();
        assert_eq!(buffer.text(), "hello ");
        assert_eq!(buffer.cursor(), (0, 6));
    }

    #[test]
    fn delete_previous_word_skips_trailing_whitespace_before_the_word() {
        let mut buffer = ComposeBuffer::new();
        for c in "hello world   ".chars() {
            buffer.insert_char(c);
        }
        buffer.delete_previous_word();
        assert_eq!(buffer.text(), "hello ");
    }

    #[test]
    fn delete_previous_word_at_a_line_start_merges_into_the_previous_line() {
        let mut buffer = ComposeBuffer::new();
        buffer.insert_char('a');
        buffer.newline();
        buffer.insert_char('b');
        buffer.move_left(); // cursor at col 0 of the second line
        buffer.delete_previous_word();
        assert_eq!(
            buffer.text(),
            "ab",
            "at col 0 past the first line this is a line-join, not a no-op"
        );
        assert_eq!(buffer.cursor(), (0, 1));
    }

    #[test]
    fn handle_key_ctrl_w_deletes_the_previous_word() {
        let mut buffer = ComposeBuffer::new();
        for c in "hello world".chars() {
            buffer.insert_char(c);
        }
        let outcome = handle_key(
            &mut buffer,
            key_mod(KeyCode::Char('w'), KeyModifiers::CONTROL),
            &default_keys(),
        );
        assert_eq!(outcome, ComposeOutcome::Continue);
        assert_eq!(buffer.text(), "hello ");
    }

    #[test]
    fn handle_key_alt_backspace_deletes_the_previous_word() {
        let mut buffer = ComposeBuffer::new();
        for c in "hello world".chars() {
            buffer.insert_char(c);
        }
        let outcome = handle_key(
            &mut buffer,
            key_mod(KeyCode::Backspace, KeyModifiers::ALT),
            &default_keys(),
        );
        assert_eq!(outcome, ComposeOutcome::Continue);
        assert_eq!(buffer.text(), "hello ");
    }

    #[test]
    fn move_up_and_down_clamp_the_column_to_the_shorter_line() {
        let mut buffer = ComposeBuffer::new();
        for c in "abcdef".chars() {
            buffer.insert_char(c);
        }
        buffer.newline();
        buffer.insert_char('x'); // second line: "x", cursor at col 1
        buffer.move_up();
        assert_eq!(
            buffer.cursor(),
            (0, 1),
            "column clamps to the shorter target line"
        );
    }

    // ---- ComposeKeymap::resolve (issue #27) ---------------------------

    fn overrides(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn default_keymap_matches_the_pre_issue_27_hardcoded_bindings() {
        let keys = ComposeKeymap::default();
        assert_eq!(
            keys.save,
            KeyChord::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
        );
        assert_eq!(keys.cancel, KeyChord::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(hint_text(&keys), "Enter newline · C-s save · Esc cancel");
    }

    #[test]
    fn resolve_rebinds_both_save_and_cancel() {
        let keys = ComposeKeymap::resolve(&overrides(&[("save", "C-x"), ("cancel", "C-g")]))
            .expect("both entries are valid");
        assert_eq!(
            keys.save,
            KeyChord::new(KeyCode::Char('x'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            keys.cancel,
            KeyChord::new(KeyCode::Char('g'), KeyModifiers::CONTROL)
        );
        assert_eq!(hint_text(&keys), "Enter newline · C-x save · C-g cancel");
    }

    #[test]
    fn resolve_rebinding_only_save_leaves_cancel_at_its_default() {
        let keys =
            ComposeKeymap::resolve(&overrides(&[("save", "C-x")])).expect("valid single entry");
        assert_eq!(
            keys.save,
            KeyChord::new(KeyCode::Char('x'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            keys.cancel,
            ComposeKeymap::default().cancel,
            "cancel keeps its Esc default when only save is overridden"
        );
    }

    #[test]
    fn resolve_rejects_an_unrecognized_key_name() {
        let err = ComposeKeymap::resolve(&overrides(&[("quit", "C-x")]))
            .expect_err("compose only has save/cancel");
        assert!(err.contains("[keys.compose]"), "{err}");
        assert!(err.contains("quit"), "{err}");
    }

    #[test]
    fn resolve_rejects_malformed_notation() {
        let err = ComposeKeymap::resolve(&overrides(&[("save", "C-NotAKey")]))
            .expect_err("bad key name in notation");
        assert!(err.contains("[keys.compose] save"), "{err}");
    }

    #[test]
    fn resolve_rejects_a_multi_chord_sequence() {
        let err = ComposeKeymap::resolve(&overrides(&[("save", "g g")]))
            .expect_err("compose has no trie to resolve a sequence through");
        assert!(err.contains("single key"), "{err}");
    }

    #[test]
    fn resolve_rejects_a_bare_unmodified_character() {
        // Binding save to a plain "x" would make that letter untypeable —
        // save/cancel are checked before the insert fallback in
        // `handle_key`.
        let err = ComposeKeymap::resolve(&overrides(&[("save", "x")]))
            .expect_err("a bare character must never be accepted");
        assert!(err.contains("untypeable") || err.contains("typed"), "{err}");
    }

    #[test]
    fn resolve_rejects_a_named_key_that_also_types_a_character() {
        // "Space" is exactly as untypeable-making as a literal " " would
        // be — same bug, spelled differently.
        let err = ComposeKeymap::resolve(&overrides(&[("cancel", "Space")]))
            .expect_err("Space is a plain typable character under another name");
        assert!(err.contains("cancel"), "{err}");
    }

    #[test]
    fn resolve_rejects_save_and_cancel_bound_to_the_same_key() {
        let err = ComposeKeymap::resolve(&overrides(&[("save", "C-x"), ("cancel", "C-x")]))
            .expect_err("save and cancel must stay distinguishable");
        assert!(err.contains("save and cancel"), "{err}");
    }

    #[test]
    fn resolve_with_no_overrides_returns_the_defaults() {
        let keys = ComposeKeymap::resolve(&HashMap::new()).expect("empty overrides are valid");
        assert_eq!(keys, ComposeKeymap::default());
    }

    #[test]
    fn handle_key_honors_a_rebound_save_and_stops_honoring_the_old_default() {
        let keys = ComposeKeymap::resolve(&overrides(&[("save", "C-x")])).unwrap();
        let mut buffer = ComposeBuffer::new();

        // The new binding saves...
        assert_eq!(
            handle_key(
                &mut buffer,
                key_mod(KeyCode::Char('x'), KeyModifiers::CONTROL),
                &keys
            ),
            ComposeOutcome::Save
        );
        // ...and the old hardcoded `C-s` no longer does (nor does it insert
        // a literal control character — `text_input::recognize` swallows
        // any other control-modified char, same as it always has).
        let outcome = handle_key(
            &mut buffer,
            key_mod(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &keys,
        );
        assert_eq!(outcome, ComposeOutcome::Continue);
        assert_eq!(
            buffer.text(),
            "",
            "C-s must not have inserted anything either"
        );
    }

    #[test]
    fn handle_key_still_inserts_a_plain_character_that_collides_with_neither_binding() {
        let keys =
            ComposeKeymap::resolve(&overrides(&[("save", "C-x"), ("cancel", "C-g")])).unwrap();
        let mut buffer = ComposeBuffer::new();
        let outcome = handle_key(&mut buffer, key(KeyCode::Char('s')), &keys);
        assert_eq!(outcome, ComposeOutcome::Continue);
        assert_eq!(
            buffer.text(),
            "s",
            "plain 's' must still type a letter once save has moved off C-s"
        );
    }
}
