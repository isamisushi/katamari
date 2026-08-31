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

use crate::support::harness::SPLASH_MARKER;
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

    // Tab now means `FocusNextPane`, not `NextSymbol` (issue #13) — and as
    // of issue #14 the root diff view is no longer single-pane, so Tab
    // really does move focus to the files pane rather than being a no-op.
    // Proven the same way `tests/e2e/focus.rs` proves it (no PTY test here
    // inspects cell color): `gd` only ever reports the files-focus-gate
    // note while `Files` owns focus. The active symbol's underline must
    // still not move — focusing a different pane is not the same as
    // cycling the symbol within this one. The two `clear_jump_status`
    // calls above already moved the cursor two rows down onto content
    // (row 3), so there's a real active symbol here to prove didn't move.
    h.wait_for_text("\u{b7} 3/");
    let before = h.with_screen(underlined_cells);
    h.send(Key::Tab);
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("definition: focus the diff pane first");
    assert_eq!(
        h.with_screen(underlined_cells),
        before,
        "focusing the files pane must not move the diff's active symbol"
    );

    // Tab again cycles back to `Diff` (the root view's only other pane).
    // `l` is the vim preset's real `NextSymbol` binding (issue #13) —
    // sending it must move the active symbol, proving focus genuinely
    // landed back on the diff pane rather than merely that Tab was
    // pressed a second time.
    h.send(Key::Tab);
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
    // `FocusNextPane`, which (issue #14) now really does move focus to the
    // files pane rather than being a no-op — proven the same
    // files-focus-gate way `kitty_supported` proves it. The active
    // symbol's underline must not move, and a literal Tab must still
    // never be mistaken for `Ctrl-i`/`JumpForward` (the actual byte
    // collision this test exists to guard against). The two
    // `clear_jump_status` calls above already moved the cursor two rows
    // down onto content (row 3), so there's a real active symbol here to
    // prove didn't move.
    h.wait_for_text("\u{b7} 3/");
    let before = h.with_screen(underlined_cells);
    h.send(Key::Tab);
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("definition: focus the diff pane first");
    assert_eq!(
        h.with_screen(underlined_cells),
        before,
        "focusing the files pane must not move the diff's active symbol"
    );
    assert!(
        !h.screen_contents().contains("jump: no later position"),
        "a literal Tab must never be mistaken for Ctrl-i when the kitty protocol isn't active"
    );

    // Tab again cycles back to `Diff`. `l` is the vim preset's real
    // `NextSymbol` binding — sending it must move the active symbol,
    // unaffected by the kitty protocol not being active (this binding has
    // nothing to do with the Tab/Ctrl-i byte collision above), and proving
    // focus genuinely landed back on the diff pane rather than merely that
    // Tab was pressed a second time.
    h.send(Key::Tab);
    h.send(Key::Char('l'));
    h.wait_until(Duration::from_secs(2), |s| {
        underlined_cells(s) != before && !underlined_cells(s).is_empty()
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

// ---- startup splash vs. a slow-to-answer kitty probe ----------------------

/// The bug this pair of `ui::mod` functions exists to fix, reproduced for
/// real: `enable_kitty_keyboard_protocol`'s probe is a synchronous read that
/// crossterm only bounds at 2s, and on a terminal that never answers, `ktmr`
/// used to sit there with nothing drawn since `init_terminal` entered the
/// alternate screen — indistinguishable from a hang. `probe_reply_delay`
/// (see [`SpawnOptions::probe_reply_delay`]) reproduces that stall on
/// demand, without actually waiting out crossterm's full 2s timeout to
/// prove it: the reply still arrives, just held back long enough (well
/// under crossterm's own bound) for a test to observe the screen mid-wait.
///
/// Uses [`Harness::spawn_without_ready_wait`] rather than the ordinary
/// `Harness::spawn`, since `spawn`'s readiness wait deliberately skips past
/// any frame containing [`SPLASH_MARKER`] — exactly the frame this test
/// needs to catch.
#[test]
fn splash_is_visible_while_the_kitty_probe_is_still_pending() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn_without_ready_wait(
        repo.path(),
        SpawnOptions {
            probe_reply_delay: Some(Duration::from_millis(500)),
            ..Default::default()
        },
    );

    // Caught well inside the 500ms the probe reply is being held back —
    // `ktmr` is blocked inside `supports_keyboard_enhancement`'s
    // synchronous read for that whole window, so the real diff view can't
    // have painted yet. Confirms this is genuinely the splash standing in
    // for a still-black screen, not the two racing onto the same frame.
    h.wait_for_text(SPLASH_MARKER);
    assert!(
        !h.screen_contents().contains("README.md"),
        "the real diff view must not have rendered yet while the kitty \
         probe reply is still being withheld"
    );

    // Once the (delayed) reply lands, startup finishes and the splash is
    // replaced by the real UI, same as every other test in this suite.
    h.wait_for_text("README.md");
    assert!(
        !h.screen_contents().contains(SPLASH_MARKER),
        "the splash must not linger once the real UI has painted"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

// ---- issue #13's emacs preset: M-f/M-b over real wire bytes ---------------

/// Issue #13's emacs preset binds `M-f`/`M-b` to `NextSymbol`/`PrevSymbol`
/// (real emacs `forward-word`/`backward-word`, repurposed here for the
/// active symbol — see `keymap::emacs_preset`'s own doc comment for why).
/// `keymap::tests::emacs_meta_f_and_meta_b_resolve_to_next_and_prev_symbol`
/// already proves the binding table entry is correct against a hand-built
/// `KeyEvent`; this proves the same claim through real bytes crossterm has
/// to decode — a bare `ESC` immediately followed by `f`/`b` under the
/// legacy fallback, or the kitty CSI-u modified form once the protocol has
/// negotiated (see `Key::AltChar`'s docs) — two genuinely different parsing
/// paths through crossterm, so this runs once per `KittyMode` rather than
/// trusting one to stand in for the other, the same reasoning
/// `kitty_supported`/`kitty_unsupported` above already follow for
/// `M-Left`/`M-Right`.
fn emacs_meta_f_and_meta_b_move_the_active_symbol(kitty_mode: KittyMode) {
    let repo = fixture::basic_repo();
    // Opt into the emacs preset the same way `tests/e2e/doctor.rs`/
    // `mouse.rs`/`update_check.rs` already write ad hoc `.katamari/config.toml`
    // fixtures: directly, rather than adding a new `fixture::` constructor
    // just one test in this file needs. Committed immediately (unlike those
    // other tests, which never need a deterministic bottom row) so it
    // doesn't itself show up as a fourth working-tree diff entry ahead of
    // `basic_repo`'s own three — `Action::Bottom` below needs the diff's
    // real last row to land on `todo.txt`'s pre-existing content exactly as
    // `basic_repo` wrote it, not shift onto this config write instead.
    std::fs::create_dir_all(repo.path().join(".katamari")).unwrap();
    std::fs::write(
        repo.path().join(".katamari").join("config.toml"),
        "keymap = \"emacs\"\n",
    )
    .unwrap();
    fixture::git(repo.path(), &["add", ".katamari/config.toml"]);
    fixture::git(repo.path(), &["commit", "-q", "-m", "enable emacs keymap"]);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            kitty_mode,
            ..Default::default()
        },
    );
    h.wait_for_text("README.md");

    // The status bar's own "· {cursor+1}/{total}" position indicator
    // (`ui::status_bar`) is the only reliable witness that `Action::Bottom`
    // actually fired below: `basic_repo`'s whole diff is short enough to
    // render in full on this harness's 30-row terminal regardless of
    // cursor position, so waiting for any particular line's *text* to
    // appear would prove nothing about whether the cursor moved at all.
    const POSITION_MARKER: &str = "\u{b7} 1/";
    // `wait_for_text("README.md")` above only proves the diff pane's own
    // content painted, not that the (separately positioned) status bar's
    // own frame landed alongside it — wait for the exact substring this
    // parses below, rather than assuming it rode in on the same frame.
    h.wait_for_text(POSITION_MARKER);
    let initial = h.screen_contents();
    let digits_start = initial
        .find(POSITION_MARKER)
        .map(|i| i + POSITION_MARKER.len())
        .expect("initial cursor position must render as \"· 1/N\"");
    let total: usize = initial[digits_start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("row count after \"· 1/\" must be a plain integer");

    // `M->` is this preset's own `Action::Bottom` (real emacs
    // `end-of-buffer`) — lands the cursor on the diff's last row,
    // `todo.txt`'s own single `Add` line ("- write more fixtures"), a real
    // content row with several word-like symbols to cycle through, and
    // (per `App::update`'s "cursor moved" tail) resets `active_symbol` to 0
    // there. Reached this way rather than vim's `j`/`G`, which this preset
    // doesn't bind at all.
    h.send(Key::AltChar('>'));
    h.wait_for_text(&format!("\u{b7} {total}/{total}"));
    let before = h.with_screen(underlined_cells);
    assert!(
        !before.is_empty(),
        "the bottom row must have a real active symbol to cycle from; screen:\n{}",
        h.screen_contents()
    );

    // The actual payoff: real Alt-f bytes on the wire must reach
    // `Action::NextSymbol`, moving the underline off `active_symbol == 0`.
    h.send(Key::AltChar('f'));
    h.wait_until(Duration::from_secs(2), |s| {
        underlined_cells(s) != before && !underlined_cells(s).is_empty()
    });
    let after_forward = h.with_screen(underlined_cells);
    assert_ne!(
        after_forward, before,
        "M-f must have actually moved the active symbol"
    );

    // And real Alt-b bytes must reach `Action::PrevSymbol`, moving it back
    // to exactly where it started — proof this is real cycling, not some
    // other action that happened to change the screen.
    h.send(Key::AltChar('b'));
    h.wait_until(Duration::from_secs(2), |s| underlined_cells(s) == before);

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn emacs_meta_f_and_meta_b_move_the_active_symbol_over_kitty_csi_u_bytes() {
    emacs_meta_f_and_meta_b_move_the_active_symbol(KittyMode::Supported);
}

#[test]
fn emacs_meta_f_and_meta_b_move_the_active_symbol_over_legacy_escape_bytes() {
    emacs_meta_f_and_meta_b_move_the_active_symbol(KittyMode::Unsupported);
}
