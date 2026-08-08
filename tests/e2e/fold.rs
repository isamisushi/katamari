//! Issue #2: unfolding a fold row (`z o`) reveals the unchanged lines git
//! omitted between two hunks, and `z c` hides them again — end to end,
//! through the real compiled binary and a real `git` subprocess. See
//! `support::fixture::fold_gap_repo`'s docs for the fixture's exact shape
//! and why `GAPMARKER_UNIQUE_TEXT` is the deciding witness.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn z_o_reveals_a_mid_file_marker_and_z_c_hides_it_again() {
    let repo = fixture::fold_gap_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("big.txt");
    h.wait_for_text("\u{00b7}\u{00b7}\u{00b7}");
    assert!(
        !h.screen_contents().contains("GAPMARKER_UNIQUE_TEXT"),
        "the marker sits inside the folded gap — must not be visible yet"
    );

    // Row layout (see `fold_gap_repo`'s docs): FileHeader, HunkHeader,
    // 7 hunk-0 rows, the between-hunks Gap row — flat index 9, position
    // indicator 10/20. `j` nine times from the top lands the cursor there.
    for _ in 0..9 {
        h.send(Key::Char('j'));
    }
    h.wait_for_text("\u{b7} 10/20");

    h.send(Key::Char('z'));
    h.send(Key::Char('o'));
    h.wait_until(Duration::from_secs(3), |screen| {
        screen.contents().contains("GAPMARKER_UNIQUE_TEXT")
    });
    assert!(
        h.screen_contents().contains("GAPMARKER_UNIQUE_TEXT"),
        "z o must reveal the gap's hidden lines, marker included"
    );

    h.send(Key::Char('z'));
    h.send(Key::Char('c'));
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("GAPMARKER_UNIQUE_TEXT")
    });
    assert!(
        !h.screen_contents().contains("GAPMARKER_UNIQUE_TEXT"),
        "z c must fold the marker back out of view"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
