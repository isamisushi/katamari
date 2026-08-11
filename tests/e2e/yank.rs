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
fn y_writes_a_real_osc_52_sequence_whose_decoded_payload_matches_the_format() {
    // Every other test in this file only checks the status-bar note — the
    // observable witness `ui::mod::handle_action` leaves behind (see this
    // file's module docs on why). `write_osc52` (src/ui/clipboard.rs)
    // writes straight to `io::stdout()`, bypassing ratatui's own `Terminal`
    // — exactly the kind of interleaving/corruption risk only a real
    // compiled binary through a real PTY can surface. `Harness` normally
    // never looks past the parsed screen because `vt100::Parser::new`'s
    // default callbacks silently discard OSC 52 (per its own crate docs);
    // `Harness::last_osc52_clipboard` (tests/support/harness.rs) opts into
    // vt100's `Callbacks::copy_to_clipboard` hook instead, so this test can
    // assert on the actual escape sequence's decoded payload rather than
    // just ktmr's own claim about it.
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 160,
            ..Default::default()
        },
    );

    h.wait_for_text("README.md");
    // Same positioning as `v_j_y_yanks_the_selection_and_reports_counts`:
    // two `j` presses land on row 2 ("# Sample project", the hunk's first
    // context line, old:new 1:1); `V` then one `j` extends onto row 3 (the
    // blank context line right after it, old:new 2:2) — a 2-line selection
    // entirely within README.md's single hunk.
    h.send(Key::Char('j'));
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 3/");

    h.send(Key::Char('V'));
    h.wait_for_text("VISUAL");
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 4/");

    h.send(Key::Char('y'));
    h.wait_for_text("via OSC 52; terminal support required");

    // The documented format (src/ui/clipboard.rs's `format_diff_selection`
    // docs / issue #17 req 4): path header, `old:new | line` column header,
    // then one `old:new | <marker><text>` row per selected line — a space
    // marker for context, blank text for the blank second line.
    let expected = "README.md\nold:new | line\n1:1 |  # Sample project\n2:2 |  ";
    assert_eq!(
        h.last_osc52_clipboard().as_deref(),
        Some(expected),
        "the real OSC 52 payload must decode to the documented format"
    );

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
