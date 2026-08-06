//! Basic cursor-movement smoke test: `j`/`k` against the real binary. The
//! status bar's `{repo_name} · {cursor+1}/{total_rows}` position indicator
//! (`ui::status_bar::render`) is the stable, always-present thing to assert
//! on — no reliance on which row happens to be highlighted or how wide the
//! terminal is.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn j_and_k_move_the_position_indicator() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("\u{b7} 1/");

    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 2/");

    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");

    h.send(Key::Char('k'));
    h.wait_for_text("\u{b7} 2/");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
