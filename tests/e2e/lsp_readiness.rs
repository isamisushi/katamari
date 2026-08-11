//! Issue #11's responsive-LSP-readiness E2E coverage: against a real
//! `ktmr` process talking to a real (if fake — see
//! `support::fixture::lsp_readiness_repo`/`support::fake_lsp_server`)
//! language server whose `initialize` response is deliberately delayed,
//! prove (a) the diff renders and ordinary interaction — movement, help,
//! quit — stays responsive throughout that delay, and (b) a `gd` pressed
//! during it reports "not ready" immediately and never turns into a late
//! jump once the server does come up, while a retry after `Ready` actually
//! dispatches. `tests/e2e/navigation.rs`/`help.rs` cover `j`/`k`/`?` on
//! their own already; this only needs enough of the same to show none of
//! it is gated on LSP readiness, which no purely in-process `App`/`View`
//! test can prove — the whole point is a real child process, a real event
//! loop, and real wall-clock delay in the picture.
//!
//! A wide terminal (`cols: 160`, versus the suite's usual 100) is
//! deliberate: this file's status-bar messages are the longest in the
//! suite ("LSP: stubls is starting; go to definition is not ready yet"),
//! and `ui::status_bar::render`'s one status line doesn't wrap — a
//! terminal too narrow for the whole line would clip exactly the trailing
//! words a `wait_for_text` call here needs to see, which would read as a
//! feature regression rather than the fixture's own width choice.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn movement_help_and_quit_respond_before_the_server_initializes() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH (fake LSP server fixture needs it)");
        return;
    }
    // A generous delay: long enough that every interaction below —
    // several key round trips plus quitting — has finished well inside
    // it purely because none of it waits on LSP at all, not because the
    // timing happened to work out.
    let repo = fixture::lsp_readiness_repo(5.0, 0.0);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("main.stub");
    h.wait_for_text("\u{b7} 1/");

    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 2/");
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");
    h.send(Key::Char('k'));
    h.wait_for_text("\u{b7} 2/");

    // `?` opens and closes the help popup — same witness text
    // `help.rs::question_mark_closes_the_popup_in_browse_mode` uses.
    h.send(Key::Char('?'));
    h.wait_for_text("j/k scroll");
    h.send(Key::Char('?'));
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("j/k scroll")
    });

    // `q` exits well before the fake server would ever answer
    // `initialize` — if quitting waited on LSP shutdown/startup in any
    // way, this would time out long before the 5s delay above elapses.
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(3));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// Issue #12's Esc-from-a-definition-opened-`FileView` PTY coverage. Chose
/// the fake-LSP fixture over the git-only `LogView`-nested-`Diff` fallback
/// the issue's plan also allowed: `ui::mod::cancel_diff_view` pops a pushed
/// `File`/`Diff`/`Timeline`/`Log` through the exact same branch, so a
/// nested-`Diff` test (see `tests/e2e/log.rs`) already covers that code
/// path — but a real `FileView` push only ever happens through
/// `ui::navigation::navigate_to`'s "push a fresh view" branch, which a
/// nested-diff jump never reaches. This is the one test in the suite that
/// exercises it end to end.
#[test]
fn esc_pops_a_definition_opened_file_view_back_to_the_diff() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH (fake LSP server fixture needs it)");
        return;
    }
    let repo = fixture::lsp_readiness_repo_with_definition_target(0.0, 0.0);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 1/");

    // Land on `main.stub`'s added row — a valid go-to-definition target.
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 2/");
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");

    // `gd` — the fake server (configured with a definition target) points
    // at `other.stub`, a file that's part of this repo but not part of
    // this diff, so `navigate_to` must push a fresh `FileView` rather than
    // moving the cursor within the diff already on screen.
    //
    // Even with both delays at `0.0`, the fake server's `initialize`
    // handshake still takes real wall-clock time (process spawn, a stdio
    // round trip) — this is issue #11's readiness gate working correctly,
    // not a bug, so retry `gd` for a bit rather than asserting the very
    // first press wins that race.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        h.send(Key::Char('g'));
        h.send(Key::Char('d'));
        std::thread::sleep(Duration::from_millis(100));
        if h.screen_contents().contains("other.stub") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "gd never reached a Ready server within 5s; screen:\n{}",
            h.screen_contents()
        );
    }
    h.wait_for_text("target line one");

    // `Esc` pops exactly the pushed `FileView`, landing back on the diff
    // being reviewed — not quitting, not clearing anything else. The root
    // diff's own cursor was never touched while the `FileView` was on top,
    // so it's still on the same row it left from.
    h.send(Key::Esc);
    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 3/");
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("target line one")
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn a_not_ready_definition_press_never_fires_late_and_a_retry_after_ready_dispatches() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH (fake LSP server fixture needs it)");
        return;
    }
    let repo = fixture::lsp_readiness_repo(2.0, 1.2);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 1/");

    // Movement is responsive well before the fake server's 2s
    // `initialize` delay elapses, landing on `main.stub`'s first
    // `Context` row ("alpha") — a valid hover/goto target.
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 2/");
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");
    h.send(Key::Char('k'));
    h.wait_for_text("\u{b7} 2/");
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");

    // `gd`, pressed while the server is still starting, must explain
    // itself immediately and create no pending request — not the
    // generic "goto: …" in-flight ellipsis a dispatched request shows.
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("is starting");
    h.wait_for_text("not ready yet");
    assert!(
        !h.screen_contents().contains("goto: \u{2026}"),
        "a not-ready press must not be overwritten by the generic in-flight \
         ellipsis; screen:\n{}",
        h.screen_contents()
    );

    // Wait past the fake server's `initialize` delay without touching
    // the keyboard at all. If the pre-ready `gd` above had been queued
    // and fired once the server came up — the exact bug issue #11 exists
    // to rule out — the view would have navigated away from `main.stub`
    // by now. There's nowhere else in this one-file repo it could have
    // landed but the same file at the same row, so still seeing that is
    // the proof nothing fired.
    std::thread::sleep(Duration::from_millis(2_700));
    let after_wait = h.screen_contents();
    assert!(
        after_wait.contains("\u{b7} 3/"),
        "cursor must not have moved on its own once the server became \
         ready; screen:\n{after_wait}"
    );
    assert!(
        after_wait.contains("main.stub"),
        "must still be showing main.stub, never navigated elsewhere; \
         screen:\n{after_wait}"
    );

    // Retrying now that the server is `Ready` actually dispatches: a
    // fresh in-flight indicator appears, distinct from the earlier
    // not-ready message.
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("goto: \u{2026}");

    // Movement stays responsive while that dispatched request is itself
    // deliberately slow to answer (`definition_delay_secs`).
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 4/");
    h.send(Key::Char('k'));
    h.wait_for_text("\u{b7} 3/");

    // Once the fake server finally answers (with no location — see its
    // docs), the definition action resolves normally, same as it always
    // has for a `Ready` server with nothing to offer.
    h.wait_for_text("goto: no definition found");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// `Action::Hover`'s (`K`) own proof of issue #11's readiness gate —
/// `check_action_readiness` covers `Hover`/`GotoDefinition`/`FindReferences`
/// identically, and `gd`'s side of that is already proven above by
/// `a_not_ready_definition_press_never_fires_late_and_a_retry_after_ready_dispatches`;
/// this mirrors it for `K`, plus the one behavior specific to `Hover`: its
/// not-ready message routes through `goto_status`, not `hover_state`'s own
/// `set_message`, specifically so `HoverState::status_hint`'s `"hover: "`
/// prefix never doubles up into `"hover: LSP: …"` — the exact bug the
/// introducing commit's message calls out by name.
#[test]
fn a_not_ready_hover_press_never_fires_late_and_a_retry_after_ready_shows_the_popup() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH (fake LSP server fixture needs it)");
        return;
    }
    let repo = fixture::lsp_readiness_repo(2.0, 0.0);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 1/");

    // Land on `main.stub`'s `GOTO_TARGET_TOKEN` add row — a valid hover
    // target, same movement the `gd` tests above use.
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 2/");
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");

    // `K`, pressed while the server is still starting, must explain itself
    // immediately and open no popup — not even the pending state a
    // dispatched request would show.
    h.send(Key::Char('K'));
    h.wait_for_text("LSP: stubls is starting; hover is not ready yet");
    let not_ready_screen = h.screen_contents();
    assert!(
        !not_ready_screen.contains("HOVER_INFO_UNIQUE"),
        "a not-ready press must never open the hover popup; screen:\n{not_ready_screen}"
    );
    assert!(
        !not_ready_screen.contains("hover: LSP:"),
        "the not-ready message must reach the screen through goto_status \
         verbatim, never re-wrapped in hover_state's own \"hover: \" \
         prefix; screen:\n{not_ready_screen}"
    );

    // Wait past the fake server's `initialize` delay without touching the
    // keyboard at all. If the pre-ready `K` above had been queued and fired
    // once the server came up — the exact bug issue #11 exists to rule out
    // — a hover popup would now be open with nobody at the keyboard.
    std::thread::sleep(Duration::from_millis(2_700));
    let after_wait = h.screen_contents();
    assert!(
        !after_wait.contains("HOVER_INFO_UNIQUE"),
        "no popup must appear on its own once the server becomes ready; \
         screen:\n{after_wait}"
    );
    assert!(
        after_wait.contains("\u{b7} 3/") && after_wait.contains("main.stub"),
        "cursor must not have moved on its own; screen:\n{after_wait}"
    );

    // Retrying now that the server is `Ready` actually dispatches. The fake
    // server answers `textDocument/hover` immediately regardless of either
    // configured delay (see `fake_lsp_server.py`'s module docs), so — unlike
    // `gd`'s retry above — no further wait past the dispatch is needed.
    h.send(Key::Char('K'));
    h.wait_for_text("HOVER_INFO_UNIQUE");

    // `q` quits straight through the open hover popup — `run`'s resolver
    // intercepts `Action::Quit` unconditionally ahead of `handle_action`
    // (see its own doc comment: "an open hover/references/scope-menu
    // overlay, all included"), so no `Esc` is needed first.
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// `Action::FindReferences`'s (`g r`) own proof of issue #11's readiness
/// gate — the third action `check_action_readiness` covers, alongside
/// `Hover`/`GotoDefinition` above, and otherwise entirely uncovered: no
/// test in the suite presses `g r` or references `Action::FindReferences`
/// at all before this one. Mirrors
/// `a_not_ready_definition_press_never_fires_late_and_a_retry_after_ready_dispatches`'s
/// shape, with two deliberate departures from what that test (and this
/// one's own initial draft) would suggest:
///
/// - The retry doesn't chase the transient `"references: …"` in-flight
///   text. `fake_lsp_server.py` has no delay knob for
///   `textDocument/references` (only `textDocument/definition` sleeps
///   `definition_delay_secs`), so unlike `gd`'s 1.2s-delayed retry there is
///   no non-racy window in which that text is guaranteed to still be on
///   screen when checked.
/// - The retry's resolved text is *not* `"references: none found"`.
///   `fake_lsp_server.py`'s `initialize` response only declares
///   `definitionProvider`/`hoverProvider` (see that script's module docs),
///   never `referencesProvider` — and `LspManager`'s dispatch worker
///   (`src/lsp/manager.rs`'s `Op::References` arm) checks
///   `client.supports_references()` and refuses locally with
///   `LspError::Io("server does not advertise textDocument/references
///   support")` *before* ever writing a `textDocument/references` request
///   to the child process at all, so the generic id-present fallback this
///   test originally assumed it would hit is never reached. That refusal
///   is still a genuine dispatch through the real async channel (unlike
///   the pre-ready refusal, it only ever happens once `check_action_readiness`
///   reports `Ready`), so it's exactly as good a "retry after ready really
///   dispatches" witness as a clean result would have been.
#[test]
fn a_not_ready_find_references_press_never_fires_late_and_a_retry_after_ready_resolves() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH (fake LSP server fixture needs it)");
        return;
    }
    let repo = fixture::lsp_readiness_repo(2.0, 0.0);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 1/");

    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 2/");
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");

    // `g r`, pressed while the server is still starting, must explain
    // itself immediately and create no pending request — not the generic
    // "references: …" in-flight ellipsis a dispatched request shows.
    h.send(Key::Char('g'));
    h.send(Key::Char('r'));
    h.wait_for_text("LSP: stubls is starting; find references is not ready yet");
    assert!(
        !h.screen_contents().contains("references: \u{2026}"),
        "a not-ready press must not be overwritten by the generic in-flight \
         ellipsis; screen:\n{}",
        h.screen_contents()
    );

    // Wait past the fake server's `initialize` delay without touching the
    // keyboard at all. If the pre-ready `g r` above had been queued and
    // fired once the server came up, a references result would now be on
    // screen with nobody at the keyboard.
    std::thread::sleep(Duration::from_millis(2_700));
    let after_wait = h.screen_contents();
    assert!(
        !after_wait.contains("references: \u{2026}") && !after_wait.contains("does not advertise"),
        "no references result must appear on its own once the server \
         becomes ready; screen:\n{after_wait}"
    );
    assert!(
        after_wait.contains("\u{b7} 3/") && after_wait.contains("main.stub"),
        "cursor must not have moved on its own; screen:\n{after_wait}"
    );

    // Retrying now that the server is `Ready` actually dispatches: this
    // fixture's fake server never advertises `referencesProvider`, so
    // `LspManager` refuses locally with this exact message once (and only
    // once) it's actually reached the real, `Ready`-gated dispatch path —
    // see this test's own doc comment above for why that's still the right
    // "retry after ready dispatches" proof here.
    h.send(Key::Char('g'));
    h.send(Key::Char('r'));
    h.wait_for_text(
        "references: lsp transport io error: server does not advertise \
         textDocument/references support",
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// Issue #12's `Ctrl-o`/`Ctrl-i` jump history, proven against a real,
/// LSP-populated entry — `kitty.rs`'s own `Ctrl-o`/`Ctrl-i` coverage only
/// ever exercises the empty-stack "no earlier/later position" case, and
/// `focus.rs`/`mouse.rs` only prove `Ctrl-o`'s back leg from a file-tree- or
/// click-sourced jump, never `Ctrl-i` forward and never a `gd`-pushed entry.
/// Builds on `esc_pops_a_definition_opened_file_view_back_to_the_diff`
/// above (same fixture, same gd-retry-loop reasoning) but swaps its `Esc`
/// for `Ctrl-o`, to prove the pop is a genuine jump-history back leg —
/// `App::row_for_target`'s history-vs-fresh nearest-line-tolerance selector
/// (`navigate_to`'s `record_history` argument) only ever runs through this
/// path, never `Esc`'s plain view-stack pop — then adds `Ctrl-i` to prove
/// the forward leg lands back in the same pushed `FileView`, and finally
/// `q` directly from on top of that `FileView`, the one variant
/// `log.rs`'s quit-from-everywhere coverage doesn't reach (its own tests
/// only pop a *nested Diff* with `Esc` before quitting).
#[test]
fn ctrl_o_and_ctrl_i_round_trip_a_real_definition_jump_and_q_quits_from_the_pushed_file_view() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH (fake LSP server fixture needs it)");
        return;
    }
    let repo = fixture::lsp_readiness_repo_with_definition_target(0.0, 0.0);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 1/");

    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 2/");
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");

    // Same real-handshake-takes-real-time reasoning as
    // `esc_pops_a_definition_opened_file_view_back_to_the_diff` above: retry
    // `gd` rather than assume the very first press wins the readiness race.
    // Unlike that test's own loop, each attempt here waits to *confirm*
    // whether the server answered "not ready" (safe to retry) or actually
    // dispatched (`"other.stub"` on screen, meaning the `FileView` landed)
    // before ever sending another `g d` — this test cares about
    // `JumpStack`'s exact contents, not just what ends up on screen, so an
    // extra, unconfirmed retry landing *after* the first one already
    // resolved would fire a second go-to-definition from inside the freshly
    // pushed `FileView` itself and push a spurious second entry ahead of
    // the one this test means to pop back through with `Ctrl-o`.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        h.send(Key::Char('g'));
        h.send(Key::Char('d'));
        h.wait_until(Duration::from_secs(2), |screen| {
            let text = screen.contents();
            text.contains("other.stub") || text.contains("not ready yet")
        });
        if h.screen_contents().contains("other.stub") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "gd never reached a Ready server within 5s; screen:\n{}",
            h.screen_contents()
        );
    }
    h.wait_for_text("target line one");

    // `Ctrl-o` pops the pushed `FileView` back to the root diff — on screen
    // this looks identical to `Esc`'s pop, but only `Ctrl-o` gets there via
    // `JumpStack::back`/`navigate_to(..., record_history: false)`.
    h.send(Key::CtrlO);
    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 3/");
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("target line one")
    });

    // `Ctrl-i` returns forward to exactly the `FileView` `Ctrl-o` just
    // popped — proof `JumpStack::forward` actually holds the entry `back`
    // pushed onto it, not just that pressing it changes something.
    h.send(Key::CtrlI);
    h.wait_for_text("target line one");
    h.wait_for_text("other.stub");

    // `q` from directly on top of the pushed `FileView` — `run`'s resolver
    // intercepts `Action::Quit` unconditionally ahead of `handle_action`, on
    // whatever view is on top, so this needs no `Esc` first (unlike
    // `esc_pops_a_definition_opened_file_view_back_to_the_diff` above,
    // which pops to the diff before quitting).
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// Issue #13/#14's async-response-vs-focus race (the catch-all `other =>`
/// arm in `handle_action`): a `gd`/`gr` response that resolves after the
/// reviewer has already tabbed off `Diff` to `Files` must be dropped
/// silently — never navigate, never push a `FileView`, never yank focus
/// back to `Diff` and clobber wherever the sidebar had scrolled to.
/// `focus.rs`'s fixture has no configured language, so this race is
/// structurally unreachable there (`gd` can never actually dispatch); this
/// is the one place in the suite with both a real delayed LSP response and
/// a real `Tab` press to race it against.
#[test]
fn tabbing_off_diff_while_a_definition_response_is_in_flight_drops_it_silently() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH (fake LSP server fixture needs it)");
        return;
    }
    // `definition_target: true` (via `_with_definition_target`) so a stale
    // apply would have something concrete to wrongly show — a real
    // `Location` in a real sibling file, not just a silent `None` a bug
    // could hide behind.
    let repo = fixture::lsp_readiness_repo_with_definition_target(0.0, 2.0);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 1/");

    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 2/");
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");

    // Retry `gd` until it actually dispatches — `"goto: …"` is the
    // in-flight indicator a dispatched request shows, distinct from the
    // "not ready" message a press before the handshake completes would
    // show. Same real-handshake-takes-real-time reasoning as
    // `esc_pops_a_definition_opened_file_view_back_to_the_diff` above.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        h.send(Key::Char('g'));
        h.send(Key::Char('d'));
        std::thread::sleep(Duration::from_millis(100));
        if h.screen_contents().contains("goto: \u{2026}") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "gd never reached a Ready server within 5s; screen:\n{}",
            h.screen_contents()
        );
    }

    // Tab off the diff pane immediately — the response is still a full
    // `definition_delay_secs: 2.0` away, comfortably longer than the sleep
    // below needs to wait past.
    h.send(Key::Tab);

    // Wait past the definition delay without touching the keyboard again.
    // If the stale response had applied anyway, it would have pushed
    // `other.stub`'s `FileView` on top, showing "target line one".
    std::thread::sleep(Duration::from_millis(2_700));
    let after_wait = h.screen_contents();
    assert!(
        !after_wait.contains("target line one") && !after_wait.contains("other.stub"),
        "a stale gd response arriving after Tab must never push a \
         FileView; screen:\n{after_wait}"
    );
    assert!(
        after_wait.contains("\u{b7} 3/") && after_wait.contains("main.stub"),
        "the diff cursor must never move on its own; screen:\n{after_wait}"
    );

    // Focus is genuinely still on `Files`, not silently kicked back to
    // `Diff` by the dropped response's apply path — `gd` from `Files`
    // focus always reports the files-focus gate message (checked ahead of
    // `handle_action`'s main dispatch, regardless of any pending request).
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("definition: focus the diff pane first");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
