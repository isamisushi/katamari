//! Talks to language servers over LSP so the diff and file views can offer
//! hover (M3a) and, in later milestones, go-to-definition/references and
//! diagnostics. Three layers, each hiding the one below from the UI:
//! [`transport`] is a generic JSON-RPC-over-stdio pipe with no LSP-specific
//! knowledge beyond the three server-to-client requests every server
//! expects answered; [`client`] speaks the LSP handshake and the specific
//! requests M3a needs (`initialize`, `textDocument/didOpen`,
//! `textDocument/hover`) over one such pipe; [`manager`] and [`adapter`] own
//! *which* server to spawn for a given file and keep exactly one connection
//! alive per (language, workspace root) pair, spawning lazily and queuing
//! requests made before the connection is ready.

pub mod adapter;
pub mod client;
pub mod diagnostics;
pub mod manager;
pub mod transport;

pub use client::{DefinitionResult, HoverResult, ReferencesResult};
pub use diagnostics::{DiagnosticsStore, parse_publish_diagnostics};
pub use manager::{LspManager, ServerEvent};
pub use transport::{LspError, LspEvent};

/// A short human-readable summary of a `$/progress` notification's payload
/// — rust-analyzer's indexing spinner, most notably. `None` for a `"kind":
/// "end"` report, so callers (the TUI's status bar, `ktmr lsp-check`'s
/// progress log) can treat that as "clear the indicator" rather than
/// printing a final "done" flash. General-purpose parsing of a JSON-RPC
/// notification payload belongs here, next to the rest of this module's LSP
/// vocabulary, rather than duplicated in every place that displays it.
pub fn progress_status_text(params: &serde_json::Value) -> Option<String> {
    let value = params.get("value")?;
    if value.get("kind").and_then(serde_json::Value::as_str) == Some("end") {
        return None;
    }
    let title = value.get("title").and_then(serde_json::Value::as_str);
    let message = value.get("message").and_then(serde_json::Value::as_str);
    let percentage = value.get("percentage").and_then(serde_json::Value::as_u64);

    let mut text = title.or(message).unwrap_or("lsp").to_owned();
    // Only append `message` when it's adding information beyond what
    // `text` already holds. `title.or(message)` above means `text` *is*
    // `message` whenever `title` is absent — appending it again in that
    // case would print the same string twice (`"foo: foo"`), not a
    // title/detail pair.
    if let Some(msg) = message
        && title.is_some_and(|t| t != msg)
    {
        text.push_str(&format!(": {msg}"));
    }
    if let Some(pct) = percentage {
        text.push_str(&format!(" ({pct}%)"));
    }
    Some(text)
}
