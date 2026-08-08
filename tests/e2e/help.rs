//! Issue #3's `?` help popup, end to end: opening it, live-filtering with
//! `/`, and closing it with the two-`Esc` sequence (clear the filter, then
//! close) — through the real compiled binary, the way
//! `tests/e2e/scope_menu.rs` covers its own popup. Everything about *which*
//! rows exist and how filtering matches them is already pinned down by
//! `ui::help`'s colocated unit tests; this only exercises what only shows
//! up once a real terminal and the real keymap resolver are involved: `?`
//! actually reaching `Action::OpenHelp`, the popup rendering over a live
//! `View::Diff` session, and the modal raw-key routing in `ui::mod`'s event
//! loop actually taking effect.

use crate::support::harness::DEFAULT_WAIT;
use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn open_filter_and_close_the_help_popup() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff"],
            ..Default::default()
        },
    );

    // Starts on the dirty working tree, same as a plain `ktmr diff`.
    h.wait_for_text("todo.txt");

    // `?` opens the popup — the unfiltered `Browse` view shows every group,
    // Navigation first, so `CursorDown`'s row is on screen right away.
    h.send(Key::Char('?'));
    h.wait_for_text("Move down one row");
    h.wait_for_text("Navigation");

    // `/` enters `Filter` mode; typing "definition" narrows the list down
    // to just `GotoDefinition`'s row (the only action whose description or
    // config name contains that substring). `GotoDefinition`'s own row
    // ("Go to definition") is a bad thing to `wait_for_text` on here — it's
    // already on screen in the *unfiltered* view above, so waiting for it
    // would race the filter actually taking effect rather than prove it
    // did. Waiting for `CursorDown`'s now-filtered-out row to vanish is the
    // real signal.
    h.send(Key::Char('/'));
    for c in "definition".chars() {
        h.send(Key::Char(c));
    }
    h.wait_until(DEFAULT_WAIT, |screen| {
        !screen.contents().contains("Move down one row")
    });
    assert!(
        h.screen_contents().contains("Go to definition"),
        "the surviving GotoDefinition row should still be visible; screen:\n{}",
        h.screen_contents()
    );

    // First `Esc`: back to `Browse` with the filter cleared — the full
    // list (including `CursorDown`'s row again) reappears.
    h.send(Key::Esc);
    h.wait_for_text("Move down one row");

    // Second `Esc`: closes the popup outright. Its footer text is the
    // stable "is the popup still open" signal — wait for it to vanish
    // rather than racing a fixed sleep against it (mirroring
    // `scope_menu.rs`'s own close-and-wait-for-absence pattern).
    h.send(Key::Esc);
    h.wait_until(DEFAULT_WAIT, |screen| {
        !screen.contents().contains("j/k scroll")
    });

    // `q` still quits the session cleanly once the popup is gone — the
    // resolver is back to seeing ordinary keys again, not the popup's own
    // raw-key bypass.
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn question_mark_closes_the_popup_in_browse_mode() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff"],
            ..Default::default()
        },
    );
    h.wait_for_text("todo.txt");

    h.send(Key::Char('?'));
    h.wait_for_text("j/k scroll");

    // `?` is also a close key in `Browse` mode (alongside `q`/`Esc`) — a
    // reviewer who reaches for the same key that opened the popup to close
    // it again shouldn't be met with anything else happening instead.
    h.send(Key::Char('?'));
    h.wait_until(DEFAULT_WAIT, |screen| {
        !screen.contents().contains("j/k scroll")
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
