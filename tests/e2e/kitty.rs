//! The M10 milestone's headline pair: does the kitty keyboard protocol
//! probe/reply exchange (M9b) actually work end-to-end against real
//! crossterm parsing, in both directions the real world presents —
//! `kitty_supported` (probe answered with both flags-query and DA1 halves)
//! and `kitty_unsupported` (DA1 only, the tmux/plain-terminal case).
//!
//! Neither test can exercise a genuine jump-stack round trip: every actual
//! push onto `JumpStack` happens either via `gd`/`gr` (needs a live
//! language server, out of scope for this suite's LSP-free fixtures — see
//! `support::fixture`'s docs) or via `Ctrl-o`/`Ctrl-i` themselves, which
//! only *pop*, never push (`ui::navigation::navigate_to`'s `record_history`
//! path). So instead of a hollow "press a key, assert nothing crashed"
//! check, both tests press `Ctrl-o` then the mode's jump-forward key against
//! an empty stack and assert the specific "no earlier/later position"
//! status text — proof the actual key bytes on the wire were parsed as
//! `JumpBack`/`JumpForward` and dispatched, not just that the process didn't
//! crash.

use crate::support::screen::underlined_cells;
use crate::support::{Harness, Key, KittyMode, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn kitty_supported() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            kitty_mode: KittyMode::Supported,
            ..Default::default()
        },
    );

    // M9b's discoverability fix, observed for real: a kitty-capable
    // terminal's hint bar names `C-i` as the jump-forward alias, matching
    // neovim.
    h.wait_for_text("C-o/C-i");
    h.wait_for_text("README.md");

    // `Ctrl-o` (0x0f, unambiguous in either mode) with an empty back-stack.
    h.send(Key::CtrlO);
    h.wait_for_text("jump: no earlier position");

    // The actual payoff: a kitty-disambiguated `Ctrl-i` (`\x1b[105;5u`) must
    // reach `Action::JumpForward`, not `Action::NextSymbol` — proving
    // crossterm parsed the kitty-protocol escape sequence as a real
    // Ctrl-modified key, distinguishable from the plain Tab this exact byte
    // sequence would otherwise collide with.
    h.send(Key::CtrlI);
    h.wait_for_text("jump: no later position");

    // Tab itself must still mean `NextSymbol`, unaffected by the kitty
    // protocol being active — checked via the active symbol's underline
    // moving, the only on-screen signal `NextSymbol` produces. `NextSymbol`
    // is a no-op on the header row the cursor starts on (see
    // `App::cursor_row_text`'s docs — a header has no line content to scan
    // for symbols), so this moves onto a content line first.
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");
    let before = h.with_screen(underlined_cells);
    h.send(Key::Tab);
    h.wait_until(Duration::from_secs(2), |s| {
        underlined_cells(s) != before && !underlined_cells(s).is_empty()
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn kitty_unsupported() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            kitty_mode: KittyMode::Unsupported,
            ..Default::default()
        },
    );

    // The fallback hint: no kitty protocol, so jump-forward's only bound
    // key is `C-t`.
    h.wait_for_text("C-o/C-t");
    h.wait_for_text("README.md");

    h.send(Key::CtrlO);
    h.wait_for_text("jump: no earlier position");

    // `Ctrl-t` (0x14) is the fallback's real binding, unambiguous in both
    // modes — must still reach `JumpForward` when the kitty protocol never
    // activated at all.
    h.send(Key::CtrlT);
    h.wait_for_text("jump: no later position");

    // The mirror image of `kitty_supported`'s Tab check: in this mode, raw
    // `0x09` is *only* ever a literal Tab (no `Ctrl-i` binding exists to
    // collide with it — see `vim_preset`'s docs on why binding one there
    // would silently steal every Tab press). Sending it must move the
    // active symbol, not produce a jump-status message. As in
    // `kitty_supported`, move onto a content line first — `NextSymbol` is a
    // no-op on the header row the cursor starts on.
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");
    let before = h.with_screen(underlined_cells);
    h.send(Key::Tab);
    h.wait_until(Duration::from_secs(2), |s| {
        underlined_cells(s) != before && !underlined_cells(s).is_empty()
    });
    assert!(
        !h.screen_contents().contains("jump: no later position"),
        "a literal Tab must never be mistaken for Ctrl-i when the kitty protocol isn't active"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
