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

use crate::support::screen::{region_text, row_has_reversed_cell, underlined_cells};
use crate::support::{Harness, Key, MouseButton, MouseKey, SpawnOptions, fixture};
use std::time::Duration;

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

// ---- issue #21: file-tree clicks ------------------------------------------

/// A full press-then-release pair — what a real terminal always sends for
/// one click, and what `ui::mod`'s dispatch actually needs (only `Down`
/// does anything; see that arm's own docs), so every click test below
/// sends both rather than only the byte katamari happens to react to.
fn click(h: &Harness, button: MouseButton, column: u16, row: u16) {
    h.send_mouse(MouseKey::Down {
        button,
        column,
        row,
        shift: false,
    });
    h.send_mouse(MouseKey::Up {
        button,
        column,
        row,
        shift: false,
    });
}

/// As [`click`], with SGR's shift-modifier bit set — issue #22 req 4's
/// shift-click, which extends a visual selection but never chases
/// definition even when it lands on an identifier.
fn shift_click(h: &Harness, column: u16, row: u16) {
    h.send_mouse(MouseKey::Down {
        button: MouseButton::Left,
        column,
        row,
        shift: true,
    });
    h.send_mouse(MouseKey::Up {
        button: MouseButton::Left,
        column,
        row,
        shift: true,
    });
}

/// Spawns `fixture::tree_repo()` (staged) and waits for its tree to render
/// fully expanded — every click test below that targets a specific row by
/// screen position reasons from this exact, fixed layout: `sidebar::render`'s
/// inner rect starts at screen row 1 (row 0 is the outer sidebar rect's own
/// top border), and one `VisibleRow` is always exactly one screen row (see
/// `ui::mouse`'s own docs on why — no wrapping), so row `1 + n` below is
/// `visible_rows[n]`:
///
/// ```text
/// row 1: src                                       (dir, depth 0)
/// row 2: src/nested                                (dir, depth 1)
/// row 3: src/nested/deep                           (dir, depth 2)
/// row 4: src/nested/deep/NESTED_MARKER_UNIQUE.txt  (file, depth 3)
/// row 5: src/aaa_padding.txt                       (file, depth 1)
/// row 6: src/doomed.txt                            (file, depth 1, deleted)
/// row 7: src/new_name.txt                          (file, depth 1, renamed)
/// ```
///
/// `TREE_CLICK_COL` lands inside every one of these rows' own rendered
/// content except `src`'s (whose 3-character label ends well before column
/// 10) — clicking past a short label still hits that row, not a miss,
/// since hit-testing is row-based, never text-based (`files_row_at`'s own
/// docs).
const TREE_CLICK_COL: u16 = 10;

fn spawn_tree_repo() -> (fixture::FixtureRepo, Harness) {
    let repo = fixture::tree_repo();
    let h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff", "--staged"],
            ..Default::default()
        },
    );
    h.wait_for_text("new_name.txt");
    h.wait_for_text("NESTED_MARKER_UNIQUE");
    // `wait_for_text` only proves *some* substring landed, not that every
    // still-in-flight initial-paint frame is done — the no-op click tests
    // below take a "before" snapshot right after this returns, and a frame
    // that lands moments later (unrelated to any click) would make that
    // snapshot spuriously stale. Settling here, once, up front, is cheaper
    // than every caller re-deriving its own guard.
    wait_for_stable_screen(&h, Duration::from_secs(2));
    (repo, h)
}

/// Polls until two reads of the full screen 50ms apart are identical
/// (capped at `timeout`, after which it just returns whatever the last read
/// was) — a stronger settle guard than `wait_for_text` alone; see
/// `spawn_tree_repo`'s own docs on why the no-op click tests need it.
fn wait_for_stable_screen(h: &Harness, timeout: Duration) -> String {
    let deadline = std::time::Instant::now() + timeout;
    let mut previous = h.screen_contents();
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let current = h.screen_contents();
        if current == previous || std::time::Instant::now() >= deadline {
            return current;
        }
        previous = current;
    }
}

#[test]
fn clicking_a_directory_row_toggles_only_that_directory() {
    let (_repo, mut h) = spawn_tree_repo();

    // Row 2 is "nested" — collapsing it hides rows 3/4 ("deep" and the
    // marker file) but must leave "src" (row 1, still expanded) and its
    // other child "new_name.txt" (row 7, a sibling of "nested" — not a
    // descendant) completely alone. Scoped to the sidebar's own columns
    // (issue #26): the marker file now sorts first in canonical (tree)
    // order, so its diff-pane header is on screen regardless of the
    // sidebar's collapse state — a whole-screen check would no longer be
    // an unambiguous witness for the *sidebar* row disappearing.
    click(&h, MouseButton::Left, TREE_CLICK_COL, 2);
    h.wait_until(Duration::from_secs(3), |screen| {
        !region_text(screen, SIDEBAR_COLS.0, SIDEBAR_COLS.1).contains("NESTED_MARKER_UNIQUE")
    });
    h.wait_for_text("\u{25b8}"); // the collapsed glyph now exists somewhere
    assert!(
        h.screen_contents().contains("new_name.txt"),
        "a sibling of the collapsed directory must stay visible"
    );
    assert!(
        h.with_screen(|screen| row_has_reversed_cell(screen, 2, SIDEBAR_COLS.0, SIDEBAR_COLS.1)),
        "the clicked directory row must show as selected (reversed), proving focus moved to Files"
    );

    // Clicking the same screen position again re-expands it: "nested"
    // itself never moved rows (only its descendants were hidden/shown).
    click(&h, MouseButton::Left, TREE_CLICK_COL, 2);
    h.wait_for_text("NESTED_MARKER_UNIQUE");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn clicking_a_file_row_opens_it_keeps_files_focus_and_ctrl_o_reverses_it() {
    let (_repo, mut h) = spawn_tree_repo();

    // A real source location to leave from, reached purely by keyboard.
    // The initial diff cursor now lands on the *first* file in canonical
    // (tree) order (issue #26) — the tiny nested marker file, sorted ahead
    // of every plain file directly under `src` — so `] f` (next-file)
    // moves on to `aaa_padding.txt`'s own header first. Both files
    // together no longer overflow the viewport the way `aaa_padding.txt`
    // alone used to when it was first (its whole point per `tree_repo`'s
    // own docs), so 44 further `j` presses (row 0 = FileHeader, row 1 =
    // HunkHeader, row `n + 1` = content line `n`) land deep enough into
    // its 60 lines that the click below genuinely has to scroll to reach
    // the marker file instead.
    h.send(Key::Char(']'));
    h.send(Key::Char('f'));
    for _ in 0..44 {
        h.send(Key::Char('j'));
    }
    h.wait_for_text("padding line 43");

    // Row 4 is the nested marker file — a different file entirely, and
    // near the very top of the diff pane, so landing there should scroll
    // `padding line 43` out of view.
    click(&h, MouseButton::Left, TREE_CLICK_COL, 4);
    h.wait_for_text("MARKER_CONTENT_LINE_UNIQUE");
    h.wait_until(Duration::from_secs(3), |screen| {
        !region_text(screen, DIFF_COLS.0, DIFF_COLS.1).contains("padding line 43")
    });

    // The req-2 crux: the clicked row reads as *selected* (reversed), not
    // merely as the diff cursor's background file (which would render
    // cyan/underlined instead — see `sidebar::render`'s own docs) — proof
    // that the click left `Files` focused rather than handing it to `Diff`
    // the way `Enter` does.
    assert!(
        h.with_screen(|screen| row_has_reversed_cell(screen, 4, SIDEBAR_COLS.0, SIDEBAR_COLS.1)),
        "the clicked file row must still read as selected — focus stayed on Files"
    );

    // Ctrl-o retraces the click's own jump, landing back on the exact row
    // it left from regardless of which pane currently has keyboard focus.
    h.send(Key::CtrlO);
    h.wait_for_text("padding line 43");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn clicking_an_ancestor_directory_of_the_selection_reselects_that_directory() {
    let (_repo, mut h) = spawn_tree_repo();

    // Tab + navigate onto the nested marker file's own row — req 4 needs a
    // *pre-existing* selection on a descendant before the directory click.
    h.send(Key::Tab);
    h.send(Key::Char('g'));
    h.send(Key::Char('g'));
    h.send(Key::Char('j')); // "nested"
    h.send(Key::Char('j')); // "deep"
    h.send(Key::Char('j')); // the marker file itself

    // Click "nested" (row 2) — its descendant (the marker file, currently
    // selected) is about to be hidden by the collapse. Sidebar-scoped —
    // see `clicking_a_directory_row_toggles_only_that_directory`'s own
    // docs on why a whole-screen check no longer works (issue #26).
    click(&h, MouseButton::Left, TREE_CLICK_COL, 2);
    h.wait_until(Duration::from_secs(3), |screen| {
        !region_text(screen, SIDEBAR_COLS.0, SIDEBAR_COLS.1).contains("NESTED_MARKER_UNIQUE")
    });
    assert!(
        h.with_screen(|screen| row_has_reversed_cell(screen, 2, SIDEBAR_COLS.0, SIDEBAR_COLS.1)),
        "req 4: the directory itself must end up selected once its descendant is hidden"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn scrolled_tree_click_hits_the_right_row() {
    let repo = fixture::many_files_repo(40);
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("file000.txt");

    // Two wheel ticks (3 rows each, per `mouse::WHEEL_SCROLL_ROWS`) shift
    // the sidebar's scroll offset well clear of the top.
    h.send_mouse(MouseKey::ScrollDown { column: 10, row: 8 });
    h.send_mouse(MouseKey::ScrollDown { column: 10, row: 8 });
    h.wait_until(Duration::from_secs(3), |screen| {
        !region_text(screen, SIDEBAR_COLS.0, SIDEBAR_COLS.1).contains("file000.txt")
    });
    // The predicate above can already be true after just the *first* tick
    // (one tick's 3-row offset alone is enough to scroll file000.txt out of
    // view), so it alone doesn't prove the second tick has also landed —
    // without this, reading `target_line` can race a still-in-flight event
    // and see a row that shifts again before the click below reaches it.
    // A short settle, the same fixed-wait idiom
    // `mouse_disabled_via_config_ignores_wheel_bytes` uses, is far cheaper
    // than a debounce-polling loop and comfortably outlasts two PTY writes.
    std::thread::sleep(Duration::from_millis(300));

    // Read whichever file the (now-scrolled) sidebar actually shows at
    // screen row 5, rather than assuming the exact scroll-offset
    // arithmetic — the click below must hit *this* row regardless.
    let target_line = h.with_screen(|screen| {
        region_text(screen, SIDEBAR_COLS.0, SIDEBAR_COLS.1)
            .lines()
            .nth(5)
            .unwrap()
            .to_owned()
    });
    let start = target_line.find("file").expect("row 5 shows a file name");
    let target_file = target_line[start..start + "fileNNN.txt".len()].to_owned();

    click(&h, MouseButton::Left, 10, 5);
    h.wait_until(Duration::from_secs(3), |screen| {
        region_text(screen, DIFF_COLS.0, DIFF_COLS.1).contains(&target_file)
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn border_hint_status_and_blank_space_clicks_are_no_ops() {
    let (_repo, mut h) = spawn_tree_repo();
    let before = h.screen_contents();

    click(&h, MouseButton::Left, TREE_CLICK_COL, 0); // sidebar's own top border/title
    click(&h, MouseButton::Left, TREE_CLICK_COL, 8); // blank space below the last tree row (7)
    click(&h, MouseButton::Left, TREE_CLICK_COL, 29); // the bottom status bar strip

    // No "wait for a change" predicate makes sense here — the whole point
    // is that nothing changes (mirrors `mouse_disabled_via_config_ignores_wheel_bytes`'s
    // own reasoning for a short fixed wait instead).
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        h.screen_contents(),
        before,
        "border/blank/status clicks must leave the screen completely unchanged"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn right_click_on_a_real_tree_row_opens_a_context_menu() {
    // Issue #20-#22 reserved right-click as issue #23's seam (a no-op
    // here, until #23 landed); #23 wires it up to a real context menu — see
    // `tests/e2e/context_menu.rs` for the full open/invoke/close behavior.
    // This stays here as the lightweight regression guard for the seam
    // itself: a right-click on a real tree row is no longer a no-op.
    let (_repo, mut h) = spawn_tree_repo();
    let before = h.screen_contents();

    click(&h, MouseButton::Right, TREE_CLICK_COL, 2); // row 2: "nested"
    h.wait_until(Duration::from_secs(3), |screen| screen.contents() != before);
    assert!(
        h.screen_contents().contains("directory"),
        "right-clicking a directory row must open its context menu; screen:\n{}",
        h.screen_contents()
    );

    h.send(Key::Esc);
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn a_sidebar_click_cannot_reach_the_tree_through_an_open_modal() {
    // The fully-modal overlays (scope menu here; compose/units-setup share
    // the same event-loop gate) intercept every keystroke before it can
    // reach the App — a sidebar click must be blocked the same way, or a
    // mouse could produce a keyboard-unreachable state: tree
    // selection/focus/diff cursor mutating beneath an open modal whose
    // own rect only covers the diff pane.
    let (_repo, mut h) = spawn_tree_repo();
    h.send(Key::Char('o'));
    h.wait_for_text("Working tree");
    let before = wait_for_stable_screen(&h, Duration::from_secs(3));

    click(&h, MouseButton::Left, TREE_CLICK_COL, 4);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        h.screen_contents(),
        before,
        "a tree click while the scope menu is open must change nothing — \
         not the menu, not the selection, not the diff"
    );

    h.send(Key::Esc);
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// The wheel-shaped sibling of the click test above, and the regression
/// test for the bug that motivated the event loop's modal guard on the
/// wheel arm: the three fully-modal overlays record `ScrollTarget` rects
/// spanning only the diff pane, so the sidebar's own `DiffFiles` rect
/// stayed hittable beneath an open scope menu and a wheel tick there
/// scrolled the file list out from under it — a state no keyboard
/// sequence can produce. Part (1) proves these exact wheel bytes at these
/// exact coordinates do scroll the sidebar in this session (and measures
/// how fast), so part (2)'s bounded "nothing moved" wait is sized by
/// evidence rather than hope; part (3) proves the block is scoped to the
/// modal being open, not a sticky disable.
#[test]
fn a_sidebar_wheel_cannot_scroll_the_file_list_through_an_open_modal() {
    let repo = fixture::many_files_repo(30);
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("file000.txt");
    wait_for_settled_diff_render(&h);

    // ---- (1) no modal: the same wheel input provably scrolls ----------
    let started = std::time::Instant::now();
    for _ in 0..6 {
        h.send_mouse(MouseKey::ScrollDown { column: 10, row: 8 });
    }
    h.wait_until(Duration::from_secs(3), |screen| {
        !region_text(screen, SIDEBAR_COLS.0, SIDEBAR_COLS.1).contains("file000.txt")
    });
    let measured_scroll_latency = started.elapsed();

    // Back to the top so part (2) watches the same well-known first row.
    for _ in 0..6 {
        h.send_mouse(MouseKey::ScrollUp { column: 10, row: 8 });
    }
    h.wait_until(Duration::from_secs(3), |screen| {
        region_text(screen, SIDEBAR_COLS.0, SIDEBAR_COLS.1).contains("file000.txt")
    });

    // ---- (2) scope menu open: the identical wheel input is inert ------
    h.send(Key::Char('o'));
    h.wait_for_text("Working tree");
    let sidebar_before =
        h.with_screen(|screen| region_text(screen, SIDEBAR_COLS.0, SIDEBAR_COLS.1));
    for _ in 0..6 {
        h.send_mouse(MouseKey::ScrollDown { column: 10, row: 8 });
    }
    std::thread::sleep((measured_scroll_latency * 3).max(Duration::from_millis(300)));
    let sidebar_after = h.with_screen(|screen| region_text(screen, SIDEBAR_COLS.0, SIDEBAR_COLS.1));
    assert_eq!(
        sidebar_after, sidebar_before,
        "a sidebar wheel while the scope menu is open must not scroll the file list"
    );
    // "Revision" is the menu-only marker here (the "Revision…" entry):
    // "Working tree" also names the scope the view is showing, so it can
    // legitimately appear outside the menu.
    assert!(
        h.screen_contents().contains("Revision"),
        "the scope menu itself must still be open"
    );

    // ---- (3) help open: the same sidebar tick is inert too ------------
    // Help isn't in the wheel arm's coarse guard (its popup is itself
    // wheel-scrollable) — the block for its *uncovered margins* is a
    // separate point-check, so it gets its own half here: column 10 sits
    // left of the centered popup (x = 15 on a 100-column terminal),
    // squarely in the margin the finer check must capture.
    h.send(Key::Esc);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("Revision")
    });
    // Only columns left of the popup (x = 15) stay visible sidebar once
    // help opens, so the before/after comparison is restricted to that
    // margin strip — each row's status letter and the file name's leading
    // digits land inside it, which is enough to betray any scroll.
    let margin_before = h.with_screen(|screen| region_text(screen, 0, 15));
    h.send(Key::Char('?'));
    h.wait_for_text("Navigation");
    for _ in 0..6 {
        h.send_mouse(MouseKey::ScrollDown { column: 10, row: 8 });
    }
    std::thread::sleep((measured_scroll_latency * 3).max(Duration::from_millis(300)));
    let margin_under_help = h.with_screen(|screen| region_text(screen, 0, 15));
    assert_eq!(
        margin_under_help, margin_before,
        "a sidebar-margin wheel while help is open must not scroll the file list"
    );

    // ---- (4) overlays closed: the wheel path works again --------------
    h.send(Key::Esc);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("Navigation")
    });
    for _ in 0..6 {
        h.send_mouse(MouseKey::ScrollDown { column: 10, row: 8 });
    }
    h.wait_until(Duration::from_secs(3), |screen| {
        !region_text(screen, SIDEBAR_COLS.0, SIDEBAR_COLS.1).contains("file000.txt")
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn a_truncated_label_still_resolves_to_the_right_row() {
    // The sidebar's own width (`SIDEBAR_WIDTH`) is a fixed 30 columns
    // regardless of the terminal's total width, so row 4's label —
    // "NESTED_MARKER_UNIQUE.txt", 24 display columns wide, deep enough
    // (depth 3) that indentation alone eats 6 of the inner rect's 28 — is
    // already truncated at this harness's ordinary default width; no
    // extra-narrow terminal needed to exercise it. `files_row_at` only
    // ever does row arithmetic (see its own docs), never inspects rendered
    // text, so hit-testing this row must succeed identically to any other.
    let (_repo, mut h) = spawn_tree_repo();
    let row4 = h.with_screen(|screen| {
        region_text(screen, SIDEBAR_COLS.0, SIDEBAR_COLS.1)
            .lines()
            .nth(4)
            .unwrap()
            .to_owned()
    });
    assert!(
        !row4.contains("NESTED_MARKER_UNIQUE.txt"),
        "row 4's full label must not fit untruncated at the sidebar's fixed width: {row4:?}"
    );

    click(&h, MouseButton::Left, TREE_CLICK_COL, 4);
    h.wait_for_text("MARKER_CONTENT_LINE_UNIQUE");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

// ---- issue #22: code-pane clicks and definitions --------------------------

/// The screen `(row, col)` of the first cell showing `App::active_symbol`'s
/// underline once the cursor is known to be sitting on it — the harness's
/// own way of asking "where does this identifier actually render," rather
/// than hand-computing gutter/sidebar widths (see `a_truncated_label_...`
/// above for why this suite avoids that). Callers park the cursor on the
/// target row via plain keyboard navigation first, read this, then move the
/// cursor away again before clicking — so the click under test is the only
/// thing that ever puts the cursor there.
fn active_symbol_cell(h: &Harness) -> (u16, u16) {
    h.with_screen(|screen| {
        underlined_cells(screen)
            .first()
            .copied()
            .expect("cursor's active symbol must render at least one underlined cell")
    })
}

#[test]
fn a_plain_click_past_any_identifier_only_positions_the_cursor() {
    let repo = fixture::many_files_repo(3);
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("FIRST_MARKER");
    // `wait_for_settled_diff_render` checks for `many_files_repo(30)`'s own
    // `file003.txt` marker, absent from this smaller 3-file fixture —
    // "LAST_MARKER" (file002's own marker) is this fixture's equivalent
    // "the whole diff has rendered" witness.
    h.wait_for_text("LAST_MARKER");

    // file000's own add row ("+alpha FIRST_MARKER"), reached purely by
    // keyboard: FileHeader(0)/HunkHeader(1)/Del(2)/Add(3) — the position to
    // prove `Ctrl-o` returns to.
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 4/");

    // file001's own add row ("+alpha CHANGED") sits at flat index 10 (seven
    // rows per file: FileHeader/HunkHeader/Del/Add/Context/Context/a
    // trailing fold row — this 3-line file's hunk doesn't know it's already
    // at EOF, see `RenderRow::Gap`'s docs), screen row 11 (one border row
    // above the first flat row). Column 90 is deep in the pane's blank
    // trailing space — well past "alpha CHANGED" and its gutter, so no
    // symbol can possibly sit under it regardless of exact gutter
    // arithmetic.
    click(&h, MouseButton::Left, 90, 11);
    h.wait_for_text("\u{b7} 11/");

    std::thread::sleep(Duration::from_millis(300));
    let after_click = h.screen_contents();
    assert!(
        !after_click.contains("goto:"),
        "a gutter/whitespace-past-text click must never attempt LSP work; screen:\n{after_click}"
    );

    // The click's own jump reverses cleanly, same as a keyboard move would.
    h.send(Key::CtrlO);
    h.wait_for_text("\u{b7} 4/");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// Issue #22's close-and-consume rule for the references/units panels,
/// proven through the real event loop: the first click while the
/// "Definitions" panel is open dismisses it and must never *also* act on
/// the pane behind it — the diff pane's rect stays hittable above the
/// panel's bottom strip, so a fallen-through click would silently move
/// the cursor beneath the panel, a state no keyboard sequence can
/// produce. Part (1) calibrates: the same click at the same coordinates,
/// with no panel open, provably repositions the cursor in this session
/// (and measures how fast), so part (2)'s "nothing moved" wait is sized
/// by evidence. The two-`Location` fixture mode exists precisely because
/// no single-location or null definition answer can ever open the panel.
#[test]
fn a_click_while_the_definitions_panel_is_open_closes_it_and_reaches_nothing_beneath() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH (fake LSP server fixture needs it)");
        return;
    }
    let repo = fixture::lsp_readiness_repo_with_two_definition_targets(0.0, 0.0);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );
    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 1/");

    // Land on the GOTO_TARGET_TOKEN add row (FileHeader/HunkHeader/
    // Context/Context/Add), same as the go-to-definition click test below.
    for _ in 0..4 {
        h.send(Key::Char('j'));
    }
    h.wait_for_text("\u{b7} 5/");

    // ---- (1) no panel: this exact click provably moves the cursor -----
    // Located from the rendered screen rather than hardcoded: "alpha" is
    // the hunk's first context row's own text, well right of the
    // line-number gutter (where a click is a no-op) — anywhere that isn't
    // the current row works, since all this part pins down is "these
    // coordinates reposition the cursor at all."
    let (target_col, target_row) = h.with_screen(|screen| {
        let (rows, cols) = screen.size();
        for row in 0..rows {
            let mut text = String::new();
            for col in 0..cols {
                // Blank cells report empty contents — pad them so string
                // index equals screen column (all-ASCII rows here).
                match screen.cell(row, col).map(|c| c.contents()) {
                    Some(c) if !c.is_empty() => text.push_str(c),
                    _ => text.push(' '),
                }
            }
            if let Some(idx) = text.find("alpha") {
                return (idx as u16 + 2, row);
            }
        }
        panic!("the alpha context row must be on screen");
    });
    let started = std::time::Instant::now();
    click(&h, MouseButton::Left, target_col, target_row);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("\u{b7} 5/")
    });
    let measured_click_latency = started.elapsed();

    // Back to the add row for part (2) — via Top, since the click above
    // left the cursor at a row this test deliberately never hardcodes.
    h.send(Key::Char('g'));
    h.send(Key::Char('g'));
    h.wait_for_text("\u{b7} 1/");
    for _ in 0..4 {
        h.send(Key::Char('j'));
    }
    h.wait_for_text("\u{b7} 5/");

    // ---- (2) panel open: the identical click closes it and nothing else
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("Definitions");

    click(&h, MouseButton::Left, target_col, target_row);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("Definitions")
    });
    std::thread::sleep((measured_click_latency * 3).max(Duration::from_millis(300)));
    let screen = h.screen_contents();
    assert!(
        screen.contains("\u{b7} 5/"),
        "the click that dismissed the panel must be consumed, never also \
         reposition the cursor beneath it; screen:\n{screen}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn clicking_an_identifier_on_an_add_row_activates_go_to_definition() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH (fake LSP server fixture needs it)");
        return;
    }
    let repo = fixture::lsp_readiness_repo_with_definition_target(0.0, 0.0);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 1/");

    // Land on the GOTO_TARGET_TOKEN add row (FileHeader/HunkHeader/Context/
    // Context/Add) to read where its one identifier actually renders...
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 5/");
    let (row, col) = active_symbol_cell(&h);

    // ...then move the cursor away again, so the click below is the only
    // thing that ever lands it back there.
    h.send(Key::Char('g'));
    h.send(Key::Char('g'));
    h.wait_for_text("\u{b7} 1/");

    // Even with both fixture delays at `0.0`, the fake server's
    // `initialize` handshake still takes real wall-clock time — retry the
    // click for a bit rather than asserting the very first one wins that
    // race, same reasoning as `esc_pops_a_definition_opened_file_view_back_to_the_diff`.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        click(&h, MouseButton::Left, col, row);
        std::thread::sleep(Duration::from_millis(100));
        if h.screen_contents().contains("other.stub") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "click never reached a Ready server within 5s; screen:\n{}",
            h.screen_contents()
        );
    }
    h.wait_for_text("target line one");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn a_click_while_the_server_is_still_starting_reports_not_ready_and_never_fires_late() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH (fake LSP server fixture needs it)");
        return;
    }
    let repo = fixture::lsp_readiness_repo(5.0, 0.0);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 1/");

    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 5/");
    let (row, col) = active_symbol_cell(&h);

    h.send(Key::Char('g'));
    h.send(Key::Char('g'));
    h.wait_for_text("\u{b7} 1/");

    click(&h, MouseButton::Left, col, row);
    // The click still positions the cursor even though the server isn't
    // ready — only the LSP action itself is gated.
    h.wait_for_text("\u{b7} 5/");
    h.wait_for_text("is starting");
    h.wait_for_text("not ready yet");
    assert!(
        !h.screen_contents().contains("goto: \u{2026}"),
        "a not-ready click must not be overwritten by the generic in-flight \
         ellipsis; screen:\n{}",
        h.screen_contents()
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn a_side_by_side_old_cell_click_positions_but_never_chases_definition() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH (fake LSP server fixture needs it)");
        return;
    }
    let repo = fixture::lsp_readiness_repo_with_definition_target(0.0, 0.0);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 1/");

    h.send(Key::Char('s')); // side-by-side layout

    // The first Context row ("alpha") — unchanged, so it renders
    // identically (and at the same flat index) on both the old and new
    // side; a real symbol either side could otherwise resolve.
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");
    let mut cells = h.with_screen(underlined_cells);
    cells.sort_by_key(|&(_, col)| col);
    assert!(
        cells.len() >= 2,
        "the same Context row must underline its identifier on both sides; got {cells:?}"
    );
    let (old_row, old_col) = cells[0]; // leftmost = the old/left column

    h.send(Key::Char('g'));
    h.send(Key::Char('g'));
    h.wait_for_text("\u{b7} 1/");

    click(&h, MouseButton::Left, old_col, old_row);
    h.wait_for_text("\u{b7} 3/");

    std::thread::sleep(Duration::from_millis(300));
    let after_click = h.screen_contents();
    assert!(
        !after_click.contains("goto:"),
        "an old-side click must never chase go-to-definition; screen:\n{after_click}"
    );
    assert!(
        !after_click.contains("other.stub") && !after_click.contains("target line one"),
        "an old-side click must never navigate anywhere; screen:\n{after_click}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn shift_click_extends_the_visual_selection_and_never_chases_definition() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH (fake LSP server fixture needs it)");
        return;
    }
    let repo = fixture::lsp_readiness_repo_with_definition_target(0.0, 0.0);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 1/");

    // Where the GOTO_TARGET_TOKEN add row's own identifier renders.
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 5/");
    let (row, col) = active_symbol_cell(&h);

    // The anchor: back to the top, down onto the first Context row
    // ("alpha"), then `V` to start a selection there.
    h.send(Key::Char('g'));
    h.send(Key::Char('g'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");
    h.send(Key::Char('V'));
    h.wait_for_text("VISUAL");

    shift_click(&h, col, row);
    h.wait_for_text("\u{b7} 5/");

    let after_click = h.screen_contents();
    assert!(
        !after_click.contains("goto:"),
        "req 4: shift-click must never chase definition, even on a real identifier; \
         screen:\n{after_click}"
    );
    assert!(
        !after_click.contains("other.stub") && !after_click.contains("target line one"),
        "req 4: shift-click must never navigate; screen:\n{after_click}"
    );

    // The selection itself extended down to the clicked row: `y` reports
    // exactly the 3 rows between (and including) the anchor and the
    // shift-clicked target ("alpha", "beta", "GOTO_TARGET_TOKEN") — proof
    // this was a real extension, not just a plain reposition that would
    // have left (or cancelled) a 1-line selection.
    h.send(Key::Char('y'));
    h.wait_for_text("via OSC 52; terminal support required");
    assert!(
        h.screen_contents()
            .contains("yanked 3 line(s) across 1 file(s)"),
        "screen:\n{}",
        h.screen_contents()
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

// ---- issue #23: the `?` help popup gates mouse clicks too -----------------

/// Compose/scope-menu/units-setup are fully modal *and* their recorded
/// rects cover the whole diff pane, so a stray click has nowhere uncovered
/// to land — help is the one overlay where that second half doesn't hold:
/// its centered popup leaves margins on every side (`help::popup_rect`'s
/// 70%/80% sizing at this harness's default 100x30 puts the popup's own
/// right edge at column ~85), so `column: 90` at any row is guaranteed to
/// sit in the uncovered strip while still lining up with a real, otherwise-
/// clickable diff row (the same file000 Add row, at column 90, row 4,
/// `right_click_a_diff_row_and_confirming_add_comment_opens_compose` in
/// `tests/e2e/context_menu.rs` already proves opens "Add comment" and
/// `a_plain_click_past_any_identifier_only_positions_the_cursor` above
/// proves repositions the cursor — both need help's own extra `&& help.is_none()`
/// guard on top of the fully-modal trio's, or this margin would leak
/// clicks straight through to the pane help is supposed to be blocking
/// every keystroke for.
#[test]
fn a_click_in_helps_uncovered_margin_reaches_neither_the_menu_nor_the_diff_pane() {
    let repo = fixture::many_files_repo(3);
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("FIRST_MARKER");
    h.wait_for_text("LAST_MARKER");
    h.wait_for_text("\u{b7} 1/");

    h.send(Key::Char('?'));
    h.wait_for_text("Move down one row");
    h.wait_for_text("Navigation");

    // Right-click: would ordinarily open a context menu (per
    // `right_click_a_diff_row_and_confirming_add_comment_opens_compose`,
    // the identical column/row). No "wait for a change" predicate makes
    // sense for a claim that nothing happens — same fixed-wait idiom
    // `a_sidebar_click_cannot_reach_the_tree_through_an_open_modal` and
    // `border_hint_status_and_blank_space_clicks_are_no_ops` already use
    // for this shape of assertion.
    click(&h, MouseButton::Right, 90, 4);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !h.screen_contents().contains("Add comment"),
        "a right-click in help's own uncovered margin must never open a \
         context menu underneath it; screen:\n{}",
        h.screen_contents()
    );
    assert!(
        h.screen_contents().contains("Navigation"),
        "help must still be the thing on screen; screen:\n{}",
        h.screen_contents()
    );

    // Left-click: would ordinarily reposition the cursor onto row 4 (per
    // `a_plain_click_past_any_identifier_only_positions_the_cursor`).
    click(&h, MouseButton::Left, 90, 4);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        h.screen_contents().contains("\u{b7} 1/"),
        "a left-click in help's own uncovered margin must never move the \
         diff cursor beneath it; screen:\n{}",
        h.screen_contents()
    );

    // Close help and repeat the identical left-click — proving the click
    // mechanism itself really works, and was genuinely gated above rather
    // than coincidentally inert (the same "second click for real" pattern
    // `mouse.rs`'s own directory-toggle test uses for its inverse: proving
    // symmetry rather than trusting a single observation).
    h.send(Key::Esc);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("j/k scroll")
    });
    click(&h, MouseButton::Left, 90, 4);
    h.wait_for_text("\u{b7} 4/");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

// ---- issue #24: debounced pointer details ---------------------------------

/// The one deliberately-real-timing test in this suite's #24 coverage:
/// resting the pointer (no click, no button) on a real identifier for
/// comfortably longer than `pointer_hover::POINTER_HOVER_DEBOUNCE` (400ms)
/// produces a hover popup, sourced from a real (if fake) language server
/// response — `fake_lsp_server.py`'s fixed `HOVER_INFO_UNIQUE` body — without
/// ever touching the keyboard cursor position shown in the status bar.
/// Everything else about the debounce/cancellation state machine is proven
/// deterministically in `ui::pointer_hover`'s own unit tests (`Instant`
/// injected, zero sleeps); this is only here to prove the real event loop —
/// real `Moved` bytes, a real wall clock, a real child-process LSP — wires
/// the same thing together end to end.
#[test]
fn resting_the_pointer_on_a_ready_symbol_shows_a_hover_popup_after_the_debounce() {
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

    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 1/");

    // Land on the GOTO_TARGET_TOKEN add row to read where its one
    // identifier actually renders (FileHeader/HunkHeader/Context/Context/
    // Add — same layout `clicking_an_identifier_on_an_add_row_activates_go_to_definition`
    // above already relies on), then move the keyboard cursor back off it —
    // the debounce below must reach that row purely through the pointer.
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 5/");
    let (row, col) = active_symbol_cell(&h);

    h.send(Key::Char('g'));
    h.send(Key::Char('g'));
    h.wait_for_text("\u{b7} 1/");

    // Even at `init_delay_secs: 0.0`, the fake server's `initialize`
    // handshake still takes real wall-clock time, and passive hover
    // deliberately never calls `ensure_started` itself (req 4) — `warm_up_root`
    // already kicked the server off at session startup regardless, so this
    // is purely "wait for it," the same retry-a-real-race reasoning
    // `clicking_an_identifier_on_an_add_row_activates_go_to_definition`
    // above uses for the identical handshake.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        h.send_mouse(MouseKey::Moved { column: col, row });
        // Comfortably past the 400ms debounce plus the event loop's own
        // up-to-100ms poll latency.
        std::thread::sleep(Duration::from_millis(700));
        if h.screen_contents().contains("HOVER_INFO_UNIQUE") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "pointer rest never produced a hover popup within 5s (server \
             readiness race); screen:\n{}",
            h.screen_contents()
        );
    }

    // The keyboard cursor never left row 1 — passive hover has its own
    // anchor/target entirely separate from `App::cursor` (req 3).
    assert!(h.screen_contents().contains("\u{b7} 1/"));

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// Req 5: any keypress cancels an in-progress debounce outright, before it
/// can ever fire — resting on a real identifier, then pressing a key well
/// inside the 400ms window, must never let the original rest's popup appear
/// even after enough wall-clock time has passed that it otherwise would
/// have.
#[test]
fn a_keypress_before_the_debounce_elapses_cancels_it_and_no_popup_ever_appears() {
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

    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 1/");

    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 5/");
    let (row, col) = active_symbol_cell(&h);

    h.send(Key::Char('g'));
    h.send(Key::Char('g'));
    h.wait_for_text("\u{b7} 1/");

    h.send_mouse(MouseKey::Moved { column: col, row });
    // Well inside the 400ms debounce — nowhere near due yet.
    std::thread::sleep(Duration::from_millis(150));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 2/");
    h.send(Key::Char('k'));
    h.wait_for_text("\u{b7} 1/");

    // Comfortably past where the original rest's debounce would have fired,
    // had the keypress not cancelled it.
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        !h.screen_contents().contains("HOVER_INFO_UNIQUE"),
        "a keypress before the debounce elapses must cancel it outright; screen:\n{}",
        h.screen_contents()
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// Req 5's *second* cancellation hook, alongside the keypress one just
/// above: "any mouse event that isn't bare motion" — a wheel tick included,
/// not only a click — cancels an armed debounce outright. `ui::mod`'s own
/// dispatch checks `mouse_event.kind != MouseEventKind::Moved` once, before
/// it even looks at *which* kind of non-motion event arrived, so a wheel
/// tick and a click share one guard; this test only needs to prove the
/// wheel half, since a click's own cancellation is already implied by every
/// click test elsewhere in this file never once producing a stray popup.
#[test]
fn a_wheel_tick_before_the_debounce_elapses_cancels_it_and_no_popup_ever_appears() {
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

    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 1/");

    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 5/");
    let (row, col) = active_symbol_cell(&h);

    h.send(Key::Char('g'));
    h.send(Key::Char('g'));
    h.wait_for_text("\u{b7} 1/");

    h.send_mouse(MouseKey::Moved { column: col, row });
    // Well inside the 400ms debounce — nowhere near due yet.
    std::thread::sleep(Duration::from_millis(150));
    // A harmless diff-pane wheel tick at the same spot — already proven
    // side-effect-free for cursor position by
    // `wheel_over_the_diff_pane_scrolls_it_not_the_sidebar` — is enough to
    // trip the `!= Moved` guard regardless of what `scroll_at` itself then
    // does with it.
    h.send_mouse(MouseKey::ScrollDown { column: col, row });

    // Comfortably past where the original rest's debounce would have
    // fired, had the wheel tick not cancelled it.
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        !h.screen_contents().contains("HOVER_INFO_UNIQUE"),
        "a wheel tick before the debounce elapses must cancel it outright; screen:\n{}",
        h.screen_contents()
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// Issue #24 reqs 8-9's second target kind: resting the pointer on a
/// file-tree row shows *local* metadata (no LSP round trip — `tree_note_for`
/// reads straight off `App::visible_rows`/`App::files`, already in memory),
/// and it clears the moment the pointer leaves. Two rows prove two of
/// `file_tree::tooltip_line`'s branches (a directory's changed-descendant
/// count, a rename's old→new path) — the plain-file branch has no PTY test
/// of its own either, but shares the exact same dispatch (`fire_pointer_hover`'s
/// `Tree` arm, `tree_note_for`'s one shared call) these two already prove
/// reaches the real event loop, so it would be redundant coverage, not
/// additional risk.
#[test]
fn resting_the_pointer_on_a_tree_row_shows_its_local_metadata_and_clears_on_leave() {
    let (_repo, mut h) = spawn_tree_repo();

    // Row 2 ("src/nested") is a directory; its only descendant with a
    // change is `NESTED_MARKER_UNIQUE.txt`, nested one level deeper still
    // inside "deep" — `descendant_file_count` rolls that up regardless of
    // depth, so the tooltip reports exactly one changed file.
    h.send_mouse(MouseKey::Moved {
        column: TREE_CLICK_COL,
        row: 2,
    });
    // A `wait_until` rather than a fixed sleep-then-assert: the 400ms
    // debounce's expiry is checked by the app's own tick, and under a
    // fully parallel suite run that tick (plus the render behind it) can
    // land arbitrarily later than any margin a sleep would hardcode —
    // waiting on the tooltip itself is the only load-proof shape.
    h.wait_until(Duration::from_secs(3), |screen| {
        screen.contents().contains("src/nested (1 changed)")
    });

    // Row 7 ("src/new_name.txt") is a rename — a different pointer target,
    // so this re-arms a fresh debounce rather than reusing the one above.
    h.send_mouse(MouseKey::Moved {
        column: TREE_CLICK_COL,
        row: 7,
    });
    h.wait_until(Duration::from_secs(3), |screen| {
        screen
            .contents()
            .contains("src/old_name.txt \u{2192} src/new_name.txt")
    });

    // Row 8, blank space below the last tree row — the same no-op position
    // `border_hint_status_and_blank_space_clicks_are_no_ops` uses — resolves
    // to no target at all, so `resolve_target`'s `None` case cancels
    // outright: req 9's "clears when the pointer leaves."
    h.send_mouse(MouseKey::Moved {
        column: TREE_CLICK_COL,
        row: 8,
    });
    h.wait_until(Duration::from_secs(2), |screen| {
        !screen.contents().contains("src/old_name.txt")
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// `[ui] mouse_hover` (issue #24) is a second, independent gate from `[ui]
/// mouse` (issue #20's own config test, `mouse_disabled_via_config_ignores_wheel_bytes`
/// above): disabling it must suppress the passive popup even on a server
/// that's fully `Ready` and would otherwise answer immediately, while
/// leaving ordinary mouse capture — a plain click still positions the
/// cursor — completely untouched, proving the two flags really do gate two
/// separate `MouseEventKind` arms rather than one shared "is the mouse on
/// at all" switch.
#[test]
fn mouse_hover_disabled_via_config_suppresses_the_passive_popup_but_not_clicks() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH (fake LSP server fixture needs it)");
        return;
    }
    let repo = fixture::lsp_readiness_repo(0.0, 0.0);
    // `lsp_readiness_repo` already writes (and, issue #26, commits)
    // `.katamari/config.toml` for the fake server's own `[lsp.servers.stubls]`
    // section (see that function's docs) — append `[ui] mouse_hover = false`
    // to it rather than overwriting, the same one-file-two-concerns shape a
    // real reviewer's own config would have. Committed again immediately:
    // left as an uncommitted change, `.katamari` (a directory) would sort
    // ahead of `main.stub` in the diff pane's canonical order, breaking
    // this test's own fixed-keypress navigation to `main.stub`'s content
    // below — a config edit has nothing to do with what this test covers.
    let config_path = repo.path().join(".katamari").join("config.toml");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str("\n[ui]\nmouse_hover = false\n");
    std::fs::write(&config_path, config).unwrap();
    // `add` just this one file, not `-a`/`-A` — `main.stub`'s own
    // uncommitted `GOTO_TARGET_TOKEN` edit (from `lsp_readiness_repo`) must
    // stay right where this test needs it: in the working-tree diff.
    fixture::git(repo.path(), &["add", ".katamari/config.toml"]);
    fixture::git(repo.path(), &["commit", "-q", "-m", "mouse_hover = false"]);

    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("GOTO_TARGET_TOKEN");
    h.wait_for_text("\u{b7} 1/");

    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 5/");
    let (row, col) = active_symbol_cell(&h);

    h.send(Key::Char('g'));
    h.send(Key::Char('g'));
    h.wait_for_text("\u{b7} 1/");

    // Rest on the ready symbol across the same total real-time budget
    // `resting_the_pointer_on_a_ready_symbol_shows_a_hover_popup_after_the_debounce`
    // gives the identical server-handshake race — a single short wait
    // would risk a vacuous pass (the server just wasn't `Ready` yet, not
    // that the flag suppressed anything); re-sending `Moved` across the
    // full 5s budget rules that out without ever giving `mouse_hover_enabled`
    // a chance to matter.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        h.send_mouse(MouseKey::Moved { column: col, row });
        std::thread::sleep(Duration::from_millis(700));
    }
    assert!(
        !h.screen_contents().contains("HOVER_INFO_UNIQUE"),
        "[ui] mouse_hover = false must suppress the passive popup even on a \
         ready symbol; screen:\n{}",
        h.screen_contents()
    );

    // Mouse capture itself is untouched by this flag — a plain click still
    // positions the cursor (`mouse_hover_enabled` only gates the `Moved`
    // passive-hover arm, a separate guard from `mouse_enabled`/capture
    // itself).
    click(&h, MouseButton::Left, col, row);
    h.wait_for_text("\u{b7} 5/");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
