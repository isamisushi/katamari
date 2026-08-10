//! One place that knows how a logical keypress becomes bytes on the wire —
//! test bodies name a [`Key`], never an escape sequence, so a harness test
//! reads as "press jump-forward" rather than a comment explaining why some
//! literal `\x1b[105;5u` means that. The actual encodings come straight from
//! the milestone's verified research (see the M10 task's "Key encodings the
//! harness must send"), not reverse-engineered here.

use super::harness::KittyMode;

/// A logical keypress a test body can ask the harness to send. Encodes
/// differently depending on the fake terminal's [`KittyMode`] — the same
/// distinction the real kitty keyboard protocol makes, which is the whole
/// point of the `kitty_supported`/`kitty_unsupported` test pair.
#[derive(Debug, Clone, Copy)]
pub enum Key {
    /// `Ctrl-i` — bound to `Action::JumpForward` only when the terminal
    /// disambiguates it from a plain Tab (kitty protocol active). Sending
    /// this under [`KittyMode::Unsupported`] would be nonsensical (there's
    /// no way to send a legacy-encoded `Ctrl-i` distinguishable from Tab —
    /// that's the entire reason the binding doesn't exist in that mode), so
    /// [`Key::encode`] deliberately has no `Unsupported` arm for it.
    CtrlI,
    /// `Ctrl-t` — has no default binding at all as of issue #12 (the
    /// pre-#12 `Action::JumpForward` fallback this used to exercise is
    /// gone; `M-Right` replaced it). Kept as a `Key` variant specifically
    /// so `tests/e2e/kitty.rs` can assert it now resolves to *nothing*.
    /// Unambiguous in both modes (`0x14`, not shared with any other key).
    CtrlT,
    /// `Ctrl-o` — `Action::JumpBack`, unambiguous in both modes (`0x0f`).
    CtrlO,
    /// `Alt-Left` — `Action::JumpBack`'s terminal-agnostic alias (issue
    /// #12), encoded the same xterm `CSI 1 ; 3 <letter>` modified-arrow form
    /// in both kitty modes — crossterm's legacy parser recognizes this
    /// regardless of whether the kitty keyboard protocol negotiated, so
    /// unlike `Key::CtrlI` there's no mode-specific encoding to pick
    /// between (same shape as `Key::CtrlT`/`Key::CtrlO` above).
    AltLeft,
    /// `Alt-Right` — `Action::JumpForward`'s terminal-agnostic alias, and
    /// (per issue #12) forward's *canonical* binding in a terminal that
    /// can't tell `Ctrl-i` from a plain Tab apart. See [`Key::AltLeft`]'s
    /// docs for the encoding.
    AltRight,
    /// `Ctrl-s` — the comment-compose overlay's `ComposeOutcome::Save`,
    /// unambiguous in both modes (`0x13`).
    CtrlS,
    /// A literal Tab / `Action::FocusNextPane` (issue #13 split pane focus
    /// out of the pre-#13 `Action::NextSymbol` Tab used to mean). Encodes
    /// to the same `0x09` byte as legacy `Ctrl-i` in
    /// [`KittyMode::Unsupported`] — that collision is the exact thing under
    /// test there.
    Tab,
    /// Shift-Tab / `Action::FocusPrevPane`. Unlike [`Key::CtrlI`], this is
    /// unambiguous with a plain Tab in *both* modes — a real terminal never
    /// sends a bare `0x09` for Shift-Tab, so there's a genuine legacy
    /// encoding (`ESC [ Z`, the standard xterm/legacy BackTab sequence
    /// crossterm's non-kitty parser recognizes) rather than `CtrlI`'s
    /// "panics under `Unsupported`" shape.
    BackTab,
    /// Enter / `Action::Confirm` — `0x0D` (carriage return), the byte a
    /// real terminal sends for the Enter key and what crossterm's legacy
    /// parser recognizes as `KeyCode::Enter` in both kitty modes, so this
    /// has a single encoding regardless of `KittyMode`.
    Enter,
    /// A plain character, sent as its raw UTF-8 bytes.
    Char(char),
    /// Esc. The one key whose *legacy* encoding (a bare `0x1b`) is
    /// genuinely ambiguous with the start of any other escape sequence —
    /// which is exactly what `DISAMBIGUATE_ESCAPE_CODES` exists to fix: a
    /// kitty-protocol terminal sends a real Escape keypress as its CSI-u
    /// form (`ESC [ 27 u`, codepoint 27 being Escape's own) precisely so a
    /// lone `0x1b` byte is never emitted standalone once the flag is
    /// active. `KittyMode::Unsupported` still sends the legacy bare byte —
    /// crossterm's own escape-timeout heuristic there is what turns "ESC,
    /// then nothing else arrives" into `KeyCode::Esc`.
    Esc,
}

impl Key {
    /// The bytes this key produces on the wire under `mode` — what a real
    /// terminal emulator would have sent crossterm, either in kitty
    /// disambiguation form or the legacy form every terminal falls back to.
    pub fn encode(self, mode: KittyMode) -> Vec<u8> {
        match self {
            Key::CtrlI => match mode {
                KittyMode::Supported => b"\x1b[105;5u".to_vec(),
                KittyMode::Unsupported => {
                    panic!(
                        "Key::CtrlI has no legacy encoding distinguishable from Tab; \
                         send Key::Tab instead under KittyMode::Unsupported"
                    )
                }
            },
            Key::CtrlT => vec![0x14],
            Key::CtrlO => vec![0x0f],
            Key::AltLeft => b"\x1b[1;3D".to_vec(),
            Key::AltRight => b"\x1b[1;3C".to_vec(),
            Key::CtrlS => vec![0x13],
            Key::Tab => vec![0x09],
            Key::BackTab => match mode {
                KittyMode::Supported => b"\x1b[9;2u".to_vec(),
                KittyMode::Unsupported => b"\x1b[Z".to_vec(),
            },
            Key::Enter => vec![0x0d],
            Key::Char(c) => {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
            Key::Esc => match mode {
                KittyMode::Supported => b"\x1b[27u".to_vec(),
                KittyMode::Unsupported => vec![0x1b],
            },
        }
    }
}
