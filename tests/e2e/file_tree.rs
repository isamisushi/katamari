//! Issue #15: the changed-files sidebar renders as a collapsible directory
//! tree, proven against the real binary the same way issue #14's flat
//! files-pane focus/movement was (`tests/e2e/focus.rs`). Spawns
//! `fixture::tree_repo()` under `ktmr diff --staged` (see that fixture's
//! own docs on why: a real `rename from`/`rename to` pair only ever
//! appears in a *staged* diff) and drives Tab → navigate → `Space` collapse
//! → `Space` expand → `Enter` open, checking the sidebar's own collapse
//! state against the screen rather than the diff pane's (which never
//! changes in response to a tree toggle at all).

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn tab_navigate_collapse_expand_and_open_a_nested_file() {
    let repo = fixture::tree_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff", "--staged"],
            ..Default::default()
        },
    );

    // The renamed file's new path proves the rename made it into this
    // (staged) diff at all — `fixture::tree_repo`'s own docs explain why an
    // ordinary working-tree diff could never show one. The tree starts
    // fully expanded (req 4), so the marker file's own sidebar row is
    // already visible too — at the default 100x30 terminal,
    // `src/aaa_padding.txt`'s 60 added lines push its *diff-pane* header
    // well below the initial viewport, which is what makes the collapse
    // check below an unambiguous sidebar witness rather than a coincidence
    // of diff-pane scroll.
    h.wait_for_text("new_name.txt");
    h.wait_for_text("NESTED_MARKER_UNIQUE");

    // Tab focuses Files; `gg` (Top) lands on the tree's root "src" row
    // regardless of wherever `files_selection` happened to be tracking the
    // diff cursor beforehand.
    h.send(Key::Tab);
    h.send(Key::Char('g'));
    h.send(Key::Char('g'));
    // One row down from "src" is "nested" — a directory, since #15 sorts
    // directories ahead of files at every level, and "nested" is the only
    // directory directly under "src" in this fixture.
    h.send(Key::Char('j'));

    // Space collapses "nested": its descendants ("deep" and the marker
    // file within it) disappear from the sidebar — and since the marker
    // text was never on screen in the diff pane to begin with, its total
    // absence from the screen now unambiguously witnesses the sidebar
    // collapse, not a diff-pane scroll coincidence.
    h.send(Key::Char(' '));
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("NESTED_MARKER_UNIQUE")
    });
    // A collapsed directory still shows its own row and the collapsed
    // disclosure glyph.
    h.wait_for_text("\u{25b8}");

    // Space again re-expands it — the marker file's row returns.
    h.send(Key::Char(' '));
    h.wait_for_text("NESTED_MARKER_UNIQUE");

    // Navigate down onto the marker file's own row ("nested" -> "deep" ->
    // the file) and open it.
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Enter);

    // Enter on a file row jumps the diff cursor to that file's header and
    // hands focus back to Diff — the file's own unique content line, only
    // ever produced by this file, is the witness that the jump actually
    // landed there (not just that the filename is still spelled out
    // somewhere in the sidebar).
    h.wait_for_text("MARKER_CONTENT_LINE_UNIQUE");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
