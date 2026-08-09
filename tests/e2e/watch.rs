//! The default working-tree watcher, end to end: a bare `ktmr` session must
//! notice an edit made after startup and render the new diff without a keypress.
//! This deliberately uses no CLI arguments, since that was the invocation
//! path that previously constructed a non-watching session.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn bare_session_refreshes_after_a_working_tree_edit() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    // This is content from the initial diff. Waiting for it proves the
    // session has rendered before the edit below, and therefore that the
    // marker is evidence of a refresh rather than startup output.
    h.wait_for_text("A brand new line five.");
    const MARKER: &str = "LIVE_REFRESH_UNIQUE_MARKER";
    assert!(!h.screen_contents().contains(MARKER));

    // Replace an already-visible added line rather than appending below the
    // hunk: the latter would land in a collapsed unchanged-context gap and
    // make a successful refresh indistinguishable from no refresh on screen.
    let updated_content = format!(
        "# Sample project\n\nThis is line two, updated.\nThis is line three.\nThis is line four.\nA brand new line five. {MARKER}\n"
    );
    std::fs::write(repo.path().join("README.md"), &updated_content)
        .expect("failed to edit watched fixture file");

    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains(MARKER)
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn no_watch_opt_out_hides_the_live_refresh_indicator() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff", "--no-watch"],
            ..Default::default()
        },
    );

    h.wait_for_text("A brand new line five.");
    let contents = h.screen_contents();
    assert!(
        !contents.contains("⦿ watch"),
        "--no-watch must omit the live refresh indicator; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
