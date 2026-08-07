//! `--show-keys` end-to-end: the overlay chip actually reaches a real
//! terminal screen when asked for, collapses a repeated key into `j ×N`,
//! and stays completely absent when the flag isn't passed — the one thing
//! the unit-level `ui::key_display` tests can't prove on their own, since
//! they exercise the accumulator in isolation from `ratatui`'s real cell
//! grid and the event loop that feeds it.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn show_keys_flag_renders_a_collapsed_chip_for_repeated_presses() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff", "--show-keys"],
            ..Default::default()
        },
    );

    h.wait_for_text("README.md");

    // A lone `j` is too common a substring elsewhere on screen (hint text,
    // file names) to assert on by itself — the `×2` collapse marker is the
    // first point in this sequence that's unambiguously the key-display
    // chip and nothing else.
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("j \u{d7}2");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn without_the_flag_no_key_chip_ever_appears() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("README.md");

    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    // Give the (absent) chip every chance to show up before asserting it
    // never did — `wait_for_text` would only prove the opposite case.
    std::thread::sleep(Duration::from_millis(200));
    let contents = h.screen_contents();
    assert!(
        !contents.contains('\u{d7}'),
        "no key-display chip should render without --show-keys; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
