//! Branch-vs-base diff scope: `ktmr diff --branch`, the scope menu's
//! "Branch vs base" entry, the `B` keybinding, and the empty-state
//! placeholder's live hint — end to end, through the real compiled binary
//! and a real cloned-from-a-bare-upstream fixture (see
//! `crate::support::fixture::branch_ahead_of_main_repo`), the one way to
//! prove `git symbolic-ref -q --short refs/remotes/origin/HEAD` and the
//! two-endpoint moving-scope refresh actually work against real refs on
//! disk, not just the `vcs::base`/`ui::mod` unit tests' canned inputs.
//!
//! Every git-only test here resolves its base to `origin/main`, not the
//! bare `main` the fixture's local branch is also named — the fixture
//! clones from a real bare "upstream" with `refs/remotes/origin/HEAD` set,
//! so `crate::vcs::base::detect_base`'s precedence (jj trunk, then
//! `origin/HEAD`, then a local `main`) picks `origin/HEAD` first, exactly
//! as it should whenever both exist. See that module's own docs for why.

use crate::support::harness::DEFAULT_WAIT;
use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn branch_flag_opens_the_branch_diff_with_the_right_label() {
    let repo = fixture::branch_ahead_of_main_repo();

    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff", "--branch"],
            ..Default::default()
        },
    );

    h.wait_for_text("FEATURE_MARKER_TWO");
    h.wait_for_text("feature vs origin/main (+2)");

    h.send(Key::Char('q'));
    let status = h.wait_exit(DEFAULT_WAIT);
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// A clean working tree two commits ahead of its detected base shows the
/// empty-state placeholder's third hint line, and pressing the bound key
/// (`B`) swaps straight to the branch-vs-base diff — the two halves of the
/// same affordance, checked together since the second only means anything
/// once the first has already proven the hint names the right base/count.
#[test]
fn clean_tree_placeholder_shows_the_hint_and_b_swaps_to_the_branch_diff() {
    let repo = fixture::branch_ahead_of_main_repo();

    // Wider than the suite's default 100 columns: the hint line ("o opens
    // the scope menu · q quits · B review this branch against origin/main
    // (+2 commits)") is long enough to clip mid-word at the default width
    // (`render_empty_state`'s centered `Paragraph` doesn't wrap) — widening
    // proves the real, full text rather than settling for whatever prefix
    // happens to survive truncation.
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("working tree clean");
    let contents = h.screen_contents();
    assert!(
        contents.contains("review this branch against origin/main (+2 commits)"),
        "expected the branch-vs-base hint line; screen:\n{contents}"
    );

    h.send(Key::Char('B'));
    h.wait_for_text("FEATURE_MARKER_TWO");
    h.wait_for_text("feature vs origin/main (+2)");

    h.send(Key::Char('q'));
    let status = h.wait_exit(DEFAULT_WAIT);
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn scope_menu_entry_opens_the_branch_diff() {
    let repo = fixture::branch_ahead_of_main_repo();

    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("working tree clean");

    // No colocated jj here, so the menu's entries are exactly Working tree /
    // Staged / Branch vs base (origin/main) / Log / Revision… / GitHub PR… —
    // two `j` presses from the top lands on the third entry.
    h.send(Key::Char('o'));
    h.wait_for_text("Branch vs base (origin/main)");
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Enter);

    h.wait_for_text("FEATURE_MARKER_TWO");
    h.wait_for_text("feature vs origin/main (+2)");

    h.send(Key::Char('q'));
    let status = h.wait_exit(DEFAULT_WAIT);
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// `HEAD` already equal to its detected base (checked out on `main` itself,
/// the fixture's clean base branch) — the whole point of `ahead == 0`'s
/// gate: no hint line, and the scope-menu entry is simply absent rather
/// than shown-and-disabled.
#[test]
fn ahead_zero_on_main_itself_hides_both_the_hint_and_the_menu_entry() {
    let repo = fixture::branch_ahead_of_main_repo();
    fixture::git(repo.path(), &["checkout", "-q", "main"]);

    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("working tree clean");
    let contents = h.screen_contents();
    assert!(
        !contents.contains("review this branch"),
        "HEAD already equals its base — no hint should show; screen:\n{contents}"
    );

    h.send(Key::Char('o'));
    h.wait_for_text("Working tree");
    assert!(
        !h.screen_contents().contains("Branch vs base"),
        "ahead == 0 — the menu entry must be absent, not just unreachable"
    );

    h.send(Key::Esc);
    h.send(Key::Char('q'));
    let status = h.wait_exit(DEFAULT_WAIT);
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// The two-endpoint moving-scope refresh (issue #8's V1 boundary extended
/// to this scope — see `ui::mod::MovingScopeState::base`'s docs): a new
/// commit landing on `HEAD`'s side re-diffs the open branch-vs-base scope
/// live, no key sent, and the status label's `(+N)` count comes along with
/// it — mirrors `tests/e2e/moving_scope.rs`'s amend-and-refresh pattern,
/// sized the same non-flaky way (a real bounded wait, no fixed sleep).
#[test]
fn a_new_commit_on_the_feature_branch_refreshes_the_open_scope_live() {
    let repo = fixture::branch_ahead_of_main_repo();

    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff", "--branch"],
            ..Default::default()
        },
    );
    h.wait_for_text("feature vs origin/main (+2)");
    h.wait_for_text("FEATURE_MARKER_TWO");

    const MARKER: &str = "FEATURE_MARKER_THREE";
    std::fs::write(
        repo.path().join("notes.txt"),
        format!("alpha\nbeta\ngamma\nFEATURE_MARKER_ONE\nFEATURE_MARKER_TWO\n{MARKER}\n"),
    )
    .expect("failed to edit the fixture's tracked file");
    fixture::git(
        repo.path(),
        &["commit", "-q", "-am", "feature commit three"],
    );

    // No key sent — only the ref-watcher -> resolve -> re-diff pipeline can
    // surface the new marker and the updated count together.
    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains(MARKER)
    });
    assert!(
        h.screen_contents().contains("feature vs origin/main (+3)"),
        "the (+N) count must update alongside the new commit's content; screen:\n{}",
        h.screen_contents()
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(DEFAULT_WAIT);
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// A reviewer-found gap: committing away a dirty working tree with nothing
/// else touched (`git add -A && git commit`, the ordinary case) must still
/// flip the plain working-tree scope to the empty-state placeholder — and
/// bring the branch-vs-base hint line up with it — even though `.git/` sits
/// in the file-system watcher's own excludes and a commit alone touches no
/// tracked file on disk. Only the ref-watcher's `AppEvent::RevisionChanged`
/// tick (which the commit's `refs/heads/feature` update does fire) can
/// notice this transition; see `refresh_branch_vs_base_hint`'s docs in
/// `ui::mod` for why that tick has to re-run the working-tree diff itself,
/// not just the hint's `+N` count.
#[test]
fn committing_away_a_dirty_tree_shows_the_placeholder_and_hint_with_no_file_touch() {
    let repo = fixture::branch_ahead_of_main_repo();
    const MARKER: &str = "UNCOMMITTED_EDIT_MARKER";
    std::fs::write(
        repo.path().join("notes.txt"),
        format!("alpha\nbeta\ngamma\nFEATURE_MARKER_ONE\nFEATURE_MARKER_TWO\n{MARKER}\n"),
    )
    .expect("failed to dirty the fixture's tracked file");

    // Plain `ktmr diff`, not `--branch` — the default working-tree scope
    // this hint/refresh actually belongs to. Wider than the suite's default
    // 100 columns for the same reason
    // `clean_tree_placeholder_shows_the_hint_and_b_swaps_to_the_branch_diff`
    // is: the hint line clips mid-word at the default width.
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );
    h.wait_for_text(MARKER);

    // Commit exactly what's dirty and nothing more — no other write to any
    // tracked file, so the file-system watcher has nothing to say about
    // this at all.
    fixture::git(repo.path(), &["add", "-A"]);
    fixture::git(
        repo.path(),
        &["commit", "-q", "-m", "commit away the dirty edit"],
    );

    // No key sent — only the ref-watcher -> re-diff pipeline can notice the
    // tree just went clean. Waited for as one combined condition (not a
    // "wait for the headline, then snapshot-assert the rest") so a run
    // under parallel test-suite load, where the placeholder's first frame
    // and the hint's `+N` landing can be split across two redraws of the
    // very same already-computed state, never trips a spurious failure —
    // both fields are set together inside one `refresh_branch_vs_base_hint`
    // call, so this converges the moment that call has run at all.
    h.wait_until(Duration::from_secs(10), |screen| {
        let contents = screen.contents();
        contents.contains("nothing to review")
            && contents.contains("review this branch against origin/main (+3 commits)")
    });
    let contents = h.screen_contents();
    assert!(
        contents.contains("nothing to review"),
        "expected the placeholder to fully render, not just its headline; screen:\n{contents}"
    );
    assert!(
        contents.contains("review this branch against origin/main (+3 commits)"),
        "expected the hint's ahead-count to include the just-committed change; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(DEFAULT_WAIT);
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// Issue #8's own jj angle, extended to this scope: a colocated repo's
/// `--branch` resolves its base through `trunk()` (which wins outright over
/// `origin/HEAD` in `vcs::base::detect_base`'s precedence — see that
/// module's docs) rather than the git fallback chain the tests above
/// exercise.
#[test]
fn jj_colocated_trunk_base_opens_the_branch_diff() {
    if !fixture::jj_available() {
        eprintln!("skipping: jj not on PATH");
        return;
    }
    let repo = fixture::jj_branch_ahead_of_trunk_repo();

    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff", "--branch"],
            ..Default::default()
        },
    );

    h.wait_for_text("JJ_FEATURE_MARKER");
    // `@` is this app's own working-copy display convention (see
    // `vcs::base::branch_vs_base_label`'s docs) — the exact `(+N)` isn't
    // asserted here: jj's working-copy commit itself counts as one more
    // "ahead" entry on top of the one real `jj commit` this fixture makes
    // (empirically confirmed while building the fixture), which is a jj
    // modeling detail orthogonal to what this test actually covers.
    h.wait_for_text("@ vs main (+");

    h.send(Key::Char('q'));
    let status = h.wait_exit(DEFAULT_WAIT);
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
