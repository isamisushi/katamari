//! The agent-workspace exclusion end to end (`crate::vcs::git::DEFAULT_AGENT_WORKSPACE_PREFIXES`):
//! untracked content under a pruned, un-gitignored `.claude/worktrees/<x>/`
//! never reaches the diff or the live-refresh watcher by default, the
//! startup status bar names the count and the prefix when something was
//! actually hidden, `[diff] agent_workspaces = false` disables all of it,
//! and — untracked-only filtering, the one hard boundary this feature must
//! never cross — a *committed* file under the same prefix always reviews
//! normally either way. See `fixture::agent_workspace_repo`'s own docs for
//! the exact fixture shape shared by every test below.

use crate::support::harness::DEFAULT_WAIT;
use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

/// The default-on case: `regenerated.txt` (untracked, under the prefix)
/// never renders, `tracked.txt`'s edit (outside the prefix) and
/// `committed.txt`'s edit (tracked, *inside* the prefix — the untracked-only
/// boundary) both do, and the one-time startup note names both the count
/// (exactly one file was hidden — `committed.txt`'s tracked edit is never
/// counted, since `untracked_files` never sees it in the first place) and
/// the matched prefix.
#[test]
fn agent_workspace_junk_is_hidden_with_a_startup_note() {
    let repo = fixture::agent_workspace_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            // Generous width so the status-bar note isn't truncated before
            // this test can assert on its full text (see
            // `tests/e2e/update_check.rs`'s identical reasoning for its own
            // status-bar notice assertion).
            cols: 220,
            ..SpawnOptions::default()
        },
    );

    h.wait_for_text("alpha, updated");
    let screen = h.screen_contents();

    assert!(
        !screen.contains("regenerated"),
        "untracked content under an agent-workspace prefix must never render: {screen}"
    );
    assert!(
        screen.contains("alpha, updated"),
        "an ordinary tracked edit outside the prefix must still render: {screen}"
    );
    assert!(
        screen.contains("committed.txt"),
        "a committed file's edit *inside* the prefix must still render — \
         filtering is untracked-only: {screen}"
    );
    assert!(
        screen.contains("agent workspace")
            && screen.contains("1 untracked file")
            && screen.contains(".claude/worktrees"),
        "the startup note must name both the count and the matched prefix: {screen}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(DEFAULT_WAIT);
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// `[diff] agent_workspaces = false` disables the whole feature: the same
/// untracked junk that stayed hidden above now renders, and — since nothing
/// was excluded — the startup note never appears at all.
#[test]
fn agent_workspaces_false_shows_the_content_and_suppresses_the_note() {
    let repo = fixture::agent_workspace_repo();
    std::fs::create_dir_all(repo.path().join(".katamari")).unwrap();
    std::fs::write(
        repo.path().join(".katamari").join("config.toml"),
        "[diff]\nagent_workspaces = false\n",
    )
    .unwrap();

    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 220,
            ..SpawnOptions::default()
        },
    );

    h.wait_until(Duration::from_secs(5), |screen| {
        screen.contents().contains("regenerated")
    });
    let screen = h.screen_contents();

    assert!(
        screen.contains("regenerated"),
        "agent_workspaces = false must let the untracked junk render: {screen}"
    );
    assert!(
        !screen.contains("agent workspace"),
        "nothing was excluded, so the startup note must not appear: {screen}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(DEFAULT_WAIT);
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// The watcher side of the same exclusion, `tests/e2e/watch_filtering.rs`'s
/// own `gitignored_directories_are_never_watched` idiom: an edit to the
/// ordinary tracked file outside the prefix is the positive control,
/// measuring how long a real refresh actually takes in this session before
/// trusting a bounded wait's absence as "correctly never watched" rather
/// than "hasn't had time yet." Both the *initial* registration walk (this
/// directory already existed at spawn time, so only that walk ever had a
/// chance to prune it — dynamic re-registration never runs on a directory
/// that was never new) and the always-on event-time filter are exercised
/// together here: a write into the already-present `regenerated.txt` must
/// never trigger a refresh through either layer.
#[test]
fn churn_under_an_agent_workspace_prefix_never_triggers_a_refresh() {
    let repo = fixture::agent_workspace_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("alpha, updated");

    const TRACKED_MARKER: &str = "AGENT_WORKSPACE_TRACKED_MARKER";
    let refresh_started = std::time::Instant::now();
    std::fs::write(
        repo.path().join("tracked.txt"),
        format!("alpha, updated {TRACKED_MARKER}\nbeta\ngamma\n"),
    )
    .expect("failed to edit the fixture's tracked file");
    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains(TRACKED_MARKER)
    });
    let measured_refresh_latency = refresh_started.elapsed();

    const JUNK_MARKER: &str = "AGENT_WORKSPACE_JUNK_MARKER";
    std::fs::write(
        repo.path()
            .join(".claude")
            .join("worktrees")
            .join("agentA")
            .join("regenerated.txt"),
        format!("junk {JUNK_MARKER}\n"),
    )
    .expect("failed to edit the fixture's agent-workspace file");

    // Bounded wait, then assert absence — non-flaky for the same reason
    // `gitignored_directories_are_never_watched` is: the positive control
    // just above already proved how long a real refresh takes in this
    // session, so "still not there" after three times that (floored at
    // 1.5s) means correctly never watched, not merely not-yet-refreshed.
    std::thread::sleep((measured_refresh_latency * 3).max(Duration::from_millis(1_500)));
    let screen = h.screen_contents();
    assert!(
        !screen.contains(JUNK_MARKER),
        "a write inside an agent-workspace prefix must never trigger a refresh; screen:\n{screen}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(DEFAULT_WAIT);
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
