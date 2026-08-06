//! M13: soft-wrapping long diff lines. `[ui] wrap = true` (the default)
//! must surface content a narrow pane would otherwise cut off, on a
//! continuation row marked with `↪`; `wrap = false` must restore the exact
//! truncate-at-the-pane-edge behavior every prior milestone had. See
//! `support::fixture::long_line_repo`'s docs for exactly how the fixture's
//! one long line is built and why `TAILMARKER` is the deciding witness.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn wrap_on_reveals_tail_content_a_narrow_pane_would_otherwise_truncate() {
    let repo = fixture::long_line_repo(true);
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("long.txt");
    h.wait_until(Duration::from_secs(3), |screen| {
        screen.contents().contains("TAILMARKER")
    });

    let contents = h.screen_contents();
    assert!(
        contents.contains("TAILMARKER"),
        "a wrapped continuation row should surface the line's tail; screen:\n{contents}"
    );
    assert!(
        contents.contains('\u{21aa}'),
        "a continuation row should carry the wrap marker glyph; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn wrap_off_restores_truncation_at_the_pane_edge() {
    let repo = fixture::long_line_repo(false);
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("long.txt");
    // The hunk's trailing context line only appears once the whole diff
    // pane (not just the sidebar entry `wait_for_text` above already
    // confirmed) has actually drawn a frame with this hunk's content.
    h.wait_for_text("three");

    let contents = h.screen_contents();
    assert!(
        !contents.contains("TAILMARKER"),
        "wrap = false must truncate the line before its tail; screen:\n{contents}"
    );
    assert!(
        !contents.contains('\u{21aa}'),
        "wrap = false must never draw a continuation row; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
