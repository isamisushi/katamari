//! One language server connection: the `initialize`/`initialized` handshake,
//! the small set of requests M3a needs (`textDocument/didOpen`,
//! `textDocument/hover`), and a graceful shutdown sequence. Everything here
//! speaks in [`lsp_types`] structures — [`crate::lsp::transport::Transport`]
//! underneath knows nothing about LSP semantics, only JSON-RPC framing.

use crate::lsp::transport::{LspError, Transport};
use lsp_types::{
    ClientCapabilities, ClientInfo, DidOpenTextDocumentParams, GeneralClientCapabilities, Hover,
    HoverClientCapabilities, HoverParams, InitializeParams, InitializeResult, InitializedParams,
    MarkupKind, Position, PositionEncodingKind, ServerCapabilities, TextDocumentClientCapabilities,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, Uri, WindowClientCapabilities,
    WorkspaceFolder,
};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// A hover response: `None` when the server has nothing to say about the
/// position (a valid, common outcome — whitespace, punctuation, an unknown
/// symbol — not an error).
pub type HoverResult = Option<Hover>;

/// How long [`Client::start`] waits for the `initialize` response before
/// giving up. Generous because a server can be slow to *answer*
/// `initialize` on a cold start (spawning its own worker threads, reading
/// config) even though the much slower project-wide indexing happens
/// afterward, reported via `$/progress`, and isn't waited on here at all —
/// [`crate::lsp::manager::LspManager`] considers the server `Ready` as soon
/// as this handshake completes.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long [`Client::shutdown`] waits for the server to answer the
/// `shutdown` request and then to exit on its own after `exit`, before
/// [`Transport::kill`] forces it. Matches the milestone's "kill after 2s".
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

pub struct Client {
    transport: Transport,
    // Stored for M3b's diagnostics/go-to-definition, which need to check
    // e.g. `definition_provider`/`references_provider` before issuing a
    // request a server never advertised it supports. M3a only ever calls
    // `textDocument/hover` unconditionally.
    #[allow(dead_code)]
    capabilities: ServerCapabilities,
    position_encoding: PositionEncodingKind,
}

impl Client {
    /// Spawns `command` and runs the `initialize`/`initialized` handshake to
    /// completion. Blocking — callers run this on a background thread (see
    /// [`crate::lsp::manager`]) so the render loop is never stalled waiting
    /// on a language server's cold start.
    pub fn start(command: Command, workspace_root: &Path) -> Result<Self, LspError> {
        let transport = Transport::spawn(command)?;
        let root_uri = file_uri(workspace_root)?;
        let workspace_name = workspace_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| workspace_root.display().to_string());

        let params = InitializeParams {
            process_id: Some(std::process::id()),
            #[allow(deprecated)] // `root_uri` is deprecated in favor of
            // `workspace_folders`, but rust-analyzer and several other
            // servers still key their initial project discovery off it;
            // sending both costs nothing and is the compatible choice.
            root_uri: Some(root_uri.clone()),
            capabilities: client_capabilities(),
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root_uri,
                name: workspace_name,
            }]),
            client_info: Some(ClientInfo {
                name: "katamari".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            ..Default::default()
        };

        let rx = transport.request::<InitializeParams, InitializeResult>("initialize", params);
        let result = recv_with_timeout(rx, INITIALIZE_TIMEOUT)?;
        transport.notify("initialized", InitializedParams {});

        let position_encoding = result
            .capabilities
            .position_encoding
            .clone()
            .unwrap_or(PositionEncodingKind::UTF16);

        Ok(Self {
            transport,
            capabilities: result.capabilities,
            position_encoding,
        })
    }

    #[allow(dead_code)] // see the field's comment
    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.capabilities
    }

    /// The encoding the server picked for `Position::character` (from the
    /// `[utf-8, utf-16]` this client offered, or `utf-16` if the server
    /// didn't say — the LSP-mandated default). Every position sent to or
    /// read from this server must be converted through
    /// [`crate::diff::ColumnMap`] using this encoding, not assumed.
    pub fn position_encoding(&self) -> &PositionEncodingKind {
        &self.position_encoding
    }

    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    /// Announces a file's full content to the server. `version` is the
    /// document's LSP version counter — 1 for a file the server has never
    /// seen before; M3a never edits a document after opening it, so callers
    /// never need to send anything above 1.
    pub fn did_open(&self, uri: Uri, language_id: &str, version: i32, content: &str) {
        self.transport.notify(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id: language_id.to_owned(),
                    version,
                    text: content.to_owned(),
                },
            },
        );
    }

    /// Requests hover information at `position` (already converted to this
    /// server's negotiated encoding — see [`Self::position_encoding`]).
    /// Non-blocking; the caller polls or selects on the returned receiver.
    pub fn hover(&self, uri: Uri, position: Position) -> Receiver<Result<HoverResult, LspError>> {
        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position,
            },
            work_done_progress_params: Default::default(),
        };
        self.transport.request("textDocument/hover", params)
    }

    /// `shutdown` request, then `exit` notification, then a bounded wait for
    /// the process to exit on its own before [`Transport::kill`] forces it —
    /// the sequence the LSP spec asks a well-behaved client to follow so the
    /// server can flush/clean up before it goes.
    pub fn shutdown(&self) {
        let rx = self
            .transport
            .request::<(), serde_json::Value>("shutdown", ());
        let _ = recv_with_timeout(rx, SHUTDOWN_GRACE);
        self.transport.notify("exit", ());

        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while Instant::now() < deadline {
            if self.transport.has_exited() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        self.transport.kill();
    }
}

fn recv_with_timeout<R>(
    rx: Receiver<Result<R, LspError>>,
    timeout: Duration,
) -> Result<R, LspError> {
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => {
            Err(LspError::Io(format!("no response within {timeout:?}")))
        }
        Err(RecvTimeoutError::Disconnected) => Err(LspError::Closed),
    }
}

fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        text_document: Some(TextDocumentClientCapabilities {
            hover: Some(HoverClientCapabilities {
                dynamic_registration: Some(false),
                content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
            }),
            ..Default::default()
        }),
        general: Some(GeneralClientCapabilities {
            position_encodings: Some(vec![
                PositionEncodingKind::UTF8,
                PositionEncodingKind::UTF16,
            ]),
            ..Default::default()
        }),
        // Without this, a server isn't allowed to send unsolicited
        // `$/progress` notifications at all (a `workDoneToken` on a
        // *specific* request is a separate mechanism) — rust-analyzer's
        // indexing spinner, which `ui::mod`'s status bar and `ktmr
        // lsp-check`'s progress log both depend on, would simply never
        // arrive without declaring this.
        window: Some(WindowClientCapabilities {
            work_done_progress: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The `TextDocumentSyncCapability` this client would request if it
/// declared one explicitly. M3a never edits documents (it only opens files
/// read-only to hover over them), so nothing in `client_capabilities` above
/// actually asks for change notifications — this exists only as the
/// documented value a future milestone's edit support would wire in, kept
/// here rather than invented fresh so the "full, not incremental" choice the
/// milestone spec calls for is recorded in one place.
#[allow(dead_code)]
fn intended_sync_capability() -> TextDocumentSyncCapability {
    TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)
}

/// Converts an absolute filesystem path to a `file://` URI, percent-encoding
/// every byte outside the URI-safe set (which includes every non-ASCII
/// UTF-8 byte, so a Japanese path round-trips correctly). Callers must pass
/// an absolute path — a relative one would produce a `file://` URI with no
/// well-defined base, which servers are not required to accept.
pub fn file_uri(path: &Path) -> Result<Uri, LspError> {
    let path_str = path
        .to_str()
        .ok_or_else(|| LspError::Io(format!("{} is not valid UTF-8", path.display())))?;

    let mut encoded = String::with_capacity(path_str.len() + 8);
    for byte in path_str.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    let uri_str = if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    };
    uri_str
        .parse::<Uri>()
        .map_err(|e| LspError::Io(format!("invalid file uri for {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_encodes_an_absolute_ascii_path() {
        let uri = file_uri(Path::new("/tmp/src/main.rs")).unwrap();
        assert_eq!(uri.as_str(), "file:///tmp/src/main.rs");
    }

    #[test]
    fn file_uri_percent_encodes_non_ascii_and_spaces() {
        let uri = file_uri(Path::new("/tmp/日本語 dir/main.rs")).unwrap();
        // Each UTF-8 byte of "日本語" becomes its own %XX escape; the space
        // does too, since it's outside the unreserved set.
        assert!(
            uri.as_str()
                .starts_with("file:///tmp/%E6%97%A5%E6%9C%AC%E8%AA%9E%20dir/main.rs")
        );
    }
}
