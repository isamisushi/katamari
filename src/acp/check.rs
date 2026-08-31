//! The engine behind `ktmr agent-check` — spawn an adapter, open a
//! session, send one prompt, stream the turn to stdout, exit. Lives here
//! rather than in `main.rs` (where `run_lsp_check` lives) because unlike
//! the LSP check it needs no main.rs plumbing, and keeping it beside the
//! module it smoke-tests keeps the whole spike greppable in one place.
//!
//! Output is line-oriented and stable on purpose: the E2E suite greps
//! these exact prefixes (`adapter:`, `initialize:`, `session:`,
//! `permission:`, `stop:`), the same contract `lsp-check`'s output has.

use super::client::{
    AcpClient, PROTOCOL_VERSION, choose_allow_option, choose_reject_option, describe_update,
    parse_new_session, permission_outcome,
};
use super::transport::AcpEvent;
use crate::vcs::DiffSource;
use crate::vcs::git::GitSource;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long to allow the handshake steps individually. Generous because
/// the npx fallback downloads the adapter package on first use — slow
/// once, cached after.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);

/// The prompt used when none is given: point the agent at the repo's own
/// comment workflow rather than inlining the comments, so the check
/// exercises the same `ktmr comments` round trip a real reviewing agent
/// uses (and the repo's installed review skill, when present, applies).
/// `pub(crate)` so `ui::mod`'s `Action::PushCommentsToAgent` handler can
/// send this exact same text — one source of truth for "what does pushing
/// open comments to the agent actually say," shared by the headless check
/// and the live TUI session rather than two prompts that could drift.
pub(crate) const DEFAULT_PROMPT: &str = "Address the open katamari review comments in this repository: run \
     `ktmr comments list --json` to see them, make each requested change, and mark each one \
     resolved with `ktmr comments resolve <id>`.";

pub fn run(
    prompt: Option<String>,
    adapter_override: Option<String>,
    timeout_secs: u64,
) -> Result<(), String> {
    let resolution = super::adapter::resolve(adapter_override.as_deref())?;
    println!("adapter: {}", resolution.description);

    let source = GitSource::discover(Path::new("."))
        .map_err(|e| format!("not inside a git repository: {e}"))?;
    let cwd_str = source
        .repo_root()
        .map_err(|e| format!("could not resolve the repository root: {e}"))?
        .to_string_lossy()
        .into_owned();

    let client = AcpClient::spawn(resolution.command)
        .map_err(|e| format!("failed to spawn adapter: {e}"))?;
    let events = client
        .transport()
        .take_events()
        .expect("first and only take_events");

    let init = client
        .initialize()
        .recv_timeout(HANDSHAKE_TIMEOUT)
        .map_err(|_| handshake_failure(&client, "initialize produced no response"))?
        .map_err(|e| handshake_failure(&client, &e.to_string()))?;
    match init.get("protocolVersion").and_then(|v| v.as_i64()) {
        Some(v) if v == PROTOCOL_VERSION => println!("initialize: ok (protocol v{v})"),
        // A different (or missing) version is a hard failure, not a
        // curiosity to print: --adapter accepts any command, and speaking
        // v1 shapes at something that answered v2 produces confusing
        // downstream errors this line exists to preempt.
        Some(v) => {
            return Err(format!(
                "agent answered protocol v{v}; this client speaks v{PROTOCOL_VERSION}"
            ));
        }
        None => return Err(format!("initialize result had no protocolVersion: {init}")),
    }

    let new_session = client
        .new_session(&cwd_str)
        .recv_timeout(HANDSHAKE_TIMEOUT)
        .map_err(|_| handshake_failure(&client, "session/new produced no response"))?
        .map_err(|e| handshake_failure(&client, &e.to_string()))?;
    let session = parse_new_session(&new_session)
        .ok_or_else(|| format!("session/new result had no sessionId: {new_session}"))?;
    println!(
        "session: {} (mode: {})",
        session.session_id,
        session.current_mode.as_deref().unwrap_or("none"),
    );

    // acceptEdits auto-approves the agent's file edits while still routing
    // anything else through session/request_permission — the mode a
    // review loop wants. Only requested when the agent offers it, and a
    // refusal is reported rather than fatal: the turn still works in the
    // default mode, it just asks permission for every edit (which the
    // pump below auto-allows anyway).
    if session.available_modes.iter().any(|m| m == "acceptEdits") {
        match client
            .set_mode(&session.session_id, "acceptEdits")
            .recv_timeout(HANDSHAKE_TIMEOUT)
        {
            Ok(Ok(_)) => println!("mode: acceptEdits"),
            Ok(Err(e)) => println!("mode: acceptEdits refused: {e}"),
            Err(_) => println!("mode: acceptEdits unanswered"),
        }
    }

    let text = prompt.as_deref().unwrap_or(DEFAULT_PROMPT);
    let prompt_rx = client.prompt(&session.session_id, text);
    println!("prompt: sent");

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if let Ok(result) = prompt_rx.try_recv() {
            return finish_turn(result);
        }
        if Instant::now() >= deadline {
            let _ = client.cancel(&session.session_id);
            return Err(format!("turn did not finish within {timeout_secs}s"));
        }
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(AcpEvent::Notification { method, params }) => {
                if method == "session/update"
                    && let Some(line) = describe_update(&params)
                {
                    println!("{line}");
                }
            }
            Ok(AcpEvent::Request { id, method, params }) => {
                if method == "session/request_permission" {
                    let tool = params
                        .get("toolCall")
                        .and_then(|t| t.get("title"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("(tool)");
                    match choose_allow_option(&params) {
                        Some(option_id) => {
                            let _ = client
                                .transport()
                                .respond(id, permission_outcome(Some(&option_id)));
                            println!("permission: allowed {tool} ({option_id})");
                        }
                        // No allow option offered: answer *something* (an
                        // unanswered request stalls the turn) — an
                        // explicit reject when one is on the menu, the
                        // cancelled outcome only as the last resort.
                        None => match choose_reject_option(&params) {
                            Some(option_id) => {
                                let _ = client
                                    .transport()
                                    .respond(id, permission_outcome(Some(&option_id)));
                                println!("permission: rejected {tool} ({option_id})");
                            }
                            None => {
                                let _ = client.transport().respond(id, permission_outcome(None));
                                println!("permission: no options for {tool} — cancelled");
                            }
                        },
                    }
                } else {
                    let _ = client.transport().respond_method_not_found(id, &method);
                    println!("request: {method} -> method not found");
                }
            }
            Ok(AcpEvent::Closed { reason }) => {
                // The reader thread completes the prompt's pending slot
                // strictly before it reports Closed, so an adapter that
                // answered the turn and then exited — a normal shape for
                // a short-lived process — has its (successful) result
                // sitting in prompt_rx right now. Only an actually
                // unanswered close is a failure.
                if let Ok(result) = prompt_rx.try_recv() {
                    return finish_turn(result);
                }
                return Err(format!(
                    "agent closed mid-turn ({}); stderr: {}",
                    reason.unwrap_or_else(|| "clean eof".to_string()),
                    tail_or_placeholder(&client),
                ));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("event channel disconnected".to_string());
            }
        }
    }
}

/// Reports the turn's stop reason — the single exit point for both ways
/// a finished turn is noticed (the ordinary poll, and the drain after a
/// `Closed` event raced it).
fn finish_turn(
    result: Result<serde_json::Value, super::transport::AcpError>,
) -> Result<(), String> {
    let outcome = result.map_err(|e| format!("prompt failed: {e}"))?;
    let stop = outcome
        .get("stopReason")
        .and_then(|s| s.as_str())
        .unwrap_or("(none)");
    println!("stop: {stop}");
    Ok(())
}

/// A handshake error message that always carries the adapter's stderr —
/// the difference between "initialize timed out" and "initialize timed
/// out: `sh: claude-agent-acp: not found`".
fn handshake_failure(client: &AcpClient, what: &str) -> String {
    format!("{what}; stderr: {}", tail_or_placeholder(client))
}

fn tail_or_placeholder(client: &AcpClient) -> String {
    let tail = client.transport().stderr_tail();
    if tail.trim().is_empty() {
        "(empty)".to_string()
    } else {
        tail.trim_end().to_string()
    }
}
