//! Issue #20: SGR mouse-wheel byte sequences for the E2E suite —
//! `tests/support/key.rs`'s sibling for pointer input. Test bodies name a
//! [`MouseKey`], never an escape sequence, the same "no raw bytes in a test
//! body" rule `Key` follows.

/// Which physical button a [`MouseKey::Down`]/[`MouseKey::Up`] names —
/// re-declared here (rather than importing crossterm's own three-way
/// `MouseButton`) so this module stays as self-contained as
/// [`super::key::Key`] (a raw byte sequence out, nothing but `column`/
/// `row`/`button` in). No `Middle` variant: nothing in this suite (issue
/// #21's tree click, or its right-click-does-nothing regression test)
/// needs it, and an unconstructed variant would be dead code — see
/// [`MouseKey::encode`]'s docs for where SGR's `Cb = 1` would slot in if a
/// future issue ever does.
#[derive(Debug, Clone, Copy)]
pub enum MouseButton {
    Left,
    Right,
}

/// A logical mouse event a test body can ask the harness to send.
/// `column`/`row` are 0-based screen coordinates — [`MouseKey::encode`]
/// converts them to the wire's 1-based ones, mirroring crossterm's own
/// `-1` on the parsing side (see `crossterm::event::sys::unix::parse::parse_csi_sgr_mouse`).
#[derive(Debug, Clone, Copy)]
pub enum MouseKey {
    ScrollUp {
        column: u16,
        row: u16,
    },
    ScrollDown {
        column: u16,
        row: u16,
    },
    /// Issue #21: a primary/secondary press — `super::Harness::send_mouse`
    /// with this is what a click test body sends to land on a tree row.
    /// `shift` is SGR's own `Cb` shift-modifier bit (issue #22 req 4's
    /// shift-click) — `false` for every pre-#22 click, which never touched
    /// a modifier at all.
    Down {
        button: MouseButton,
        column: u16,
        row: u16,
        shift: bool,
    },
    /// The matching release. Katamari's own dispatch only ever reacts to
    /// `Down` (see `ui::mod`'s `Event::Mouse` arm — there's no drag/
    /// double-click in scope), so no test sends this on its own; it exists
    /// so a click test can send a complete press-then-release pair the way
    /// a real terminal always would, in case a future harness assertion
    /// ever needs the `Up` byte to have been seen at all. Carries `shift`
    /// too, purely so a press/release pair can share one literal shape —
    /// katamari's dispatch never reads it off an `Up` byte.
    Up {
        button: MouseButton,
        column: u16,
        row: u16,
        shift: bool,
    },
}

impl MouseKey {
    /// SGR mouse mode's `ESC [ < Cb ; Cx ; Cy M`/`m` — unlike
    /// [`super::key::Key`], there's no [`super::harness::KittyMode`]
    /// parameter: SGR mouse reporting is a wholly separate protocol from
    /// the kitty keyboard protocol's key-disambiguation flags, negotiated
    /// (and, here, hardcoded) independently of whichever
    /// [`KittyMode`](super::harness::KittyMode) a harness answers the
    /// keyboard probe with.
    ///
    /// `Cb` for a wheel tick is `64`/`65` (crossterm's `parse_cb`: button
    /// number `(cb & 3) | ((cb & 0xC0) >> 4)` — `64` decodes to button `4`,
    /// `65` to button `5`), always terminated `M`: scroll events have no
    /// separate release, so SGR's lowercase-`m` "release" variant never
    /// applies to them (see crossterm's `parse_csi_sgr_mouse`, which only
    /// flips `Down` kinds on `m`). `Cb` for a plain button press/release is
    /// just the button number itself — `0`/`1`/`2` for left/middle/right
    /// (this module only ever sends `0`/`2`, see [`MouseButton`]'s docs),
    /// the same `cb & 3` decode with none of the wheel/motion bits (`0x40`/
    /// `0x20`) set — with the press/release distinction carried entirely by
    /// the trailing byte instead (`M` press, `m` release). `shift` sets
    /// `Cb`'s `0x04` bit (crossterm's `parse_cb`: `cb & 0b0000_0100 ==
    /// 0b0000_0100` decodes to `KeyModifiers::SHIFT`) — never set for a
    /// wheel tick, which has no modifier vocabulary this suite uses.
    pub fn encode(self) -> Vec<u8> {
        const SHIFT_BIT: u8 = 0b0000_0100;
        let (cb, column, row, trailing) = match self {
            MouseKey::ScrollUp { column, row } => (64, column, row, 'M'),
            MouseKey::ScrollDown { column, row } => (65, column, row, 'M'),
            MouseKey::Down {
                button,
                column,
                row,
                shift,
            } => (
                button.cb() | if shift { SHIFT_BIT } else { 0 },
                column,
                row,
                'M',
            ),
            MouseKey::Up {
                button,
                column,
                row,
                shift,
            } => (
                button.cb() | if shift { SHIFT_BIT } else { 0 },
                column,
                row,
                'm',
            ),
        };
        format!("\x1b[<{cb};{};{}{trailing}", column + 1, row + 1).into_bytes()
    }
}

impl MouseButton {
    fn cb(self) -> u8 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Right => 2,
        }
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

    #[test]
    fn left_down_encodes_button_0_pressed_at_one_based_coordinates() {
        assert_eq!(
            MouseKey::Down {
                button: MouseButton::Left,
                column: 5,
                row: 10,
                shift: false,
            }
            .encode(),
            b"\x1b[<0;6;11M".to_vec()
        );
    }

    #[test]
    fn left_up_encodes_button_0_released_with_a_lowercase_trailing_byte() {
        assert_eq!(
            MouseKey::Up {
                button: MouseButton::Left,
                column: 0,
                row: 0,
                shift: false,
            }
            .encode(),
            b"\x1b[<0;1;1m".to_vec()
        );
    }

    #[test]
    fn right_down_encodes_button_2() {
        assert_eq!(
            MouseKey::Down {
                button: MouseButton::Right,
                column: 0,
                row: 0,
                shift: false,
            }
            .encode(),
            b"\x1b[<2;1;1M".to_vec()
        );
    }

    #[test]
    fn shift_left_down_sets_cbs_shift_bit() {
        assert_eq!(
            MouseKey::Down {
                button: MouseButton::Left,
                column: 0,
                row: 0,
                shift: true,
            }
            .encode(),
            b"\x1b[<4;1;1M".to_vec()
        );
    }
}
