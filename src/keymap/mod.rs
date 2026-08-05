//! Named actions, key-sequence notation, and a trie that resolves multi-key
//! sequences (`gg`, `]c`) the same way single keys resolve (`q`). Keeping
//! resolution generic over sequences — rather than special-casing "first key
//! then second key" in the event loop — is what lets an emacs-style preset
//! reuse this module later: `C-x` prefixes are just longer sequences through
//! the same trie.

use crossterm::event::{KeyCode, KeyModifiers};
use std::collections::HashMap;

/// A named editing operation. The event loop and `App::update` only ever
/// speak in `Action`s, never in raw key events — that's what keeps
/// `ui::app` testable without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    CursorDown,
    CursorUp,
    HalfPageDown,
    HalfPageUp,
    Top,
    Bottom,
    NextHunk,
    PrevHunk,
    NextFile,
    PrevFile,
    ToggleSidebar,
    ToggleLayout,
    Quit,
}

/// One key press, normalized for matching: the SHIFT modifier is dropped
/// because a shifted letter already arrives as its uppercase `KeyCode::Char`
/// (crossterm reports `Char('G')`, not `Char('g') + SHIFT`), so keeping SHIFT
/// in the comparison would make otherwise-identical chords fail to match on
/// terminals that report it inconsistently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyChord {
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self {
            code,
            modifiers: modifiers - KeyModifiers::SHIFT,
        }
    }

    /// Parses one whitespace-delimited token of key-sequence notation:
    /// `C-d` for Ctrl-d, `]` or `g` for a plain char, or a named key like
    /// `Esc`/`Enter`/`Tab`.
    fn parse_token(token: &str) -> Self {
        if let Some(rest) = token.strip_prefix("C-") {
            let code = Self::parse_key_name(rest);
            return Self::new(code, KeyModifiers::CONTROL);
        }
        Self::new(Self::parse_key_name(token), KeyModifiers::NONE)
    }

    fn parse_key_name(name: &str) -> KeyCode {
        match name {
            "Esc" => KeyCode::Esc,
            "Enter" => KeyCode::Enter,
            "Tab" => KeyCode::Tab,
            "Backspace" => KeyCode::Backspace,
            "Left" => KeyCode::Left,
            "Right" => KeyCode::Right,
            "Up" => KeyCode::Up,
            "Down" => KeyCode::Down,
            "Home" => KeyCode::Home,
            "End" => KeyCode::End,
            "PageUp" => KeyCode::PageUp,
            "PageDown" => KeyCode::PageDown,
            single if single.chars().count() == 1 => KeyCode::Char(single.chars().next().unwrap()),
            other => panic!("unrecognized key name in keymap notation: {other:?}"),
        }
    }

    /// Notation for the status bar's pending-sequence indicator; the inverse
    /// of [`Self::parse_token`] for the cases the vim preset uses.
    fn notation(self) -> String {
        let key = match self.code {
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Esc => "Esc".to_string(),
            KeyCode::Enter => "Enter".to_string(),
            KeyCode::Tab => "Tab".to_string(),
            KeyCode::Backspace => "Backspace".to_string(),
            KeyCode::Left => "Left".to_string(),
            KeyCode::Right => "Right".to_string(),
            KeyCode::Up => "Up".to_string(),
            KeyCode::Down => "Down".to_string(),
            KeyCode::Home => "Home".to_string(),
            KeyCode::End => "End".to_string(),
            KeyCode::PageUp => "PageUp".to_string(),
            KeyCode::PageDown => "PageDown".to_string(),
            other => format!("{other:?}"),
        };
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            format!("C-{key}")
        } else {
            key
        }
    }
}

impl From<crossterm::event::KeyEvent> for KeyChord {
    fn from(event: crossterm::event::KeyEvent) -> Self {
        Self::new(event.code, event.modifiers)
    }
}

/// A key sequence such as `g g` or `] c`, parsed from space-separated
/// notation tokens.
#[derive(Debug, Clone)]
pub struct KeySeq(Vec<KeyChord>);

impl KeySeq {
    pub fn parse(notation: &str) -> Self {
        Self(
            notation
                .split_whitespace()
                .map(KeyChord::parse_token)
                .collect(),
        )
    }
}

#[derive(Default)]
struct TrieNode {
    children: HashMap<KeyChord, TrieNode>,
    action: Option<Action>,
}

/// A trie over key sequences. Built once from a preset (e.g.
/// [`vim_preset`]) and then queried through a [`Resolver`] as keys arrive
/// one at a time.
pub struct Keymap {
    root: TrieNode,
}

impl Keymap {
    pub fn from_bindings(bindings: &[(KeySeq, Action)]) -> Self {
        let mut root = TrieNode::default();
        for (seq, action) in bindings {
            let mut node = &mut root;
            for chord in &seq.0 {
                node = node.children.entry(*chord).or_default();
            }
            node.action = Some(*action);
        }
        Self { root }
    }

    pub fn resolver(&self) -> Resolver<'_> {
        Resolver {
            root: &self.root,
            current: &self.root,
            pending: Vec::new(),
        }
    }
}

/// The outcome of feeding one key press into a [`Resolver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepResult {
    /// The sequence so far is a prefix of at least one binding; waiting for
    /// more keys.
    Pending,
    /// The sequence completed a binding.
    Matched(Action),
    /// The key doesn't continue any pending sequence; the resolver has been
    /// reset to accept a fresh sequence starting from the next key.
    Cancelled,
}

/// Tracks in-progress multi-key sequence matching against a [`Keymap`].
/// Lives across the event loop's key reads: one `Resolver` per session,
/// fed one key at a time.
pub struct Resolver<'a> {
    root: &'a TrieNode,
    current: &'a TrieNode,
    pending: Vec<KeyChord>,
}

impl<'a> Resolver<'a> {
    pub fn feed(&mut self, chord: KeyChord) -> StepResult {
        match self.current.children.get(&chord) {
            Some(node) if node.action.is_some() => {
                let action = node.action.unwrap();
                self.reset();
                StepResult::Matched(action)
            }
            Some(node) => {
                self.current = node;
                self.pending.push(chord);
                StepResult::Pending
            }
            None => {
                self.reset();
                StepResult::Cancelled
            }
        }
    }

    fn reset(&mut self) {
        self.current = self.root;
        self.pending.clear();
    }

    /// The keys matched so far, in notation form, for display in the status
    /// bar (e.g. `"g"` while waiting for a second `g` to complete `gg`).
    pub fn pending_display(&self) -> String {
        self.pending
            .iter()
            .map(|c| c.notation())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The default keymap: vim-style bindings, expressed as data rather than
/// hardcoded into the event loop, so an emacs preset can later live
/// alongside it as another function of the same shape.
pub fn vim_preset() -> Vec<(KeySeq, Action)> {
    [
        ("j", Action::CursorDown),
        ("k", Action::CursorUp),
        ("C-d", Action::HalfPageDown),
        ("C-u", Action::HalfPageUp),
        ("g g", Action::Top),
        ("G", Action::Bottom),
        ("] c", Action::NextHunk),
        ("[ c", Action::PrevHunk),
        ("] f", Action::NextFile),
        ("[ f", Action::PrevFile),
        ("b", Action::ToggleSidebar),
        ("s", Action::ToggleLayout),
        ("q", Action::Quit),
    ]
    .into_iter()
    .map(|(notation, action)| (KeySeq::parse(notation), action))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(c: char) -> KeyChord {
        KeyChord::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyChord {
        KeyChord::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn double_g_resolves_to_top_through_pending_state() {
        let keymap = Keymap::from_bindings(&vim_preset());
        let mut resolver = keymap.resolver();
        assert_eq!(resolver.feed(chord('g')), StepResult::Pending);
        assert_eq!(resolver.pending_display(), "g");
        assert_eq!(resolver.feed(chord('g')), StepResult::Matched(Action::Top));
        assert_eq!(resolver.pending_display(), "");
    }

    #[test]
    fn bracket_c_resolves_to_next_hunk() {
        let keymap = Keymap::from_bindings(&vim_preset());
        let mut resolver = keymap.resolver();
        assert_eq!(resolver.feed(chord(']')), StepResult::Pending);
        assert_eq!(
            resolver.feed(chord('c')),
            StepResult::Matched(Action::NextHunk)
        );
    }

    #[test]
    fn invalid_continuation_cancels_pending_sequence() {
        let keymap = Keymap::from_bindings(&vim_preset());
        let mut resolver = keymap.resolver();
        assert_eq!(resolver.feed(chord('g')), StepResult::Pending);
        assert_eq!(resolver.feed(chord('x')), StepResult::Cancelled);
        assert_eq!(resolver.pending_display(), "");
        // Resolver is usable again after a cancellation.
        assert_eq!(resolver.feed(chord('q')), StepResult::Matched(Action::Quit));
    }

    #[test]
    fn single_key_control_d_matches_immediately() {
        let keymap = Keymap::from_bindings(&vim_preset());
        let mut resolver = keymap.resolver();
        assert_eq!(
            resolver.feed(ctrl('d')),
            StepResult::Matched(Action::HalfPageDown)
        );
    }

    #[test]
    fn shifted_uppercase_char_matches_bottom() {
        let keymap = Keymap::from_bindings(&vim_preset());
        let mut resolver = keymap.resolver();
        let shifted = KeyChord::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(resolver.feed(shifted), StepResult::Matched(Action::Bottom));
    }
}
