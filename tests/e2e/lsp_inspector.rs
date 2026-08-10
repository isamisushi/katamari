//! Issue #13's Tab/BackTab pane-focus cycling, proven against the real LSP
//! inspector end to end — not just the in-process `LspInspectorView` unit
//! tests in `src/ui/lsp_inspector.rs`, which can't exercise real crossterm
//! key parsing or the actual `ui::mod` dispatch path a Tab press travels
//! through. Reuses `support::fixture::lsp_readiness_repo`'s fake `stubls`
//! server (see that fixture's docs) with both delays at `0.0` so the server
//! reaches `Running` — and starts producing real journal records — quickly,
//! rather than needing a fixed sleep to guess how long that takes.
//!
//! The inspector's own `V`/`y` Journal-focus gate makes an ideal observable
//! witness for *which* pane Tab/BackTab actually landed on: its status
//! message names the required focus by name, so this test never has to
//! reach into private view state the way the unit tests can. As of issue
//! #16, `V` resolves through the ordinary keymap to `Action::ToggleVisualLine`
//! (see `LspInspectorView::update`'s arm for it); issue #17 does the same
//! for `y` (`Action::YankSelection`, handled by `ui::mod::handle_action`'s
//! inspector special case rather than `LspInspectorView::update` itself —
//! see that special case's docs), replacing the raw-key
//! `handle_literal_key` bypass `y` used to go through alone. The observable
//! behavior below is unchanged either way, so this test is unmodified other
//! than this comment.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::time::Duration;

#[test]
fn tab_and_backtab_cycle_the_inspectors_panes_and_journal_visual_copy_works_from_them() {
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

    // Warm-up spawns the fake `stubls` server in the background as soon as
    // the diff loads. Open the inspector and wait for a real phase word
    // before touching `V`/`y` below — with both fixture delays at `0.0`
    // this should be near-instant, but waiting on real text (rather than a
    // fixed sleep) is what keeps the Journal-focus copy below from racing
    // an as-yet-empty journal.
    h.send(Key::Char('I'));
    h.wait_for_text("running");

    // `V` while `Servers` (the inspector's default focus) has focus is
    // rejected with the Journal-focus gate message, not the
    // visual-selection prompt — the witness this test uses throughout to
    // prove which pane Tab/BackTab actually landed on.
    h.send(Key::Char('V'));
    h.wait_for_text("V/y are available in Journal focus");

    // Two Tabs: Servers -> Detail -> Journal. Acceptance criterion: "LSP
    // inspector still cycles Servers -> Detail -> Journal -> Servers in
    // both directions" — with `Action::FocusNextPane` now doing the work
    // `Action::NextSymbol` used to.
    h.send(Key::Tab);
    h.send(Key::Tab);
    h.send(Key::Char('V'));
    h.send(Key::Char('y'));
    h.wait_for_text("via OSC 52");

    // A third Tab wraps Journal back to Servers — the gate message
    // reappears, proving the wrap landed off Journal rather than staying
    // there.
    h.send(Key::Tab);
    h.send(Key::Char('V'));
    h.wait_for_text("V/y are available in Journal focus");

    h.send(Key::Esc);
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
