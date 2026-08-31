//! Rendering guards the unit-level test suite can't provide on its own,
//! since these depend on how `ratatui`'s cells actually land on a real
//! terminal grid: CJK double-width rendering end-to-end
//! (`renders_japanese_diff`), the M9 hint-wrapping fix actually wrapping
//! rather than truncating on a narrow terminal (`narrow_terminal_wraps_hints`),
//! and the empty-diff placeholder actually reaching a real spawned session
//! (`empty_diff_shows_a_placeholder_not_blank_panes`,
//! `empty_diff_placeholder_vanishes_once_a_watched_edit_lands`).

use crate::support::screen::{hint_line_count, wide_cell_contents};
use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn renders_japanese_diff() {
    let repo = fixture::japanese_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("greeting.txt");
    h.wait_for_text("\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}"); // こんにちは

    // The pipeline's actual payoff: at least one cell holding a Japanese
    // character is rendered as double-width, not just present as text —
    // `unicode-width`'s CJK-wide classification made it all the way through
    // parsing, highlighting, tab expansion, and truncation into what
    // `ratatui` handed the terminal.
    let wide = h.with_screen(wide_cell_contents);
    assert!(
        !wide.is_empty(),
        "expected at least one double-width cell in the rendered diff; screen:\n{}",
        h.screen_contents()
    );
    assert!(
        wide.iter().any(
            |s| "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\u{4e16}\u{754c}".contains(s.as_str())
        ),
        "expected a double-width cell to hold one of the diff's Japanese characters, got {wide:?}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn narrow_terminal_wraps_hints() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 60,
            rows: 30,
            ..Default::default()
        },
    );

    h.wait_for_text("README.md");

    // The collapsed default fits one line even at 60 columns — wrapping
    // only has something to wrap once the full list is expanded with `.`.
    h.send(Key::Char('.'));
    h.wait_for_text("hunk");

    let contents = h.screen_contents();
    let wrapped = hint_line_count(&contents);
    assert!(
        wrapped > 1,
        "expected the expanded hint bar to wrap onto more than one line at 60 columns, got {wrapped}\nscreen:\n{contents}"
    );

    // The M9 bug this guards against: an item pushed past the first hint
    // row must still be shown intact on its own line, not clipped the way a
    // fixed single-row status bar used to cut it off. `move` (the curated
    // list's first item) and `hunk` (mid-list) both comfortably survive
    // `hints::MAX_HINT_LINES`' 3-line cap at 60 columns — unlike the very
    // last items (`toggle`/`quit`), which this width drops entirely, so
    // they're not useful witnesses for "wrapped, not truncated".
    let hint_lines: Vec<&str> = contents
        .lines()
        .filter(|line| line.trim_start().starts_with('\u{b7}'))
        .collect();
    let move_line = hint_lines.iter().position(|l| l.contains("j/k move"));
    let hunk_line = hint_lines.iter().position(|l| l.contains("hunk"));
    assert!(
        move_line.is_some() && hunk_line.is_some() && move_line != hunk_line,
        "expected `move` and `hunk` hints on different wrapped lines, got hint lines: {hint_lines:?}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// `ktmr diff` in a repo with nothing uncommitted used to render two
/// completely empty bordered panes — indistinguishable from a session still
/// loading. This is that bug's real-terminal repro, guarding the fix: the
/// diff pane must show the scope-aware placeholder text instead, and the
/// hint line naming `o`/`q` proves the wording was read off the live
/// keymap rather than baked in (see `diff_view::render_empty_state`'s docs).
#[test]
fn empty_diff_shows_a_placeholder_not_blank_panes() {
    let repo = fixture::clean_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("working tree clean");
    let contents = h.screen_contents();
    assert!(
        contents.contains("nothing to review"),
        "expected the full working-tree headline; screen:\n{contents}"
    );
    assert!(
        contents.contains("opens the scope menu"),
        "expected a hint naming the scope-menu key; screen:\n{contents}"
    );
    assert!(
        contents.contains("quits"),
        "expected a hint naming the quit key; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// The placeholder is derived from `App::rows` on every rederive, not
/// cached from the first empty paint — a live-refresh session that starts
/// clean and then gains a real change must swap straight to the real diff,
/// with no keypress, the same way `tests/e2e/watch.rs`'s already-showing-content
/// case proves a refresh reaches the screen at all.
#[test]
fn empty_diff_placeholder_vanishes_once_a_watched_edit_lands() {
    let repo = fixture::clean_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("working tree clean");

    const MARKER: &str = "LIVE_REFRESH_FROM_EMPTY_UNIQUE_MARKER";
    std::fs::write(
        repo.path().join("notes.txt"),
        format!("alpha {MARKER}\nbeta\ngamma\n"),
    )
    .expect("failed to edit watched fixture file");

    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains(MARKER)
    });
    let contents = h.screen_contents();
    assert!(
        !contents.contains("working tree clean"),
        "the placeholder must be gone once a real change lands; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
