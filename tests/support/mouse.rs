//! Issue #20: SGR mouse-wheel byte sequences for the E2E suite —
//! `tests/support/key.rs`'s sibling for pointer input. Test bodies name a
//! [`MouseKey`], never an escape sequence, the same "no raw bytes in a test
//! body" rule `Key` follows.

/// A logical wheel event a test body can ask the harness to send.
/// `column`/`row` are 0-based screen coordinates — [`MouseKey::encode`]
/// converts them to the wire's 1-based ones, mirroring crossterm's own
/// `-1` on the parsing side (see `crossterm::event::sys::unix::parse::parse_csi_sgr_mouse`).
#[derive(Debug, Clone, Copy)]
pub enum MouseKey {
    ScrollUp { column: u16, row: u16 },
    ScrollDown { column: u16, row: u16 },
}

impl MouseKey {
    /// SGR mouse mode's `ESC [ < Cb ; Cx ; Cy M` — unlike [`super::key::Key`],
    /// there's no [`super::harness::KittyMode`] parameter: SGR mouse
    /// reporting is a wholly separate protocol from the kitty keyboard
    /// protocol's key-disambiguation flags, negotiated (and, here,
    /// hardcoded) independently of whichever [`KittyMode`](super::harness::KittyMode)
    /// a harness answers the keyboard probe with. `Cb = 64` is wheel-up,
    /// `65` is wheel-down (crossterm's `parse_cb`: button number `(cb & 3)
    /// | ((cb & 0xC0) >> 4)` — `64` decodes to button `4`, `65` to button
    /// `5`); the trailing byte is always the uppercase `M` press form
    /// (scroll events have no separate release, so SGR's lowercase-`m`
    /// "release" variant never applies to them — see crossterm's
    /// `parse_csi_sgr_mouse`, which only flips `Down` kinds on `m`).
    pub fn encode(self) -> Vec<u8> {
        let (cb, column, row) = match self {
            MouseKey::ScrollUp { column, row } => (64, column, row),
            MouseKey::ScrollDown { column, row } => (65, column, row),
        };
        format!("\x1b[<{cb};{};{}M", column + 1, row + 1).into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_up_encodes_button_64_at_one_based_coordinates() {
        assert_eq!(
            MouseKey::ScrollUp { column: 5, row: 10 }.encode(),
            b"\x1b[<64;6;11M".to_vec()
        );
    }

    #[test]
    fn scroll_down_encodes_button_65_at_one_based_coordinates() {
        assert_eq!(
            MouseKey::ScrollDown { column: 0, row: 0 }.encode(),
            b"\x1b[<65;1;1M".to_vec()
        );
    }
}
