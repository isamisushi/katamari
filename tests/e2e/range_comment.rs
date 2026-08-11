//! Issue #19: `c` comments a valid visual selection as one range —
//! end-to-end through the real compiled binary, the same way issue #16's
//! own visual mode (`tests/e2e/visual.rs`) and #17's yank (`tests/e2e/yank.rs`)
//! are. `basic_repo`'s `README.md` rows (see those two files' own docs for
//! the same layout, reused here):
//!
//! ```text
//! 0  FileHeader
//! 1  HunkHeader
//! 2  ctx  new1  "# Sample project"
//! 3  ctx  new2  ""
//! 4  del        "This is line two."
//! 5  add  new3  "This is line two, updated."
//! 6  ctx  new4  "This is line three."
//! 7  ctx  new5  "This is line four."
//! 8  add  new6  "A brand new line five."
//! ```

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::process::Command;
use std::time::Duration;

#[test]
fn selecting_two_context_lines_and_saving_creates_one_range_comment() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            cols: 120,
            ..Default::default()
        },
    );

    h.wait_for_text("README.md");
    // Six `j` presses from row 0 land on row 6, "This is line three."
    // (new_line 4) — see this file's own docs for the row layout.
    for _ in 0..6 {
        h.send(Key::Char('j'));
    }
    h.wait_for_text("\u{b7} 7/");

    h.send(Key::Char('V'));
    h.wait_for_text("VISUAL");
    // One more `j` extends onto row 7, "This is line four." (new_line 5) —
    // both context rows, contiguous, one file: a valid range.
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 8/");

    h.send(Key::Char('c'));
    h.wait_for_text("README.md:4-5");

    for c in "looks good".chars() {
        h.send(Key::Char(c));
    }
    h.send(Key::CtrlS);
    h.wait_for_text("comment: saved");

    let contents = h.screen_contents();
    assert!(
        !contents.contains("VISUAL"),
        "a successful save must clear the selection; screen:\n{contents}"
    );
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
    // The comment body renders once, under the range's first line — not
    // duplicated under both.
    assert_eq!(
        contents.matches("looks good").count(),
        1,
        "the saved comment's body must appear exactly once; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn selecting_across_a_deletion_is_refused_and_keeps_the_selection() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("README.md");
    // Four `j` presses land on row 4, the deleted "This is line two." row.
    for _ in 0..4 {
        h.send(Key::Char('j'));
    }
    h.wait_for_text("\u{b7} 5/");

    h.send(Key::Char('V'));
    h.wait_for_text("VISUAL");
    // Extend onto row 5, the replacement add row — the selection now spans
    // a deletion and its replacement, which the range rules always refuse.
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 6/");

    h.send(Key::Char('c'));
    h.wait_for_text("comment: selection includes a deleted line");

    let contents = h.screen_contents();
    assert!(
        contents.contains("VISUAL"),
        "a rejected range must never clear the selection; screen:\n{contents}"
    );
    assert!(
        !contents.contains("C-s save"),
        "a rejected range must never open the compose overlay; screen:\n{contents}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// Issue #12's fix (`db40b06`) moved `Action::Quit`'s resolver arm to
/// *after* the compose/search/revision/help-filter `else if` chain in
/// `ui::mod::run`'s event loop (see `ui::compose::handle_key`'s own doc
/// comment: routing compose's raw prose through the keymap resolver first
/// "would fire `Action::Quit` on a stray `q` instead of typing one"). That
/// ordering has no compiler check behind it — a future reshuffle of the
/// chain would silently turn every `q` typed into an open comment back
/// into an app-wide quit, and nothing purely in-process (calling
/// `compose::handle_key` directly, the way its own unit tests do) can
/// prove the *top-level dispatch* still reaches compose before the
/// resolver at all. This sends a real `q` through the real event loop and
/// checks both halves: the character actually lands as text, and the
/// session is still the same live process afterward, not a fresh one that
/// happened to leave stale content on screen.
#[test]
fn typing_q_while_composing_inserts_it_as_text_rather_than_quitting() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("README.md");
    for _ in 0..6 {
        h.send(Key::Char('j'));
    }
    h.wait_for_text("\u{b7} 7/");

    h.send(Key::Char('V'));
    h.wait_for_text("VISUAL");
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 8/");

    h.send(Key::Char('c'));
    h.wait_for_text("README.md:4-5");

    // A stray global quit fired on the first `q` here would never send the
    // rest of the string at all (the child would already be tearing down
    // its terminal state), so waiting for the *whole* literal substring —
    // not just checking for the letter `q` in isolation — is what proves
    // every character, `q` included, reached the compose buffer as plain
    // text rather than being intercepted.
    for c in "quick fix".chars() {
        h.send(Key::Char(c));
    }
    h.wait_for_text("quick fix");

    h.send(Key::CtrlS);
    h.wait_for_text("comment: saved");

    let contents = h.screen_contents();
    assert!(
        contents.matches("quick fix").count() == 1,
        "the saved comment's body must appear exactly once: {contents:?}"
    );

    // Belt-and-suspenders: the same process, still alive, still treats a
    // bare `q` at the diff root (no overlay open) as the ordinary global
    // quit it's always been — proving the earlier `q` was special-cased by
    // compose owning input, not by `q` having stopped meaning quit at all.
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn cancelling_compose_on_a_valid_range_leaves_the_same_selection_in_place() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());

    h.wait_for_text("README.md");
    for _ in 0..6 {
        h.send(Key::Char('j'));
    }
    h.wait_for_text("\u{b7} 7/");

    h.send(Key::Char('V'));
    h.wait_for_text("VISUAL");
    h.send(Key::Char('j'));
    h.wait_for_text("\u{b7} 8/");

    h.send(Key::Char('c'));
    h.wait_for_text("README.md:4-5");

    // Esc while composing cancels the overlay without ever reaching the
    // diff view's own Esc handling — the selection must survive untouched
    // (req 6).
    h.send(Key::Esc);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("C-s save")
    });
    h.wait_for_text("VISUAL");
    assert!(
        h.screen_contents().contains("\u{b7} 8/"),
        "cancelling compose must not move the cursor"
    );

    // Re-pressing `c` against the same, still-active selection must derive
    // the exact same target.
    h.send(Key::Char('c'));
    h.wait_for_text("README.md:4-5");

    h.send(Key::Esc);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("C-s save")
    });
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// AGENTS.md's own promised review loop, proven against the real
/// components rather than assumed from the pieces' separate unit tests:
/// "a live `ktmr diff` session picks this up immediately, no restart
/// needed." Commit 1b83c35 wired `ktmr comments add --end-line`/`resolve`
/// through the pre-existing M6 comments watcher
/// (`watch::spawn_comments_watcher`, running unconditionally for every
/// `View::Diff` root regardless of `--no-watch` — see `ui::mod::start_comments`'s
/// docs), and 934eb6b fixed the range case's once-only body render. Nothing
/// in-process can reach the actual mechanism under test here: a background
/// `notify` thread noticing a *second*, real `ktmr` process's file write,
/// `COMMENTS_DEBOUNCE`, and `AppEvent::CommentsChanged` crossing a channel
/// into a running event loop — proven here with zero keypress sent between
/// each CLI write and the screen picking it up.
#[test]
fn a_live_session_picks_up_a_cli_added_range_comment_and_its_resolution_with_no_keypress() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(repo.path(), SpawnOptions::default());
    h.wait_for_text("README.md");

    let add = Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .args([
            "comments",
            "add",
            "README.md",
            "4",
            "reviewed via cli",
            "--end-line",
            "5",
        ])
        .current_dir(repo.path())
        .output()
        .expect("failed to spawn ktmr comments add");
    assert!(
        add.status.success(),
        "ktmr comments add failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr),
    );

    // No keypress between the CLI write above and this wait — the text can
    // only appear here if the live session's own comments watcher noticed
    // the second process's write and pushed a redraw on its own.
    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains("reviewed via cli")
    });

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
        "the live-picked-up range's start row must carry the open marker: {three_row:?}"
    );
    assert!(
        four_row.contains('\u{25C6}'),
        "the live-picked-up range's end row must carry the open marker too: {four_row:?}"
    );

    // No spacing between the `add` whose pickup was just confirmed and the
    // `list`/`resolve` round trips below — deliberately. The resolve's own
    // file write routinely lands inside `watch::COMMENTS_DEBOUNCE`'s 100ms
    // window of the add's forwarded signal on a warm run, which makes this
    // half of the test double as live coverage of the watcher's
    // defer-not-drop guarantee: a write inside the window must produce a
    // deferred signal at the window's expiry, not vanish (see
    // `CommentsWatchSession::run` — under the old drop-inside-the-window
    // throttle, this exact back-to-back CLI sequence wedged on a screen
    // that never dims, 100% reproducibly).

    let list = Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .args(["comments", "list", "--json"])
        .current_dir(repo.path())
        .output()
        .expect("failed to spawn ktmr comments list");
    assert!(
        list.status.success(),
        "ktmr comments list --json failed:\nstderr: {}",
        String::from_utf8_lossy(&list.stderr),
    );
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    let line = list_stdout.lines().next().expect(
        "comments list --json should print exactly one line for the one comment added above",
    );
    let value: serde_json::Value =
        serde_json::from_str(line).expect("comments list --json must print valid JSON");
    let id = value["id"].as_str().expect("comment id").to_owned();

    let resolve = Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .args(["comments", "resolve", &id])
        .current_dir(repo.path())
        .output()
        .expect("failed to spawn ktmr comments resolve");
    assert!(
        resolve.status.success(),
        "ktmr comments resolve failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&resolve.stdout),
        String::from_utf8_lossy(&resolve.stderr),
    );

    // Again: no keypress. The dimmed marker and the `[resolved ...]` header
    // only appear here if the watcher picked up this second write too.
    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains('\u{25C7}')
    });
    let contents = h.screen_contents();
    assert!(
        contents.contains("resolved"),
        "the comment block's header must relabel to resolved: {contents:?}"
    );
    let three_row = contents
        .lines()
        .find(|l| l.contains("This is line three."))
        .expect("range start row");
    let four_row = contents
        .lines()
        .find(|l| l.contains("This is line four."))
        .expect("range end row");
    assert!(
        three_row.contains('\u{25C7}'),
        "the resolved range's start row must switch to the dim marker: {three_row:?}"
    );
    assert!(
        four_row.contains('\u{25C7}'),
        "the resolved range's end row must switch to the dim marker too: {four_row:?}"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

/// `ktmr comments add`'s `--end-line` validation, through the real
/// compiled binary rather than `build_add_comment` called directly as a
/// Rust function (the unit tests' own coverage, in `src/main.rs`'s `mod
/// tests`) — the gap being exactly clap's derive mapping of `--end-line`
/// onto `Add::end_line` (a plausible rename typo site with no compiler
/// check tying it to the unit tests, which never touch clap at all) and
/// `main`'s `Result<()> -> stderr/exit-code` convention on a validation
/// failure.
#[test]
fn cli_add_with_a_reversed_end_line_is_rejected_and_writes_nothing() {
    let repo = fixture::basic_repo();

    let output = Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .args([
            "comments",
            "add",
            "README.md",
            "5",
            "oops",
            "--end-line",
            "4",
        ])
        .current_dir(repo.path())
        .output()
        .expect("failed to spawn ktmr comments add");

    assert!(
        !output.status.success(),
        "a reversed --end-line must be rejected, not accepted"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must not be before the start line"),
        "stderr should explain the rejection: {stderr}"
    );
    assert!(
        !repo
            .path()
            .join(".katamari")
            .join("comments.jsonl")
            .exists(),
        "a rejected add must never write a partial record"
    );
}

/// As above, the happy path: `--end-line` parses and validates, and the
/// resulting range comment reads back correctly through `list` (plain
/// text, `file:start-end` plus the body) and `export --format md` (a
/// `### file:start-end` heading with the covered lines quoted verbatim) —
/// all through the real binary, real argv, and a real `GitSource::discover`
/// repo-root lookup from `current_dir`, none of which `build_add_comment`/
/// `format_export_markdown`'s own unit tests exercise.
#[test]
fn cli_add_list_and_export_round_trip_a_range_comment() {
    let repo = fixture::basic_repo();

    let add = Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .args([
            "comments",
            "add",
            "README.md",
            "4",
            "cli range",
            "--end-line",
            "5",
        ])
        .current_dir(repo.path())
        .output()
        .expect("failed to spawn ktmr comments add");
    assert!(
        add.status.success(),
        "ktmr comments add failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr),
    );
    let add_stdout = String::from_utf8_lossy(&add.stdout);
    assert!(
        add_stdout.contains("README.md:4-5"),
        "add's confirmation line should name the range: {add_stdout}"
    );

    let list = Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .args(["comments", "list"])
        .current_dir(repo.path())
        .output()
        .expect("failed to spawn ktmr comments list");
    assert!(list.status.success());
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(list_stdout.contains("README.md:4-5"), "{list_stdout}");
    assert!(list_stdout.contains("cli range"), "{list_stdout}");

    let export = Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .args(["comments", "export", "--format", "md"])
        .current_dir(repo.path())
        .output()
        .expect("failed to spawn ktmr comments export");
    assert!(export.status.success());
    let export_stdout = String::from_utf8_lossy(&export.stdout);
    assert!(
        export_stdout.contains("### README.md:4-5"),
        "{export_stdout}"
    );
    assert!(
        export_stdout.contains("This is line three."),
        "{export_stdout}"
    );
    assert!(
        export_stdout.contains("This is line four."),
        "{export_stdout}"
    );
    assert!(export_stdout.contains("cli range"), "{export_stdout}");
}
