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
    /// `Ctrl-t` — `Action::JumpForward`'s always-available fallback binding,
    /// unambiguous in both modes (`0x14`, not shared with any other key).
    CtrlT,
    /// `Ctrl-o` — `Action::JumpBack`, unambiguous in both modes (`0x0f`).
    CtrlO,
    /// A literal Tab / `Action::NextSymbol`. Encodes to the same `0x09` byte
    /// as legacy `Ctrl-i` in [`KittyMode::Unsupported`] — that collision is
    /// the exact thing under test there.
    Tab,
    /// Enter / `Action::Confirm` — `0x0D` (carriage return), the byte a
    /// real terminal sends for the Enter key and what crossterm's legacy
    /// parser recognizes as `KeyCode::Enter` in both kitty modes, so this
    /// has a single encoding regardless of `KittyMode`.
    Enter,
    /// A plain character, sent as its raw UTF-8 bytes.
    Char(char),
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
            Key::Tab => vec![0x09],
            Key::Enter => vec![0x0d],
            Key::Char(c) => {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
    }
}
