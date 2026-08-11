//! Issue #16: `V` starts/extends/cancels a logical visual-line selection in
//! the main diff pane — proven through the real compiled binary the same
//! way issue #14's pane-focus gate is (`tests/e2e/focus.rs`): the cursor
//! position indicator (`\u{b7} N/`) and the status-bar note are the
//! observable witnesses, since no PTY test in this suite inspects cell
//! color (see `ui::diff_view`'s own render tests for that instead).

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn v_starts_a_selection_extends_with_j_and_esc_cancels_without_moving_the_cursor() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("README.md");
    // Row 0 is the file header, row 1 the hunk header — two `j` presses
    // land on row 2, the first real content line (see `focus.rs`'s own use
    // of this exact fixture/position for the same reason).
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");

    h.send(Key::Char('V'));
    h.wait_for_text("visual: j/k extend");
    h.wait_for_text("VISUAL");

    // Extending the selection moves the cursor exactly the way ordinary `j`
    // always has (req 4: movement extends for free) — now on row 3 (1-based
    // "4/").
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 4/");

    // Esc cancels the selection without moving the cursor any further.
    h.send(Key::Esc);
    h.wait_for_text("visual selection cancelled");
    let contents = h.screen_contents();
    assert!(
        contents.contains("\u{b7} 4/"),
        "Esc must cancel without moving the cursor; screen:\n{contents}"
    );
    assert!(
        !contents.contains("VISUAL"),
        "the persistent VISUAL indicator must clear once cancelled; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn v_pressed_again_cancels_the_selection_the_same_way_esc_does() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("README.md");
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");

    h.send(Key::Char('V'));
    h.wait_for_text("VISUAL");

    h.send(Key::Char('V'));
    h.wait_for_text("visual selection cancelled");
    let contents = h.screen_contents();
    assert!(
        contents.contains("\u{b7} 3/"),
        "cancelling a second time never moves the cursor; screen:\n{contents}"
    );
    assert!(!contents.contains("VISUAL"));

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn tab_off_the_diff_pane_cancels_a_live_selection_leaving_no_stale_anchor() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("README.md");
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");

    h.send(Key::Char('V'));
    h.wait_for_text("VISUAL");

    // Issue #16 req 10's 7th invalidation trigger: `Action::FocusNextPane`
    // carries its own `self.visual_anchor = None` (app.rs), separate from
    // the shared `rederive()` funnel the other six triggers go through —
    // leaving `Diff` focus must cancel the selection outright, not just
    // stop rendering its indicator while focus is elsewhere. `basic_repo`
    // has two files, so the sidebar is visible and `Tab` genuinely moves
    // focus to `Files` rather than being the no-op it is on a one-file diff
    // (see `focus.rs`'s own note on that case).
    h.send(Key::Tab);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("VISUAL")
    });

    // Prove the anchor is truly gone, not just the indicator hidden while
    // `Files` owns focus: `BackTab` returns focus to `Diff`, and a bare `y`
    // there demands a fresh `V` — the exact message `yank.rs`'s
    // `y_without_a_selection_names_the_required_step` uses to mean "no
    // selection exists."
    h.send(Key::BackTab);
    h.send(Key::Char('y'));
    h.wait_for_text("yank: press V to select lines first");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn v_on_the_file_header_row_reports_nothing_selectable() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("README.md");
    // No movement at all — the cursor starts on row 0, the file header.
    h.send(Key::Char('V'));
    h.wait_for_text("visual: no selectable source line here");
    assert!(
        !h.screen_contents().contains("VISUAL"),
        "a rejected V must never show the persistent selection indicator"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
