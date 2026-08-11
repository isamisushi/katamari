//! Issue #23: the right-click context menu, end to end against the real
//! compiled `ktmr` binary — opening it on an identifier and confirming
//! go-to-definition through a real (if fake — see
//! `support::fixture::lsp_readiness_repo_with_definition_target`) language
//! server, a disabled entry staying open and explaining itself, invoking
//! "Add comment"/toggling a directory through the menu, `Esc` closing it,
//! `q` quitting the whole session while it's open, and the mouse-only
//! retarget/close rule. Mirrors `tests/e2e/scope_menu.rs`'s own split
//! between what a real terminal/subprocess proves and what
//! `ui::context_menu`'s own unit tests already cover in-process.
//!
//! No resize test: `support::Harness` sizes the PTY once, at `spawn` time
//! (see its own docs), with no way to resize it mid-session — the
//! `Event::Resize` arm this issue adds (`ui::mod`'s event loop, right next
//! to the mouse-event match) is a plain three-line unconditional close with
//! nothing view-specific in it, exercised by neither this harness (no
//! resize primitive) nor a unit test (it lives inline in `run`'s match, not
//! as an extractable pure function) — a real gap, not an oversight.

use crate::support::screen::underlined_cells;
use crate::support::{Harness, Key, MouseButton, MouseKey, SpawnOptions, fixture};
use std::time::Duration;

/// A full press-then-release pair, mirroring `tests/e2e/mouse.rs::click` —
/// `ui::mod`'s dispatch only ever reacts to `Down`, but a real terminal
/// always sends both.
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

/// As `tests/e2e/mouse.rs::active_symbol_cell` — the screen `(col, row)` of
/// the cursor's active-symbol underline, read after parking the cursor on
/// the target row via plain keyboard navigation. Duplicated rather than
/// imported: `mouse.rs`'s own copy is a private test-module function, not
/// reachable from this file.
fn active_symbol_cell(h: &Harness) -> (u16, u16) {
    h.with_screen(|screen| {
        underlined_cells(screen)
            .first()
            .copied()
            .map(|(row, col)| (col, row))
            .expect("cursor's active symbol must render at least one underlined cell")
    })
}

/// The screen `(col, row)` of the first cell where `text` starts rendering
/// — how these tests locate a menu entry to left-click without having to
/// know in advance exactly which entries rendered above it (that depends on
/// whether the LSP triad appeared, which most of these tests have no
/// reason to pin down). Panics with the full screen dumped if `text` never
/// appears on one line.
fn find_text(h: &Harness, text: &str) -> (u16, u16) {
    h.with_screen(|screen| {
        let (rows, cols) = screen.size();
        for row in 0..rows {
            let mut line = String::new();
            let mut cols_by_byte = Vec::with_capacity(cols as usize);
            for col in 0..cols {
                if let Some(cell) = screen.cell(row, col) {
                    cols_by_byte.push((line.len(), col));
                    line.push_str(cell.contents());
                }
            }
            if let Some(byte_idx) = line.find(text) {
                let col = cols_by_byte
                    .iter()
                    .rev()
                    .find(|(b, _)| *b <= byte_idx)
                    .map(|(_, c)| *c)
                    .unwrap_or(0);
                return (col, row);
            }
        }
        panic!("no row contains {text:?}; screen:\n{}", screen.contents());
    })
}

#[test]
fn right_click_an_identifier_opens_a_menu_and_confirming_go_to_definition_navigates() {
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
    let (col, row) = active_symbol_cell(&h);

    // ...then move away again, so the right-click below is the only thing
    // that ever lands the cursor back there.
    h.send(Key::Char('g'));
    h.send(Key::Char('g'));
    h.wait_for_text("\u{b7} 1/");

    // Even with both fixture delays at `0.0`, the fake server's
    // `initialize` handshake still takes real wall-clock time — retry the
    // right-click for a bit rather than asserting the very first one wins
    // that race (same shape as `mouse.rs`'s own LSP-gated click tests).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        click(&h, MouseButton::Right, col, row);
        std::thread::sleep(Duration::from_millis(100));
        if h.screen_contents().contains("Go to definition") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "right-click never reached a Ready server within 5s; screen:\n{}",
            h.screen_contents()
        );
    }
    // req 5: an enabled entry, no disabled-reason suffix trailing it.
    assert!(
        !h.screen_contents().contains("Go to definition \u{2014}"),
        "a ready server's entry must render enabled, no reason appended; screen:\n{}",
        h.screen_contents()
    );

    // Down once (Hover documentation -> Go to definition), Enter invokes
    // it through the exact same dispatch keyboard `gd` uses.
    h.send(Key::Char('j'));
    h.send(Key::Enter);

    h.wait_for_text("target line one");
    // The menu itself is gone — replaced by the pushed `FileView`.
    assert!(!h.screen_contents().contains("Go to definition"));

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn right_click_while_the_server_is_starting_shows_a_disabled_reason_and_stays_open_on_confirm() {
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
    let (col, row) = active_symbol_cell(&h);
    h.send(Key::Char('g'));
    h.send(Key::Char('g'));
    h.wait_for_text("\u{b7} 1/");

    // The server hasn't had 5s to become ready yet — this right-click
    // lands well before that.
    click(&h, MouseButton::Right, col, row);
    h.wait_for_text("Go to definition");
    h.wait_for_text("is starting");
    h.wait_for_text("not ready yet");

    // Down to "Go to definition", Enter — disabled, so it must report the
    // reason on the status line (req: teaches, never invokes) and leave the
    // menu open, not close it the way an enabled confirm would.
    h.send(Key::Char('j'));
    h.send(Key::Enter);
    h.wait_for_text("not ready yet");
    assert!(
        h.screen_contents().contains("Go to definition"),
        "a disabled entry's Confirm must never close the menu; screen:\n{}",
        h.screen_contents()
    );
    assert!(
        !h.screen_contents().contains("goto: \u{2026}"),
        "a disabled entry must never actually dispatch a request; screen:\n{}",
        h.screen_contents()
    );

    h.send(Key::Esc);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("Go to definition")
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn right_click_a_diff_row_and_confirming_add_comment_opens_compose() {
    let repo = fixture::many_files_repo(3);
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("FIRST_MARKER");
    h.wait_for_text("LAST_MARKER");

    // file000's own Add row ("+alpha FIRST_MARKER") sits at flat index 3
    // (FileHeader/HunkHeader/Del/Add), screen row 4 (one border row above
    // the first flat row) — the same fixed layout `mouse.rs`'s own click
    // tests rely on. Column 90 is deep in blank trailing space, well past
    // any identifier, so this lands on the row regardless of where its one
    // symbol renders.
    click(&h, MouseButton::Right, 90, 4);
    h.wait_for_text("Add comment");

    let (col, row) = find_text(&h, "Add comment");
    click(&h, MouseButton::Left, col + 1, row);

    h.wait_for_text("comment: file000.txt");
    // The menu closed the instant the click resolved to a real entry.
    assert!(!h.screen_contents().contains("Add comment"));

    h.send(Key::Esc);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("comment: file000.txt")
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn right_click_a_directory_row_and_confirming_the_toggle_collapses_it() {
    let repo = fixture::tree_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            args: vec!["diff", "--staged"],
            ..Default::default()
        },
    );
    h.wait_for_text("new_name.txt");
    h.wait_for_text("NESTED_MARKER_UNIQUE");

    // Row 2 (0 = sidebar's own top border, 1 = "src") is "src/nested" — see
    // `tests/e2e/mouse.rs`'s `spawn_tree_repo` docs for the full fixed
    // layout this test relies on.
    click(&h, MouseButton::Right, 10, 2);
    h.wait_for_text("Collapse directory");
    // "src/nested" has one nested directory ("deep") — the bulk pair must
    // be offered too (req 4).
    h.wait_for_text("Expand all descendants");
    h.wait_for_text("Collapse all descendants");

    let (col, row) = find_text(&h, "Collapse directory");
    click(&h, MouseButton::Left, col + 1, row);

    // Collapsing "src/nested" hides its descendants — "deep" and the
    // marker file nested inside it both disappear from the sidebar, the
    // same unambiguous witness `file_tree.rs`'s own collapse test uses.
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("NESTED_MARKER_UNIQUE")
    });
    assert!(!h.screen_contents().contains("Collapse directory"));

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn esc_closes_the_menu_and_q_quits_the_whole_session_while_one_is_open() {
    let repo = fixture::many_files_repo(3);
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("FIRST_MARKER");
    h.wait_for_text("LAST_MARKER");

    click(&h, MouseButton::Right, 90, 4); // file000's own Add row
    h.wait_for_text("Add comment");
    // The click itself already repositioned the cursor onto the row it
    // targeted (`position_cursor_from_click` — resolving a menu target
    // still moves the cursor, only the identifier-chases-`gd` follow-up is
    // skipped, see `mouse::handle_right_click`'s docs), so the "before"
    // snapshot to compare against is captured *after* that, once the
    // repositioning has already happened but before Esc runs.
    let positioned = h.with_screen(|screen| {
        let mut text = String::new();
        for col in 30..100 {
            if let Some(cell) = screen.cell(4, col) {
                text.push_str(cell.contents());
            }
        }
        text
    });

    // Esc closes exactly the menu — the diff row it positioned stays
    // exactly as it rendered the instant the click landed.
    h.send(Key::Esc);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("Add comment")
    });
    let after_esc = h.with_screen(|screen| {
        let mut text = String::new();
        for col in 30..100 {
            if let Some(cell) = screen.cell(4, col) {
                text.push_str(cell.contents());
            }
        }
        text
    });
    assert_eq!(
        after_esc, positioned,
        "closing the menu must not touch the diff row it positioned"
    );
    assert!(
        !h.screen_contents().contains("comment:"),
        "Esc must close the menu without invoking anything; screen:\n{}",
        h.screen_contents()
    );

    // Reopen it, then prove `q` quits the whole session rather than being
    // swallowed by the menu's own interception (issue #23's binding
    // constraint: the menu is built entirely out of `Action` arms, never a
    // raw-key bypass, so the resolver's global-quit intercept — which runs
    // *before* `handle_action` is ever reached — still sees it).
    click(&h, MouseButton::Right, 90, 4);
    h.wait_for_text("Add comment");
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(
        status.success(),
        "q must quit the whole session even with the menu open, got {status:?}"
    );
}

#[test]
fn right_click_retargets_a_valid_new_row_and_closes_on_a_miss() {
    let repo = fixture::many_files_repo(3);
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("FIRST_MARKER");
    h.wait_for_text("LAST_MARKER");

    // file000's own Add row.
    click(&h, MouseButton::Right, 90, 4);
    h.wait_for_text("Add comment");
    let after_first = h.screen_contents();

    // file001's own Context row (flat index 11: FileHeader(7)/HunkHeader(8)/
    // Del(9)/Add(10)/Context(11) — screen row 12) — a second, different
    // valid target. A retarget must actually move the cursor onto it, not
    // just leave the same popup sitting where it was.
    click(&h, MouseButton::Right, 90, 12);
    h.wait_for_text("Add comment"); // still showing a menu, just retargeted
    h.wait_until(Duration::from_secs(3), |screen| {
        screen.contents() != after_first
    });

    // A right-click on blank space below the sidebar's last row closes the
    // menu outright rather than retargeting onto nothing.
    click(&h, MouseButton::Right, 10, 25);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("Add comment")
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// Issue #23 req 10: a watch-mode refresh closes an open context menu
/// unconditionally, not only when the diff cursor's own row happened to
/// survive the refresh — see `ui::mod::handle_watch_refresh`'s own comment
/// on why a `TreeDir`/`TreeFile` target can't rely on that survival check.
/// Mirrors `tests/e2e/watch.rs::bare_session_refreshes_after_a_working_tree_edit`'s
/// "bare session, no `--no-watch`" shape (the only way to get a real
/// watcher thread) plus this file's own right-click-opens-a-menu setup.
/// Edits `file001.txt` — a file the menu was never anchored to — rather
/// than `file000.txt`: the close is supposed to fire on *any* refresh, so
/// proving it against an unrelated file's edit is the stronger claim, and
/// sidesteps any race between the write landing and the menu's own target
/// row changing.
#[test]
fn a_watch_refresh_closes_an_open_context_menu_and_reports_why() {
    let repo = fixture::many_files_repo(3);
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("FIRST_MARKER");
    h.wait_for_text("LAST_MARKER");

    // file000's own Add row, exactly as this file's other menu tests.
    click(&h, MouseButton::Right, 90, 4);
    h.wait_for_text("Add comment");

    std::fs::write(
        repo.path().join("file001.txt"),
        "alpha CHANGED\nbeta\ngamma\nWATCH_MARKER\n",
    )
    .expect("failed to edit watched fixture file");

    // Same generous, evidence-bounded wait `watch.rs` already tolerates for
    // the watcher's real debounce/reread latency — never a fixed sleep.
    h.wait_until(Duration::from_secs(10), |screen| {
        !screen.contents().contains("Add comment")
    });
    assert!(
        h.screen_contents().contains("closed: file changed"),
        "a watch refresh that closed the menu must say why on the status \
         line; screen:\n{}",
        h.screen_contents()
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// Issue #23's menu is derived from `App::comment_target()`/`App::visual_active()`
/// (`ui::context_menu::diff_row_entries`), and `diff_pane_menu_target`'s own
/// docs are explicit that a right-click repositions the cursor via
/// `App::position_cursor_from_click` but never cancels an active visual
/// selection the way the keyboard's own `Esc`/a failed range-comment save
/// can. This chains that fact through a real right-click: open a `V`
/// selection first (the same two-row selection
/// `tests/e2e/range_comment.rs::selecting_two_context_lines_and_saving_creates_one_range_comment`
/// uses, so its compose-header/marker witnesses apply unchanged here), then
/// right-click a row the selection already covers, and confirm the menu
/// offers the range-labeled comment entry (not a stray single-line one) and
/// still calls the selection "active" rather than "startable."
#[test]
fn right_click_on_an_active_visual_selection_keeps_it_and_opens_the_range_labeled_menu() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("README.md");
    // Six `j` presses land on row 6, "This is line three." (new_line 4) —
    // see `range_comment.rs`'s own docs for this fixture's row layout.
    for _ in 0..6 {
        h.send(Key::Char('j'));
    }
    h.wait_for_text("\u{b7} 7/");

    h.send(Key::Char('V'));
    h.wait_for_text("VISUAL");
    // Extends onto row 7, "This is line four." (new_line 5) — both context
    // rows, contiguous, one file: a valid two-line range.
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 8/");

    // Right-click row 7's own screen row (one border row above its flat
    // index) — the row the selection already extends onto, not a fresh
    // cursor position.
    click(&h, MouseButton::Right, 90, 8);
    h.wait_for_text("Add comment (2 lines)");
    assert!(
        h.screen_contents().contains("Cancel visual selection"),
        "a right-click landing on the selection's own row must not cancel \
         it; screen:\n{}",
        h.screen_contents()
    );
    assert!(
        !h.screen_contents().contains("Start visual selection"),
        "the selection survived, so the startable label must not appear; \
         screen:\n{}",
        h.screen_contents()
    );

    let (col, row) = find_text(&h, "Add comment (2 lines)");
    click(&h, MouseButton::Left, col + 1, row);

    // Same compose target the keyboard `c` path reaches for this identical
    // selection — proving the click's row resolution landed on the same
    // range, not a collapsed single line.
    h.wait_for_text("README.md:4-5");

    for c in "right-click range".chars() {
        h.send(Key::Char(c));
    }
    h.send(Key::CtrlS);
    h.wait_for_text("comment: saved");

    let contents = h.screen_contents();
    let three_row = contents
        .lines()
        .find(|l| l.contains("This is line three."))
        .expect("range start row");
    let four_row = contents
        .lines()
        .find(|l| l.contains("This is line four."))
        .expect("range end row");
    assert!(
        three_row.contains('\u{25C6}'),
        "the range's start row must carry the open marker: {three_row:?}"
    );
    assert!(
        four_row.contains('\u{25C6}'),
        "the range's end row must carry the open marker too: {four_row:?}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
