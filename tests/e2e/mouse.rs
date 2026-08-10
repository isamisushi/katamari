//! Issue #20: mouse capture lifecycle and wheel routing, proven against the
//! real compiled `ktmr` binary — SGR mouse bytes injected via
//! `support::mouse::MouseKey`/`Harness::send_mouse`, the same "no raw
//! escape bytes in a test body" shape `support::key::Key` gives keyboard
//! tests. A PTY harness has no real terminal deciding whether to *report*
//! mouse events at all (unlike a real xterm gated on DECSET 1000/1006), so
//! these tests exercise exactly what `ui::mod::event_loop`'s own dispatch
//! does with a byte that arrived — which is also why `[ui] mouse = false`
//! needs its own app-level gate (see that arm's docs in `ui::mod`) rather
//! than relying solely on `EnableMouseCapture` never having been sent.

use crate::support::{Harness, Key, MouseKey, SpawnOptions, fixture};
use std::time::Duration;

/// The text every cell in columns `[col_start, col_end)` renders, one line
/// per screen row — lets a test compare (or search) one pane's own content
/// without a substring that might also appear in a neighboring pane (a
/// sidebar file name and that same file's diff header both contain the
/// file name, for instance). `col_end` is clamped to the screen's actual
/// width so a caller can always just pass an oversized upper bound (e.g.
/// `100` on a 100-column terminal) for "to the right edge."
fn region_text(screen: &vt100::Screen, col_start: u16, col_end: u16) -> String {
    let (rows, cols) = screen.size();
    let col_end = col_end.min(cols);
    let mut text = String::new();
    for row in 0..rows {
        for col in col_start..col_end {
            if let Some(cell) = screen.cell(row, col) {
                text.push_str(cell.contents());
            }
        }
        text.push('\n');
    }
    text
}

/// `diff_layout`'s sidebar column span at the default `SpawnOptions` width
/// (`SIDEBAR_WIDTH = 30`, sidebar first) — shared by every test below that
/// needs to read (or click into) "just the sidebar" vs. "just the diff
/// pane" at the harness's default 100-column terminal.
const SIDEBAR_COLS: (u16, u16) = (0, 30);
const DIFF_COLS: (u16, u16) = (30, 100);

/// Waits until the diff pane's *own* region has rendered past
/// `fixture::many_files_repo`'s fourth file — not just `wait_for_text`'s
/// "this substring appeared somewhere," which a still-settling first frame
/// can satisfy while later rows are still blank. Every test below that
/// snapshots the diff pane before sending any wheel event calls this first,
/// so an "unchanged" assertion can't accidentally pass just because the
/// "before" snapshot was itself taken mid-render.
fn wait_for_settled_diff_render(h: &Harness) {
    h.wait_until(Duration::from_secs(3), |screen| {
        region_text(screen, DIFF_COLS.0, DIFF_COLS.1).contains("file003.txt")
    });
}

#[test]
fn wheel_over_the_sidebar_scrolls_files_not_the_diff_pane() {
    let repo = fixture::many_files_repo(30);
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("file000.txt");
    wait_for_settled_diff_render(&h);

    let diff_before = h.with_screen(|screen| region_text(screen, DIFF_COLS.0, DIFF_COLS.1));

    // Column 10, row 8: inside the sidebar's list content, comfortably
    // clear of its top border/title row. Six ticks (18 rows of requested
    // scroll) comfortably exceeds 30 files' worth of clamp headroom, so
    // this reaches the sidebar's furthest useful offset regardless of the
    // exact viewport height a 100x30 terminal works out to.
    for _ in 0..6 {
        h.send_mouse(MouseKey::ScrollDown { column: 10, row: 8 });
    }
    h.wait_until(Duration::from_secs(3), |screen| {
        !region_text(screen, SIDEBAR_COLS.0, SIDEBAR_COLS.1).contains("file000.txt")
    });

    let diff_after = h.with_screen(|screen| region_text(screen, DIFF_COLS.0, DIFF_COLS.1));
    assert_eq!(
        diff_after, diff_before,
        "a wheel over the sidebar must never move the diff pane's own cursor or scroll"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn wheel_over_the_diff_pane_scrolls_it_not_the_sidebar() {
    let repo = fixture::many_files_repo(30);
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("FIRST_MARKER");
    wait_for_settled_diff_render(&h);

    let sidebar_before =
        h.with_screen(|screen| region_text(screen, SIDEBAR_COLS.0, SIDEBAR_COLS.1));

    // `FIRST_MARKER` sits a handful of rows into `file000.txt`'s own hunk
    // — near the very top of the diff pane's content — so a modest
    // downward scroll reliably pushes it out of view without needing to
    // know the pane's exact viewport height.
    for _ in 0..4 {
        h.send_mouse(MouseKey::ScrollDown {
            column: 60,
            row: 10,
        });
    }
    h.wait_until(Duration::from_secs(3), |screen| {
        !region_text(screen, DIFF_COLS.0, DIFF_COLS.1).contains("FIRST_MARKER")
    });

    let sidebar_after = h.with_screen(|screen| region_text(screen, SIDEBAR_COLS.0, SIDEBAR_COLS.1));
    assert_eq!(
        sidebar_after, sidebar_before,
        "a wheel over the diff pane must never move the sidebar's own selection or scroll"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn wheel_over_the_diff_pane_never_steals_focus_back_from_files() {
    let repo = fixture::many_files_repo(30);
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("file000.txt");
    wait_for_settled_diff_render(&h);

    // Tab focuses Files — proven, as `tests/e2e/focus.rs` does, by the
    // files-focus gate: `gd` only ever reports this note while `Files`
    // owns keyboard focus.
    h.send(Key::Tab);
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("definition: focus the diff pane first");

    let diff_before = h.with_screen(|screen| region_text(screen, DIFF_COLS.0, DIFF_COLS.1));
    for _ in 0..4 {
        h.send_mouse(MouseKey::ScrollDown {
            column: 60,
            row: 10,
        });
    }
    // The wheel must still have done *something* — otherwise "focus is
    // unchanged" would be true only because nothing happened at all.
    h.wait_until(Duration::from_secs(3), |screen| {
        region_text(screen, DIFF_COLS.0, DIFF_COLS.1) != diff_before
    });

    // Keyboard focus must still be on Files — the wheel scrolled the pane
    // under the pointer (the diff) without touching it, req 5's "wheel
    // scrolling does not steal keyboard focus."
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("definition: focus the diff pane first");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn inspector_journal_wheel_scrolls_and_disengages_follow() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH (fake LSP server fixture needs it)");
        return;
    }
    let repo = fixture::lsp_readiness_repo(0.0, 0.0);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );
    h.wait_for_text("main.stub");

    h.send(Key::Char('I'));
    // Both fixture delays are `0.0`, so the fake server reaches `Running`
    // (and starts producing real journal records) quickly; wait on that
    // real text rather than a fixed sleep, same as
    // `tests/e2e/lsp_inspector.rs`.
    h.wait_for_text("running");
    h.wait_for_text("following");

    // At 160 columns the inspector renders its wide layout (Servers left,
    // Detail/Journal stacked on the right — see `LspInspectorView::render`).
    // The bottom-right corner is always inside Journal regardless of the
    // exact border/title row count: Servers only occupies the left column,
    // and Detail is a fixed-height band at the *top* of the right column,
    // never its bottom.
    h.send_mouse(MouseKey::ScrollUp {
        column: 150,
        row: 26,
    });
    h.wait_for_text("paused; G to follow");

    h.send(Key::Esc);
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn mouse_disabled_via_config_ignores_wheel_bytes() {
    let repo = fixture::many_files_repo(10);
    std::fs::create_dir_all(repo.path().join(".katamari")).unwrap();
    std::fs::write(
        repo.path().join(".katamari").join("config.toml"),
        "[ui]\nmouse = false\n",
    )
    .unwrap();

    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("file000.txt");

    let before = h.screen_contents();
    for _ in 0..5 {
        h.send_mouse(MouseKey::ScrollDown {
            column: 60,
            row: 10,
        });
    }
    // No "wait for a change" predicate makes sense here — the whole point
    // is that nothing changes. A short fixed wait gives the (disabled)
    // routing every chance to misbehave before asserting it didn't.
    std::thread::sleep(Duration::from_millis(300));
    let after = h.screen_contents();
    assert_eq!(
        after, before,
        "[ui] mouse = false must leave the screen completely unchanged by wheel bytes"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
