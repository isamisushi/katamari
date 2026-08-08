//! Issue #5's `/` search prompt, end to end: confirming with Enter jumps
//! the viewport to a match far below the fold, `n` cycles matches with a
//! wraparound status note, `Esc` cancels cleanly (a second `/` reopens
//! without any lingering state), and a term hidden inside a *collapsed*
//! fold reports no matches until `z o` unfolds it — through the real
//! compiled binary, the way `tests/e2e/fold.rs`/`help.rs` cover their own
//! overlays. Everything about match computation, smartcase, and
//! wraparound itself is already pinned down by `ui::search`'s colocated
//! unit tests; this only exercises what only shows up once a real terminal
//! and the real keymap resolver are involved.

use crate::support::harness::DEFAULT_WAIT;
use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

fn type_text(h: &Harness, text: &str) {
    for c in text.chars() {
        h.send(Key::Char(c));
    }
}

#[test]
fn slash_search_confirms_and_jumps_to_a_term_far_below_the_viewport() {
    let repo = fixture::search_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff"],
            ..Default::default()
        },
    );
    h.wait_for_text("search.txt");
    assert!(
        !h.screen_contents().contains("SEARCH_TARGET_UNIQUE"),
        "the target sits far below what a 30-row terminal shows at the top of the diff"
    );

    h.send(Key::Char('/'));
    type_text(&h, "SEARCH_TARGET_UNIQUE");
    h.send(Key::Enter);
    h.wait_until(DEFAULT_WAIT, |screen| {
        screen.contents().contains("SEARCH_TARGET_UNIQUE")
    });
    assert!(h.screen_contents().contains("SEARCH_TARGET_UNIQUE"));

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// "line" appears exactly 5 times across `basic_repo`'s README.md diff (a
/// del row, an add row, and two unchanged context rows all say "line", plus
/// one more add row — see the fixture's own working-tree edit) — small and
/// deterministic enough to send exactly enough `n` presses to land on the
/// wraparound, then check the note without racing a later press clearing
/// it again.
#[test]
fn n_cycles_matches_and_wraps_with_a_status_note() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff"],
            ..Default::default()
        },
    );
    h.wait_for_text("todo.txt");

    h.send(Key::Char('/'));
    type_text(&h, "line");
    h.send(Key::Enter);
    h.wait_for_text("line");

    // 5 matches: `n` from the first one advances 0->1->2->3->4, and the
    // 5th press wraps 4->0 — landing exactly on "search wrapped" with no
    // further keys sent to race it away again.
    for _ in 0..5 {
        h.send(Key::Char('n'));
    }
    h.wait_until(Duration::from_secs(5), |screen| {
        screen.contents().contains("search wrapped")
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn esc_cancels_the_prompt_and_a_second_slash_reopens_cleanly() {
    let repo = fixture::search_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff"],
            ..Default::default()
        },
    );
    h.wait_for_text("search.txt");

    h.send(Key::Char('/'));
    type_text(&h, "SEARCH_TARGET_UNIQUE");
    h.send(Key::Esc);
    // The viewport is back where it started — the target is off-screen
    // again. There's no observable "highlight" via plain screen text (the
    // harness only sees rendered characters, not styles — see this
    // module's docs), so this is the behavior-based witness the spec calls
    // for: cancelling really did restore the pre-search position.
    h.wait_until(DEFAULT_WAIT, |screen| {
        !screen.contents().contains("SEARCH_TARGET_UNIQUE")
    });

    // Reopening and confirming a fresh query still works — no state from
    // the cancelled prompt lingered.
    h.send(Key::Char('/'));
    type_text(&h, "SEARCH_TARGET_UNIQUE");
    h.send(Key::Enter);
    h.wait_until(DEFAULT_WAIT, |screen| {
        screen.contents().contains("SEARCH_TARGET_UNIQUE")
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn a_term_inside_a_collapsed_fold_reports_no_matches_until_unfolded() {
    let repo = fixture::fold_gap_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("big.txt");
    h.wait_for_text("\u{00b7}\u{00b7}\u{00b7}");

    h.send(Key::Char('/'));
    type_text(&h, "GAPMARKER_UNIQUE_TEXT");
    h.send(Key::Enter);
    // The status note's own text ("no matches: GAPMARKER_UNIQUE_TEXT")
    // legitimately contains the marker string — that's the confirmation
    // search found nothing, not a false positive — so the negative
    // assertion is on the *fold row* still being collapsed (the
    // dot-leader fold marker — see `fold.rs`), not on the marker text's
    // total absence from the screen.
    h.wait_for_text("no matches: GAPMARKER_UNIQUE_TEXT");
    assert!(
        h.screen_contents().contains("\u{00b7}\u{00b7}\u{00b7}"),
        "the fold row must still be collapsed — the marker was never revealed"
    );

    // Confirming with zero matches restores the cursor to exactly where
    // `/` was pressed — vim incsearch parity (see
    // `App::recompute_search_live`'s docs): mid-query, "G" alone
    // coincidentally matches the capital G inside "CHANGED" elsewhere in
    // this fixture and jumps the cursor there, but the full query narrows
    // to zero matches and the cursor snaps straight back to the top before
    // Enter is even pressed — no `gg` needed to get back to a known
    // position. Row layout mirrors `tests/e2e/fold.rs`: `j` nine times from
    // the top lands the cursor on the between-hunks fold row.
    for _ in 0..9 {
        h.send(Key::Char('j'));
    }
    h.wait_for_text("\u{b7} 10/20");

    h.send(Key::Char('z'));
    h.send(Key::Char('o'));
    h.wait_until(Duration::from_secs(3), |screen| {
        screen.contents().contains("GAPMARKER_UNIQUE_TEXT")
    });

    // The confirmed search recomputed over the newly revealed rows purely
    // from `z o` itself, with no need to search again: `n` now finds the
    // single match — and, being the only one, immediately "wraps" back to
    // itself — rather than repeating the earlier "no matches" note.
    h.send(Key::Char('n'));
    h.wait_for_text("search wrapped");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
