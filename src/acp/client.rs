//! The ACP v1 protocol layer over [`crate::acp::transport`]: the
//! `initialize` handshake, session creation, prompt turns, and the pure
//! helpers that interpret the agent's `session/update` stream and
//! `session/request_permission` requests. Only the subset a
//! prompt-and-stream client needs is implemented — katamari advertises no
//! `fs`/`terminal` capabilities, so the agent edits the working tree with
//! its own tools (which is exactly what a diff-review tool wants: the
//! watcher sees those writes and re-diffs live, no protocol plumbing
//! required).
//!
//! Everything here works on `serde_json::Value` plus a few tiny structs
//! rather than a generated type set: ACP has no Rust types crate the way
//! LSP has `lsp-types`, the official SDK is async-shaped (katamari is
//! std-threads only), and the handful of fields this client reads doesn't
//! justify hand-writing the full schema.

use super::transport::{AcpError, Transport};
use serde_json::Value;
use std::process::Command;
use std::sync::mpsc::Receiver;

/// The wire protocol version this client speaks. v2 exists only as a
/// draft (July 2026) that no shipping agent implements; per the spec,
/// `initialize` negotiates downward, so a v2-capable agent still answers
/// a v1 client with v1.
pub const PROTOCOL_VERSION: i64 = 1;

/// What `session/new` came back with: the id every later call needs, and
/// the optional permission-mode surface (the claude adapter exposes
/// `default`/`acceptEdits`/`plan`/`bypassPermissions` here).
#[derive(Debug, Clone)]
pub struct NewSession {
    pub session_id: String,
    pub current_mode: Option<String>,
    pub available_modes: Vec<String>,
}

/// One connected agent process. Dropping it kills the adapter (via the
/// transport's own drop), so no orphaned node process outlives a session.
pub struct AcpClient {
    transport: Transport,
}

impl AcpClient {
    pub fn spawn(command: Command) -> std::io::Result<Self> {
        Ok(Self {
            transport: Transport::spawn(command)?,
        })
    }

    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    /// The `initialize` handshake. Advertises an empty capability set on
    /// purpose: no `fs` (the agent's own tools write the working tree —
    /// see the module docs), no `terminal` (the claude adapter never
    /// calls it anyway). Returns the raw result for callers that want to
    /// inspect `agentCapabilities`/`authMethods`.
    pub fn initialize(&self) -> Receiver<Result<Value, AcpError>> {
        self.transport.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {},
            }),
        )
    }

    /// Creates a session rooted at `cwd` (must be absolute — the agent
    /// resolves every relative path against it). `mcpServers` is required
    /// by the v1 schema even when empty.
    pub fn new_session(&self, cwd: &str) -> Receiver<Result<Value, AcpError>> {
        self.transport.request(
            "session/new",
            serde_json::json!({
                "cwd": cwd,
                "mcpServers": [],
            }),
        )
    }

    /// Switches the session's permission mode (only meaningful when
    /// `session/new` listed the mode as available).
    pub fn set_mode(&self, session_id: &str, mode_id: &str) -> Receiver<Result<Value, AcpError>> {
        self.transport.request(
            "session/set_mode",
            serde_json::json!({
                "sessionId": session_id,
                "modeId": mode_id,
            }),
        )
    }

    /// Sends one prompt turn. The response — just a stop reason — arrives
    /// only when the whole turn ends, so the caller must keep draining
    /// transport events (and answering permission requests) while holding
    /// this receiver, or the agent stalls mid-turn. ACP v1 allows one
    /// active prompt per session; queue further prompts until this one
    /// resolves.
    pub fn prompt(&self, session_id: &str, text: &str) -> Receiver<Result<Value, AcpError>> {
        self.transport.request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session_id,
                "prompt": [ { "type": "text", "text": text } ],
            }),
        )
    }

    /// Cancels the in-flight turn. A notification, not a request — the
    /// agent acknowledges by finishing the turn with `stopReason:
    /// "cancelled"`.
    pub fn cancel(&self, session_id: &str) -> Result<(), AcpError> {
        self.transport.notify(
            "session/cancel",
            serde_json::json!({ "sessionId": session_id }),
        )
    }
}

/// Parses a `session/new` result. The modes shape is nested
/// (`modes.currentModeId` + `modes.availableModes[].id`) and entirely
/// optional — an agent without a mode concept just omits it.
pub fn parse_new_session(result: &Value) -> Option<NewSession> {
    let session_id = result.get("sessionId")?.as_str()?.to_string();
    let modes = result.get("modes");
    let current_mode = modes
        .and_then(|m| m.get("currentModeId"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let available_modes = modes
        .and_then(|m| m.get("availableModes"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some(NewSession {
        session_id,
        current_mode,
        available_modes,
    })
}

/// Picks which option to grant from a `session/request_permission`'s
/// `options` array: the first `allow_once`, else the first `allow_*` of
/// any kind. `allow_once` is preferred over `allow_always` deliberately —
/// an automated approver must not write durable "always allow" state into
/// the agent's memory on the reviewer's behalf. Returns the `optionId`
/// to answer with, or `None` when the agent offered no allow option at
/// all — then [`choose_reject_option`] picks the explicit refusal.
pub fn choose_allow_option(params: &Value) -> Option<String> {
    choose_option(params, "allow_once", "allow")
}

/// The refusal counterpart: the first `reject_once`, else any `reject_*`.
/// ACP's outcome for "the client declines this tool call" is *selecting*
/// an offered reject option; the `cancelled` outcome means the prompt
/// turn itself is being abandoned, which is only the honest answer when
/// the agent offered nothing at all.
pub fn choose_reject_option(params: &Value) -> Option<String> {
    choose_option(params, "reject_once", "reject")
}

fn choose_option(params: &Value, exact_kind: &str, kind_prefix: &str) -> Option<String> {
    let options = params.get("options")?.as_array()?;
    let exact = options.iter().find_map(|o| {
        (o.get("kind")?.as_str()? == exact_kind)
            .then(|| o.get("optionId")?.as_str().map(str::to_string))?
    });
    exact.or_else(|| {
        options.iter().find_map(|o| {
            o.get("kind")?
                .as_str()?
                .starts_with(kind_prefix)
                .then(|| o.get("optionId")?.as_str().map(str::to_string))?
        })
    })
}

/// The reply body granting (or, with `selected=None`, cancelling) a
/// permission request.
pub fn permission_outcome(selected: Option<&str>) -> Value {
    match selected {
        Some(option_id) => serde_json::json!({
            "outcome": { "outcome": "selected", "optionId": option_id }
        }),
        None => serde_json::json!({
            "outcome": { "outcome": "cancelled" }
        }),
    }
}

/// A one-line human description of a `session/update` notification, or
/// `None` for updates that aren't worth a line (chunk types this client
/// doesn't render). Used by `agent-check`'s trace output and unit-tested
/// against the update shapes the claude adapter actually sends.
pub fn describe_update(params: &Value) -> Option<String> {
    let update = params.get("update")?;
    let kind = update.get("sessionUpdate")?.as_str()?;
    match kind {
        "agent_message_chunk" => {
            let text = update.get("content")?.get("text")?.as_str()?;
            Some(format!("agent: {}", text.trim_end()))
        }
        "agent_thought_chunk" => None,
        "tool_call" => {
            let title = update
                .get("title")
                .and_then(|t| t.as_str())
                .or_else(|| update.get("kind").and_then(|k| k.as_str()))
                .unwrap_or("(unnamed)");
            Some(format!("tool: {title}"))
        }
        "tool_call_update" => {
            let status = update.get("status")?.as_str()?;
            // Only terminal states get a line; streaming in_progress
            // updates would flood the trace.
            matches!(status, "completed" | "failed").then(|| format!("tool {status}"))
        }
        "plan" => {
            let n = update
                .get("entries")
                .and_then(|e| e.as_array())
                .map(Vec::len)
                .unwrap_or(0);
            Some(format!("plan: {n} step(s)"))
        }
        "current_mode_update" => {
            let mode = update.get("currentModeId")?.as_str()?;
            Some(format!("mode: {mode}"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_new_session_reads_id_and_modes() {
        let result = serde_json::json!({
            "sessionId": "sess-1",
            "modes": {
                "currentModeId": "default",
                "availableModes": [
                    {"id": "default", "name": "Default"},
                    {"id": "acceptEdits", "name": "Accept Edits"},
                ],
            },
        });
        let parsed = parse_new_session(&result).unwrap();
        assert_eq!(parsed.session_id, "sess-1");
        assert_eq!(parsed.current_mode.as_deref(), Some("default"));
        assert_eq!(parsed.available_modes, vec!["default", "acceptEdits"]);
    }

    #[test]
    fn parse_new_session_tolerates_an_agent_with_no_mode_concept() {
        let parsed = parse_new_session(&serde_json::json!({"sessionId": "s"})).unwrap();
        assert_eq!(parsed.session_id, "s");
        assert!(parsed.current_mode.is_none());
        assert!(parsed.available_modes.is_empty());
    }

    #[test]
    fn choose_allow_option_prefers_allow_once_over_allow_always() {
        let params = serde_json::json!({
            "options": [
                {"optionId": "always", "name": "Always", "kind": "allow_always"},
                {"optionId": "once", "name": "Once", "kind": "allow_once"},
                {"optionId": "no", "name": "No", "kind": "reject_once"},
            ]
        });
        assert_eq!(choose_allow_option(&params).as_deref(), Some("once"));
    }

    #[test]
    fn choose_allow_option_falls_back_to_any_allow_kind() {
        let params = serde_json::json!({
            "options": [
                {"optionId": "no", "name": "No", "kind": "reject_once"},
                {"optionId": "always", "name": "Always", "kind": "allow_always"},
            ]
        });
        assert_eq!(choose_allow_option(&params).as_deref(), Some("always"));
    }

    #[test]
    fn choose_allow_option_returns_none_when_only_rejects_are_offered() {
        let params = serde_json::json!({
            "options": [ {"optionId": "no", "name": "No", "kind": "reject_once"} ]
        });
        assert!(choose_allow_option(&params).is_none());
    }

    #[test]
    fn choose_reject_option_selects_an_explicit_refusal_when_offered() {
        // A reject-only request must be answered by *selecting* the
        // reject option — the cancelled outcome is reserved for "no
        // option fits at all" (see choose_reject_option's docs).
        let params = serde_json::json!({
            "options": [
                {"optionId": "never", "name": "Never", "kind": "reject_always"},
                {"optionId": "no", "name": "No", "kind": "reject_once"},
            ]
        });
        assert_eq!(choose_reject_option(&params).as_deref(), Some("no"));
        assert!(choose_reject_option(&serde_json::json!({"options": []})).is_none());
    }

    #[test]
    fn describe_update_renders_message_chunks_and_tool_calls() {
        let msg = serde_json::json!({
            "sessionId": "s",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "hello\n"},
            }
        });
        assert_eq!(describe_update(&msg).as_deref(), Some("agent: hello"));

        let tool = serde_json::json!({
            "sessionId": "s",
            "update": {"sessionUpdate": "tool_call", "title": "Edit src/api.ts"},
        });
        assert_eq!(
            describe_update(&tool).as_deref(),
            Some("tool: Edit src/api.ts")
        );
    }

    #[test]
    fn describe_update_keeps_thoughts_and_in_progress_noise_off_the_trace() {
        let thought = serde_json::json!({
            "update": {"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}}
        });
        assert!(describe_update(&thought).is_none());

        let in_progress = serde_json::json!({
            "update": {"sessionUpdate": "tool_call_update", "status": "in_progress"}
        });
        assert!(describe_update(&in_progress).is_none());
    }
}
