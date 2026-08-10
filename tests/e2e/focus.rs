//! Issue #14: Tab/BackTab make the changed-files pane a real, independently
//! browsable focus target beside the diff pane, proven against the real
//! binary the same way issue #13's Tab/BackTab pane-focus cycling was
//! (`tests/e2e/lsp_inspector.rs`). No PTY test in this suite inspects cell
//! color, so — matching that file's own idiom — a focus-gated status
//! message is the observable witness for "which pane actually has focus
//! right now": `gd` reports `files_focus_blocked_message`'s text only
//! while `Files` owns focus, and `App::hover_query`'s ordinary "nothing to
//! jump from here" note (a file-header row has no target) once `Diff` owns
//! it again — neither needs an LSP server, so both stay safe against
//! `basic_repo`'s plain-text fixture.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn tab_focus_files_movement_enter_backtab_and_ctrl_o_all_behave() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("README.md");
    // Row 0 is the first file's own header, row 1 its hunk header — neither
    // has a source line for `Ctrl-o` to return to later. Two presses land
    // on row 2, the hunk's first real content line, giving the flow below
    // a genuine position to leave from and later retrace.
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    let start_position = "\u{b7} 3/";
    h.wait_for_text(start_position);

    // Tab focuses Files — proven by the files-focus gate: `gd` only ever
    // reports this note while `Files` owns focus.
    h.send(Key::Tab);
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("definition: focus the diff pane first");

    // Moving in Files (down to the diff's second file) must not scroll or
    // move the diff cursor at all until Enter — acceptance criterion:
    // "Moving in Files does not scroll or move the diff until Enter."
    h.send(Key::Char('j'));
    std::thread::sleep(Duration::from_millis(300));
    let contents = h.screen_contents();
    assert!(
        contents.contains(start_position),
        "files-pane movement must not move the diff cursor; screen:\n{contents}"
    );

    // Enter jumps the diff cursor to the selected file's header and hands
    // focus back to Diff — its header row has no hover/goto target, so
    // `gd` now reports the ordinary "nothing here" note instead of the
    // files-focus one, proving both the jump and the refocus happened.
    h.send(Key::Enter);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains(start_position)
    });
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("goto: nothing to jump from here");

    // BackTab cycles focus backward — from `Diff`, straight back to
    // `Files` in this two-pane order.
    h.send(Key::BackTab);
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("definition: focus the diff pane first");

    // Ctrl-o retraces the Enter-confirmed jump regardless of which pane
    // currently has focus (`Files`, right now), landing back on the exact
    // row it left from, and — issue #14's plan decision 3 — hands focus
    // back to `Diff` too, whether or not a reviewer ever pressed Tab/BackTab
    // again themselves. `start_position`'s row is real content (not a
    // header), so `gd` from there would attempt a real go-to-definition
    // instead of reporting "nothing to jump from here" — proving the
    // refocus the same way every earlier step did instead: Tab only
    // reaches `Files` (and only then does `gd` report the blocked note) if
    // it started from `Diff`.
    h.send(Key::CtrlO);
    h.wait_for_text(start_position);
    h.send(Key::Tab);
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("definition: focus the diff pane first");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn tab_and_enter_still_work_at_a_narrow_width_with_wrapped_cjk_content() {
    // Doubles as req 9's one-file-diff focus-cycling case (`long.txt` is
    // this fixture's only file) and the narrow-terminal/CJK-wrap guard
    // plan-14 calls for: a width where both panes' `PaneChrome` borders
    // eat into an already tight budget is exactly where a hand-counted
    // border-width drift between `App::content_width` and the real
    // rendered inner rect (issue #14's req 8) would show up first.
    let repo = fixture::long_line_repo(true);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 60,
            rows: 30,
            ..Default::default()
        },
    );

    h.wait_for_text("long.txt");
    // At this width the wrapped content column is narrow enough that
    // `TAILMARKER` itself can land split across two continuation rows
    // (unlike `wrap.rs`'s 100-column tests, which are exactly calibrated
    // so it never does — see `fixture::long_line_repo`'s docs) — the wrap
    // marker glyph is the width-independent witness that wrapping still
    // happened at all.
    h.wait_until(Duration::from_secs(3), |screen| {
        screen.contents().contains('\u{21aa}')
    });

    // Shorter substrings than the two full-width checks in the other test
    // — the status note doesn't word-wrap, so the full sentence would be
    // clipped at this width; each of these still names the one message
    // that can appear here (this fixture's `.txt`/`.toml` files have no
    // configured language to attempt a real go-to-definition against).
    h.send(Key::Tab);
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("definition:");

    h.send(Key::Enter);
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("goto:");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
