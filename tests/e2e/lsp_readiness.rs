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
