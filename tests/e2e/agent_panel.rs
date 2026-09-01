//! The resident ACP agent's TUI integration (`a`/`A`/`p` — see
//! `crate::acp::session`, `ui::ask`, `ui::agent_panel`), driven through the
//! real compiled binary against `support/fake_acp_agent.py`, the same fake
//! `tests/e2e/agent_check.rs` uses for the headless `agent-check` path.
//! What only this suite can prove, that no in-process test can: the
//! permission request actually renders as a TUI modal and is gated on a
//! real human keypress (never auto-granted the way `agent-check`'s own
//! loop grants it), the streamed transcript is visible once the panel is
//! opened, and the session survives a reject and keeps working for a
//! second turn.
//!
//! `[agent].adapter` is deliberately honored only from the *global*
//! `~/.config/katamari/config.toml` (see `AgentConfig`'s docs on why), so
//! every test here points a per-test `$HOME` at a config naming the fake
//! agent rather than writing a repo-local `.katamari/config.toml` the way
//! most of this suite's other fixtures do.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::process::Command;
use std::time::Duration;

/// Points a fresh, isolated `$HOME` at `[agent].adapter = "python3 <fake
/// agent script>"` and returns the `SpawnOptions` a test should hand to
/// [`Harness::spawn`] — `home` must outlive the harness (its `TempDir`
/// would otherwise delete the config out from under a still-running
/// session), so callers keep it alive by binding it alongside the harness.
fn agent_spawn_options() -> (SpawnOptions, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("failed to create a fixture $HOME");
    std::fs::create_dir_all(home.path().join(".config").join("katamari"))
        .expect("failed to create fixture $HOME/.config/katamari");
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/fake_acp_agent.py"
    );
    std::fs::write(
        home.path()
            .join(".config")
            .join("katamari")
            .join("config.toml"),
        format!("[agent]\nadapter = \"python3 {script}\"\n"),
    )
    .expect("failed to write fixture global config.toml");

    let mut opts = SpawnOptions::default();
    opts.extra_env
        .push(("HOME".into(), home.path().as_os_str().to_owned()));
    (opts, home)
}

/// Opens the ask overlay on `basic_repo`'s third row (a real `Context`
/// line — see `tests/e2e/compose.rs::open_compose`'s identical setup) and
/// waits for its hint line, proving the overlay actually opened (shared
/// with the comment overlay — see `compose::render_editor`'s docs).
fn open_ask(h: &Harness) {
    h.wait_for_text("README.md");
    for _ in 0..3 {
        h.send(Key::Char('j'));
    }
    h.send(Key::Char('a'));
    h.wait_for_text("C-s save");
}

fn type_text(h: &Harness, text: &str) {
    for c in text.chars() {
        h.send(Key::Char(c));
    }
}

#[test]
fn ask_streams_the_agents_reply_and_gates_the_edit_on_a_real_keypress() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let repo = fixture::basic_repo();
    let (opts, _home) = agent_spawn_options();
    let mut h = Harness::spawn(repo.path(), opts);

    open_ask(&h);
    type_text(&h, "what changed here?");
    h.send(Key::CtrlS);

    // The permission request must render as a real modal, naming the
    // fake agent's tool — never auto-granted (unlike `agent-check`'s own
    // headless loop).
    h.wait_for_text("agent permission");
    h.wait_for_text("Edit acp-marker.txt");
    assert!(
        !repo.path().join("acp-marker-1.txt").exists(),
        "the edit must not land before a human actually grants permission"
    );

    h.send(Key::Char('y'));
    // Open the panel right away — its own poll-by-revision refresh picks
    // up the transcript as it streams in, so there's no race against
    // exactly when the fake agent's (near-instant) reply arrives.
    h.send(Key::Char('A'));
    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains("received:")
    });
    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains("done")
    });

    assert!(
        repo.path().join("acp-marker-1.txt").is_file(),
        "granting permission must have let the fake agent's edit land"
    );

    // Closing the panel must never kill the session, and reopening it
    // must show the same transcript picked up where it left off — not a
    // fresh, empty one.
    h.send(Key::Esc);
    h.wait_until(Duration::from_secs(3), |screen| {
        !screen.contents().contains("received:")
    });
    h.send(Key::Char('A'));
    h.wait_for_text("received:");
    h.wait_for_text("done");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn rejecting_permission_reports_a_status_and_the_session_still_works_afterward() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let repo = fixture::basic_repo();
    let (opts, _home) = agent_spawn_options();
    let mut h = Harness::spawn(repo.path(), opts);

    open_ask(&h);
    type_text(&h, "explain this line");
    h.send(Key::CtrlS);
    h.wait_for_text("agent permission");
    h.send(Key::Esc); // reject

    h.send(Key::Char('A'));
    h.wait_for_text("turn finished (refusal)");
    assert!(
        !repo.path().join("acp-marker-1.txt").exists(),
        "a rejected permission must never let the edit land"
    );
    h.send(Key::Esc); // close the panel, back to the diff

    // The session must self-heal and take a second turn normally, not be
    // left wedged by the first turn's refusal.
    h.send(Key::Char('a'));
    h.wait_for_text("C-s save");
    type_text(&h, "one more time");
    h.send(Key::CtrlS);
    h.wait_for_text("agent permission");
    h.send(Key::Char('y'));
    h.send(Key::Char('A'));
    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains("turn finished (end_turn)")
    });
    assert!(
        repo.path().join("acp-marker-2.txt").is_file(),
        "the second turn must complete normally after the first was rejected"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn push_comments_to_agent_sends_the_default_prompt_and_opens_the_panel() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let repo = fixture::basic_repo();
    let add = Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .args([
            "comments",
            "add",
            "README.md",
            "3",
            "please double check this line",
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

    let (opts, _home) = agent_spawn_options();
    let mut h = Harness::spawn(repo.path(), opts);
    h.wait_for_text("README.md");

    h.send(Key::Char('p'));
    // `p` opens the panel immediately (see `open_agent_panel`'s docs), so
    // the permission modal renders on top of it — same "must win every
    // hit-test" placement `agent_permission` always gets.
    h.wait_for_text("agent permission");
    h.send(Key::Char('y'));
    // The prompt text `check::DEFAULT_PROMPT` sends starts with this exact
    // sentence — echoed back by the fake agent's `received:` chunk, the
    // one place this suite can assert on the constructed text without
    // parsing raw JSON-RPC off the wire.
    h.wait_until(Duration::from_secs(10), |screen| {
        screen
            .contents()
            .contains("received: Address the open katamari review comments")
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn cancel_key_with_nothing_running_is_a_harmless_no_op() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let repo = fixture::basic_repo();
    let (opts, _home) = agent_spawn_options();
    let mut h = Harness::spawn(repo.path(), opts);
    h.wait_for_text("README.md");

    h.send(Key::CtrlG);
    h.wait_for_text("nothing to cancel");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn cancel_key_stops_a_running_turn_and_a_fresh_ask_works_immediately() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let repo = fixture::basic_repo();
    let (opts, _home) = agent_spawn_options();
    let mut h = Harness::spawn(repo.path(), opts);

    open_ask(&h);
    type_text(&h, "SLOW_CANCELLABLE");
    h.send(Key::CtrlS);

    // Open the panel and prove the turn is genuinely in flight (no
    // permission asked on this path — the fake agent's `SLOW_CANCELLABLE`
    // branch streams a chunk and then just blocks) before cancelling it.
    h.send(Key::Char('A'));
    h.wait_for_text("thinking slowly");

    h.send(Key::CtrlG);
    h.wait_for_text("you: cancelled the turn");
    h.wait_for_text("idle");

    // The session — same adapter process, same session id — must still
    // work right after cancelling: a genuinely fresh, diff-anchored ask
    // isn't blocked by anything left over from the abandoned turn.
    h.send(Key::Esc); // close the panel, back to the diff
    h.send(Key::Char('a')); // same row `open_ask` already put the cursor on
    h.wait_for_text("C-s save");
    type_text(&h, "a fresh question");
    h.send(Key::CtrlS);
    h.wait_for_text("agent permission");
    h.send(Key::Char('y'));
    h.send(Key::Char('A'));
    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains("turn finished (end_turn)")
    });
    assert!(
        repo.path().join("acp-marker-2.txt").is_file(),
        "the fresh turn must complete normally right after a cancel"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn cancel_key_declines_a_pending_permission_and_the_session_still_works() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let repo = fixture::basic_repo();
    let (opts, _home) = agent_spawn_options();
    let mut h = Harness::spawn(repo.path(), opts);

    open_ask(&h);
    type_text(&h, "explain this line");
    h.send(Key::CtrlS);
    h.wait_for_text("agent permission");

    // Cancel, not y/n — must decline the pending request and stop the
    // whole turn in one gesture, unlike n/Esc which only decline that one
    // tool call.
    h.send(Key::CtrlG);
    h.wait_until(Duration::from_secs(5), |screen| {
        !screen.contents().contains("agent permission")
    });

    h.send(Key::Char('A'));
    h.wait_for_text("you: cancelled the turn");
    h.wait_for_text("permission declined");
    assert!(
        !repo.path().join("acp-marker-1.txt").exists(),
        "a cancelled turn must never let the pending edit land"
    );

    // Self-heals: a second, ordinary turn works normally afterward.
    h.send(Key::Esc); // close the panel, back to the diff
    h.send(Key::Char('a'));
    h.wait_for_text("C-s save");
    type_text(&h, "one more time");
    h.send(Key::CtrlS);
    h.wait_for_text("agent permission");
    h.send(Key::Char('y'));
    h.send(Key::Char('A'));
    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains("turn finished (end_turn)")
    });
    assert!(
        repo.path().join("acp-marker-2.txt").is_file(),
        "the second turn must complete normally after the first was cancelled"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn follow_up_from_the_panel_sends_the_next_prompt_into_the_same_session() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let repo = fixture::basic_repo();
    let (opts, _home) = agent_spawn_options();
    let mut h = Harness::spawn(repo.path(), opts);

    // First turn, anchored to a diff row — proves the fixture, then leaves
    // the panel open for the follow-up below.
    open_ask(&h);
    type_text(&h, "what changed here?");
    h.send(Key::CtrlS);
    h.wait_for_text("agent permission");
    h.send(Key::Char('y'));
    h.send(Key::Char('A'));
    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains("turn finished (end_turn)")
    });
    assert!(repo.path().join("acp-marker-1.txt").is_file());

    // `a` from inside the still-open panel: a context-less follow-up, no
    // diff row/selection to re-anchor to — the session already has the
    // transcript so far.
    h.send(Key::Char('a'));
    h.wait_for_text("C-s save");
    h.wait_for_text("ask: follow-up");
    type_text(&h, "and what about performance?");
    h.send(Key::CtrlS);
    h.wait_for_text("you: and what about performance?");
    h.wait_for_text("agent permission");
    h.send(Key::Char('y'));
    // Both turns end in `end_turn` — the first one's own "turn finished"
    // line is still on screen, so a plain `contains` would trivially match
    // it instantly rather than actually waiting for this second turn's
    // completion. Waiting for the *second* occurrence is what proves this
    // wait is really about the follow-up turn.
    h.wait_until(Duration::from_secs(10), |screen| {
        screen
            .contents()
            .matches("turn finished (end_turn)")
            .count()
            >= 2
    });
    assert!(
        repo.path().join("acp-marker-2.txt").is_file(),
        "the follow-up turn must complete normally in the same session"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn follow_up_while_a_turn_is_running_is_rejected_and_names_the_cancel_key() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let repo = fixture::basic_repo();
    let (opts, _home) = agent_spawn_options();
    let mut h = Harness::spawn(repo.path(), opts);

    open_ask(&h);
    type_text(&h, "SLOW_CANCELLABLE");
    h.send(Key::CtrlS);
    h.send(Key::Char('A'));
    h.wait_for_text("thinking slowly");

    h.send(Key::Char('a'));
    h.wait_for_text("C-g");
    assert!(
        !h.screen_contents().contains("C-s save"),
        "the follow-up overlay must never have opened while a turn is running"
    );

    // Clean shutdown even with a turn still in flight: quitting must not
    // wait on the wedged fake agent (see `AgentStore::shutdown`'s docs).
    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn cancel_auto_declines_a_permission_that_straggles_in_after_an_immediate_follow_up_is_attempted() {
    // Regression test for the misattribution bug: a cancelled turn's own
    // `session/request_permission` arriving *after* the reviewer has
    // already attempted a follow-up prompt must never be answered as if it
    // belonged to that follow-up (see `fake_acp_agent.py`'s
    // `STRAGGLING_PERMISSION` docs for exactly the ordering this proves —
    // cancel ack, then an immediate follow-up attempt, then the straggler,
    // then finally the abandoned turn's own response). The follow-up itself
    // must still go through afterward — held (queued), not dropped, while
    // the old turn's own response was still outstanding.
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let repo = fixture::basic_repo();
    let (opts, _home) = agent_spawn_options();
    let mut h = Harness::spawn(repo.path(), opts);

    open_ask(&h);
    type_text(&h, "STRAGGLING_PERMISSION");
    h.send(Key::CtrlS);
    h.send(Key::Char('A'));
    h.wait_for_text("thinking slowly");

    h.send(Key::CtrlG);
    h.wait_for_text("you: cancelled the turn");
    h.wait_for_text("idle");

    // Attempt a follow-up right away, before the straggler (or the
    // abandoned turn's own delayed response) has arrived — this is the
    // exact window the misattribution bug needed: `turn_abandoned` used to
    // clear the instant this was merely *requested*, not once the old
    // turn's own response actually landed. The fake agent holds its final
    // response open for a beat (see its own docs) specifically so this
    // attempt reliably lands inside that window rather than winning a race
    // against local IPC latency.
    h.send(Key::Esc);
    h.send(Key::Char('a'));
    h.wait_for_text("C-s save");
    type_text(&h, "an immediate follow-up");
    h.send(Key::CtrlS);
    h.send(Key::Char('A'));

    // The straggler must be auto-declined, not resurrected as a live
    // permission modal a stray `y` from the reviewer (now mid follow-up)
    // could grant.
    h.wait_for_text("auto-declined permission");
    assert!(
        !h.screen_contents().contains("agent permission"),
        "the straggling permission request must never render as a live modal"
    );
    assert!(
        !repo.path().join("acp-straggler-marker.txt").exists(),
        "the cancelled turn's straggling tool call must never be granted"
    );

    // The queued follow-up must only actually dispatch once the old turn's
    // own response has landed — proving this isn't just "eventually every
    // message shows up somewhere," check the auto-decline is strictly
    // before the follow-up's own line in the transcript.
    h.wait_for_text("an immediate follow-up");
    let screen = h.screen_contents();
    let declined_at = screen
        .find("auto-declined permission")
        .expect("already waited for this text");
    let follow_up_at = screen
        .find("an immediate follow-up")
        .expect("already waited for this text");
    assert!(
        declined_at < follow_up_at,
        "the straggler's auto-decline must be visible before the queued follow-up dispatches, got:\n{screen}"
    );

    // And the queued follow-up must complete as an entirely normal turn —
    // held, not silently dropped, by the draining window it landed in.
    h.wait_for_text("agent permission");
    h.send(Key::Char('y'));
    h.wait_until(Duration::from_secs(10), |screen| {
        screen.contents().contains("turn finished (end_turn)")
    });
    assert!(
        repo.path().join("acp-marker-2.txt").is_file(),
        "the queued follow-up must complete normally once the drain resolved"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn cancel_key_during_a_slow_handshake_takes_effect_immediately_not_after_it_finishes() {
    // Regression test: `TurnState::Spawning`'s "C-g cancel" footer hint
    // used to be inert — a cancel queued while the manager thread was
    // blocked inside `spawn_and_handshake`'s own synchronous
    // `recv_timeout` just sat unprocessed until that step finished or timed
    // out on its own. `wait_for_text`'s default 3s budget (`DEFAULT_WAIT`,
    // far under the artificial handshake delay below) is the actual proof
    // here: if C-g were still inert, this would time out and panic rather
    // than ever reach "idle".
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let repo = fixture::basic_repo();
    let (mut opts, _home) = agent_spawn_options();
    opts.extra_env
        .push(("KATAMARI_FAKE_ACP_HANDSHAKE_DELAY_SECS".into(), "10".into()));
    let mut h = Harness::spawn(repo.path(), opts);

    open_ask(&h);
    type_text(&h, "hello");
    h.send(Key::CtrlS);
    h.send(Key::Char('A'));
    h.wait_for_text("spawning adapter");

    h.send(Key::CtrlG);
    h.wait_for_text("you: cancelled the turn");
    h.wait_for_text("idle");

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn push_comments_to_agent_with_nothing_open_reports_a_status_and_sends_nothing() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let repo = fixture::basic_repo();
    let (opts, _home) = agent_spawn_options();
    let mut h = Harness::spawn(repo.path(), opts);
    h.wait_for_text("README.md");

    h.send(Key::Char('p'));
    h.wait_for_text("no open comments to push");
    assert!(
        !h.screen_contents().contains("agent permission"),
        "nothing should have been sent to the agent at all"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}
