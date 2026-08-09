//! One language server connection: the `initialize`/`initialized` handshake,
//! the small set of requests M3a needs (`textDocument/didOpen`,
//! `textDocument/hover`), and a graceful shutdown sequence. Everything here
//! speaks in [`lsp_types`] structures — [`crate::lsp::transport::Transport`]
//! underneath knows nothing about LSP semantics, only JSON-RPC framing.

use crate::lsp::transport::{LspError, StderrSink, Transport};
use lsp_types::{
    ClientCapabilities, ClientInfo, DiagnosticClientCapabilities, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidOpenTextDocumentParams, DocumentDiagnosticParams,
    DocumentDiagnosticReportResult, FileEvent, GeneralClientCapabilities, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverClientCapabilities, HoverParams, InitializeParams,
    InitializeResult, InitializedParams, Location, MarkupKind, PartialResultParams, Position,
    PositionEncodingKind, PublishDiagnosticsClientCapabilities, ReferenceContext, ReferenceParams,
    ServerCapabilities, TextDocumentClientCapabilities, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Uri,
    VersionedTextDocumentIdentifier, WindowClientCapabilities, WorkspaceFolder,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// A hover response: `None` when the server has nothing to say about the
/// position (a valid, common outcome — whitespace, punctuation, an unknown
/// symbol — not an error).
pub type HoverResult = Option<Hover>;

/// A `textDocument/definition` response: `None` when the server has no
/// definition to offer (the cursor isn't on a resolvable symbol) — as
/// unremarkable an outcome as an empty hover. The three shapes a server may
/// reply with (a single location, several, or richer `LocationLink`s with
/// separate "origin selection" and "target" ranges) are normalized to a flat
/// list of locations by [`crate::ui::navigation::definition_locations`],
/// which is where callers should look, not here.
pub type DefinitionResult = Option<GotoDefinitionResponse>;

/// A `textDocument/references` response: `None` and `Some(vec![])` are both
/// "found nothing" in practice, but kept distinct because a server
/// returning `None` (no support/no answer) vs. an explicit empty array (0
/// references) are different enough facts that flattening them early would
/// lose information a future caller might want.
pub type ReferencesResult = Option<Vec<Location>>;

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
    capabilities: ServerCapabilities,
    position_encoding: PositionEncodingKind,
    server_name: Option<String>,
    server_version: Option<String>,
}

impl Client {
    /// Spawns `command` and runs the `initialize`/`initialized` handshake to
    /// completion. Blocking — callers run this on a background thread (see
    /// [`crate::lsp::manager`]) so the render loop is never stalled waiting
    /// on a language server's cold start. `initialization_options` is sent
    /// verbatim as `initialize`'s server-specific `initializationOptions`
    /// field — `None` for every server except a katamari-managed
    /// typescript-language-server, which needs it to find its
    /// peer-installed `typescript` (see
    /// [`crate::lsp::adapter::ResolvedServer`]'s docs); every other server
    /// initializes exactly as before this parameter existed.
    #[allow(dead_code)]
    pub fn start(
        command: Command,
        workspace_root: &Path,
        initialization_options: Option<serde_json::Value>,
    ) -> Result<Self, LspError> {
        Self::start_with_stderr_sink(command, workspace_root, initialization_options, None)
    }

    pub fn start_with_stderr_sink(
        command: Command,
        workspace_root: &Path,
        initialization_options: Option<serde_json::Value>,
        stderr_sink: Option<StderrSink>,
    ) -> Result<Self, LspError> {
        let transport = Transport::spawn_with_stderr(command, stderr_sink)?;
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
            initialization_options,
            ..Default::default()
        };

        let rx = transport.request::<InitializeParams, InitializeResult>("initialize", params);
        let result = recv_with_timeout(rx, INITIALIZE_TIMEOUT)
            .map_err(|e| augment_with_stderr(e, &transport.stderr_tail()))?;
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
            server_name: result.server_info.as_ref().map(|info| info.name.clone()),
            server_version: result.server_info.and_then(|info| info.version),
        })
    }

    /// Whether this server advertised `textDocument/definition` support in
    /// its `initialize` response. Checked before issuing the request rather
    /// than just trying it and handling a `MethodNotFound` error, so a
    /// server that never implemented go-to-definition reports "not
    /// supported" immediately instead of after a round trip that was always
    /// going to fail.
    pub fn supports_definition(&self) -> bool {
        self.capabilities.definition_provider.is_some()
    }

    /// As [`Self::supports_definition`], for `textDocument/references`.
    pub fn supports_references(&self) -> bool {
        self.capabilities.references_provider.is_some()
    }

    /// Whether this server advertised `diagnosticProvider` — LSP 3.17's
    /// pull model (`textDocument/diagnostic`), the alternative to unsolicited
    /// `textDocument/publishDiagnostics` notifications. Some servers (the
    /// JetBrains kotlin-lsp, notably) speak *only* the pull model and never
    /// push at all, so [`crate::lsp::manager::LspManager`] uses this the
    /// same way it uses [`Self::supports_definition`]: to decide whether a
    /// request is worth issuing at all, not to interpret a failure after
    /// the fact.
    pub fn supports_diagnostic_pull(&self) -> bool {
        self.capabilities.diagnostic_provider.is_some()
    }

    /// The encoding the server picked for `Position::character` (from the
    /// `[utf-8, utf-16]` this client offered, or `utf-16` if the server
    /// didn't say — the LSP-mandated default). Every position sent to or
    /// read from this server must be converted through
    /// [`crate::diff::ColumnMap`] using this encoding, not assumed.
    pub fn position_encoding(&self) -> &PositionEncodingKind {
        &self.position_encoding
    }

    pub fn server_name(&self) -> Option<&str> {
        self.server_name.as_deref()
    }

    pub fn server_version(&self) -> Option<&str> {
        self.server_version.as_deref()
    }

    pub fn supports_hover(&self) -> bool {
        self.capabilities.hover_provider.is_some()
    }

    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    /// Announces a file's full content to the server. `version` is the
    /// document's LSP version counter — 1 for a file the server has never
    /// seen before; a document that's since changed on disk is resynced via
    /// [`Self::did_change`] instead, never by opening it a second time.
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

    /// Resyncs an already-open document to `content` — the working-tree
    /// watch's way of telling a server "this file changed on disk" for a
    /// document it already has open. Always a full-document replacement
    /// (one `contentChanges` entry with no `range`, i.e.
    /// `TextDocumentSyncKind::FULL`) rather than an incremental edit: this
    /// client never tracks the fine-grained edit that produced the new
    /// content (it only ever sees "the file is different now," from a
    /// filesystem watcher, not a keystroke), so full-document sync is the
    /// only form that's actually available to send, and every server this
    /// client talks to accepts it regardless of which sync kind it
    /// negotiated. `version` must be strictly greater than whatever this
    /// document's last announced version was — callers get that from
    /// [`crate::lsp::manager::LspManager`]'s per-file version counter, never
    /// by guessing.
    pub fn did_change(&self, uri: Uri, version: i32, content: &str) {
        self.transport.notify(
            "textDocument/didChange",
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier { uri, version },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: content.to_owned(),
                }],
            },
        );
    }

    /// Tells the server about changes to files it was never `didOpen`'d
    /// for — a plain notification, not scoped to any one document, so a
    /// server can invalidate whatever project-wide state (dependency
    /// graphs, cross-file resolution caches) it keeps for files outside the
    /// editor's open set. This is the complement to [`Self::did_change`]:
    /// that method resyncs a document's *content* for a server that already
    /// has it open; this one is the only way a server learns about a
    /// filesystem change to a file it doesn't.
    pub fn did_change_watched_files(&self, changes: Vec<FileEvent>) {
        self.transport.notify(
            "workspace/didChangeWatchedFiles",
            DidChangeWatchedFilesParams { changes },
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

    /// Requests go-to-definition at `position`. Callers should check
    /// [`Self::supports_definition`] first — this method itself doesn't,
    /// since a caller occasionally has a reason to ask anyway (`ktmr
    /// lsp-check`'s smoke test, most notably, which wants to see the
    /// server's actual answer or error rather than a client-side guess).
    pub fn definition(
        &self,
        uri: Uri,
        position: Position,
    ) -> Receiver<Result<DefinitionResult, LspError>> {
        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        self.transport.request("textDocument/definition", params)
    }

    /// Requests every reference to the symbol at `position`, including its
    /// declaration (`includeDeclaration: true` — a reviewer asking "where
    /// else is this used" wants the declaration listed alongside the uses,
    /// not filtered out).
    pub fn references(
        &self,
        uri: Uri,
        position: Position,
    ) -> Receiver<Result<ReferencesResult, LspError>> {
        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration: true,
            },
        };
        self.transport.request("textDocument/references", params)
    }

    /// Pulls `uri`'s current diagnostics (LSP 3.17 `textDocument/diagnostic`).
    /// Callers should check [`Self::supports_diagnostic_pull`] first — this
    /// method doesn't, matching [`Self::definition`]'s reasoning.
    /// `previous_result_id` is the `resultId` this client stored from that
    /// document's last pull (see
    /// [`crate::lsp::diagnostics::PulledDocument`]); passing it lets a
    /// server reply with a cheap "unchanged" report instead of resending
    /// identical diagnostics. `None` for a document's first pull, when
    /// there's nothing yet to compare against.
    pub fn pull_diagnostics(
        &self,
        uri: Uri,
        previous_result_id: Option<String>,
    ) -> Receiver<Result<DocumentDiagnosticReportResult, LspError>> {
        let params = DocumentDiagnosticParams {
            text_document: TextDocumentIdentifier { uri },
            identifier: None,
            previous_result_id,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        self.transport.request("textDocument/diagnostic", params)
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

/// Appends `stderr_tail` to `error`'s message when it's non-empty — the fix
/// for a documented failure mode where a language server dies almost
/// immediately for an environment reason (wrong JDK version, a missing
/// dependency, ...) and a naive client just sees its `initialize` request
/// time out or the pipe close, reporting a generic "no response within 30s"
/// that gives no hint why. Called only from [`Client::start`]'s `initialize`
/// failure path (see [`crate::lsp::transport::Transport::stderr_tail`]'s
/// docs), not from every possible transport error site — a request that
/// fails *after* a successful `initialize` already has a running server that
/// answered at least one message correctly, so its stderr is far less likely
/// to explain that particular failure the way it explains a server that
/// never got off the ground at all. `LspError::Closed` carries no message of
/// its own to append to, so it's turned into an `Io` with one; every other
/// variant keeps its shape and just gets its message extended.
fn augment_with_stderr(error: LspError, stderr_tail: &str) -> LspError {
    let tail = stderr_tail.trim();
    if tail.is_empty() {
        return error;
    }
    match error {
        LspError::Io(msg) => LspError::Io(format!("{msg} — server stderr: {tail}")),
        LspError::Json(msg) => LspError::Json(format!("{msg} — server stderr: {tail}")),
        LspError::Server { code, message } => LspError::Server {
            code,
            message: format!("{message} — server stderr: {tail}"),
        },
        LspError::Closed => LspError::Io(format!(
            "lsp transport closed before a response arrived — server stderr: {tail}"
        )),
    }
}

fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        text_document: Some(TextDocumentClientCapabilities {
            hover: Some(HoverClientCapabilities {
                dynamic_registration: Some(false),
                content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
            }),
            // Some servers (typescript-language-server confirmed; others
            // may follow suit) treat this capability as permission to push
            // `textDocument/publishDiagnostics` at all, silently withholding
            // every diagnostic — not just extra detail — when it's absent,
            // even after a normal `didOpen`. rust-analyzer pushes
            // diagnostics unconditionally so this went unnoticed there, but
            // declaring it (with no sub-fields needed; we don't use
            // `relatedInformation`/tag/version support) is required for
            // `]d`/`[d` and the diagnostic gutter to work on TypeScript.
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities::default()),
            // The pull-model counterpart to `publish_diagnostics`, above —
            // required for a server that speaks LSP 3.17's
            // `textDocument/diagnostic` instead of (not in addition to)
            // pushing (kotlin-lsp; see `Client::supports_diagnostic_pull`)
            // to advertise `diagnosticProvider` at all, the same way
            // `publish_diagnostics` gates whether TS pushes anything.
            // `related_document_support: Some(true)` lets a server fold a
            // cross-file diagnostic (e.g. a macro error surfacing in a
            // header) into one response instead of requiring a separate
            // pull per affected document.
            diagnostic: Some(DiagnosticClientCapabilities {
                dynamic_registration: Some(false),
                related_document_support: Some(true),
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

/// The inverse of [`file_uri`]: a `file://` URI (as returned in a
/// `Location`/`LocationLink` from a go-to-definition or references
/// response) back to a filesystem path, percent-decoding every `%XX`
/// escape. Returns `None` for anything not a `file://` URI (a server could
/// in principle point at some other scheme, though none `katamari` talks to
/// does) or that doesn't decode to valid UTF-8.
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let path_part = s.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(path_part.len());
    let mut chars = path_part.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next()?;
            let lo = chars.next()?;
            let hex = [hi, lo];
            let hex_str = std::str::from_utf8(&hex).ok()?;
            bytes.push(u8::from_str_radix(hex_str, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    let decoded = String::from_utf8(bytes).ok()?;
    Some(PathBuf::from(decoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- augment_with_stderr -----------------------------------------------

    #[test]
    fn augment_with_stderr_appends_a_non_empty_tail_to_an_io_error() {
        let err = LspError::Io("no response within 30s".to_owned());
        let augmented = augment_with_stderr(err, "jdtls requires at least Java 21\n");
        assert_eq!(
            augmented.to_string(),
            "lsp transport io error: no response within 30s — server stderr: jdtls requires at \
             least Java 21"
        );
    }

    #[test]
    fn augment_with_stderr_turns_closed_into_an_io_error_with_the_tail_appended() {
        let augmented = augment_with_stderr(LspError::Closed, "segfault\n");
        assert_eq!(
            augmented.to_string(),
            "lsp transport io error: lsp transport closed before a response arrived — server \
             stderr: segfault"
        );
    }

    #[test]
    fn augment_with_stderr_leaves_the_error_untouched_when_stderr_is_empty() {
        let augmented = augment_with_stderr(LspError::Closed, "");
        assert_eq!(
            augmented.to_string(),
            "lsp transport closed before a response arrived"
        );
    }

    #[test]
    fn augment_with_stderr_treats_whitespace_only_stderr_as_empty() {
        let augmented = augment_with_stderr(LspError::Closed, "   \n\n  ");
        assert_eq!(
            augmented.to_string(),
            "lsp transport closed before a response arrived"
        );
    }

    #[test]
    fn augment_with_stderr_extends_a_server_errors_message_without_losing_its_code() {
        let err = LspError::Server {
            code: -32602,
            message: "invalid params".to_owned(),
        };
        let augmented = augment_with_stderr(err, "bad config\n");
        assert!(matches!(augmented, LspError::Server { code: -32602, .. }));
        assert_eq!(
            augmented.to_string(),
            "lsp server error -32602: invalid params — server stderr: bad config"
        );
    }

    #[test]
    fn initialize_capabilities_declare_publish_diagnostics() {
        // Regression guard for a silent-failure class of bug: spec-compliant
        // servers (typescript-language-server confirmed) withhold every
        // `textDocument/publishDiagnostics` notification when the client
        // doesn't declare this capability, so dropping it doesn't fail any
        // request — diagnostics just never arrive. Asserting on the
        // serialized JSON pins the wire shape a server actually sees.
        //
        // Extended to cover `textDocument/diagnostic` (the LSP 3.17 pull
        // model) alongside it, for the same reason: kotlin-lsp advertises
        // `diagnosticProvider` and only answers pulls, so this client must
        // declare `textDocument.diagnostic` or a spec-compliant pull-only
        // server has no obligation to answer sensibly either.
        let caps = serde_json::to_value(client_capabilities()).unwrap();
        assert!(
            caps.pointer("/textDocument/publishDiagnostics").is_some(),
            "initialize must declare textDocument.publishDiagnostics: {caps}"
        );
        assert!(
            caps.pointer("/textDocument/diagnostic").is_some(),
            "initialize must declare textDocument.diagnostic: {caps}"
        );
    }

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

    #[test]
    fn uri_to_path_round_trips_file_uri() {
        let path = Path::new("/tmp/src/main.rs");
        let uri = file_uri(path).unwrap();
        assert_eq!(uri_to_path(&uri).unwrap(), path);
    }

    #[test]
    fn uri_to_path_decodes_percent_escapes() {
        let path = Path::new("/tmp/日本語 dir/main.rs");
        let uri = file_uri(path).unwrap();
        assert_eq!(uri_to_path(&uri).unwrap(), path);
    }

    #[test]
    fn uri_to_path_rejects_non_file_schemes() {
        let uri: Uri = "https://example.com/foo".parse().unwrap();
        assert_eq!(uri_to_path(&uri), None);
    }
}
