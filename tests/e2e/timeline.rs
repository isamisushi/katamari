//! Issue #13's pane-focus split, inside the jj snapshot timeline (`t` from
//! `ktmr diff`), end to end. `TimelineView::update`'s own module doc
//! narrates the regression this guards: before the split, Tab/BackTab
//! (then still `NextSymbol`/`PrevSymbol`) were intercepted unconditionally
//! at the top of `TimelineView::update`, so a reviewer could never reach
//! the *nested* diff pane's own symbol cycling — every Tab press bounced
//! `Focus` back to `List` first, no matter which pane already had it. Now
//! Tab/BackTab are their own actions (`Action::FocusNextPane`/
//! `FocusPrevPane`, cycled through `pane::cycle_focus` against
//! `FOCUS_ORDER`), and `l`/`h` (`NextSymbol`/`PrevSymbol`) only fall
//! through to the nested `App::update` once `Focus::Diff` actually owns
//! the keypress.
//!
//! `TimelineView::update`'s own `#[cfg(test)]` module already drives this
//! directly (`focus_next_pane_and_focus_prev_pane_toggle_list_and_diff_focus`,
//! `next_symbol_reaches_the_nested_diffs_cycle_symbol_once_the_diff_pane_is_focused`)
//! — solid at the `View::update` level. What it can't reach is real
//! crossterm key parsing and `ui::mod`'s actual dispatch path a live Tab
//! press travels through before `TimelineView::update` is ever called at
//! all, the same reasoning `tests/e2e/lsp_inspector.rs` gives for why its
//! sibling view needed a PTY twin alongside its own unit tests.

use crate::support::screen::underlined_cells;
use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::{Duration, Instant};

/// One continuous session, deliberately, rather than several smaller
/// tests: every later step depends on where the one before it left the
/// cursor and sub-focus, the same "each step is only meaningful given the
/// one before it" shape `tests/e2e/kitty.rs`'s `kitty_supported` already
/// uses for its own Tab/pane-focus checks.
#[test]
fn tab_and_backtab_cycle_list_diff_sub_focus_and_l_reaches_the_nested_diff_pane_once_focused() {
    if !fixture::jj_available() {
        eprintln!("skipping: jj not on PATH");
        return;
    }
    let repo = fixture::jj_timeline_repo();

    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff"],
            ..Default::default()
        },
    );
    // The root diff loads before `t` is even pressed — proves `ktmr`
    // started cleanly against this fixture. Colocation's own auto-export
    // (never a real `jj commit` here — see `fixture::jj_timeline_repo`'s
    // docs) is what put `a.txt` where `GitSource`'s unborn-HEAD fallback
    // could show it as a plain working-tree addition.
    h.wait_for_text("SYMONE SYMTWO");

    h.send(Key::Char('t'));
    // `render_list`'s own block title — unique to the Timeline view, unlike
    // "SYMONE SYMTWO" which was already on screen before `t` was pressed
    // and so can't tell a test "the Timeline actually opened" on its own.
    h.wait_for_text("snapshots");

    // Tab: `List` -> `Diff` sub-focus. `G`/`k` is the same two-step
    // `src/ui/timeline_view.rs`'s own
    // `next_symbol_reaches_the_nested_diffs_cycle_symbol_once_the_diff_pane_is_focused`
    // unit test uses to reach the fixture's one real content row with an
    // identifier to cycle: `Bottom` first lands on the trailing fold-gap
    // row this two-line file's diff renders below its one real content
    // line, and `CursorUp` from there is what actually reaches it. Both
    // only mean anything once forwarded to the nested `App` — while
    // `List` still owned focus they'd move the *snapshot* selection
    // instead (`TimelineView::update_list`), never touching the diff pane
    // at all.
    h.send(Key::Tab);
    h.send(Key::Char('G'));
    h.send(Key::Char('k'));
    // `active_symbol` defaults to `0` (the row's first token, "SYMONE")
    // the moment the cursor lands on a row that has any — no `l` needed
    // yet to see an underline at all (`diff_view`'s `content_line`,
    // `active_symbol` handling). Waiting for a non-empty underline set
    // (rather than `wait_for_text`, which the text was already showing
    // before Tab/G/k ever moved anything) is what actually proves the
    // cursor settled on the row, not merely that the keys were sent.
    h.wait_until(Duration::from_secs(2), |screen| {
        !underlined_cells(screen).is_empty()
    });
    let on_symone = h.with_screen(underlined_cells);

    // The fix under test: `l` (`Action::NextSymbol`) must reach the nested
    // `App` now that `Diff` sub-focus owns it, moving off "SYMONE" onto
    // "SYMTWO" — before issue #13, the unconditional Tab-intercept at the
    // top of `TimelineView::update` meant no keypress meant for symbol-
    // cycling could ever land here at all.
    let moved_at = Instant::now();
    h.send(Key::Char('l'));
    h.wait_until(Duration::from_secs(2), |screen| {
        let cells = underlined_cells(screen);
        !cells.is_empty() && cells != on_symone
    });
    let measured_move_latency = moved_at.elapsed();
    let on_symtwo = h.with_screen(underlined_cells);

    // BackTab: `Diff` -> `List` sub-focus (the only other pane — Tab and
    // BackTab both just swap between the two, see `FOCUS_ORDER`). Once
    // back on `List`, `l` must revert to being a no-op against the nested
    // diff pane exactly the way it always was pre-#13
    // (`TimelineView::update_list`'s `_ => {}` fallback) — the negative
    // half of the same regression the positive check above guards, sized
    // off that check's own measured latency (floored, for a
    // suspiciously-fast measurement) rather than a guessed constant — the
    // same technique `tests/e2e/moving_scope.rs`'s amend test uses for its
    // own negative assertion, and for the same reason: this run's own
    // evidence for "how long a real change here takes to render," not a
    // constant that happened to work.
    h.send(Key::BackTab);
    h.send(Key::Char('l'));
    std::thread::sleep((measured_move_latency * 3).max(Duration::from_millis(300)));
    assert_eq!(
        h.with_screen(underlined_cells),
        on_symtwo,
        "`l` must not reach the nested diff pane while `List` sub-focus owns \
         the keypress"
    );

    // One more Tab proves focus genuinely round-tripped back to `Diff`
    // rather than getting stuck somewhere BackTab left it: `l` reaches the
    // nested pane again and wraps the (two-symbol) cycle back onto
    // "SYMONE".
    h.send(Key::Tab);
    h.send(Key::Char('l'));
    h.wait_until(Duration::from_secs(2), |screen| {
        underlined_cells(screen) == on_symone
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(crate::support::harness::DEFAULT_WAIT);
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
