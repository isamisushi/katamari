//! The filtered-registration pruning rules end to end: a gitignored
//! directory and a fake nested checkout, both present before a session
//! ever spawns, must never trigger live refresh, while an ordinary tracked
//! edit still does — and a plain directory created *after* the session has
//! started must be dynamically picked up rather than silently going
//! unwatched forever.

use crate::support::harness::DEFAULT_WAIT;
use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::process::Command;
use std::time::Duration;

/// Both the positive proof and the negative assertion live in one
/// sequenced session, the same call `tests/e2e/moving_scope.rs` makes and
/// explains in its own docs: "nothing happened" after writing into the
/// gitignored directories is only meaningful evidence of correct filtering
/// once *this session* has already shown the refresh pipeline fires at all
/// within a bounded wait — otherwise a flaky-slow machine and "correctly
/// filtered" would look identical.
///
/// Both `ignored/` and `agent-worktree/` in this fixture are gitignored
/// (see [`fixture::watch_filtering_repo`]'s own docs on why an *un*-
/// gitignored nested checkout can't be proven this way at all —
/// `handle_watch_refresh` re-derives the whole working-tree diff on every
/// trigger, so its content could leak in via any *unrelated* refresh
/// regardless of whether the watcher itself ever reacted to it) — this
/// test is really exercising ordinary gitignore-based filtering twice
/// over (once for a plain build-output-shaped directory, once for a
/// nested-checkout-shaped one), cross-platform, since `is_excluded` is
/// unaffected by the macOS/inotify registration split. The nested-checkout
/// rule *specifically* (independent of gitignore, inotify-only) has its
/// own isolated proof: [`nested_checkout_never_wakes_the_watcher`] below.
#[test]
fn gitignored_directories_are_never_watched() {
    let repo = fixture::watch_filtering_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    // Content from the fixture's own uncommitted edit — proves the session
    // has rendered before anything below, the same role `watch.rs`'s
    // "A brand new line five." wait plays.
    h.wait_for_text("alpha, updated");

    // ---- (1) an edit to the tracked file must still refresh live --------

    const TRACKED_MARKER: &str = "WATCH_FILTERING_TRACKED_MARKER";
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

    // ---- (2) writes inside gitignored directories must never reach the
    //          debounce window at all ------------------------------------

    const IGNORED_MARKER: &str = "WATCH_FILTERING_IGNORED_MARKER";
    const NESTED_MARKER: &str = "WATCH_FILTERING_NESTED_MARKER";
    std::fs::write(
        repo.path().join("ignored").join("output.txt"),
        format!("junk {IGNORED_MARKER}\n"),
    )
    .expect("failed to edit the gitignored fixture file");
    std::fs::write(
        repo.path()
            .join("agent-worktree")
            .join("src")
            .join("main.txt"),
        format!("placeholder {NESTED_MARKER}\n"),
    )
    .expect("failed to edit the gitignored nested-checkout fixture file");

    // Bounded wait, then assert absence — non-flaky specifically because
    // part (1) above, in this very session, already measured how long a
    // real refresh actually takes here; "still not there" after three
    // times that (floored at 1.5s) is "correctly never watched," not
    // "hasn't had time yet." Same idiom `moving_scope.rs`'s own negative
    // assertion uses, for the same reason. Deterministic (not just
    // non-flaky in practice) because both edited files are gitignored —
    // nothing, ever, re-derives them into a diff, watch-triggered or not.
    std::thread::sleep((measured_refresh_latency * 3).max(Duration::from_millis(1_500)));
    let screen = h.screen_contents();
    assert!(
        !screen.contains(IGNORED_MARKER),
        "a write inside a gitignored directory must never trigger a refresh; screen:\n{screen}"
    );
    assert!(
        !screen.contains(NESTED_MARKER),
        "a write inside a gitignored nested checkout must never trigger a refresh; screen:\n{screen}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(DEFAULT_WAIT);
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// The nested-checkout rule specifically — independent of gitignore, and
/// of `handle_watch_refresh`'s always-full-diff behavior that makes a TUI
/// screen assertion unreliable for an *un*-gitignored one (see
/// `gitignored_directories_are_never_watched`'s own docs). `ktmr
/// watch-check` is the right level instead: it reports only what
/// `watch::spawn` itself detected, with no diff computation downstream to
/// confound the result — see its own doc comment ("an E2E smoke test for
/// the whole `watch` module without a terminal").
///
/// `--flushes 1` with a bounded `--timeout-secs`: if the write below ever
/// reaches the debounce window, `watch-check` exits 0 well within the
/// timeout (real, not hypothetical — the fixture's own tracked-file case
/// and `tests/e2e/watch.rs` both prove a real edit flushes in well under a
/// second); if it correctly never does, `watch-check` times out and exits
/// non-zero. No artificial delay before the write — proving this holds
/// even in the tightest race the process can produce is a *stronger*
/// claim than giving registration a comfortable head start would be, and
/// nothing here needs debounce settling time the way a screen-content
/// assertion would.
///
/// Linux/inotify-family only: on macOS, [`watch::register`] keeps the
/// single recursive FSEvents root watch (see that function's own docs) —
/// the nested-checkout rule doesn't exist there at all, by design, so this
/// same write *would* wake a macOS session's watcher.
#[cfg(not(target_os = "macos"))]
#[test]
fn nested_checkout_never_wakes_the_watcher() {
    let repo = fixture::nested_checkout_repo();

    let child = Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .arg("watch-check")
        .arg("--dir")
        .arg(repo.path())
        .arg("--flushes")
        .arg("1")
        .arg("--timeout-secs")
        .arg("3")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn ktmr watch-check");

    std::fs::write(
        repo.path().join("worktree").join("placeholder.txt"),
        "placeholder WATCH_FILTERING_NESTED_MARKER\n",
    )
    .expect("failed to edit the nested-checkout fixture file");

    let output = child
        .wait_with_output()
        .expect("failed to wait for ktmr watch-check");
    assert!(
        !output.status.success(),
        "watch-check must time out waiting for a flush that should never come \
         (a write inside a nested checkout must never wake the watcher); \
         stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// A directory that doesn't exist yet at spawn time — unlike the fixture's
/// pre-existing `ignored/`/`agent-worktree/`, which only the *initial*
/// registration walk ever has a chance to prune or admit — must still end
/// up live-watched once it's created: `watch::WatchSession::
/// maybe_register_new_dir`'s whole reason to exist. The file written into
/// it lands essentially immediately after the `mkdir`, deliberately: on a
/// real machine that write routinely wins the race against dynamic
/// registration finishing (kernel event → `notify`'s background thread →
/// this session's channel → the watcher thread's own poll tick), which is
/// exactly the gap `maybe_register_new_dir`'s own guaranteed catch-up
/// signal exists to close — so this test doesn't just prove a new
/// directory gets a watch, it proves content raced into one before that
/// watch existed still isn't lost.
///
/// Cross-platform (unlike the nested-checkout test above): a plain new
/// directory is unaffected by either platform's registration strategy —
/// FSEvents' recursive root watch already covers it natively, and
/// `maybe_register_new_dir` covers it on the filtered strategy.
///
/// Asserts on the *sidebar's* file-tree listing rather than the diff
/// pane's own content: the diff pane's anchor-preserving scroll (a real
/// refresh already has to survive it — `tests/e2e/watch.rs`'s own
/// wheel-scroll test) keeps the cursor's row (`tracked.txt`) pinned at its
/// prior screen position, which can scroll a file sorting *before* it
/// (this one — `n` < `t`) out of the diff pane's visible rows entirely
/// even though it's fully present in `App::files`; the sidebar tree has no
/// such scroll-away-from-the-cursor behavior; both entries fit without
/// scrolling regardless of sort order.
#[test]
fn a_newly_created_directory_is_dynamically_watched() {
    let repo = fixture::watch_filtering_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("alpha, updated");

    const MARKER: &str = "newdir-marker";
    std::fs::create_dir_all(repo.path().join(MARKER)).expect("failed to create a new directory");
    std::fs::write(repo.path().join(MARKER).join("f.txt"), "hello\n")
        .expect("failed to write into the newly created directory");

    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains(MARKER)
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(DEFAULT_WAIT);
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
