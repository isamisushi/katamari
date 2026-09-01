//! Reviewed-hunk state — `r`/`R`/`m f`/`m a`, `z o`/`z c` on a collapsed
//! marker, persistence across a restart, and resurfacing after a rewrite —
//! end to end through the real compiled binary. Reuses
//! `fixture::fold_gap_repo`'s two-hunk shape (`tests/e2e/fold.rs` documents
//! its exact row layout: `FileHeader, HunkHeader, 7 hunk-0 rows, Gap, ...`,
//! 20 rows total) rather than a bespoke fixture, since it already gives two
//! disjoint hunks with real (non-context) changed lines — exactly what a
//! content-addressed hunk id needs to actually depend on.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn mark_toggle_and_bulk_mark_round_trip_in_one_session() {
    let repo = fixture::fold_gap_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("big.txt");
    assert!(h.screen_contents().contains("line 3 CHANGED"));
    // Nothing marked yet — the status bar shows no "reviewed" span at
    // all (matching `unit_filter`/`watch_mode`'s own only-when-active
    // convention), not a "0/2" baseline.
    assert!(!h.screen_contents().contains("reviewed"));

    // One `j` from the file header lands on hunk 0's own header.
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 2/20");

    h.send(Key::Char('r'));
    h.wait_for_text("\u{2713} reviewed");
    assert!(
        !h.screen_contents().contains("line 3 CHANGED"),
        "hunk 0's own content must be gone, replaced by the marker row"
    );
    h.wait_for_text("\u{b7} reviewed 1/2");

    // Marking advanced the cursor past the marker row onto hunk 1's own
    // header (the "next unreviewed hunk" jump) — two `k` presses walk it
    // back up, over the Between gap row, onto the marker itself.
    h.send(Key::Char('k'));
    h.send(Key::Char('k'));

    // `z o` on the marker reveals it again without unmarking — a peek.
    h.send(Key::Char('z'));
    h.send(Key::Char('o'));
    h.wait_for_text("line 3 CHANGED");
    h.wait_for_text("\u{b7} reviewed 1/2");

    // `z c` re-collapses it; the cursor lands back on the marker row.
    h.send(Key::Char('z'));
    h.send(Key::Char('c'));
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("line 3 CHANGED")
    });
    h.wait_for_text("\u{b7} reviewed 1/2");

    // `R` toggles it back off — content reappears because an unreviewed
    // hunk is never collapsed at all, and the status bar's progress span
    // disappears now that nothing is marked (the "reviewed: unmarked"
    // status *note* it also prints is a different, transient span — this
    // checks the progress span specifically, by its digit pattern).
    h.send(Key::Char('R'));
    h.wait_for_text("line 3 CHANGED");
    h.wait_until(Duration::from_secs(3), |screen| {
        let c = screen.contents();
        !c.contains("reviewed 1/2") && !c.contains("reviewed 2/2")
    });

    // `m f` marks every hunk in the cursor's file — there's only the one
    // file in this fixture, so this covers both hunks.
    h.send(Key::Char('m'));
    h.send(Key::Char('f'));
    h.wait_for_text("\u{b7} reviewed 2/2");
    assert!(
        !h.screen_contents().contains("line 3 CHANGED")
            && !h.screen_contents().contains("line 35 CHANGED"),
        "both hunks collapsed after mark-file: {}",
        h.screen_contents()
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// Hard requirement: a *live* watch-mode session (the default — no
/// `--no-watch`), not a restart, resurfaces a rewritten reviewed hunk on
/// its own — the untouched sibling hunk stays collapsed throughout, and
/// no keypress is sent between the on-disk rewrite and the assertion. The
/// live filesystem watcher noticing the write and pushing a redraw is the
/// actual mechanism under test, the same class of proof
/// `range_comment.rs`'s CLI-write test gives the comments watcher.
#[test]
fn a_live_watch_refresh_resurfaces_only_the_rewritten_hunk_with_no_keypress() {
    let repo = fixture::fold_gap_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("big.txt");

    h.send(Key::Char('m'));
    h.send(Key::Char('a'));
    h.wait_for_text("\u{b7} reviewed 2/2");
    assert!(
        !h.screen_contents().contains("line 3 CHANGED")
            && !h.screen_contents().contains("line 35 CHANGED"),
        "both hunks collapsed before the rewrite: {}",
        h.screen_contents()
    );

    // An agent-style edit to hunk 0's own changed line, written straight
    // to disk — no keypress before or after.
    let path = repo.path().join("big.txt");
    let content = std::fs::read_to_string(&path).expect("read big.txt");
    let rewritten = content.replace("line 3 CHANGED", "line 3 CHANGED AGAIN");
    std::fs::write(&path, rewritten).expect("rewrite big.txt");

    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains("line 3 CHANGED AGAIN")
    });
    let contents = h.screen_contents();
    assert!(
        contents.contains("\u{b7} reviewed 1/2"),
        "only the rewritten hunk resurfaces; the untouched one stays reviewed: {contents}"
    );
    assert!(
        !contents.contains("line 35 CHANGED"),
        "the untouched hunk stays collapsed with no keypress sent: {contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// `m a` (mark-visible) on this single-file fixture behaves the same as
/// `m f` — a distinct binding/status line from `mark_toggle_and_bulk_mark_round_trip_in_one_session`'s
/// `m f` coverage, per the contract's "distinct status-bar text" ask.
#[test]
fn mark_visible_reports_its_own_status_text() {
    let repo = fixture::fold_gap_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("big.txt");
    h.send(Key::Char('m'));
    h.send(Key::Char('a'));
    h.wait_for_text("reviewed: marked 2 hunks");
    h.wait_for_text("\u{b7} reviewed 2/2");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// The two hard requirements that need a real process restart to prove at
/// all: a mark surviving `.katamari/reviewed.jsonl` being read back on a
/// fresh launch, and a hunk whose on-disk content changed between launches
/// resurfacing as unreviewed while an untouched sibling hunk stays
/// collapsed.
#[test]
fn reviewed_marks_persist_across_a_restart_and_a_rewritten_hunk_resurfaces() {
    let repo = fixture::fold_gap_repo();

    {
        let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
        h.wait_for_text("big.txt");
        h.send(Key::Char('m'));
        h.send(Key::Char('a'));
        h.wait_for_text("\u{b7} reviewed 2/2");
        h.send(Key::Char('q'));
        let status = h.wait_exit(Duration::from_secs(5));
        assert!(
            status.success(),
            "first session should exit 0, got {status:?}"
        );
    }

    // Second launch, same repo directory, no keypress before the first
    // assertion — the mark can only be visible this early if it was
    // loaded from `.katamari/reviewed.jsonl` and collapsed on the very
    // first frame.
    {
        let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
        h.wait_for_text("big.txt");
        h.wait_for_text("\u{b7} reviewed 2/2");
        assert!(
            !h.screen_contents().contains("line 3 CHANGED")
                && !h.screen_contents().contains("line 35 CHANGED"),
            "both marks survived the restart, still collapsed: {}",
            h.screen_contents()
        );
        h.send(Key::Char('q'));
        let status = h.wait_exit(Duration::from_secs(5));
        assert!(
            status.success(),
            "second session should exit 0, got {status:?}"
        );
    }

    // Rewrite hunk 0's changed line on disk between launches — its
    // content id is now different from what was marked.
    let path = repo.path().join("big.txt");
    let content = std::fs::read_to_string(&path).expect("read big.txt");
    assert!(
        content.contains("line 3 CHANGED"),
        "sanity: pre-rewrite text present"
    );
    let rewritten = content.replace("line 3 CHANGED", "line 3 CHANGED AGAIN");
    std::fs::write(&path, rewritten).expect("rewrite big.txt");

    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("big.txt");
    h.wait_for_text("\u{b7} reviewed 1/2");
    assert!(
        h.screen_contents().contains("line 3 CHANGED AGAIN"),
        "the rewritten hunk resurfaces with its new content, unreviewed: {}",
        h.screen_contents()
    );
    assert!(
        !h.screen_contents().contains("line 35 CHANGED"),
        "the untouched hunk keeps its old id and stays collapsed: {}",
        h.screen_contents()
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(
        status.success(),
        "third session should exit 0, got {status:?}"
    );
}
