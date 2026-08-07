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
    /// vim-familiar direction. Forward's *canonical* binding depends on
    /// whether the terminal can tell a literal Tab from `Ctrl-i` apart: with
    /// the kitty keyboard protocol's `DISAMBIGUATE_ESCAPE_CODES` flag active
    /// they arrive as different [`crate::keymap::KeyChord`]s, so
    /// [`vim_preset`]/[`emacs_preset`] bind `C-i` (matching neovim) with
    /// `C-t` kept as a still-working alias; without it, both share the same
    /// byte on the wire, so only `C-t` is bound at all — binding `C-i`
    /// there would silently steal every Tab press meant for
    /// [`NextSymbol`](Action::NextSymbol). See `ui::mod`'s kitty-protocol
    /// startup probe and [`crate::ui::navigation::JumpStack`].
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
    /// Opens [`crate::ui::log_view::LogView`] on top of the root diff, or
    /// closes it back if it's already open — `ui::mod`'s event loop
    /// intercepts this the same way it intercepts `ToggleTimeline`, since
    /// picking a backend (jj vs git) and constructing the view aren't pure
    /// state transitions. Unlike `ToggleTimeline`, this never fails to have
    /// something to open: every repository `ktmr diff` can start in has
    /// git history, jj or not.
    ToggleLogView,
    /// Opens [`crate::ui::scope_menu`]'s popup — a keyboard-driven menu for
    /// switching what a live [`crate::ui::view::View::Diff`] session is
    /// reviewing (working tree, staged, a free-form revision, or straight
    /// to `L`/`t`'s own views) without restarting `ktmr diff` with CLI
    /// flags. `ui::mod`'s event loop intercepts this the same way it
    /// intercepts `ToggleTimeline`/`ToggleLogView`: building the menu needs
    /// to know whether a colocated jj repo was detected, which neither
    /// `App` nor `FileView` owns. Also closes the popup when it's already
    /// open, mirroring how `ToggleTimeline`/`ToggleLogView` close their own
    /// views — see `ui::mod::handle_action`'s scope-menu interception for
    /// the exact key handling once it's open (`j`/`k`/Enter/Esc, not this
    /// action again).
    OpenScopeMenu,
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

    /// Compact display notation for status-bar hints (see
    /// `ui::hints::HintItem`): consecutive *bare* chords — a single
    /// character, no modifier, like the `g`/`d` in `g d` or the `]`/`c` in
    /// `] c` — render with no separator (`gd`, `]c`), matching vim's own
    /// convention for a two-key sequence. A sequence with any modified or
    /// multi-character chord (`M-g M-n`, `C-x n`) keeps a separating space
    /// instead, since jamming `C-x` and `n` together as `C-xn` would misread
    /// as a single token. Distinct from [`Resolver::pending_display`], which
    /// always space-joins — that renders keys the user has *already
    /// pressed*, one at a time, not a single compact reference notation.
    pub fn compact_notation(&self) -> String {
        let is_bare = |chord: &KeyChord| chord.notation().chars().count() == 1;
        if self.0.iter().all(is_bare) {
            self.0.iter().map(|c| c.notation()).collect()
        } else {
            self.0
                .iter()
                .map(|c| c.notation())
                .collect::<Vec<_>>()
                .join(" ")
        }
    }

    /// Builds a sequence directly from chords a [`Resolver`] has already
    /// consumed, rather than parsing a notation string — what
    /// `ui::key_display`'s screenkey-style overlay uses to render the exact
    /// keys a reviewer just pressed (`gd`, `C-x n`) through the same
    /// [`Self::compact_notation`] formatting every hint/README reference
    /// uses, instead of inventing a second notation for "keys as typed."
    pub(crate) fn from_chords(chords: Vec<KeyChord>) -> Self {
        Self(chords)
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
    /// The bindings this trie was built from, kept alongside it for
    /// [`Self::binding_for`]'s reverse lookup — the trie itself is indexed
    /// key-sequence-first (exactly what `Resolver::feed` needs) and has no
    /// efficient way to ask "what maps to this action" without walking every
    /// path, whereas the original list already answers that in one scan and
    /// preserves the preset's insertion order (see that method's docs on why
    /// order matters here).
    bindings: Vec<(KeySeq, Action)>,
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
        Self {
            root,
            bindings: bindings.to_vec(),
        }
    }

    pub fn resolver(&self) -> Resolver<'_> {
        Resolver {
            root: &self.root,
            current: &self.root,
            pending: Vec::new(),
        }
    }

    /// The key sequence bound to `action` in this keymap, if any — the
    /// single source of truth `ui::hints` reads to render status-bar hints,
    /// so a `[keys]` rebind or switching to the emacs preset changes hint
    /// text automatically instead of a hardcoded string drifting out of
    /// sync with it (the bug this method exists to fix).
    ///
    /// When `bindings` holds more than one entry for `action`, the first one
    /// wins — preset insertion order, which every built-in preset also
    /// happens to list in "primary binding first" order. In practice this
    /// tie only matters for a pathological `[keys]` config that adds an
    /// alias without touching the original (`apply_key_overrides` rebinds
    /// the *existing* slot in place, so a normal override never creates a
    /// second entry); `None` only if `action` has no binding at all, which
    /// neither built-in preset ever produces (see this module's
    /// `every_vim_and_emacs_binding_covers_every_action_exactly_once` test).
    pub fn binding_for(&self, action: Action) -> Option<&KeySeq> {
        self.bindings
            .iter()
            .find(|(_, a)| *a == action)
            .map(|(seq, _)| seq)
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
///
/// `ci_distinguishable` is `ui::run`'s kitty-protocol startup probe result
/// (see [`Action::JumpBack`]'s docs): when `true`, `C-i` is inserted ahead
/// of `C-t` in the list below so [`Keymap::binding_for`]'s first-match rule
/// picks it as `JumpForward`'s canonical, hinted binding, while `C-t` stays
/// bound as a working alias (both entries resolve to the same action; the
/// trie has no trouble with two distinct key sequences mapping to one
/// action, only with one sequence mapping to two). When `false`, `C-i` is
/// left unbound entirely — the terminal delivers the identical byte for a
/// literal Tab and `Ctrl-i` in that case, and Tab already means
/// [`Action::NextSymbol`].
pub fn vim_preset(ci_distinguishable: bool) -> Vec<(KeySeq, Action)> {
    let mut bindings = vec![
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
    ];
    if ci_distinguishable {
        bindings.push(("C-i", Action::JumpForward));
    }
    bindings.push(("C-t", Action::JumpForward));
    bindings.extend([
        ("Enter", Action::Confirm),
        ("Tab", Action::NextSymbol),
        ("BackTab", Action::PrevSymbol),
        ("Esc", Action::Cancel),
        ("t", Action::ToggleTimeline),
        ("L", Action::ToggleLogView),
        ("o", Action::OpenScopeMenu),
        ("v", Action::ToggleRangeSelect),
        ("c", Action::AddComment),
        ("C", Action::ToggleComments),
        ("q", Action::Quit),
    ]);
    bindings
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
/// layout/timeline/comments/scope-menu toggles, symbol cycling,
/// confirm/cancel) keep
/// vim's keys rather than invent an arbitrary emacs-flavored alternative —
/// `q` for quit most of all, kept identical across both presets by design
/// (documented here rather than duplicated in each preset's own bindings).
///
/// `ci_distinguishable` has the same effect as in [`vim_preset`] — this
/// isn't a vim-specific concern, it's a terminal-capability one, so both
/// presets take and act on it identically.
pub fn emacs_preset(ci_distinguishable: bool) -> Vec<(KeySeq, Action)> {
    let mut bindings = vec![
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
    ];
    if ci_distinguishable {
        bindings.push(("C-i", Action::JumpForward));
    }
    bindings.push(("C-t", Action::JumpForward));
    bindings.extend([
        ("Enter", Action::Confirm),
        ("Tab", Action::NextSymbol),
        ("BackTab", Action::PrevSymbol),
        ("Esc", Action::Cancel),
        ("t", Action::ToggleTimeline),
        ("L", Action::ToggleLogView),
        ("o", Action::OpenScopeMenu),
        ("C-Space", Action::ToggleRangeSelect),
        ("C-c C-c", Action::AddComment),
        ("C", Action::ToggleComments),
        ("q", Action::Quit),
    ]);
    bindings
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
        Action::ToggleLogView => "toggle-log-view",
        Action::OpenScopeMenu => "open-scope-menu",
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
        "toggle-log-view" => Action::ToggleLogView,
        "open-scope-menu" => Action::OpenScopeMenu,
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
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let mut resolver = keymap.resolver();
        assert_eq!(resolver.feed(chord('g')), StepResult::Pending);
        assert_eq!(resolver.pending_display(), "g");
        assert_eq!(resolver.feed(chord('g')), StepResult::Matched(Action::Top));
        assert_eq!(resolver.pending_display(), "");
    }

    #[test]
    fn bracket_c_resolves_to_next_hunk() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let mut resolver = keymap.resolver();
        assert_eq!(resolver.feed(chord(']')), StepResult::Pending);
        assert_eq!(
            resolver.feed(chord('c')),
            StepResult::Matched(Action::NextHunk)
        );
    }

    #[test]
    fn invalid_continuation_cancels_pending_sequence() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let mut resolver = keymap.resolver();
        assert_eq!(resolver.feed(chord('g')), StepResult::Pending);
        assert_eq!(resolver.feed(chord('x')), StepResult::Cancelled);
        assert_eq!(resolver.pending_display(), "");
        // Resolver is usable again after a cancellation.
        assert_eq!(resolver.feed(chord('q')), StepResult::Matched(Action::Quit));
    }

    #[test]
    fn single_key_control_d_matches_immediately() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let mut resolver = keymap.resolver();
        assert_eq!(
            resolver.feed(ctrl('d')),
            StepResult::Matched(Action::HalfPageDown)
        );
    }

    #[test]
    fn hover_and_symbol_cycling_keys_resolve() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
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
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let mut resolver = keymap.resolver();
        let shifted = KeyChord::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(resolver.feed(shifted), StepResult::Matched(Action::Bottom));
    }

    #[test]
    fn t_resolves_to_toggle_timeline_and_v_to_toggle_range_select() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
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

    #[test]
    fn shifted_l_resolves_to_toggle_log_view_in_both_presets() {
        let shifted_l = KeyChord::new(KeyCode::Char('L'), KeyModifiers::SHIFT);
        let vim_keymap = Keymap::from_bindings(&vim_preset(false));
        let mut vim = vim_keymap.resolver();
        assert_eq!(
            vim.feed(shifted_l),
            StepResult::Matched(Action::ToggleLogView)
        );
        let emacs_keymap = Keymap::from_bindings(&emacs_preset(false));
        let mut emacs = emacs_keymap.resolver();
        assert_eq!(
            emacs.feed(shifted_l),
            StepResult::Matched(Action::ToggleLogView)
        );
    }

    #[test]
    fn o_resolves_to_open_scope_menu_in_both_presets() {
        let vim_keymap = Keymap::from_bindings(&vim_preset(false));
        let mut vim = vim_keymap.resolver();
        assert_eq!(
            vim.feed(chord('o')),
            StepResult::Matched(Action::OpenScopeMenu)
        );
        let emacs_keymap = Keymap::from_bindings(&emacs_preset(false));
        let mut emacs = emacs_keymap.resolver();
        assert_eq!(
            emacs.feed(chord('o')),
            StepResult::Matched(Action::OpenScopeMenu)
        );
    }

    fn alt(c: char) -> KeyChord {
        KeyChord::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    #[test]
    fn emacs_preset_single_key_control_n_matches_cursor_down() {
        let keymap = Keymap::from_bindings(&emacs_preset(false));
        let mut resolver = keymap.resolver();
        assert_eq!(
            resolver.feed(ctrl('n')),
            StepResult::Matched(Action::CursorDown)
        );
    }

    #[test]
    fn emacs_preset_meta_dot_matches_goto_definition() {
        let keymap = Keymap::from_bindings(&emacs_preset(false));
        let mut resolver = keymap.resolver();
        assert_eq!(
            resolver.feed(alt('.')),
            StepResult::Matched(Action::GotoDefinition)
        );
    }

    #[test]
    fn emacs_preset_c_x_n_two_chord_sequence_resolves_to_next_file() {
        let keymap = Keymap::from_bindings(&emacs_preset(false));
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
        let keymap = Keymap::from_bindings(&emacs_preset(false));
        let mut resolver = keymap.resolver();
        assert_eq!(resolver.feed(alt('g')), StepResult::Pending);
        assert_eq!(
            resolver.feed(alt('n')),
            StepResult::Matched(Action::NextDiagnostic)
        );
    }

    #[test]
    fn emacs_preset_c_c_c_c_resolves_to_add_comment() {
        let keymap = Keymap::from_bindings(&emacs_preset(false));
        let mut resolver = keymap.resolver();
        assert_eq!(resolver.feed(ctrl('c')), StepResult::Pending);
        assert_eq!(
            resolver.feed(ctrl('c')),
            StepResult::Matched(Action::AddComment)
        );
    }

    #[test]
    fn emacs_preset_control_space_resolves_to_toggle_range_select() {
        let keymap = Keymap::from_bindings(&emacs_preset(false));
        let mut resolver = keymap.resolver();
        let control_space = KeyChord::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
        assert_eq!(
            resolver.feed(control_space),
            StepResult::Matched(Action::ToggleRangeSelect)
        );
    }

    #[test]
    fn emacs_preset_q_quits_same_as_vim() {
        let keymap = Keymap::from_bindings(&emacs_preset(false));
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
            Action::ToggleLogView,
            Action::OpenScopeMenu,
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
    fn binding_for_finds_a_vim_preset_binding() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let seq = keymap.binding_for(Action::GotoDefinition).unwrap();
        assert_eq!(seq.compact_notation(), "gd");
    }

    #[test]
    fn binding_for_finds_an_emacs_preset_binding() {
        let keymap = Keymap::from_bindings(&emacs_preset(false));
        let seq = keymap.binding_for(Action::GotoDefinition).unwrap();
        assert_eq!(seq.compact_notation(), "M-.");
    }

    #[test]
    fn binding_for_returns_none_for_an_action_with_no_binding() {
        let keymap = Keymap::from_bindings(&[(KeySeq::parse("j"), Action::CursorDown)]);
        assert!(keymap.binding_for(Action::Quit).is_none());
    }

    #[test]
    fn binding_for_reflects_a_keys_override_automatically() {
        // The whole point of driving hints off `binding_for` rather than a
        // hardcoded string: rebind an action, and the "key" a hint would
        // show changes with it, with no separate hint-text update needed.
        let mut bindings = vim_preset(false);
        let slot = bindings
            .iter_mut()
            .find(|(_, a)| *a == Action::Quit)
            .unwrap();
        slot.0 = KeySeq::parse("C-q");
        let keymap = Keymap::from_bindings(&bindings);
        assert_eq!(
            keymap.binding_for(Action::Quit).unwrap().compact_notation(),
            "C-q"
        );
    }

    #[test]
    fn binding_for_prefers_the_first_matching_entry_when_more_than_one_exists() {
        let bindings = vec![
            (KeySeq::parse("x"), Action::Quit),
            (KeySeq::parse("y"), Action::Quit),
        ];
        let keymap = Keymap::from_bindings(&bindings);
        assert_eq!(
            keymap.binding_for(Action::Quit).unwrap().compact_notation(),
            "x"
        );
    }

    #[test]
    fn compact_notation_joins_bare_multi_key_sequences_without_a_separator() {
        assert_eq!(KeySeq::parse("g g").compact_notation(), "gg");
        assert_eq!(KeySeq::parse("] c").compact_notation(), "]c");
    }

    #[test]
    fn compact_notation_keeps_a_space_around_any_modified_or_named_chord() {
        // `C-x` and `n` would misread as one token ("C-xn") jammed together
        // — unlike two bare chords, at least one side here needs the space
        // to stay legible.
        assert_eq!(KeySeq::parse("C-x n").compact_notation(), "C-x n");
        assert_eq!(KeySeq::parse("M-g M-n").compact_notation(), "M-g M-n");
        assert_eq!(KeySeq::parse("C-c C-c").compact_notation(), "C-c C-c");
    }

    #[test]
    fn compact_notation_for_a_single_chord_matches_its_own_notation() {
        assert_eq!(KeySeq::parse("C-o").compact_notation(), "C-o");
        assert_eq!(KeySeq::parse("q").compact_notation(), "q");
    }

    /// The 30 actions (see the `action_name_and_action_by_name_round_trip…`
    /// test's `all` list) each get exactly one binding — except
    /// `JumpForward`, which gets a *second* one (`C-i`) precisely when
    /// `ci_distinguishable` is set, per [`vim_preset`]/[`emacs_preset`]'s
    /// docs. So this checks two things per preset: every action is still
    /// reachable (`actions.len() == 30` after dedup), and the raw entry
    /// count is exactly one more than that when the extra `C-i` alias is
    /// present, exactly equal otherwise.
    #[test]
    fn every_vim_and_emacs_binding_covers_every_action_exactly_once() {
        const ACTION_COUNT: usize = 30;
        for ci_distinguishable in [false, true] {
            for preset in [
                vim_preset(ci_distinguishable),
                emacs_preset(ci_distinguishable),
            ] {
                let mut actions: Vec<Action> = preset.iter().map(|(_, a)| *a).collect();
                actions.sort_by_key(|a| action_name(*a));
                actions.dedup();
                assert_eq!(
                    actions.len(),
                    ACTION_COUNT,
                    "every action should be reachable regardless of ci_distinguishable"
                );
                let expected_len = ACTION_COUNT + usize::from(ci_distinguishable);
                assert_eq!(
                    preset.len(),
                    expected_len,
                    "ci_distinguishable={ci_distinguishable} should add exactly the C-i alias"
                );
            }
        }
    }

    #[test]
    fn ci_distinguishable_binds_c_i_first_so_binding_for_hints_it() {
        for preset_fn in [vim_preset, emacs_preset] {
            let keymap = Keymap::from_bindings(&preset_fn(true));
            assert_eq!(
                keymap
                    .binding_for(Action::JumpForward)
                    .unwrap()
                    .compact_notation(),
                "C-i"
            );
        }
    }

    #[test]
    fn ci_distinguishable_keeps_c_t_working_as_an_alias_via_the_trie() {
        for preset_fn in [vim_preset, emacs_preset] {
            let keymap = Keymap::from_bindings(&preset_fn(true));
            let mut resolver = keymap.resolver();
            assert_eq!(
                resolver.feed(ctrl('t')),
                StepResult::Matched(Action::JumpForward)
            );
            let mut resolver = keymap.resolver();
            assert_eq!(
                resolver.feed(ctrl('i')),
                StepResult::Matched(Action::JumpForward)
            );
        }
    }

    #[test]
    fn not_ci_distinguishable_binds_only_c_t_and_leaves_c_i_unbound() {
        for preset_fn in [vim_preset, emacs_preset] {
            let keymap = Keymap::from_bindings(&preset_fn(false));
            assert_eq!(
                keymap
                    .binding_for(Action::JumpForward)
                    .unwrap()
                    .compact_notation(),
                "C-t"
            );
            // `C-i` was never inserted into the trie in this mode — feeding
            // it cancels like any other unbound key, rather than resolving
            // to whatever it happened to fall through to.
            let mut resolver = keymap.resolver();
            assert_eq!(resolver.feed(ctrl('i')), StepResult::Cancelled);
        }
    }

    /// Tab must keep meaning `NextSymbol` in both modes — the whole reason
    /// `ci_distinguishable=false` refuses to bind `C-i` at all is that an
    /// undisambiguating terminal reports a literal Tab keypress with the
    /// exact same byte, so binding `C-i` there would silently reroute every
    /// Tab press meant for symbol-cycling. Checking it explicitly under
    /// both modes pins that down rather than trusting it by omission.
    #[test]
    fn tab_still_resolves_to_next_symbol_in_both_ci_distinguishable_modes() {
        for ci_distinguishable in [false, true] {
            for preset_fn in [vim_preset, emacs_preset] {
                let keymap = Keymap::from_bindings(&preset_fn(ci_distinguishable));
                let mut resolver = keymap.resolver();
                assert_eq!(
                    resolver.feed(KeyChord::new(KeyCode::Tab, KeyModifiers::NONE)),
                    StepResult::Matched(Action::NextSymbol)
                );
            }
        }
    }

    #[test]
    fn key_seq_c_i_parses_and_round_trips_through_compact_notation() {
        let seq = KeySeq::parse("C-i");
        assert_eq!(seq.compact_notation(), "C-i");
        let ctrl_i = ctrl('i');
        assert_eq!(seq.0, vec![ctrl_i]);
    }
}
