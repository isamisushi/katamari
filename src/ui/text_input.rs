//! The single-line and word-boundary core shared by every raw-key-bypass
//! text field this codebase has — [`LineInput`] (aliased as
//! [`crate::ui::scope_menu::RevisionInput`] and
//! [`crate::ui::search::SearchInput`]), [`crate::ui::help::HelpState`]'s
//! filter field, and [`crate::ui::compose::ComposeBuffer`]'s per-line
//! editing all lean on [`char_byte_index`]/[`word_start_before`] below.
//! Before this module existed, `LineInput` and its five methods were
//! copy-pasted verbatim into `scope_menu` and `search`, and `help.rs` carried
//! a third, hand-rolled `filter: String`/`cursor: usize` pair reimplementing
//! the same insert/backspace/move logic a fourth time — four call sites with
//! nothing tying them together to catch one drifting from the others on a
//! future fix (a boundary bug, a new editing command). Issue #28's Ctrl-W/
//! Alt-Backspace word deletion is exactly that kind of fix: adding it once
//! here, via [`recognize`], reaches all four call sites at once instead of
//! needing four separate patches that could each get the word-boundary rule
//! slightly wrong in its own way.
//!
//! Every buffer here is `char`-indexed, never byte or grapheme — see
//! [`char_byte_index`]'s own docs for why. [`word_start_before`] follows
//! suit: it classifies word boundaries per-`char` (`char::is_whitespace`),
//! not by grapheme cluster, consistent with that design rather than a new,
//! more "correct" boundary rule this module's buffers were never built to
//! support.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// Converts a `char` index into `s` to the byte offset `str` methods
/// actually need — the one piece of arithmetic every buffer in this module
/// leans on to stay `char`-indexed (safe against ever splitting a UTF-8
/// sequence mid-character) while still using `String`'s byte-indexed API
/// underneath. `s.len()` past the last character (an out-of-range
/// `char_idx`), matching `str::char_indices`' own behavior of simply having
/// nothing left to yield there rather than panicking.
pub(super) fn char_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map_or(s.len(), |(byte_idx, _)| byte_idx)
}

/// Readline's unix-word-rubout boundary: starting from `cursor`, skip any
/// whitespace immediately before it, then skip the contiguous run of
/// non-whitespace before *that* — the `char` index a "delete the previous
/// word" command should cut back to. Two passes (not one) is what makes
/// `"hello world "` with the cursor at the end delete back to `"hello "`
/// (trailing space kept, "world" removed) rather than to `"hello"` (the
/// space eaten as part of the "word") or a no-op (stopping at the first
/// whitespace it sees) — real terminals' `C-w` skips trailing space before
/// looking for a word to delete, so mashing `C-w` twice after typing
/// "hello world " clears the whole line one word at a time instead of
/// getting stuck bouncing off the space. `cursor` is clamped to
/// `chars.len()` first so a caller can pass its own cursor position without
/// separately checking it's in bounds.
pub(super) fn word_start_before(chars: &[char], cursor: usize) -> usize {
    let mut i = cursor.min(chars.len());
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

/// A single-line, `char`-indexed text-editing buffer: insert/backspace/
/// delete-previous-word/move-left/move-right over one line of free-form
/// text, cursor addressed by `char` index (never byte — see
/// [`char_byte_index`]'s own docs on why that matters for anything beyond
/// ASCII). The shared shape behind
/// [`crate::ui::scope_menu::RevisionInput`] and
/// [`crate::ui::search::SearchInput`] — both are just `pub type` aliases
/// for this — since neither prompt needs anything more specific to its own
/// domain than "a user is typing one line of text." Not
/// [`crate::ui::compose::ComposeBuffer`]: that type is multi-line, with
/// `Enter` meaning "insert a newline" — exactly wrong for a field where
/// `Enter` means "submit this one line" and there's no second line to
/// navigate to.
#[derive(Debug, Default)]
pub(crate) struct LineInput {
    text: String,
    /// `char` index into `text` — not a byte offset.
    cursor: usize,
}

impl LineInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn insert_char(&mut self, c: char) {
        let byte_idx = char_byte_index(&self.text, self.cursor);
        self.text.insert(byte_idx, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = char_byte_index(&self.text, self.cursor - 1);
        let end = char_byte_index(&self.text, self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    /// `C-w`/`M-Backspace`: deletes from the cursor back to
    /// [`word_start_before`] in one step — a no-op at the start of the
    /// line, same as [`Self::backspace`] there.
    pub fn delete_previous_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.text.chars().collect();
        let start = word_start_before(&chars, self.cursor);
        let start_byte = char_byte_index(&self.text, start);
        let end_byte = char_byte_index(&self.text, self.cursor);
        self.text.replace_range(start_byte..end_byte, "");
        self.cursor = start;
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }

    /// Jumps the cursor past the last character — [`crate::ui::help::HelpState::enter_filter`]'s
    /// "resume editing where you left off" behavior for a filter that
    /// already has text in it, without exposing `cursor`/`text` themselves
    /// (both private) for a caller to set directly.
    pub fn move_to_end(&mut self) {
        self.cursor = self.text.chars().count();
    }
}

/// Renders one line of text with its `char`-column cursor position
/// underlined, so a text field shows where typing will land the same way a
/// real cursor would — `ratatui::Frame` has no text-input widget of its own
/// to delegate this to. Shared by every module that draws one of this
/// module's buffers: [`crate::ui::compose`]'s multi-line rows,
/// [`crate::ui::scope_menu`]'s revision/PR inputs,
/// [`crate::ui::help`]'s filter line, and
/// [`crate::ui::status_bar`]'s inline search prompt.
pub(super) fn cursor_marked_line(text: &str, cursor_col: usize) -> Line<'static> {
    let chars: Vec<char> = text.chars().collect();
    let before: String = chars[..cursor_col.min(chars.len())].iter().collect();
    let at = chars.get(cursor_col).copied();
    let after: String = if cursor_col < chars.len() {
        chars[cursor_col + 1..].iter().collect()
    } else {
        String::new()
    };

    let mut spans = vec![Span::raw(before)];
    spans.push(Span::styled(
        at.map_or_else(|| " ".to_owned(), String::from),
        Style::default().add_modifier(Modifier::REVERSED),
    ));
    if !after.is_empty() {
        spans.push(Span::raw(after));
    }
    Line::from(spans)
}

/// What [`recognize`] decided a raw key event means for a text buffer,
/// leaving anything context-specific (Enter, Esc, Up/Down, a save/submit/
/// clear key) to the caller — those differ per overlay (`Enter` newlines in
/// [`crate::ui::compose`] but submits in [`crate::ui::scope_menu`]/
/// [`crate::ui::search`]), so there's no one right answer this module could
/// bake in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditCommand {
    Insert(char),
    Backspace,
    DeletePreviousWord,
    MoveLeft,
    MoveRight,
}

/// The one editing-key dispatch every raw-key-bypass text field in this
/// codebase shares — [`crate::ui::compose::handle_key`],
/// [`crate::ui::scope_menu::handle_revision_key`],
/// [`crate::ui::search::handle_prompt_key`], and
/// [`crate::ui::help::handle_key`]'s `Filter` arm all match on this before
/// (or instead of) their own context-specific keys. `None` for any key none
/// of them handles as plain editing — Enter/Esc/Up/Down and any control/alt
/// combination not recognized below are left for the caller to either treat
/// specially or (per every existing call site) silently swallow, rather
/// than inserted literally: a stray `C-a`/`C-e` etc. typed out of habit
/// shouldn't drop a control character into the buffer.
///
/// Order matters: `Backspace`+`ALT` and `Char('w')`+`CONTROL` (both
/// readline's word-rubout, `Alt-Backspace`/`C-w`) are checked *before* the
/// plain-`Backspace` and plain-`Char` arms below them, so a modified key
/// never falls through to its unmodified sibling's behavior.
pub(crate) fn recognize(key: &KeyEvent) -> Option<EditCommand> {
    match key.code {
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
            Some(EditCommand::DeletePreviousWord)
        }
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(EditCommand::DeletePreviousWord)
        }
        KeyCode::Backspace => Some(EditCommand::Backspace),
        KeyCode::Left => Some(EditCommand::MoveLeft),
        KeyCode::Right => Some(EditCommand::MoveRight),
        // Any other control-modified char is left alone rather than
        // inserted literally (see this function's own docs) — `ALT` alone
        // still inserts, matching every call site's pre-existing behavior
        // for e.g. an emacs-style `M-x` typed into a text field.
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(EditCommand::Insert(c))
        }
        _ => None,
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

    // ---- word_start_before -------------------------------------------

    #[test]
    fn stops_at_the_start_of_the_word_the_cursor_sits_inside() {
        let chars: Vec<char> = "hello world".chars().collect();
        // Cursor at 8, mid "world" ("hello wo|rld").
        assert_eq!(word_start_before(&chars, 8), 6);
    }

    #[test]
    fn skips_a_run_of_trailing_whitespace_before_finding_the_word() {
        // Cursor at the very end, past three trailing spaces — the first
        // pass must skip all three before the second pass finds "world".
        let chars: Vec<char> = "hello world   ".chars().collect();
        assert_eq!(word_start_before(&chars, chars.len()), 6);
    }

    #[test]
    fn skips_a_run_of_punctuation_as_one_word() {
        let chars: Vec<char> = "hello --- world".chars().collect();
        // Cursor right after "---" (index 9): punctuation counts as
        // non-whitespace, so the whole run is one "word" to delete.
        assert_eq!(word_start_before(&chars, 9), 6);
    }

    #[test]
    fn an_empty_buffer_stays_at_zero() {
        let chars: Vec<char> = Vec::new();
        assert_eq!(word_start_before(&chars, 0), 0);
    }

    #[test]
    fn all_whitespace_before_the_cursor_collapses_to_zero() {
        let chars: Vec<char> = "   ".chars().collect();
        assert_eq!(word_start_before(&chars, 3), 0);
    }

    #[test]
    fn a_cursor_at_the_very_start_is_already_the_word_start() {
        let chars: Vec<char> = "hello".chars().collect();
        assert_eq!(word_start_before(&chars, 0), 0);
    }

    #[test]
    fn cjk_characters_count_individually_not_as_one_run() {
        // No ASCII whitespace between the two CJK "words" here, so — per
        // this function's per-`char` whitespace rule, not a script-aware
        // word-segmentation one — they're one contiguous non-whitespace
        // run, same as "helloworld" would be.
        let chars: Vec<char> = "日本語 テスト".chars().collect();
        assert_eq!(word_start_before(&chars, chars.len()), 4); // skips "テスト" only
    }

    // ---- LineInput::delete_previous_word ------------------------------

    #[test]
    fn delete_previous_word_removes_the_word_behind_the_cursor() {
        let mut input = LineInput::new();
        for c in "hello world".chars() {
            input.insert_char(c);
        }
        input.delete_previous_word();
        assert_eq!(input.text(), "hello ");
        assert_eq!(input.cursor(), 6);
    }

    #[test]
    fn delete_previous_word_from_mid_text_only_touches_the_word_behind_it() {
        let mut input = LineInput::new();
        for c in "foo bar baz".chars() {
            input.insert_char(c);
        }
        for _ in 0..4 {
            input.move_left(); // cursor between "bar" and " baz"
        }
        input.delete_previous_word();
        assert_eq!(input.text(), "foo  baz");
    }

    #[test]
    fn delete_previous_word_at_the_start_is_a_no_op() {
        let mut input = LineInput::new();
        input.delete_previous_word();
        assert_eq!(input.text(), "");
        assert_eq!(input.cursor(), 0);
    }

    // ---- recognize -----------------------------------------------------

    #[test]
    fn plain_backspace_recognizes_as_backspace() {
        assert_eq!(
            recognize(&key(KeyCode::Backspace)),
            Some(EditCommand::Backspace)
        );
    }

    #[test]
    fn alt_backspace_recognizes_as_delete_previous_word() {
        assert_eq!(
            recognize(&key_mod(KeyCode::Backspace, KeyModifiers::ALT)),
            Some(EditCommand::DeletePreviousWord)
        );
    }

    #[test]
    fn ctrl_w_recognizes_as_delete_previous_word() {
        assert_eq!(
            recognize(&key_mod(KeyCode::Char('w'), KeyModifiers::CONTROL)),
            Some(EditCommand::DeletePreviousWord)
        );
    }

    #[test]
    fn left_and_right_recognize_as_moves() {
        assert_eq!(recognize(&key(KeyCode::Left)), Some(EditCommand::MoveLeft));
        assert_eq!(
            recognize(&key(KeyCode::Right)),
            Some(EditCommand::MoveRight)
        );
    }

    #[test]
    fn a_plain_char_recognizes_as_insert() {
        assert_eq!(
            recognize(&key(KeyCode::Char('x'))),
            Some(EditCommand::Insert('x'))
        );
    }

    #[test]
    fn an_alt_modified_char_other_than_backspace_still_inserts() {
        assert_eq!(
            recognize(&key_mod(KeyCode::Char('x'), KeyModifiers::ALT)),
            Some(EditCommand::Insert('x'))
        );
    }

    #[test]
    fn a_control_modified_char_other_than_w_recognizes_as_nothing() {
        assert_eq!(
            recognize(&key_mod(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn context_specific_keys_are_left_for_the_caller() {
        for code in [KeyCode::Enter, KeyCode::Esc, KeyCode::Up, KeyCode::Down] {
            assert_eq!(recognize(&key(code)), None, "{code:?}");
        }
    }
}
