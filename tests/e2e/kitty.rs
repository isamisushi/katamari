//! The M10 milestone's headline pair: does the kitty keyboard protocol
//! probe/reply exchange (M9b) actually work end-to-end against real
//! crossterm parsing, in both directions the real world presents —
//! `kitty_supported` (probe answered with both flags-query and DA1 halves)
//! and `kitty_unsupported` (DA1 only, the tmux/plain-terminal case). Issue
//! #12 extended both with the same real-crossterm-parsing proof for
//! `M-Left`/`M-Right` (the unconditional back/forward aliases) and for
//! `Ctrl-t`/`Ctrl-]` having no default binding at all anymore.
//!
//! Neither test can exercise a genuine jump-stack round trip: every actual
//! push onto `JumpStack` happens either via `gd`/`gr` (needs a live
//! language server, out of scope for this suite's LSP-free fixtures — see
//! `support::fixture`'s docs) or via `Ctrl-o`/`Ctrl-i`/`M-Left`/`M-Right`
//! themselves, which only *pop*, never push
//! (`ui::navigation::navigate_to`'s `record_history` path). So instead of a
//! hollow "press a key, assert nothing crashed" check, both tests press
//! `Ctrl-o` then the mode's jump-forward key against an empty stack and
//! assert the specific "no earlier/later position" status text — proof the
//! actual key bytes on the wire were parsed as `JumpBack`/`JumpForward` and
//! dispatched, not just that the process didn't crash.

use crate::support::screen::underlined_cells;
use crate::support::{Harness, Key, KittyMode, SpawnOptions, fixture};
use std::time::Duration;

/// Sends `key` and waits until the status bar shows no `"jump:"` note at
/// all — clearing whatever `Ctrl-o`/`Ctrl-i`/`M-Left`/`M-Right` status text
/// a previous step in the same test left behind (any *matched* action resets
/// the status line before its own handling runs — see
/// `ui::mod::event_loop`'s `StepResult::Matched` arm). Callers use this
/// right before pressing a key whose only observable effect is jump-status
/// text, so a subsequent `wait_for_text` proves that key produced the text,
/// rather than finding text some earlier step already left on screen.
fn clear_jump_status(h: &Harness, key: Key) {
    h.send(key);
    h.wait_until(Duration::from_secs(2), |s| !s.contents().contains("jump:"));
}

/// Asserts `key` never resolves to `JumpForward`/`JumpBack` (or anything
/// else that would touch the status bar's `"jump:"` note). Unlike a
/// positive `wait_for_text` check, there's no later event to wait on that
/// would prove the absence non-racily — every *matched* action resets the
/// status line before its own handling runs (see
/// `ui::mod::event_loop`'s `StepResult::Matched` arm), so pressing a second
/// key afterward to "force a render" would clear a wrongly-set status
/// either way and prove nothing. Follows the same sleep-then-check idiom
/// `tests/e2e/show_keys.rs::without_the_flag_no_key_chip_ever_appears` uses
/// for the identical class of claim ("this must never render"): give the
/// (wrongly bound) action every chance to show up, then assert it didn't.
fn assert_key_has_no_jump_binding(h: &Harness, key: Key) {
    h.send(key);
    std::thread::sleep(Duration::from_millis(200));
    let contents = h.screen_contents();
    assert!(
        !contents.contains("jump:"),
        "expected no default binding, but the status bar shows a jump note; screen:\n{contents}"
    );
}

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
    // neovim. The jump hint only appears in the *expanded* hint bar (the
    // collapsed default shows the minimal subset — see
    // `hints::diff_view_items`), so expand it first with `.`.
    h.wait_for_text("README.md");
    h.send(Key::Char('.'));
    h.wait_for_text("C-o/C-i");

    // `Ctrl-]`/`Ctrl-t` have no default binding at all now (#12 replaces
    // the old legacy-terminal `C-t` fallback with `M-Right`, and never
    // bound a tag-stack key) — check this first, on a completely untouched
    // status bar, so there's nothing else that could be mistaken for it.
    assert_key_has_no_jump_binding(&h, Key::CtrlT);

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

    // `M-Left`/`M-Right` are unconditional aliases in both ci-distinguishable
    // states (issue #12) — clear the status line first so the text these
    // produce next proves the alias itself fired, not stale text left over
    // by the C-o/C-i checks above (which read identically for an empty
    // stack either way).
    clear_jump_status(&h, Key::Char('j'));
    h.send(Key::AltLeft);
    h.wait_for_text("jump: no earlier position");

    clear_jump_status(&h, Key::Char('j'));
    h.send(Key::AltRight);
    h.wait_for_text("jump: no later position");

    // Tab now means `FocusNextPane`, not `NextSymbol` (issue #13) — this
    // single-pane diff view has nothing to focus-cycle, so Tab must be a
    // genuine no-op here: the active symbol's underline must not move.
    // There's no later event a `wait_for_text` could key off to prove a
    // *lack* of movement non-racily, so this uses the same
    // sleep-then-assert-unchanged idiom `assert_key_has_no_jump_binding`
    // above uses for the identical class of claim. The two
    // `clear_jump_status` calls above already moved the cursor two rows
    // down onto content (row 3), so there's a real active symbol here for
    // Tab to (not) move.
    h.wait_for_text("\u{b7} 3/");
    let before = h.with_screen(underlined_cells);
    h.send(Key::Tab);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        h.with_screen(underlined_cells),
        before,
        "Tab must focus a pane (a no-op in this single-pane diff view), \
         not move the active symbol"
    );

    // `l` is the vim preset's real `NextSymbol` binding now (issue #13) —
    // sending it must move the active symbol, proving the split landed on
    // the right key rather than just proving Tab no longer does it.
    h.send(Key::Char('l'));
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

    // The fallback hint: no kitty protocol, so jump-forward's canonical key
    // is `M-Right` (#12 replaces the old `C-t` fallback). Expanded-bar-only,
    // same as the supported case above.
    h.wait_for_text("README.md");
    h.send(Key::Char('.'));
    h.wait_for_text("C-o/M-Right");

    // `Ctrl-t` has no default binding in this mode either — checked first,
    // on an untouched status bar, same reasoning as `kitty_supported`.
    // `Ctrl-i` itself is checked further down, via the raw Tab byte the two
    // genuinely share on a legacy terminal (`Key::CtrlI` has no legacy
    // encoding at all — see its docs — so it can't be sent here directly).
    assert_key_has_no_jump_binding(&h, Key::CtrlT);

    h.send(Key::CtrlO);
    h.wait_for_text("jump: no earlier position");

    // `M-Right` (`\x1b[1;3C`) is the fallback's real, canonical binding —
    // must reach `JumpForward` when the kitty protocol never activated at
    // all, the same as it does when it did (`kitty_supported` above).
    clear_jump_status(&h, Key::Char('j'));
    h.send(Key::AltRight);
    h.wait_for_text("jump: no later position");

    // `M-Left` too, as `JumpBack`'s alias.
    clear_jump_status(&h, Key::Char('j'));
    h.send(Key::AltLeft);
    h.wait_for_text("jump: no earlier position");

    // The mirror image of `kitty_supported`'s Tab check: in this mode, raw
    // `0x09` is *only* ever a literal Tab (no `Ctrl-i` binding exists to
    // collide with it) — and, as of issue #13, that literal Tab means
    // `FocusNextPane`, a no-op in this single-pane diff view. Same
    // sleep-then-assert-unchanged idiom as `kitty_supported`'s Tab check
    // (and `assert_key_has_no_jump_binding` above) for the same "prove a
    // lack of movement" reason. The two `clear_jump_status` calls above
    // already moved the cursor two rows down onto content (row 3), so
    // there's nothing more to do here before the Tab press itself.
    h.wait_for_text("\u{b7} 3/");
    let before = h.with_screen(underlined_cells);
    h.send(Key::Tab);
    std::thread::sleep(Duration::from_millis(300));
    let after_tab = h.with_screen(underlined_cells);
    assert_eq!(
        after_tab, before,
        "Tab must focus a pane (a no-op in this single-pane diff view), \
         not move the active symbol"
    );
    assert!(
        !h.screen_contents().contains("jump: no later position"),
        "a literal Tab must never be mistaken for Ctrl-i when the kitty protocol isn't active"
    );

    // `l` is the vim preset's real `NextSymbol` binding now — sending it
    // must move the active symbol, unaffected by the kitty protocol not
    // being active (this binding has nothing to do with the Tab/Ctrl-i
    // byte collision the rest of this test guards against).
    h.send(Key::Char('l'));
    h.wait_until(Duration::from_secs(2), |s| {
        underlined_cells(s) != before && !underlined_cells(s).is_empty()
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
