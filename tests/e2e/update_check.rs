//! Update-notification display: gated entirely on a cached
//! `$XDG_STATE_HOME/katamari/update-check.json` (see `update`'s module
//! docs — never a live GitHub request), so this suite fabricates that cache
//! directly via `SpawnOptions::update_state_json` rather than faking a
//! network response. `last_checked` is always set to "just now" so
//! `update::on_startup` never decides the cache is stale and spawns its own
//! background refresh — this suite, like the rest of the E2E fixtures (see
//! `support::fixture`'s module docs), must never depend on real network
//! access to stay deterministic.
//!
//! The on-quit stderr line (`update::print_exit_notice`) isn't covered
//! here: this harness reads the child's pty, which mixes stdout and stderr
//! into one stream indistinguishably from what a real terminal would show —
//! see the milestone task's note that this needs manual verification
//! instead.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before the epoch")
        .as_secs()
}

fn fresh_cache_json(latest_version: &str) -> String {
    format!(
        r#"{{"last_checked":{},"latest_version":"{latest_version}"}}"#,
        now_unix()
    )
}

#[test]
fn a_newer_cached_version_shows_the_status_bar_notice() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            // Generous width: the notice is the longest status-bar note in
            // the app ("katamari vX.Y.Z is available (you have vA.B.C) —
            // <upgrade command or releases URL>") and must not be truncated
            // for this assertion to be meaningful.
            cols: 220,
            update_state_json: Some(fresh_cache_json("99.0.0")),
            ..SpawnOptions::default()
        },
    );

    h.wait_until(Duration::from_secs(3), |screen| {
        screen.contents().contains("99.0.0 is available")
    });
    let contents = h.screen_contents();
    assert!(
        contents.contains("99.0.0 is available"),
        "expected the cached 99.0.0 update to surface as a status-bar notice; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn update_check_false_suppresses_the_notice_even_with_a_newer_cached_version() {
    let repo = fixture::basic_repo();
    std::fs::create_dir_all(repo.path().join(".katamari")).unwrap();
    std::fs::write(
        repo.path().join(".katamari").join("config.toml"),
        "[update]\ncheck = false\n",
    )
    .unwrap();

    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 220,
            update_state_json: Some(fresh_cache_json("99.0.0")),
            ..SpawnOptions::default()
        },
    );

    // Nothing to `wait_for_text` on the absence of a note — wait for the
    // diff itself to have rendered (the harness's own `spawn` wait already
    // guarantees non-empty content, but the sidebar filename is a more
    // specific signal that a real frame, not just a blank screen, is up)
    // and then assert the notice never made it in.
    h.wait_for_text("README.md");
    let contents = h.screen_contents();
    assert!(
        !contents.contains("is available"),
        "[update] check = false must suppress the notice; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
