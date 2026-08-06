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

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

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
        } else if self.row > 0 {
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&current);
        }
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

/// `pub(super)`: [`crate::ui::scope_menu`]'s single-line revision input
/// reuses this exact char-index-to-byte-index conversion rather than
/// re-deriving it — see [`ComposeBuffer`]'s docs on why char indexing
/// (never byte indexing) is the right coordinate space for a text buffer a
/// user is actively typing into.
pub(super) fn char_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map_or(s.len(), |(byte_idx, _)| byte_idx)
}

/// What [`handle_key`] decided one key press should do to the overlay as a
/// whole, beyond editing the buffer itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeOutcome {
    /// The buffer changed (or the key was a no-op); the overlay stays open.
    Continue,
    /// `C-s`: the caller should persist the buffer's text as a comment and
    /// close the overlay.
    Save,
    /// `Esc`: discard the buffer and close the overlay.
    Cancel,
}

/// Applies one raw terminal key event to `buffer`, bypassing
/// [`crate::keymap`] entirely — the compose overlay needs literal characters
/// (including IME-composed Japanese input, which crossterm delivers as
/// ordinary per-character `KeyCode::Char` events once the IME commits them),
/// not the action vocabulary every other view's input goes through. This is
/// why `ui::mod`'s event loop checks for an open overlay *before* feeding a
/// key to the keymap resolver at all: routing plain prose through the vim
/// keymap first would fire `Action::Quit` on a stray `q` instead of typing
/// one.
pub fn handle_key(buffer: &mut ComposeBuffer, key: KeyEvent) -> ComposeOutcome {
    match key.code {
        KeyCode::Esc => ComposeOutcome::Cancel,
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => ComposeOutcome::Save,
        KeyCode::Enter => {
            buffer.newline();
            ComposeOutcome::Continue
        }
        KeyCode::Backspace => {
            buffer.backspace();
            ComposeOutcome::Continue
        }
        KeyCode::Left => {
            buffer.move_left();
            ComposeOutcome::Continue
        }
        KeyCode::Right => {
            buffer.move_right();
            ComposeOutcome::Continue
        }
        KeyCode::Up => {
            buffer.move_up();
            ComposeOutcome::Continue
        }
        KeyCode::Down => {
            buffer.move_down();
            ComposeOutcome::Continue
        }
        // Any other control/alt-modified char is left alone rather than
        // inserted literally — a stray `C-a`/`C-e` etc. typed out of habit
        // shouldn't drop a control character into the comment body.
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            buffer.insert_char(c);
            ComposeOutcome::Continue
        }
        _ => ComposeOutcome::Continue,
    }
}

/// State for one open compose overlay: which row it was opened from (the
/// file/line a saved comment will anchor to — see `App::comment_target`)
/// and the buffer being edited.
pub struct ComposeState {
    pub file: String,
    pub line: u32,
    buffer: ComposeBuffer,
}

impl ComposeState {
    pub fn new(file: String, line: u32) -> Self {
        Self {
            file,
            line,
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

const HINT: &str = "Enter newline · C-s save · Esc cancel";

/// Draws the overlay near `cursor_screen_row`, reusing
/// [`crate::ui::hover_popup`]'s positioning idea (below the cursor when
/// there's room, above it otherwise) so the two floating panels this
/// codebase has feel like one family rather than two independent designs.
pub fn render(frame: &mut Frame, area: Rect, cursor_screen_row: u16, state: &ComposeState) {
    let rect = popup_rect(area, cursor_screen_row);
    frame.render_widget(Clear, rect);

    let title = format!(" comment: {}:{} ", state.file, state.line);
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
        HINT,
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Renders one line of the buffer with its `char`-column cursor position
/// underlined, so the compose overlay shows where typing will land the same
/// way a real cursor would — `ratatui::Frame` has no text-input widget of
/// its own to delegate this to. `pub(super)` rather than private:
/// [`crate::ui::scope_menu`]'s one-line revision input reuses this exact
/// rendering rather than re-deriving the same cursor math for a second kind
/// of text field.
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

    #[test]
    fn handle_key_esc_cancels() {
        let mut buffer = ComposeBuffer::new();
        assert_eq!(
            handle_key(&mut buffer, key(KeyCode::Esc)),
            ComposeOutcome::Cancel
        );
    }

    #[test]
    fn handle_key_ctrl_s_saves() {
        let mut buffer = ComposeBuffer::new();
        let outcome = handle_key(
            &mut buffer,
            key_mod(KeyCode::Char('s'), KeyModifiers::CONTROL),
        );
        assert_eq!(outcome, ComposeOutcome::Save);
    }

    #[test]
    fn handle_key_plain_char_inserts_and_continues() {
        let mut buffer = ComposeBuffer::new();
        let outcome = handle_key(&mut buffer, key(KeyCode::Char('x')));
        assert_eq!(outcome, ComposeOutcome::Continue);
        assert_eq!(buffer.text(), "x");
    }

    #[test]
    fn handle_key_enter_inserts_a_newline() {
        let mut buffer = ComposeBuffer::new();
        handle_key(&mut buffer, key(KeyCode::Char('a')));
        handle_key(&mut buffer, key(KeyCode::Enter));
        handle_key(&mut buffer, key(KeyCode::Char('b')));
        assert_eq!(buffer.text(), "a\nb");
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
}
