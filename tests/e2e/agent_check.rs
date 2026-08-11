//! `ktmr agent-check` against `support/fake_acp_agent.py`: the one place
//! the whole ACP client loop — spawn, initialize, session/new, mode
//! selection, a streamed prompt turn, the permission round trip, and the
//! stop reason — runs against a real subprocess over real pipes. The
//! in-process unit tests in `src/acp/` cover framing and the pure
//! helpers; what only this can prove is the thread choreography: the
//! prompt's response receiver and the event pump draining concurrently,
//! with a permission request answered mid-turn while `session/prompt` is
//! still pending.

use crate::support::fixture;
use std::process::Command;

#[test]
fn agent_check_runs_a_full_acp_turn_against_the_fake_agent() {
    if !fixture::python3_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let repo = fixture::basic_repo();
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/fake_acp_agent.py"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .current_dir(repo.path())
        .args([
            "agent-check",
            "--adapter",
            &format!("python3 {script}"),
            "--prompt",
            "fix the marker",
            "--timeout-secs",
            "30",
        ])
        .output()
        .expect("spawn ktmr agent-check");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "agent-check failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    assert!(stdout.contains("initialize: ok (protocol v1)"), "{stdout}");
    assert!(
        stdout.contains("session: fake-session-1 (mode: default)"),
        "{stdout}"
    );
    assert!(stdout.contains("mode: acceptEdits"), "{stdout}");
    assert!(
        stdout.contains("agent: reading the review comments"),
        "{stdout}"
    );
    assert!(
        stdout.contains("permission: allowed Edit acp-marker.txt (allow)"),
        "{stdout}"
    );
    assert!(stdout.contains("stop: end_turn"), "{stdout}");

    // The marker exists iff the fake's "edit" ran after the permission
    // grant — the on-disk witness that the round trip really gated it.
    assert!(
        repo.path().join("acp-marker.txt").is_file(),
        "the fake agent's edit should have landed after permission was granted"
    );
}
