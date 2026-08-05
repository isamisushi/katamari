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
    /// Requests hover information for the row's active symbol (see
    /// [`NextSymbol`](Action::NextSymbol)) — or, while a hover popup is
    /// already open, closes it. `ui::mod`'s event loop intercepts this
    /// action rather than forwarding it through `App`/`FileView::update`,
    /// since resolving and issuing the LSP request is not a pure state
    /// transition.
    Hover,
    /// Moves the row's "active symbol" — the identifier-like token `Hover`
    /// targets — to the next/previous one on the current line. Purely a
    /// cursor-adjacent selection index, so unlike `Hover` this *is* handled
    /// inside `App`/`FileView::update`.
    NextSymbol,
    PrevSymbol,
    /// Closes an open hover popup; otherwise a no-op. Separate from `Quit`
    /// because Esc closing a transient overlay and `q` quitting the whole
    /// view are different intents that happen to share "get me out of
    /// this" — conflating them would make Esc quit the app the moment no
    /// popup happens to be open, which is not what a user pressing Esc to
    /// dismiss something expects.
    Cancel,
    /// Requests go-to-definition for the row's active symbol — like
    /// `Hover`, intercepted by `ui::mod`'s event loop rather than reaching
    /// `App`/`FileView::update`, since issuing the LSP request and then
    /// navigating on its response are not pure state transitions.
    GotoDefinition,
    /// Requests every reference to the row's active symbol, including its
    /// declaration. Always opens the references panel, even for a single
    /// result — unlike `GotoDefinition`, which jumps straight there when
    /// there's exactly one candidate.
    FindReferences,
    /// Moves the cursor to the next/previous row bearing a diagnostic,
    /// wrapping around. Intercepted the same way `Hover`/`GotoDefinition`
    /// are, since it needs the diagnostics store `App`/`FileView` don't
    /// own.
    NextDiagnostic,
    PrevDiagnostic,
    /// Retraces the jump history one step back/forward — `Ctrl-o`/`Ctrl-i`'s
    /// vim-familiar direction, bound here to `C-o`/`C-t` since this
    /// terminal's key reporting can't distinguish a literal Tab from
    /// `Ctrl-i` (they're the same ASCII control code); see
    /// [`crate::ui::navigation::JumpStack`].
    JumpBack,
    JumpForward,
    /// Selects the highlighted entry in an open references panel and
    /// navigates to it. Also how [`crate::ui::timeline_view::TimelineView`]
    /// treats Enter: jump back to the diff view being reviewed.
    Confirm,
    /// Opens [`crate::ui::timeline_view::TimelineView`] on top of the root
    /// diff (when a jj repository was detected), or closes it back to the
    /// diff view if it's already open — `ui::mod`'s event loop intercepts
    /// this rather than forwarding it through `App`/`FileView::update`,
    /// since deciding whether jj is available and constructing the view
    /// aren't pure state transitions.
    ToggleTimeline,
    /// Timeline-only: enters/exits "range mode," where the two most
    /// recently visited list positions become the endpoints of a combined
    /// diff instead of one snapshot's diff against its immediate
    /// predecessor. A no-op in every other view.
    ToggleRangeSelect,
    /// Opens the M6 comment-compose overlay, anchored to the cursor's
    /// current row — `ui::mod`'s event loop intercepts this rather than
    /// forwarding it through `App::update`, since it needs the repo root
    /// and the comment store, neither of which `App` owns. A no-op outside
    /// [`crate::ui::view::View::Diff`] or on a row with nothing to anchor
    /// to (a header, or a `Del` row — see `App::comment_target`).
    AddComment,
    /// Toggles whether a commented row's body renders as an inline block
    /// underneath it; the gutter marker itself always shows regardless.
    /// Unlike `AddComment`, this *is* a pure state flip handled inside
    /// `App::update`, mirroring `ToggleSidebar`.
    ToggleComments,
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
    /// `C-d` for Ctrl-d, `M-x` for Alt/Meta-x (the emacs preset's prefix
    /// keys — see [`emacs_preset`]), `]` or `g` for a plain char, or a named
    /// key like `Esc`/`Enter`/`Tab`/`Space`. Returns a plain `Err` naming the
    /// bad token rather than panicking — [`KeySeq::parse`] (the built-in
    /// presets' trusted, fixed notation) turns that into a panic itself;
    /// config's `[keys]` overrides (user-supplied notation) surface it as a
    /// normal startup error instead, via [`KeySeq::try_parse`].
    fn try_parse_token(token: &str) -> Result<Self, String> {
        if let Some(rest) = token.strip_prefix("C-") {
            let code = Self::try_parse_key_name(rest)?;
            return Ok(Self::new(code, KeyModifiers::CONTROL));
        }
        if let Some(rest) = token.strip_prefix("M-") {
            let code = Self::try_parse_key_name(rest)?;
            return Ok(Self::new(code, KeyModifiers::ALT));
        }
        Ok(Self::new(
            Self::try_parse_key_name(token)?,
            KeyModifiers::NONE,
        ))
    }

    fn try_parse_key_name(name: &str) -> Result<KeyCode, String> {
        Ok(match name {
            "Esc" => KeyCode::Esc,
            "Enter" => KeyCode::Enter,
            "Tab" => KeyCode::Tab,
            "BackTab" => KeyCode::BackTab,
            "Backspace" => KeyCode::Backspace,
            "Space" => KeyCode::Char(' '),
            "Left" => KeyCode::Left,
            "Right" => KeyCode::Right,
            "Up" => KeyCode::Up,
            "Down" => KeyCode::Down,
            "Home" => KeyCode::Home,
            "End" => KeyCode::End,
            "PageUp" => KeyCode::PageUp,
            "PageDown" => KeyCode::PageDown,
            single if single.chars().count() == 1 => KeyCode::Char(single.chars().next().unwrap()),
            other => {
                return Err(format!(
                    "unrecognized key name in keymap notation: {other:?}"
                ));
            }
        })
    }

    /// Notation for the status bar's pending-sequence indicator; the inverse
    /// of [`Self::try_parse_token`] for the cases the vim and emacs presets
    /// use.
    fn notation(self) -> String {
        let key = match self.code {
            KeyCode::Char(' ') => "Space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Esc => "Esc".to_string(),
            KeyCode::Enter => "Enter".to_string(),
            KeyCode::Tab => "Tab".to_string(),
            KeyCode::BackTab => "BackTab".to_string(),
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
        } else if self.modifiers.contains(KeyModifiers::ALT) {
            format!("M-{key}")
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
    /// Parses trusted, fixed notation — the built-in presets below. Panics
    /// on a malformed token, since that can only mean a bug in this
    /// module's own preset tables, caught immediately by any test that
    /// constructs a [`Keymap`] from them.
    pub fn parse(notation: &str) -> Self {
        Self::try_parse(notation).unwrap_or_else(|e| panic!("{e}"))
    }

    /// As [`Self::parse`], but returning a `Result` instead of panicking —
    /// what config's `[keys]` overrides parse user-supplied notation
    /// strings through, so a typo becomes a startup error naming the entry
    /// (see `config::apply_key_overrides`) rather than a crash.
    pub fn try_parse(notation: &str) -> Result<Self, String> {
        notation
            .split_whitespace()
            .map(KeyChord::try_parse_token)
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
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
        ("K", Action::Hover),
        ("g d", Action::GotoDefinition),
        ("g r", Action::FindReferences),
        ("] d", Action::NextDiagnostic),
        ("[ d", Action::PrevDiagnostic),
        ("C-o", Action::JumpBack),
        ("C-t", Action::JumpForward),
        ("Enter", Action::Confirm),
        ("Tab", Action::NextSymbol),
        ("BackTab", Action::PrevSymbol),
        ("Esc", Action::Cancel),
        ("t", Action::ToggleTimeline),
        ("v", Action::ToggleRangeSelect),
        ("c", Action::AddComment),
        ("C", Action::ToggleComments),
        ("q", Action::Quit),
    ]
    .into_iter()
    .map(|(notation, action)| (KeySeq::parse(notation), action))
    .collect()
}

/// An emacs-style keymap, the same shape as [`vim_preset`] so both flow
/// through the identical trie/`Resolver` machinery — nothing about
/// multi-key resolution is vim-specific. Bindings follow real emacs
/// conventions where one exists (`M-.`/`M-?` are `xref-find-definitions`/
/// `xref-find-references`'s actual default keys; `C-Space` is
/// `set-mark-command`, the closest emacs analogue to "start a range
/// selection"; `M-g M-n`/`M-g M-p` mirror `next-error`/`previous-error`'s
/// `M-g` prefix), and a small app-specific choice where none does (`C-h`
/// for hover — this app owns every key at the terminal level, so there's no
/// real help-prefix conflict to avoid the way there would be inside actual
/// emacs). `C-x n`/`C-x p` for next/prev file exercise the same
/// two-chord-with-a-shared-prefix shape `C-x` itself is famous for, without
/// this app actually needing a `C-x` prefix for anything else. A handful of
/// bindings that have no strong identity in either editor (the sidebar/
/// layout/timeline/comments toggles, symbol cycling, confirm/cancel) keep
/// vim's keys rather than invent an arbitrary emacs-flavored alternative —
/// `q` for quit most of all, kept identical across both presets by design
/// (documented here rather than duplicated in each preset's own bindings).
pub fn emacs_preset() -> Vec<(KeySeq, Action)> {
    [
        ("C-n", Action::CursorDown),
        ("C-p", Action::CursorUp),
        ("C-v", Action::HalfPageDown),
        ("M-v", Action::HalfPageUp),
        ("M-<", Action::Top),
        ("M->", Action::Bottom),
        ("M-n", Action::NextHunk),
        ("M-p", Action::PrevHunk),
        ("C-x n", Action::NextFile),
        ("C-x p", Action::PrevFile),
        ("b", Action::ToggleSidebar),
        ("s", Action::ToggleLayout),
        ("C-h", Action::Hover),
        ("M-.", Action::GotoDefinition),
        ("M-?", Action::FindReferences),
        ("M-g M-n", Action::NextDiagnostic),
        ("M-g M-p", Action::PrevDiagnostic),
        ("C-o", Action::JumpBack),
        ("C-t", Action::JumpForward),
        ("Enter", Action::Confirm),
        ("Tab", Action::NextSymbol),
        ("BackTab", Action::PrevSymbol),
        ("Esc", Action::Cancel),
        ("t", Action::ToggleTimeline),
        ("C-Space", Action::ToggleRangeSelect),
        ("C-c C-c", Action::AddComment),
        ("C", Action::ToggleComments),
        ("q", Action::Quit),
    ]
    .into_iter()
    .map(|(notation, action)| (KeySeq::parse(notation), action))
    .collect()
}

/// `action`'s kebab-case name in config's `[keys]` table — e.g.
/// `Action::HalfPageDown` is `half-page-down`. The single source of truth
/// both directions of that table's parsing share: [`action_by_name`] is its
/// exact inverse, so a name printed by one always round-trips through the
/// other. Kept as two matches rather than a derive, since neither `Action`
/// nor this mapping has any reason to live in a proc-macro-worthy crate for
/// what's fundamentally a 28-line lookup table.
///
/// No production caller yet — only [`action_by_name`] (config's `[keys]`
/// parsing) is on that path today. Kept anyway, the same way
/// `diff::coords::ColumnMap`'s `display_len`/`utf8_len` are: it's the other
/// half of a documented round-trip this module's own tests hold it to, and
/// a natural fit for a future `ktmr config keys` listing/validation command.
#[allow(dead_code)]
pub fn action_name(action: Action) -> &'static str {
    match action {
        Action::CursorDown => "cursor-down",
        Action::CursorUp => "cursor-up",
        Action::HalfPageDown => "half-page-down",
        Action::HalfPageUp => "half-page-up",
        Action::Top => "top",
        Action::Bottom => "bottom",
        Action::NextHunk => "next-hunk",
        Action::PrevHunk => "prev-hunk",
        Action::NextFile => "next-file",
        Action::PrevFile => "prev-file",
        Action::ToggleSidebar => "toggle-sidebar",
        Action::ToggleLayout => "toggle-layout",
        Action::Hover => "hover",
        Action::NextSymbol => "next-symbol",
        Action::PrevSymbol => "prev-symbol",
        Action::Cancel => "cancel",
        Action::GotoDefinition => "goto-definition",
        Action::FindReferences => "find-references",
        Action::NextDiagnostic => "next-diagnostic",
        Action::PrevDiagnostic => "prev-diagnostic",
        Action::JumpBack => "jump-back",
        Action::JumpForward => "jump-forward",
        Action::Confirm => "confirm",
        Action::ToggleTimeline => "toggle-timeline",
        Action::ToggleRangeSelect => "toggle-range-select",
        Action::AddComment => "add-comment",
        Action::ToggleComments => "toggle-comments",
        Action::Quit => "quit",
    }
}

/// The inverse of [`action_name`] — parses a config `[keys]` entry's key
/// (e.g. `"half-page-down"`) back into an [`Action`]. `None` for anything
/// not in that table, which `config::apply_key_overrides` turns into a
/// startup error naming the unrecognized entry.
pub fn action_by_name(name: &str) -> Option<Action> {
    Some(match name {
        "cursor-down" => Action::CursorDown,
        "cursor-up" => Action::CursorUp,
        "half-page-down" => Action::HalfPageDown,
        "half-page-up" => Action::HalfPageUp,
        "top" => Action::Top,
        "bottom" => Action::Bottom,
        "next-hunk" => Action::NextHunk,
        "prev-hunk" => Action::PrevHunk,
        "next-file" => Action::NextFile,
        "prev-file" => Action::PrevFile,
        "toggle-sidebar" => Action::ToggleSidebar,
        "toggle-layout" => Action::ToggleLayout,
        "hover" => Action::Hover,
        "next-symbol" => Action::NextSymbol,
        "prev-symbol" => Action::PrevSymbol,
        "cancel" => Action::Cancel,
        "goto-definition" => Action::GotoDefinition,
        "find-references" => Action::FindReferences,
        "next-diagnostic" => Action::NextDiagnostic,
        "prev-diagnostic" => Action::PrevDiagnostic,
        "jump-back" => Action::JumpBack,
        "jump-forward" => Action::JumpForward,
        "confirm" => Action::Confirm,
        "toggle-timeline" => Action::ToggleTimeline,
        "toggle-range-select" => Action::ToggleRangeSelect,
        "add-comment" => Action::AddComment,
        "toggle-comments" => Action::ToggleComments,
        "quit" => Action::Quit,
        _ => return None,
    })
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
    fn hover_and_symbol_cycling_keys_resolve() {
        let keymap = Keymap::from_bindings(&vim_preset());
        let mut resolver = keymap.resolver();
        assert_eq!(
            resolver.feed(chord('K')),
            StepResult::Matched(Action::Hover)
        );
        assert_eq!(
            resolver.feed(KeyChord::new(KeyCode::Tab, KeyModifiers::NONE)),
            StepResult::Matched(Action::NextSymbol)
        );
        assert_eq!(
            resolver.feed(KeyChord::new(KeyCode::BackTab, KeyModifiers::NONE)),
            StepResult::Matched(Action::PrevSymbol)
        );
        assert_eq!(
            resolver.feed(KeyChord::new(KeyCode::Esc, KeyModifiers::NONE)),
            StepResult::Matched(Action::Cancel)
        );
    }

    #[test]
    fn shifted_uppercase_char_matches_bottom() {
        let keymap = Keymap::from_bindings(&vim_preset());
        let mut resolver = keymap.resolver();
        let shifted = KeyChord::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(resolver.feed(shifted), StepResult::Matched(Action::Bottom));
    }

    #[test]
    fn t_resolves_to_toggle_timeline_and_v_to_toggle_range_select() {
        let keymap = Keymap::from_bindings(&vim_preset());
        let mut resolver = keymap.resolver();
        assert_eq!(
            resolver.feed(chord('t')),
            StepResult::Matched(Action::ToggleTimeline)
        );
        assert_eq!(
            resolver.feed(chord('v')),
            StepResult::Matched(Action::ToggleRangeSelect)
        );
    }

    fn alt(c: char) -> KeyChord {
        KeyChord::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    #[test]
    fn emacs_preset_single_key_control_n_matches_cursor_down() {
        let keymap = Keymap::from_bindings(&emacs_preset());
        let mut resolver = keymap.resolver();
        assert_eq!(
            resolver.feed(ctrl('n')),
            StepResult::Matched(Action::CursorDown)
        );
    }

    #[test]
    fn emacs_preset_meta_dot_matches_goto_definition() {
        let keymap = Keymap::from_bindings(&emacs_preset());
        let mut resolver = keymap.resolver();
        assert_eq!(
            resolver.feed(alt('.')),
            StepResult::Matched(Action::GotoDefinition)
        );
    }

    #[test]
    fn emacs_preset_c_x_n_two_chord_sequence_resolves_to_next_file() {
        let keymap = Keymap::from_bindings(&emacs_preset());
        let mut resolver = keymap.resolver();
        assert_eq!(resolver.feed(ctrl('x')), StepResult::Pending);
        assert_eq!(resolver.pending_display(), "C-x");
        assert_eq!(
            resolver.feed(chord('n')),
            StepResult::Matched(Action::NextFile)
        );

        // The shared `C-x` prefix resolves its sibling chord too, proving
        // the trie handles a branching two-chord prefix generically rather
        // than special-casing this one sequence.
        assert_eq!(resolver.feed(ctrl('x')), StepResult::Pending);
        assert_eq!(
            resolver.feed(chord('p')),
            StepResult::Matched(Action::PrevFile)
        );
    }

    #[test]
    fn emacs_preset_m_g_m_n_two_chord_sequence_resolves_to_next_diagnostic() {
        let keymap = Keymap::from_bindings(&emacs_preset());
        let mut resolver = keymap.resolver();
        assert_eq!(resolver.feed(alt('g')), StepResult::Pending);
        assert_eq!(
            resolver.feed(alt('n')),
            StepResult::Matched(Action::NextDiagnostic)
        );
    }

    #[test]
    fn emacs_preset_c_c_c_c_resolves_to_add_comment() {
        let keymap = Keymap::from_bindings(&emacs_preset());
        let mut resolver = keymap.resolver();
        assert_eq!(resolver.feed(ctrl('c')), StepResult::Pending);
        assert_eq!(
            resolver.feed(ctrl('c')),
            StepResult::Matched(Action::AddComment)
        );
    }

    #[test]
    fn emacs_preset_control_space_resolves_to_toggle_range_select() {
        let keymap = Keymap::from_bindings(&emacs_preset());
        let mut resolver = keymap.resolver();
        let control_space = KeyChord::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
        assert_eq!(
            resolver.feed(control_space),
            StepResult::Matched(Action::ToggleRangeSelect)
        );
    }

    #[test]
    fn emacs_preset_q_quits_same_as_vim() {
        let keymap = Keymap::from_bindings(&emacs_preset());
        let mut resolver = keymap.resolver();
        assert_eq!(resolver.feed(chord('q')), StepResult::Matched(Action::Quit));
    }

    #[test]
    fn action_name_and_action_by_name_round_trip_every_variant() {
        let all = [
            Action::CursorDown,
            Action::CursorUp,
            Action::HalfPageDown,
            Action::HalfPageUp,
            Action::Top,
            Action::Bottom,
            Action::NextHunk,
            Action::PrevHunk,
            Action::NextFile,
            Action::PrevFile,
            Action::ToggleSidebar,
            Action::ToggleLayout,
            Action::Hover,
            Action::NextSymbol,
            Action::PrevSymbol,
            Action::Cancel,
            Action::GotoDefinition,
            Action::FindReferences,
            Action::NextDiagnostic,
            Action::PrevDiagnostic,
            Action::JumpBack,
            Action::JumpForward,
            Action::Confirm,
            Action::ToggleTimeline,
            Action::ToggleRangeSelect,
            Action::AddComment,
            Action::ToggleComments,
            Action::Quit,
        ];
        for action in all {
            let name = action_name(action);
            assert_eq!(
                action_by_name(name),
                Some(action),
                "round trip failed for {name}"
            );
        }
    }

    #[test]
    fn action_by_name_rejects_unknown_names() {
        assert_eq!(action_by_name("not-a-real-action"), None);
    }

    #[test]
    fn try_parse_reports_an_error_instead_of_panicking_on_a_bad_token() {
        assert!(KeySeq::try_parse("C-d NotAKey g").is_err());
        assert!(KeySeq::try_parse("g g").is_ok());
    }

    #[test]
    fn every_vim_and_emacs_binding_covers_every_action_exactly_once() {
        for preset in [vim_preset(), emacs_preset()] {
            let mut actions: Vec<Action> = preset.iter().map(|(_, a)| *a).collect();
            actions.sort_by_key(|a| action_name(*a));
            actions.dedup();
            assert_eq!(
                actions.len(),
                preset.len(),
                "every action should be bound exactly once per preset"
            );
        }
    }
}
