//! Issue #17: `y` copies the active visual selection via OSC 52 — proven
//! through the real compiled binary, the same way issue #16's own visual
//! selection is (`tests/e2e/visual.rs`): `ui::mod::handle_action` has no
//! in-process harness of its own (like `fold.rs`'s `ExpandFold`/
//! `CollapseFold`, the actual OSC 52 write only happens inside `run`'s
//! event loop), so the status-bar note is the only observable witness.
//! None of these tests assert that the host terminal/OS clipboard actually
//! received anything — a PTY has no real terminal behind it to accept OSC
//! 52, only `ktmr`'s own side of writing it, which is exactly what the
//! status message reports.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn y_without_a_selection_names_the_required_step() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("README.md");
    h.send(Key::Char('y'));
    h.wait_for_text("yank: press V to select lines first");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn v_j_y_yanks_the_selection_and_reports_counts() {
    let repo = fixture::basic_repo();
    // Wider than the default 100 columns: the success status (repo/position/
    // watch prefix, then "yanked N line(s) across M file(s) (B bytes) via
    // OSC 52; terminal support required") is long enough that a 100-column
    // status bar truncates the trailing caveat this test asserts on.
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("README.md");
    // Same positioning as `visual.rs`: two `j` presses land on row 2, the
    // first real content line.
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");

    h.send(Key::Char('V'));
    h.wait_for_text("VISUAL");
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 4/");

    h.send(Key::Char('y'));
    h.wait_for_text("via OSC 52; terminal support required");
    let contents = h.screen_contents();
    assert!(
        contents.contains("yanked 2 line(s) across 1 file(s)"),
        "screen:\n{contents}"
    );
    assert!(
        !contents.contains("VISUAL"),
        "a successful yank must clear the selection; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn a_second_y_after_a_successful_yank_proves_the_selection_was_cleared() {
    let repo = fixture::basic_repo();
    // Same width reasoning as `v_j_y_yanks_the_selection_and_reports_counts`.
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("README.md");
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");

    h.send(Key::Char('V'));
    h.send(Key::Char('y'));
    h.wait_for_text("via OSC 52; terminal support required");

    // Nothing is selected anymore (req 8: success clears the selection) —
    // a bare `y` right after must ask for a fresh `V`, the same message
    // `y_without_a_selection_names_the_required_step` above proves for a
    // session that never selected anything at all.
    h.send(Key::Char('y'));
    h.wait_for_text("yank: press V to select lines first");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn inspector_yank_reaches_copy_selection_through_the_named_action_resolver() {
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
    h.wait_for_text("running");

    // Two Tabs: Servers -> Detail -> Journal, same as
    // `lsp_inspector.rs`'s own Tab/BackTab test. `V`/`y` here travel
    // through the ordinary keymap resolver to `Action::ToggleVisualLine`/
    // `Action::YankSelection` — issue #17 deleted the raw-key
    // `handle_literal_key` bypass `y` used to go through alone, so this is
    // an end-to-end proof the resolver path still reaches
    // `LspInspectorView::copy_selection`.
    h.send(Key::Tab);
    h.send(Key::Tab);
    h.send(Key::Char('V'));
    h.send(Key::Char('j'));
    h.send(Key::Char('y'));
    h.wait_for_text("via OSC 52; terminal support required");

    h.send(Key::Esc);
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
