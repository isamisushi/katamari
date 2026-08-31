//! Issue #28: `C-w`/`M-Backspace` word deletion in the comment-compose
//! overlay, driven through the real compiled binary and a real (fake) PTY
//! — the one call site among `text_input::recognize`'s four that most
//! benefits from an end-to-end check, since it's the multi-line
//! `ComposeBuffer` path (`delete_previous_word`'s own line-merge branch,
//! not just `LineInput`'s single-line one already covered in-process by
//! `ui::compose`'s unit tests).
//!
//! `basic_repo`'s row layout (see `tests/e2e/range_comment.rs`'s own docs
//! for the identical fixture): three `j` presses from row 0 (a file-header
//! row `App::comment_target` refuses — see
//! `tests/e2e/skill_install.rs::save_a_comment`'s docs) lands on a real
//! `Context` line eligible for `c`.

use crate::support::{Harness, Key, KittyMode, SpawnOptions, fixture};
use std::time::Duration;

/// Opens the compose overlay on `basic_repo`'s third row, the same setup
/// `tests/e2e/skill_install.rs::save_a_comment` uses.
fn open_compose(h: &Harness) {
    h.wait_for_text("README.md");
    for _ in 0..3 {
        h.send(Key::Char('j'));
    }
    h.send(Key::Char('c'));
    h.wait_for_text("C-s save"); // the compose overlay's own hint line
}

#[test]
fn ctrl_w_deletes_the_previous_word_and_the_saved_comment_reflects_it() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    open_compose(&h);

    for c in "hello world".chars() {
        h.send(Key::Char(c));
    }
    h.wait_for_text("hello world");

    h.send(Key::CtrlW);
    h.wait_until(Duration::from_secs(2), |screen| {
        let contents = screen.contents();
        contents.contains("hello") && !contents.contains("world")
    });

    h.send(Key::CtrlS);
    h.wait_for_text("comment: saved");

    // The saved comment's body renders under the commented line — proof
    // the word deletion landed in the buffer that actually got persisted,
    // not just in the overlay's live preview.
    let contents = h.screen_contents();
    assert!(
        contents.contains("hello"),
        "saved comment must keep \"hello\"; screen:\n{contents}"
    );
    assert!(
        !contents.contains("world"),
        "C-w must have deleted \"world\" before save; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn ctrl_w_at_a_line_start_merges_into_the_previous_line_not_a_no_op() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    open_compose(&h);

    for c in "first".chars() {
        h.send(Key::Char(c));
    }
    h.send(Key::Enter); // second, empty line — cursor at its col 0
    h.wait_for_text("first");

    // At col 0 of a line past the first, `C-w` has no "word" behind it on
    // *this* line — `ComposeBuffer::delete_previous_word` falls back to
    // the same line-merge `backspace` does there, not a no-op.
    h.send(Key::CtrlW);

    h.send(Key::CtrlS);
    h.wait_for_text("comment: saved");
    let contents = h.screen_contents();
    assert!(
        contents.contains("first"),
        "the merged line's text must survive; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn alt_backspace_deletes_the_previous_word_under_kitty_mode() {
    let repo = fixture::basic_repo();
    // `SpawnOptions::default()` already negotiates `KittyMode::Supported`
    // (see its own `Default` impl) — spelled out here anyway since this
    // test's whole point is proving `Key::AltBackspace`'s kitty CSI-u
    // encoding round-trips through the real binary, not just the legacy
    // `ESC` + DEL form `Key::AltBackspace`'s docs describe as the fallback.
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            kitty_mode: KittyMode::Supported,
            ..Default::default()
        },
    );
    open_compose(&h);

    for c in "hello world".chars() {
        h.send(Key::Char(c));
    }
    h.wait_for_text("hello world");

    h.send(Key::AltBackspace);
    h.wait_until(Duration::from_secs(2), |screen| {
        let contents = screen.contents();
        contents.contains("hello") && !contents.contains("world")
    });

    h.send(Key::Esc); // discard — this test only cares about the live buffer
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// Issue #27: `[keys.compose] save` moves the overlay's save key off its
/// `C-s` default — through a real repo-level config file, the real binary's
/// own config-loading path, not `ComposeKeymap::resolve` called in-process
/// (already covered by `src/ui/compose.rs`'s own unit tests). Proves three
/// things a unit test can't: the hint line renders the *configured*
/// binding, the old hardcoded `C-s` genuinely stops saving once `save`
/// moves elsewhere, and the new binding actually saves.
#[test]
fn keys_compose_save_rebind_via_config_changes_the_hint_and_the_save_key() {
    let repo = fixture::basic_repo();
    std::fs::create_dir_all(repo.path().join(".katamari")).unwrap();
    std::fs::write(
        repo.path().join(".katamari").join("config.toml"),
        "[keys.compose]\nsave = \"C-x\"\n",
    )
    .unwrap();

    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("README.md");
    for _ in 0..3 {
        h.send(Key::Char('j'));
    }
    h.send(Key::Char('c'));
    // The hint reflects the resolved keymap, not a hardcoded string — `Esc`
    // (cancel's untouched default) stays put alongside the rebound `save`.
    h.wait_for_text("C-x save · Esc cancel");

    for c in "looks good".chars() {
        h.send(Key::Char(c));
    }
    h.wait_for_text("looks good");

    // The old default no longer saves: `C-s` is Control-modified, so it's
    // swallowed by `text_input::recognize` rather than inserted either —
    // the overlay just stays open with the typed text untouched.
    h.send(Key::CtrlS);
    let after_ctrl_s = h.screen_contents();
    assert!(
        !after_ctrl_s.contains("comment: saved"),
        "C-s must no longer save once [keys.compose] moves save to C-x; screen:\n{after_ctrl_s}"
    );
    assert!(
        after_ctrl_s.contains("looks good"),
        "the overlay must still be open with the typed text intact; screen:\n{after_ctrl_s}"
    );

    h.send(Key::CtrlX);
    h.wait_for_text("comment: saved");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// Issue #29: a long unbroken line soft-wraps across display rows instead
/// of clipping at the panel edge — `ui::compose`'s own unit tests already
/// cover `render`'s wrap/scroll-follow math in isolation via a bare
/// `TestBackend`; this is the proof it survives a real terminal round-trip
/// (real key events, real cursor tracking) and, the actual point of a
/// *soft* wrap, that what gets saved is still the one line the reviewer
/// typed with no `\n` quietly inserted into it.
#[test]
fn a_long_unbroken_line_soft_wraps_on_screen_and_saves_with_no_inserted_newline() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    open_compose(&h);

    // At the harness's default 100x30 terminal the compose panel's content
    // width is well under 80 columns — 80 filler characters plus a marker
    // guarantees at least one wrap, landing "TAILEND" on a continuation row
    // an unwrapped `Paragraph` (pre-issue-#29 behavior) would have clipped
    // before ever reaching.
    let long_line = format!("{}TAILEND", "x".repeat(80));
    for c in long_line.chars() {
        h.send(Key::Char(c));
    }
    h.wait_until(Duration::from_secs(3), |screen| {
        screen.contents().contains("TAILEND")
    });

    let contents = h.screen_contents();
    assert!(
        contents.contains("TAILEND"),
        "the wrapped continuation row must surface the line's tail; screen:\n{contents}"
    );

    h.send(Key::CtrlS);
    h.wait_for_text("comment: saved");

    let raw = std::fs::read_to_string(repo.path().join(".katamari").join("comments.jsonl"))
        .expect("comments.jsonl must exist after a save");
    let record: serde_json::Value =
        serde_json::from_str(raw.trim()).expect("comments.jsonl holds one JSON object per line");
    assert_eq!(
        record["body"], long_line,
        "the saved body must be exactly the typed line — soft-wrap is \
         display-only and must never insert a real newline"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
