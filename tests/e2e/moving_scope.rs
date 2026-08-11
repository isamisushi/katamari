//! Issue #8: a moving historical scope (`ktmr diff -r HEAD`, or here — the
//! scope menu's "Revision…" swapped onto `HEAD` — see `crate::support`'s
//! module docs) follows a commit amend live, without the reviewer ever
//! reopening the view, while a scope pinned to an already-resolved commit
//! hash stays exactly as it was. End to end, through the real compiled
//! binary and a real `git commit --amend` subprocess — the only way to
//! prove the ref-watcher (`watch::spawn_revision_watcher`) actually notices
//! a real amend on disk, which nothing at the `App`/`ui::mod` unit-test
//! level can reach.

use crate::support::harness::DEFAULT_WAIT;
use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

/// Both halves of issue #8's acceptance criteria live in one test function,
/// deliberately, rather than two independent `#[test]`s: the second half's
/// negative assertion ("a pinned hash must never pick up a later amend") is
/// only meaningful once something in *this same session* has already
/// demonstrated the refresh pipeline actually fires within the bounded wait
/// it uses — which the first half proves, immediately before, in the same
/// harness. Two separate `#[test]` functions would have no ordering
/// guarantee between them (`cargo test` runs the suite's tests in parallel,
/// in no defined order), so the second's "nothing happened" could just as
/// easily mean "hasn't ticked yet" as "correctly ignored" if it ran first,
/// or on its own. Sequencing both here removes that ambiguity entirely.
#[test]
fn head_scope_follows_an_amend_while_a_pinned_hash_scope_stays_static() {
    // A single commit ("first"), nothing else uncommitted — unlike
    // `basic_repo` (whose dirty working tree exists for *its own*
    // scope-swap test), a perfectly clean tree here means the `git add -A`
    // below can never accidentally sweep in unrelated edits, so each
    // amend's content is exactly what this test wrote.
    let repo = fixture::moving_scope_repo();

    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff"],
            ..Default::default()
        },
    );

    // `o` -> down three times (Working tree -> Staged -> Log -> Revision…)
    // -> Enter -> type "HEAD" -> Enter, the same nav pattern
    // `tests/e2e/scope_menu.rs` already establishes for a git-only repo (no
    // colocated jj here, so the menu has exactly these four entries).
    h.send(Key::Char('o'));
    h.wait_for_text("Revision");
    for _ in 0..3 {
        h.send(Key::Char('j'));
    }
    h.send(Key::Enter);
    h.wait_for_text("git rev");
    for c in "HEAD".chars() {
        h.send(Key::Char(c));
    }
    h.send(Key::Enter);

    // Swapped onto the (git-classified-moving) `HEAD` scope: its content is
    // on screen, tagged with the symbolic label — never a resolved hash —
    // exactly as M11 already established for a revision diff.
    h.wait_for_text("r: HEAD");
    h.wait_for_text("alpha");

    // ---- (1) an amend of HEAD must be picked up with no key sent --------

    const MARKER_ONE: &str = "MOVING_SCOPE_MARKER_FIRST_AMEND";
    std::fs::write(
        repo.path().join("notes.txt"),
        format!("alpha\nbeta\ngamma\n{MARKER_ONE}\n"),
    )
    .expect("failed to edit the fixture's tracked file");
    fixture::git(repo.path(), &["add", "-A"]);
    fixture::git(repo.path(), &["commit", "--amend", "-q", "--no-edit"]);

    // No key sent between the amend above and this wait — the ref-watcher,
    // the resolve-and-compare check, and the re-diff are the only things
    // that can make this marker appear. The elapsed time is measured so
    // part (2)'s negative wait below can be sized from what this machine
    // *actually* took rather than a hand-picked constant.
    let refresh_started = std::time::Instant::now();
    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains(MARKER_ONE)
    });
    let measured_refresh_latency = refresh_started.elapsed();
    // The scope label stays exactly "r: HEAD" — symbolic by construction,
    // never rewritten to the commit it currently resolves to (see
    // `App::apply_refresh`'s docs).
    assert!(
        h.screen_contents().contains("r: HEAD"),
        "the scope label must stay symbolic across the refresh; screen:\n{}",
        h.screen_contents()
    );

    // ---- (2) a scope pinned to a resolved hash never follows a later amend

    // The exact commit HEAD now names (post first amend) — an immutable
    // object id, unlike the symbolic text just swapped away from.
    let pinned_hash = repo.commit_hash("HEAD");

    h.send(Key::Char('o'));
    h.wait_for_text("Revision");
    for _ in 0..3 {
        h.send(Key::Char('j'));
    }
    h.send(Key::Enter);
    h.wait_for_text("git rev");
    for c in pinned_hash.chars() {
        h.send(Key::Char(c));
    }
    h.send(Key::Enter);
    h.wait_for_text(&format!("r: {pinned_hash}"));
    // Swapping onto the pinned hash lands on the same content `HEAD`
    // already showed (same commit, same instant) — confirms the swap
    // itself worked before the negative assertion below relies on it.
    h.wait_for_text(MARKER_ONE);

    const MARKER_TWO: &str = "MOVING_SCOPE_MARKER_SECOND_AMEND";
    std::fs::write(
        repo.path().join("notes.txt"),
        format!("alpha\nbeta\ngamma\n{MARKER_ONE}\n{MARKER_TWO}\n"),
    )
    .expect("failed to edit the fixture's tracked file");
    fixture::git(repo.path(), &["add", "-A"]);
    fixture::git(repo.path(), &["commit", "--amend", "-q", "--no-edit"]);

    // Bounded wait, then assert absence: non-flaky specifically because
    // part (1) above, in this very session, already proved the ref-watcher
    // -> resolve -> re-diff pipeline completes — and *measured* how long it
    // took. Waiting three times that (floored at 1.5s for a
    // suspiciously-fast measurement) means "still not there" is
    // "correctly never refreshed," not "hasn't had time yet," by this
    // run's own evidence rather than a constant that happened to work.
    std::thread::sleep((measured_refresh_latency * 3).max(Duration::from_millis(1_500)));
    let screen = h.screen_contents();
    assert!(
        !screen.contains(MARKER_TWO),
        "a scope pinned to an already-resolved commit hash must never pick \
         up a later amend; screen:\n{screen}"
    );
    assert!(
        screen.contains(&format!("r: {pinned_hash}")),
        "the pinned scope's own label must still be showing; screen:\n{screen}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(DEFAULT_WAIT);
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// Issue #8's headline entry point — a scope opened from the CLI
/// (`ktmr diff HEAD`) rather than through the scope menu: its
/// `revision_scope` seeding lives in `main.rs`'s own argument handling, a
/// path the menu-driven test above never touches. Positive half only — a
/// bounded wait for an amend to land needs no in-session sequencing proof
/// the way the negative assertion above does. (The CLI positional scope
/// shows no `r:` label — a pre-existing labelling inconsistency with the
/// menu path, deliberately not changed by issue #8 — so the waits here key
/// off content, not the status bar.)
#[test]
fn a_cli_opened_head_scope_follows_an_amend_too() {
    let repo = fixture::moving_scope_repo();

    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff", "HEAD"],
            ..Default::default()
        },
    );
    h.wait_for_text("alpha");

    const MARKER: &str = "MOVING_SCOPE_MARKER_CLI_AMEND";
    std::fs::write(
        repo.path().join("notes.txt"),
        format!("alpha\nbeta\ngamma\n{MARKER}\n"),
    )
    .expect("failed to edit the fixture's tracked file");
    fixture::git(repo.path(), &["add", "-A"]);
    fixture::git(repo.path(), &["commit", "--amend", "-q", "--no-edit"]);

    // No key sent — only the ref-watcher pipeline can surface the marker.
    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains(MARKER)
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(DEFAULT_WAIT);
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
