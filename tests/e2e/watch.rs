//! The default working-tree watcher, end to end: a bare `ktmr` session must
//! notice an edit made after startup and render the new diff without a keypress.
//! This deliberately uses no CLI arguments, since that was the invocation
//! path that previously constructed a non-watching session.

use crate::support::{Harness, Key, MouseKey, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn bare_session_refreshes_after_a_working_tree_edit() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    // This is content from the initial diff. Waiting for it proves the
    // session has rendered before the edit below, and therefore that the
    // marker is evidence of a refresh rather than startup output.
    h.wait_for_text("A brand new line five.");
    const MARKER: &str = "LIVE_REFRESH_UNIQUE_MARKER";
    assert!(!h.screen_contents().contains(MARKER));

    // Replace an already-visible added line rather than appending below the
    // hunk: the latter would land in a collapsed unchanged-context gap and
    // make a successful refresh indistinguishable from no refresh on screen.
    let updated_content = format!(
        "# Sample project\n\nThis is line two, updated.\nThis is line three.\nThis is line four.\nA brand new line five. {MARKER}\n"
    );
    std::fs::write(repo.path().join("README.md"), &updated_content)
        .expect("failed to edit watched fixture file");

    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains(MARKER)
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn no_watch_opt_out_hides_the_live_refresh_indicator() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff", "--no-watch"],
            ..Default::default()
        },
    );

    h.wait_for_text("A brand new line five.");
    let contents = h.screen_contents();
    assert!(
        !contents.contains("⦿ watch"),
        "--no-watch must omit the live refresh indicator; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// The text every cell in columns `[col_start, col_end)` renders, one line
/// per screen row — a private copy of `tests/e2e/mouse.rs`'s own helper of
/// the same name (kept local rather than shared: each file only needs it
/// for one narrow purpose — proving content landed in the diff pane
/// specifically, not the sidebar, which also renders file names).
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

/// `diff_layout`'s diff-pane column span at the default `SpawnOptions`
/// width (`SIDEBAR_WIDTH = 30`, sidebar first) — see `tests/e2e/mouse.rs`'s
/// identical constant for the full derivation.
const DIFF_COLS: (u16, u16) = (30, 100);

/// Issue #20's mouse wheel (`App::scroll_by`) decouples `scroll_offset`
/// from the cursor; `src/ui/refresh.rs`'s signed `Anchor::scroll_delta`
/// exists specifically so a background refresh doesn't snap that
/// decoupled viewport back onto the cursor's row (see that field's own doc
/// comment: without the sign, "a background refresh ... silently snap[s]
/// the viewport back onto a cursor the reviewer scrolled away from"). The
/// pure-state unit test proving the arithmetic
/// (`App::a_wheel_scrolled_viewport_survives_a_refresh_of_unchanged_content`)
/// calls `apply_refresh` directly on a hand-built `App`; this is the same
/// claim through the real crossterm-decoded SGR wheel bytes and the real
/// filesystem-watcher thread `ui::mod::event_loop` actually wires
/// together — a genuinely new interaction between two independently
/// shipped mechanisms that only exists once both run their real paths.
#[test]
fn a_wheel_scrolled_diff_viewport_survives_a_real_watcher_refresh() {
    const FILE_COUNT: usize = 30;
    let repo = fixture::many_files_repo(FILE_COUNT);
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("FIRST_MARKER");
    // Wait for a render past the fourth file, so the wheel-scroll snapshot
    // below isn't itself taken mid-first-paint — same reasoning
    // `tests/e2e/mouse.rs::wait_for_settled_diff_render` gives for the
    // identical fixture.
    h.wait_until(Duration::from_secs(3), |screen| {
        region_text(screen, DIFF_COLS.0, DIFF_COLS.1).contains("file003.txt")
    });

    // Four ticks (12 rows) reliably scrolls `FIRST_MARKER` — a handful of
    // rows into file000's own hunk, near the very top of the diff pane's
    // content — out of view without needing to know the pane's exact
    // viewport height; the same amount
    // `tests/e2e/mouse.rs::wheel_over_the_diff_pane_scrolls_it_not_the_sidebar`
    // already relies on for this identical fixture. Nothing here ever
    // moves the cursor (no keypress before the edit below) — the one thing
    // that would legitimately reset this decoupled state, per
    // `App::scroll_by_offset_self_heals_on_the_next_cursor_move`.
    for _ in 0..4 {
        h.send_mouse(MouseKey::ScrollDown {
            column: 60,
            row: 10,
        });
    }
    h.wait_until(Duration::from_secs(3), |screen| {
        !region_text(screen, DIFF_COLS.0, DIFF_COLS.1).contains("FIRST_MARKER")
    });

    // Read back whichever file the wheel-scrolled viewport now actually
    // shows, rather than assuming the exact scroll-offset arithmetic — the
    // same idiom `tests/e2e/mouse.rs::scrolled_tree_click_hits_the_right_row`
    // uses. It must be some file after file000 (row 0, still holding the
    // cursor untouched), so the edit below can land a fresh marker inside
    // the current viewport without disturbing the cursor's own row.
    let visible = h.with_screen(|screen| region_text(screen, DIFF_COLS.0, DIFF_COLS.1));
    let target = (1..FILE_COUNT)
        .find(|i| visible.contains(&format!("file{i:03}.txt")))
        .expect("some later file must be visible once FIRST_MARKER has scrolled out of view");

    // Reconstruct that file's already-diffed content exactly as
    // `fixture::many_files_repo` wrote it (its own marker convention: the
    // last file gets `LAST_MARKER`, every other non-`file000` file gets
    // `CHANGED`), then edit its *context* line ("beta", unchanged before
    // this edit) — landing the fresh marker on a row this file's own hunk
    // already renders inside the current viewport, rather than somewhere
    // that could scroll the viewport itself (this edit only ever inserts
    // rows strictly after the cursor's own file000 rows, so the restored
    // scroll offset — computed from the cursor's row alone — cannot
    // change; see `App::restore_scroll_from_delta`).
    let existing_marker = if target == FILE_COUNT - 1 {
        "LAST_MARKER"
    } else {
        "CHANGED"
    };
    const LIVE_MARKER: &str = "WHEEL_SURVIVES_REFRESH_UNIQUE_MARKER";
    std::fs::write(
        repo.path().join(format!("file{target:03}.txt")),
        format!("alpha {existing_marker}\nbeta {LIVE_MARKER}\ngamma\n"),
    )
    .expect("failed to edit watched fixture file");

    h.wait_until(Duration::from_secs(10), |screen| {
        region_text(screen, DIFF_COLS.0, DIFF_COLS.1).contains(LIVE_MARKER)
    });

    // The payoff: the refresh that just landed must not have snapped the
    // viewport back onto the cursor (still sitting at file000's own
    // FileHeader row — nothing in this test ever moved it).
    // `restore_scroll_from_delta`'s negative-delta branch is precisely what
    // keeps `FIRST_MARKER` off screen here instead of it reappearing.
    assert!(
        !h.with_screen(|screen| region_text(screen, DIFF_COLS.0, DIFF_COLS.1))
            .contains("FIRST_MARKER"),
        "a background refresh must not snap a wheel-scrolled viewport back onto the cursor; screen:\n{}",
        h.screen_contents()
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
