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
    /// inside `App`/`FileView::update`. Bound to vim's `l`/`h` and emacs's
    /// `M-f`/`M-b` (issue #13) — *not* Tab/BackTab, which mean
    /// [`FocusNextPane`](Action::FocusNextPane)/[`FocusPrevPane`](Action::FocusPrevPane)
    /// instead; see those variants' docs for why the two were split apart.
    NextSymbol,
    PrevSymbol,
    /// Cycles which pane has focus, forward/backward, in whichever view
    /// currently has more than one — the LSP inspector's Servers/Detail/
    /// Journal panes and the timeline's list/diff split today, with the
    /// main files/diff panes joining later (issue #14). Bound to Tab/
    /// BackTab in both presets. Deliberately a separate action from
    /// `NextSymbol`/`PrevSymbol`: before issue #13, Tab/BackTab *were*
    /// `NextSymbol`/`PrevSymbol`, repurposed per-view as ad hoc focus
    /// cycling — which meant a view with a nested `App` (the timeline)
    /// could never let symbol selection reach that nested diff, since Tab
    /// always meant "cycle my own panes" first. Splitting pane focus into
    /// its own action fixes that: a diff-view-shaped pane now sees real
    /// `NextSymbol`/`PrevSymbol` requests only when a reviewer actually
    /// pressed `l`/`h`/`M-f`/`M-b`, never a repurposed Tab. A single-pane
    /// view (`App`/`FileView`) has nothing to cycle, so both are harmless
    /// no-ops there — see [`crate::ui::pane::cycle_focus`], the shared
    /// mechanic every multi-pane view's `update` delegates to.
    FocusNextPane,
    FocusPrevPane,
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
    /// vim-familiar direction, with `Alt-Left`/`Alt-Right` bound
    /// unconditionally in both presets as terminal-agnostic aliases (issue
    /// #12). Forward's *canonical* binding depends on whether the terminal
    /// can tell a literal Tab from `Ctrl-i` apart: with the kitty keyboard
    /// protocol's `DISAMBIGUATE_ESCAPE_CODES` flag active they arrive as
    /// different [`crate::keymap::KeyChord`]s, so [`vim_preset`]/
    /// [`emacs_preset`] bind `C-i` (matching neovim) ahead of `Alt-Right`;
    /// without it, both share the same byte on the wire, so `C-i` is left
    /// unbound entirely — binding it there would silently steal every Tab
    /// press meant for [`FocusNextPane`](Action::FocusNextPane) — and
    /// `Alt-Right` becomes forward's sole, canonical binding instead. `Ctrl-t` (the
    /// pre-#12 legacy-terminal fallback) and `Ctrl-]` (vim's tag-stack key)
    /// are both deliberately left unbound — see the roadmap issue's "Why
    /// there is no tag stack." See `ui::mod`'s kitty-protocol startup probe
    /// and [`crate::ui::navigation::JumpStack`].
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
    /// Opens the live read-only language-server inspector on any view, or
    /// closes it when it is already on top of the view stack.
    ToggleLspInspector,
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
    /// Opens [`crate::ui::units_panel`]'s overlay — the diff's hunks
    /// grouped into semantic units by the user's own agent CLI (see
    /// [`crate::groups`]) — or closes it when already open. `ui::mod`'s
    /// event loop intercepts this rather than forwarding it through
    /// `App::update`: a cache miss spawns the agent CLI on a background
    /// thread and the result arrives asynchronously, none of which a pure
    /// state transition could express. A no-op outside
    /// [`crate::ui::view::View::Diff`].
    ToggleUnits,
    /// Regenerates the semantic-units grouping from scratch, skipping the
    /// `.katamari/groups.jsonl` cache — for when the agent's proposal
    /// wasn't a useful cut and the reviewer wants a fresh take on the
    /// *same* diff (the one case `ToggleUnits`'s cache-first path can
    /// never reach, since an unchanged diff always has an unchanged cache
    /// key). Intercepted by `ui::mod` for the same reason `ToggleUnits`
    /// is; the fresh result overwrites the cache by virtue of the store's
    /// last-record-wins fold. A no-op outside
    /// [`crate::ui::view::View::Diff`].
    RegenerateUnits,
    /// Expands the status bar's hint rows from the minimal always-shown
    /// subset to the full curated list, and back — see
    /// [`crate::ui::hints`]'s collapsed/expanded split. Intercepted by
    /// `ui::mod`'s event loop (the expanded flag is event-loop chrome
    /// state shared by every view, not something any one `App`/`FileView`
    /// owns), and deliberately view-independent: the hint bar exists on
    /// all of them.
    ToggleHints,
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
    /// Expands the gap a `RenderRow::Gap` fold row stands for into ordinary
    /// context rows — vimdiff's `zo`. `ui::mod`'s event loop intercepts
    /// this rather than forwarding it through `App::update` (mirroring
    /// `AddComment`'s reasoning): it needs to read the live file off disk,
    /// which is I/O `App` doesn't own. A no-op outside
    /// [`crate::ui::view::View::Diff`], off a `Gap` row, or when the diff's
    /// new side isn't the working tree (see `App::disk_is_new_side`).
    ExpandFold,
    /// Folds an expanded gap back into its `RenderRow::Gap` row — vimdiff's
    /// `zc`. Unlike `ExpandFold` this needs no I/O (the expanded rows are
    /// already in memory), but stays intercepted by `ui::mod` alongside it
    /// for the same `View::Diff`-only gating and so both fold actions read
    /// as one pair rather than one pure and one not.
    CollapseFold,
    Quit,
    /// Opens [`crate::ui::help`]'s popup — every action, grouped, with its
    /// live binding. `ui::mod`'s event loop intercepts this the same way it
    /// intercepts `OpenScopeMenu`/`ToggleTimeline`/`ToggleLogView` (building
    /// the popup's row list needs the live [`Keymap`], which neither `App`
    /// nor `FileView` owns) — but unlike `OpenScopeMenu`'s `View::Diff`-only
    /// gate, this opens from *any* view: the bindings a reviewer might have
    /// forgotten are global information, not something tied to whichever
    /// screen happens to be on top when they reach for `?`.
    OpenHelp,
    /// Opens Issue #5's `/` incremental search prompt in the diff view —
    /// vim's own `/`. `ui::mod`'s event loop intercepts this the same way
    /// it intercepts `OpenHelp`/`OpenScopeMenu`: the prompt overlay itself
    /// (raw-key-bypass, live-recomputed matches) is event-loop state, not
    /// something `App::update`'s pure `Action -> ()` shape could express.
    /// Diff-view only — a no-op everywhere else, same as `AddComment`/
    /// `ExpandFold` (see `App::update`'s docs on the shared bucket they all
    /// join).
    OpenSearch,
    /// `n`: jump to the next match of the confirmed search, wrapping
    /// around with a "search wrapped" status note. Intercepted by
    /// `ui::mod`'s event loop rather than reaching `App::update` — like
    /// `NextDiagnostic`/`PrevDiagnostic`, it needs to report a status note
    /// `App::update`'s return type has no room for, even though the actual
    /// jump is a real `App` method (`App::next_match`), not I/O.
    NextMatch,
    /// `N`: as `NextMatch`, the previous match.
    PrevMatch,
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
    /// single source of truth `ui::hints`/`ui::help` read to render
    /// status-bar hints and the `?` popup's live bindings, so a `[keys]`
    /// rebind or switching to the emacs preset changes them automatically
    /// instead of a hardcoded string drifting out of sync (the bug this
    /// method exists to fix).
    ///
    /// When `bindings` holds more than one entry for `action`, the first
    /// one that the trie actually resolves *back* to `action` wins — not
    /// just the first one naming `action` at all. `apply_key_overrides`
    /// has no collision detection: a `[keys]` override can rebind one
    /// action's sequence onto a key sequence another action already owns,
    /// in which case [`Self::from_bindings`]'s trie (built by iterating
    /// `bindings` in order and overwriting the matching node on each entry)
    /// resolves that sequence to whichever entry comes *last*, same as
    /// `Resolver::feed` would live. A naive first-match-by-action scan
    /// would happily report the *earlier*, now-shadowed entry as still
    /// bound — e.g. an override that lands `Quit` on `CursorDown`'s
    /// preset key `j` would make this method claim `CursorDown` is still
    /// bound to `j` too, when pressing `j` actually quits. Skipping a
    /// shadowed entry and continuing to scan (rather than stopping at the
    /// first name match) means this can never disagree with what
    /// `Resolver::feed` would really do — an action that only has shadowed
    /// entries correctly comes back `None` (unbound) instead of a binding
    /// that lies.
    pub fn binding_for(&self, action: Action) -> Option<&KeySeq> {
        self.bindings
            .iter()
            .find(|(seq, a)| *a == action && self.trie_resolution(seq) == Some(action))
            .map(|(seq, _)| seq)
    }

    /// Walks `seq` through this keymap's trie exactly as
    /// [`Resolver::feed`] would, one chord at a time, and returns the
    /// action a complete traversal lands on — `None` if `seq` doesn't fully
    /// resolve to *any* bound action in this trie (a dead sequence, or one
    /// left pending). The one ground truth [`Self::binding_for`] cross-checks
    /// every candidate binding against, since `bindings` (a flat list of
    /// "an action was configured with this sequence") and the trie (what a
    /// live keystroke sequence actually resolves to) can disagree the
    /// moment a `[keys]` override creates a collision — see
    /// [`Self::binding_for`]'s docs.
    fn trie_resolution(&self, seq: &KeySeq) -> Option<Action> {
        let mut node = &self.root;
        for chord in &seq.0 {
            node = node.children.get(chord)?;
        }
        node.action
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

    /// Drops a partially entered key sequence without feeding a synthetic key
    /// through the trie. Modal views use this when they consume a literal key
    /// that may also be configured as a global sequence prefix; feeding that
    /// key would risk leaving the resolver pending again.
    pub fn clear_pending(&mut self) {
        self.reset();
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
/// of `M-Right` in the list below so [`Keymap::binding_for`]'s first-match
/// rule picks it as `JumpForward`'s canonical, hinted binding, while
/// `M-Right` stays bound as a working alias (both entries resolve to the
/// same action; the trie has no trouble with two distinct key sequences
/// mapping to one action, only with one sequence mapping to two). When
/// `false`, `C-i` is left unbound entirely — the terminal delivers the
/// identical byte for a literal Tab and `Ctrl-i` in that case, and Tab
/// already means [`Action::FocusNextPane`] — so `M-Right` becomes forward's
/// sole, canonical binding. `M-Left` is `JumpBack`'s alias unconditionally,
/// alongside `C-o`, in both branches.
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
        // Familiar vim left/right motions, repurposed for the row's active
        // symbol rather than the line cursor (issue #13's epic decision 4)
        // — the uppercase pair isn't used for this since vim already claims
        // `H`/`L` for screen top/bottom (and `L` is ToggleLogView below
        // anyway), while `l`/`h`'s own line-cursor-motion meaning has no
        // katamari equivalent to conflict with (this app scrolls a rendered
        // diff, not raw text columns).
        ("l", Action::NextSymbol),
        ("h", Action::PrevSymbol),
        ("g d", Action::GotoDefinition),
        ("g r", Action::FindReferences),
        ("] d", Action::NextDiagnostic),
        ("[ d", Action::PrevDiagnostic),
        ("C-o", Action::JumpBack),
        ("M-Left", Action::JumpBack),
    ];
    if ci_distinguishable {
        bindings.push(("C-i", Action::JumpForward));
    }
    bindings.push(("M-Right", Action::JumpForward));
    bindings.extend([
        ("Enter", Action::Confirm),
        ("Tab", Action::FocusNextPane),
        ("BackTab", Action::FocusPrevPane),
        ("Esc", Action::Cancel),
        ("t", Action::ToggleTimeline),
        ("L", Action::ToggleLogView),
        ("I", Action::ToggleLspInspector),
        ("o", Action::OpenScopeMenu),
        ("u", Action::ToggleUnits),
        ("U", Action::RegenerateUnits),
        (".", Action::ToggleHints),
        ("v", Action::ToggleRangeSelect),
        ("c", Action::AddComment),
        ("C", Action::ToggleComments),
        ("/", Action::OpenSearch),
        ("n", Action::NextMatch),
        ("N", Action::PrevMatch),
        ("z o", Action::ExpandFold),
        ("z c", Action::CollapseFold),
        ("q", Action::Quit),
        ("?", Action::OpenHelp),
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
/// `M-g` prefix; `M-f`/`M-b` are `forward-word`/`backward-word`'s actual
/// keys, repurposed here for next/prev active-symbol selection — moving
/// between identifier-like tokens on the line is the closest thing this app
/// has to word motion, and Tab/BackTab are unavailable for it since issue
/// #13 gives those to pane focus instead), and a small app-specific choice
/// where none does (`C-h`
/// for hover — this app owns every key at the terminal level, so there's no
/// real help-prefix conflict to avoid the way there would be inside actual
/// emacs). `C-x n`/`C-x p` for next/prev file exercise the same
/// two-chord-with-a-shared-prefix shape `C-x` itself is famous for, without
/// this app actually needing a `C-x` prefix for anything else. A handful of
/// bindings that have no strong identity in either editor (the sidebar/
/// layout/timeline/comments/scope-menu toggles,
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
        ("M-f", Action::NextSymbol),
        ("M-b", Action::PrevSymbol),
        ("M-.", Action::GotoDefinition),
        ("M-?", Action::FindReferences),
        ("M-g M-n", Action::NextDiagnostic),
        ("M-g M-p", Action::PrevDiagnostic),
        ("C-o", Action::JumpBack),
        ("M-Left", Action::JumpBack),
    ];
    if ci_distinguishable {
        bindings.push(("C-i", Action::JumpForward));
    }
    bindings.push(("M-Right", Action::JumpForward));
    bindings.extend([
        ("Enter", Action::Confirm),
        ("Tab", Action::FocusNextPane),
        ("BackTab", Action::FocusPrevPane),
        ("Esc", Action::Cancel),
        ("t", Action::ToggleTimeline),
        ("L", Action::ToggleLogView),
        ("I", Action::ToggleLspInspector),
        ("o", Action::OpenScopeMenu),
        // Same vim-keys-for-things-with-no-emacs-identity rule as `q`/`b`/
        // `s`: semantic units have no emacs convention to defer to.
        ("u", Action::ToggleUnits),
        ("U", Action::RegenerateUnits),
        (".", Action::ToggleHints),
        ("C-Space", Action::ToggleRangeSelect),
        ("C-c C-c", Action::AddComment),
        ("C", Action::ToggleComments),
        // Same vim-keys-for-things-with-no-emacs-identity rule this preset
        // already follows for `q`/`b`/`s`/`?`: emacs has its own
        // incremental search (`C-s`), but it's a fundamentally different
        // interaction (search-as-you-move, not a confirm-then-`n`/`N`
        // two-step) — not close enough a fit to be worth reinventing here,
        // so this reuses vim's `/`/`n`/`N` verbatim, matching Issue #5's
        // reuse-vim rule.
        ("/", Action::OpenSearch),
        ("n", Action::NextMatch),
        ("N", Action::PrevMatch),
        // No strong emacs convention for fold expand/collapse, so this
        // reuses vim's `zo`/`zc` verbatim (matching outline-mode's own
        // `z`-prefixed `hide-*`/`show-*` fold commands isn't close enough a
        // fit to be worth a different binding here).
        ("z o", Action::ExpandFold),
        ("z c", Action::CollapseFold),
        ("q", Action::Quit),
        // Same vim-keys-for-things-with-no-emacs-identity rule as `q`/`b`/
        // `s` above: `?` is unbound and not a prefix of anything else in
        // this preset (`M-?` — FindReferences — is a different chord, since
        // `KeyChord` compares modifiers too), so there's no real collision
        // to design around.
        ("?", Action::OpenHelp),
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
/// what's fundamentally a plain lookup table.
///
/// Two production callers as of the help popup (issue #3): [`action_by_name`]
/// itself (config's `[keys]` parsing) and [`crate::ui::help`]'s filter
/// matching, which treats an action's kebab name as one of the substrings a
/// typed filter can match against (alongside its description and live
/// binding notation) — a reviewer half-remembering "cursor-down" as well as
/// "move" should find the same row either way.
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
        Action::FocusNextPane => "focus-next-pane",
        Action::FocusPrevPane => "focus-prev-pane",
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
        Action::ToggleLspInspector => "toggle-lsp-inspector",
        Action::OpenScopeMenu => "open-scope-menu",
        Action::ToggleUnits => "toggle-units",
        Action::RegenerateUnits => "regenerate-units",
        Action::ToggleHints => "toggle-hints",
        Action::AddComment => "add-comment",
        Action::ToggleComments => "toggle-comments",
        Action::ExpandFold => "expand-fold",
        Action::CollapseFold => "collapse-fold",
        Action::Quit => "quit",
        Action::OpenHelp => "open-help",
        Action::OpenSearch => "open-search",
        Action::NextMatch => "next-match",
        Action::PrevMatch => "prev-match",
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
        "focus-next-pane" => Action::FocusNextPane,
        "focus-prev-pane" => Action::FocusPrevPane,
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
        "toggle-lsp-inspector" => Action::ToggleLspInspector,
        "open-scope-menu" => Action::OpenScopeMenu,
        "toggle-units" => Action::ToggleUnits,
        "regenerate-units" => Action::RegenerateUnits,
        "toggle-hints" => Action::ToggleHints,
        "add-comment" => Action::AddComment,
        "toggle-comments" => Action::ToggleComments,
        "expand-fold" => Action::ExpandFold,
        "collapse-fold" => Action::CollapseFold,
        "quit" => Action::Quit,
        "open-help" => Action::OpenHelp,
        "open-search" => Action::OpenSearch,
        "next-match" => Action::NextMatch,
        "prev-match" => Action::PrevMatch,
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
    fn clear_pending_resets_without_feeding_a_consumed_modal_key() {
        let bindings = [
            (KeySeq::parse("g V"), Action::Top),
            (KeySeq::parse("q"), Action::Quit),
        ];
        let keymap = Keymap::from_bindings(&bindings);
        let mut resolver = keymap.resolver();
        assert_eq!(resolver.feed(chord('g')), StepResult::Pending);
        resolver.clear_pending();
        assert_eq!(resolver.pending_display(), "");
        // A modal view may consume V itself; it must not leave `g V` pending
        // or accidentally dispatch the configured action afterward.
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
    fn hover_focus_pane_and_cancel_keys_resolve() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let mut resolver = keymap.resolver();
        assert_eq!(
            resolver.feed(chord('K')),
            StepResult::Matched(Action::Hover)
        );
        assert_eq!(
            resolver.feed(KeyChord::new(KeyCode::Tab, KeyModifiers::NONE)),
            StepResult::Matched(Action::FocusNextPane)
        );
        assert_eq!(
            resolver.feed(KeyChord::new(KeyCode::BackTab, KeyModifiers::NONE)),
            StepResult::Matched(Action::FocusPrevPane)
        );
        assert_eq!(
            resolver.feed(KeyChord::new(KeyCode::Esc, KeyModifiers::NONE)),
            StepResult::Matched(Action::Cancel)
        );
    }

    /// Issue #13's epic decision 4: lowercase `l`/`h` select the next/prev
    /// active symbol in the vim preset — Tab/BackTab's pre-#13 job, now that
    /// those are [`Action::FocusNextPane`]/[`Action::FocusPrevPane`] instead.
    #[test]
    fn vim_l_and_h_resolve_to_next_and_prev_symbol() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let mut resolver = keymap.resolver();
        assert_eq!(
            resolver.feed(chord('l')),
            StepResult::Matched(Action::NextSymbol)
        );
        assert_eq!(
            resolver.feed(chord('h')),
            StepResult::Matched(Action::PrevSymbol)
        );
    }

    /// The emacs preset's counterpart: `M-f`/`M-b`, real emacs
    /// `forward-word`/`backward-word` keys repurposed for symbol selection
    /// (see [`emacs_preset`]'s docs).
    #[test]
    fn emacs_meta_f_and_meta_b_resolve_to_next_and_prev_symbol() {
        let keymap = Keymap::from_bindings(&emacs_preset(false));
        let mut resolver = keymap.resolver();
        assert_eq!(
            resolver.feed(alt('f')),
            StepResult::Matched(Action::NextSymbol)
        );
        assert_eq!(
            resolver.feed(alt('b')),
            StepResult::Matched(Action::PrevSymbol)
        );
    }

    /// Acceptance criterion: "Tab/BackTab never resolve to symbol actions in
    /// a fresh built-in keymap" — checked positively (they resolve to the
    /// pane-focus actions) in both presets at once, the same
    /// loop-over-both-preset-functions shape
    /// [`open_help_binds_to_plain_question_mark_in_both_presets`] uses.
    #[test]
    fn tab_and_backtab_resolve_to_focus_next_and_prev_pane_in_both_presets() {
        for preset_fn in [vim_preset, emacs_preset] {
            let keymap = Keymap::from_bindings(&preset_fn(false));
            let mut resolver = keymap.resolver();
            assert_eq!(
                resolver.feed(KeyChord::new(KeyCode::Tab, KeyModifiers::NONE)),
                StepResult::Matched(Action::FocusNextPane)
            );
            let mut resolver = keymap.resolver();
            assert_eq!(
                resolver.feed(KeyChord::new(KeyCode::BackTab, KeyModifiers::NONE)),
                StepResult::Matched(Action::FocusPrevPane)
            );
        }
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
    fn z_o_and_z_c_resolve_to_expand_and_collapse_fold_in_both_presets() {
        for preset in [vim_preset(false), emacs_preset(false)] {
            let keymap = Keymap::from_bindings(&preset);
            let mut resolver = keymap.resolver();
            assert_eq!(resolver.feed(chord('z')), StepResult::Pending);
            assert_eq!(
                resolver.feed(chord('o')),
                StepResult::Matched(Action::ExpandFold)
            );
            assert_eq!(resolver.feed(chord('z')), StepResult::Pending);
            assert_eq!(
                resolver.feed(chord('c')),
                StepResult::Matched(Action::CollapseFold)
            );
        }
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

    fn alt_left() -> KeyChord {
        KeyChord::new(KeyCode::Left, KeyModifiers::ALT)
    }

    fn alt_right() -> KeyChord {
        KeyChord::new(KeyCode::Right, KeyModifiers::ALT)
    }

    #[test]
    fn open_help_binds_to_plain_question_mark_in_both_presets() {
        for preset_fn in [vim_preset, emacs_preset] {
            let keymap = Keymap::from_bindings(&preset_fn(false));
            let mut resolver = keymap.resolver();
            assert_eq!(
                resolver.feed(chord('?')),
                StepResult::Matched(Action::OpenHelp)
            );
        }
    }

    /// Issue #5's search keys — same keys in both presets, per this
    /// milestone's reuse-vim rule (see `emacs_preset`'s own comment on
    /// `/`/`n`/`N`).
    #[test]
    fn slash_n_and_shift_n_resolve_to_search_actions_in_both_presets() {
        for preset_fn in [vim_preset, emacs_preset] {
            let keymap = Keymap::from_bindings(&preset_fn(false));

            let mut resolver = keymap.resolver();
            assert_eq!(
                resolver.feed(chord('/')),
                StepResult::Matched(Action::OpenSearch)
            );

            let mut resolver = keymap.resolver();
            assert_eq!(
                resolver.feed(chord('n')),
                StepResult::Matched(Action::NextMatch)
            );

            let mut resolver = keymap.resolver();
            let shifted_n = KeyChord::new(KeyCode::Char('N'), KeyModifiers::SHIFT);
            assert_eq!(
                resolver.feed(shifted_n),
                StepResult::Matched(Action::PrevMatch)
            );
        }
    }

    /// The verified-architecture claim this milestone's spec leans on,
    /// pinned down directly: a bare `?` (no modifiers — `KeyChord::new`
    /// strips SHIFT, so `Shift-/`'s `Char('?')` normalizes the same way)
    /// and emacs's `M-?` (`FindReferences`, a real `xref-find-references`
    /// convention — see [`emacs_preset`]'s docs) are different `KeyChord`s
    /// because `KeyChord` compares modifiers too, not just `KeyCode`. Each
    /// resolves independently to its own action in the same keymap; neither
    /// steals the other's binding.
    #[test]
    fn plain_question_mark_does_not_collide_with_emacs_meta_question_mark() {
        let keymap = Keymap::from_bindings(&emacs_preset(false));

        let mut help = keymap.resolver();
        assert_eq!(help.feed(chord('?')), StepResult::Matched(Action::OpenHelp));

        let mut refs = keymap.resolver();
        assert_eq!(
            refs.feed(alt('?')),
            StepResult::Matched(Action::FindReferences)
        );
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
            Action::FocusNextPane,
            Action::FocusPrevPane,
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
            Action::ToggleLspInspector,
            Action::OpenScopeMenu,
            Action::AddComment,
            Action::ToggleComments,
            Action::ExpandFold,
            Action::CollapseFold,
            Action::Quit,
            Action::OpenHelp,
            Action::OpenSearch,
            Action::NextMatch,
            Action::PrevMatch,
            Action::ToggleUnits,
            Action::RegenerateUnits,
            Action::ToggleHints,
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

    /// Mirrors [`binding_for_finds_a_vim_preset_binding`]/
    /// [`binding_for_finds_an_emacs_preset_binding`] for `NextSymbol` —
    /// pinning that the live-binding-driven hint text (`ui::hints`) reports
    /// `l` under vim and `M-f` under emacs, not the pre-#13 Tab either
    /// preset used to share.
    #[test]
    fn binding_for_finds_next_symbol_bound_to_l_in_vim_and_meta_f_in_emacs() {
        let vim_keymap = Keymap::from_bindings(&vim_preset(false));
        assert_eq!(
            vim_keymap
                .binding_for(Action::NextSymbol)
                .unwrap()
                .compact_notation(),
            "l"
        );
        let emacs_keymap = Keymap::from_bindings(&emacs_preset(false));
        assert_eq!(
            emacs_keymap
                .binding_for(Action::NextSymbol)
                .unwrap()
                .compact_notation(),
            "M-f"
        );
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
    fn binding_for_never_reports_an_entry_the_trie_resolves_to_a_different_action() {
        // Reproduces a real `[keys]` collision: `apply_key_overrides`
        // rebinds `Quit`'s own slot onto vim's `CursorDown` key ("j") in
        // place, with no collision detection. `from_bindings` builds the
        // trie by iterating `bindings` in order, so whichever of the two
        // entries claiming "j" comes *last* — `Quit`'s, since it's
        // rewritten after `CursorDown`'s untouched preset entry — is what
        // `Resolver::feed` actually reaches.
        let mut bindings = vim_preset(false);
        let quit = bindings
            .iter_mut()
            .find(|(_, a)| *a == Action::Quit)
            .unwrap();
        quit.0 = KeySeq::parse("j");
        let keymap = Keymap::from_bindings(&bindings);

        let mut resolver = keymap.resolver();
        assert_eq!(
            resolver.feed(chord('j')),
            StepResult::Matched(Action::Quit),
            "sanity check: the trie really does resolve \"j\" to Quit here"
        );

        // `CursorDown`'s own bindings-list entry still says "j" — but that
        // entry is shadowed in the trie, so reporting it as CursorDown's
        // live binding would be a lie a reviewer could act on (pressing
        // "j" expecting cursor movement, and quitting instead).
        assert!(
            keymap.binding_for(Action::CursorDown).is_none(),
            "CursorDown must show unbound, not a binding the trie no longer honors"
        );
        assert_eq!(
            keymap.binding_for(Action::Quit).unwrap().compact_notation(),
            "j"
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

    /// The 42 actions (see the `action_name_and_action_by_name_round_trip…`
    /// test's `all` list) each get exactly one binding — except `JumpBack`,
    /// which always gets a second one (`M-Left`), and `JumpForward`, which
    /// gets a second one (`C-i`) precisely when `ci_distinguishable` is set
    /// (per [`vim_preset`]/[`emacs_preset`]'s docs, `M-Right` *replaces* the
    /// pre-#12 `C-t` as `JumpForward`'s own single baseline binding rather
    /// than adding to it). So this checks two things per preset: every
    /// action is still reachable (`actions.len() == 42` after dedup), and
    /// the raw entry count is exactly one more than that (`JumpBack`'s
    /// unconditional `M-Left` alias) plus one more still when the extra
    /// `C-i` alias is present. As a side effect, this also forces
    /// `?`/`/`/`n`/`N` to be bound in both presets the moment `ACTION_COUNT`
    /// bumps — [`Action::OpenHelp`]/[`Action::OpenSearch`]/
    /// [`Action::NextMatch`]/[`Action::PrevMatch`] with no binding in either
    /// preset would fail the `actions.len() == ACTION_COUNT` assertion
    /// below, not just the dedicated
    /// `open_help_binds_to_plain_question_mark_in_both_presets` test.
    #[test]
    fn every_vim_and_emacs_binding_covers_every_action_exactly_once() {
        const ACTION_COUNT: usize = 42;
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
                let expected_len = ACTION_COUNT + 1 + usize::from(ci_distinguishable);
                assert_eq!(
                    preset.len(),
                    expected_len,
                    "ci_distinguishable={ci_distinguishable} should add exactly the \
                     M-Left/M-Right aliases, plus the C-i alias when set"
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
    fn ci_distinguishable_leaves_c_t_unbound_and_resolves_c_i_plus_m_left_m_right() {
        for preset_fn in [vim_preset, emacs_preset] {
            let keymap = Keymap::from_bindings(&preset_fn(true));

            // `Ctrl-]`/`Ctrl-t` have no default binding at all now (#12) —
            // neither was ever a tag-stack key here, and `C-t`'s old
            // fallback role is gone.
            let mut resolver = keymap.resolver();
            assert_eq!(resolver.feed(ctrl('t')), StepResult::Cancelled);

            let mut resolver = keymap.resolver();
            assert_eq!(
                resolver.feed(ctrl('i')),
                StepResult::Matched(Action::JumpForward)
            );
            let mut resolver = keymap.resolver();
            assert_eq!(
                resolver.feed(ctrl('o')),
                StepResult::Matched(Action::JumpBack)
            );
            let mut resolver = keymap.resolver();
            assert_eq!(
                resolver.feed(alt_left()),
                StepResult::Matched(Action::JumpBack)
            );
            let mut resolver = keymap.resolver();
            assert_eq!(
                resolver.feed(alt_right()),
                StepResult::Matched(Action::JumpForward)
            );
        }
    }

    #[test]
    fn not_ci_distinguishable_binds_m_right_as_canonical_forward_and_leaves_c_i_c_t_unbound() {
        for preset_fn in [vim_preset, emacs_preset] {
            let keymap = Keymap::from_bindings(&preset_fn(false));
            assert_eq!(
                keymap
                    .binding_for(Action::JumpForward)
                    .unwrap()
                    .compact_notation(),
                "M-Right"
            );
            assert_eq!(
                keymap
                    .binding_for(Action::JumpBack)
                    .unwrap()
                    .compact_notation(),
                "C-o"
            );
            // `C-i` was never inserted into the trie in this mode — feeding
            // it cancels like any other unbound key, rather than resolving
            // to whatever it happened to fall through to.
            let mut resolver = keymap.resolver();
            assert_eq!(resolver.feed(ctrl('i')), StepResult::Cancelled);
            // Nor was `C-t` — it has no default binding in either mode.
            let mut resolver = keymap.resolver();
            assert_eq!(resolver.feed(ctrl('t')), StepResult::Cancelled);

            let mut resolver = keymap.resolver();
            assert_eq!(
                resolver.feed(alt_left()),
                StepResult::Matched(Action::JumpBack)
            );
            let mut resolver = keymap.resolver();
            assert_eq!(
                resolver.feed(alt_right()),
                StepResult::Matched(Action::JumpForward)
            );
        }
    }

    /// Tab must keep meaning `FocusNextPane` in both modes — the whole
    /// reason `ci_distinguishable=false` refuses to bind `C-i` at all is
    /// that an undisambiguating terminal reports a literal Tab keypress
    /// with the exact same byte, so binding `C-i` there would silently
    /// reroute every Tab press meant for pane-focus cycling. Checking it
    /// explicitly under both modes pins that down rather than trusting it
    /// by omission.
    #[test]
    fn tab_still_resolves_to_focus_next_pane_in_both_ci_distinguishable_modes() {
        for ci_distinguishable in [false, true] {
            for preset_fn in [vim_preset, emacs_preset] {
                let keymap = Keymap::from_bindings(&preset_fn(ci_distinguishable));
                let mut resolver = keymap.resolver();
                assert_eq!(
                    resolver.feed(KeyChord::new(KeyCode::Tab, KeyModifiers::NONE)),
                    StepResult::Matched(Action::FocusNextPane)
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
