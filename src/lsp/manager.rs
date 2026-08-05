//! Owns every language server this session spawns: lazily starts one per
//! `(language, workspace root)` the first time something needs it, keeps it
//! alive for the rest of the session, and queues requests made before it's
//! ready instead of making the caller wait or retry.
//! [`LspManager::hover`]/[`LspManager::definition`]/[`LspManager::references`]
//! are the request entry points the UI calls — each always returns
//! immediately with a `Receiver`, whether the server is already `Ready`,
//! still `Starting` (the request is queued), or will never work at all (the
//! `Receiver` gets its one `Err` right away, no spawning required). All
//! three share one dispatch state machine (see [`Op`]/[`Self::submit`]);
//! only what happens once a connection is available differs between them.

use crate::diff::ColumnMap;
use crate::lsp::adapter::{self, Language};
use crate::lsp::client::{Client, DefinitionResult, HoverResult, ReferencesResult, file_uri};
use crate::lsp::transport::{LspError, LspEvent};
use lsp_types::{FileChangeType, FileEvent, Position, PositionEncodingKind};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a single hover round-trip is allowed to take before this
/// manager gives up on it and reports a timeout — long enough to cover a
/// busy server still catching up on indexing, short enough that a truly
/// wedged server doesn't leave a request pending for the rest of the
/// session. Go-to-definition and find-references share the same budget:
/// none of the three is expected to be meaningfully slower than the others
/// against a healthy server.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The most a single server's not-yet-ready queue will hold. Bounded
/// because a user tapping `K`/`gd`/`gr` repeatedly while a server is still
/// starting shouldn't accumulate unbounded queued work — only the most
/// recent requests are worth answering anyway, since [`crate::ui`]'s
/// generation counter discards stale hover responses on arrival regardless,
/// and a stale go-to-definition/references answer is just as likely to be
/// unwanted.
const MAX_QUEUED_PER_SERVER: usize = 4;

/// How many files [`LspManager::warm_up`] will proactively `didOpen` for a
/// single call — see that method's docs for why this exists at all.
const WARM_UP_CAP: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    NotStarted,
    Starting,
    Ready,
    Unavailable { reason: String },
    Crashed { reason: String },
}

type ServerKey = (Language, PathBuf);

/// One spawned server's events, tagged with which server sent them —
/// M3a forwarded these unlabeled (fine with exactly one server per
/// session); M3b can run up to four languages concurrently, so a
/// `$/progress` tick or `publishDiagnostics` notification needs to say
/// which server it came from for a caller juggling more than one.
#[derive(Debug, Clone)]
pub struct ServerEvent {
    pub language: Language,
    /// The workspace root half of this event's server key — kept alongside
    /// `language` for symmetry and because a future multi-root session
    /// (two Rust workspaces open in the same review, say) will need it to
    /// disambiguate two servers that share a language; no caller needs
    /// that yet, so nothing reads it today.
    #[allow(dead_code)]
    pub root: PathBuf,
    pub event: LspEvent,
}

/// What to do once a server is `Ready` and the target file/position are
/// known. Each variant owns the channel its result is reported through, so
/// a queued or in-flight request can be resolved (with a result) or failed
/// (with an error) without the caller needing to know which of the three
/// request kinds it is — see [`Op::fail`].
enum Op {
    Hover(Sender<Result<HoverResult, LspError>>),
    Definition(Sender<Result<DefinitionResult, LspError>>),
    References(Sender<Result<ReferencesResult, LspError>>),
    /// No LSP request at all — just makes sure the file has been
    /// `didOpen`'d, so diagnostics start flowing for it. The sender only
    /// ever carries `Ok(())`; nothing currently reads the receiving half,
    /// but a proper `Result` (rather than discarding the outcome
    /// silently) keeps this variant honest about the one way it, too, can
    /// fail — the file couldn't be read, or no server is configured — the
    /// same as every other `Op`.
    WarmUp(Sender<Result<(), LspError>>),
}

impl Op {
    fn fail(self, err: LspError) {
        match self {
            Op::Hover(tx) => {
                let _ = tx.send(Err(err));
            }
            Op::Definition(tx) => {
                let _ = tx.send(Err(err));
            }
            Op::References(tx) => {
                let _ = tx.send(Err(err));
            }
            Op::WarmUp(tx) => {
                let _ = tx.send(Err(err));
            }
        }
    }
}

struct QueuedRequest {
    file: PathBuf,
    line_text: String,
    line: u32,
    /// The display column of the symbol to target, converted to the
    /// server's negotiated position encoding once dispatched. `None` for
    /// [`Op::WarmUp`], which has no position to compute — it only needs the
    /// file opened, not a request issued at some point within it.
    display_col: Option<usize>,
    op: Op,
}

struct ServerEntry {
    state: ServerState,
    client: Option<Arc<Client>>,
    queue: VecDeque<QueuedRequest>,
    /// Files already announced to this server via `textDocument/didOpen`,
    /// mapped to the LSP version number last sent for them. A file with no
    /// entry here has never been opened; re-sending `didOpen` for one that
    /// does (without an intervening `didClose`) is a protocol violation
    /// most servers won't like, so every dispatch checks this map before
    /// opening. Once a file is open, [`LspManager::sync_changed_files`]
    /// bumps its version and sends `textDocument/didChange` here whenever a
    /// watch-mode refresh finds it changed on disk — this map is the only
    /// place that version counter lives, so a resync can never reuse or
    /// skip a number.
    versions: HashMap<PathBuf, i32>,
}

impl ServerEntry {
    fn new() -> Self {
        Self {
            state: ServerState::NotStarted,
            client: None,
            queue: VecDeque::new(),
            versions: HashMap::new(),
        }
    }
}

/// How many files [`LspManager::warm_up`] actually opened versus how many
/// eligible files (a detected language, regardless of whether that
/// language's server turns out to be available) it saw in total — the
/// difference is what a caller shows as "+N more files not opened for
/// diagnostics" when the diff is bigger than [`WARM_UP_CAP`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarmUpSummary {
    pub opened: usize,
    pub total_eligible: usize,
}

impl WarmUpSummary {
    pub fn capped(&self) -> bool {
        self.total_eligible > self.opened
    }
}

pub struct LspManager {
    events_tx: Sender<ServerEvent>,
    servers: Arc<Mutex<HashMap<ServerKey, ServerEntry>>>,
    /// Config's `[lsp.servers.<lang>]` overrides — see
    /// [`adapter::resolve_server`]'s docs. Shared via `Arc` rather than
    /// cloned per spawn, since it's read-only for this manager's whole
    /// lifetime and every `spawn_server` thread needs its own handle.
    overrides: Arc<HashMap<String, crate::config::ServerOverride>>,
}

impl LspManager {
    pub fn new(
        events_tx: Sender<ServerEvent>,
        overrides: Arc<HashMap<String, crate::config::ServerOverride>>,
    ) -> Self {
        Self {
            events_tx,
            servers: Arc::new(Mutex::new(HashMap::new())),
            overrides,
        }
    }

    /// The coarse state of the server that would handle `file`, for a
    /// status-bar indicator. Never spawns anything — this is a read, not a
    /// request.
    pub fn state(&self, file: &Path, git_root: &Path) -> ServerState {
        let Some(key) = self.key_for(file, git_root) else {
            return ServerState::Unavailable {
                reason: "no language server configured for this file type".to_owned(),
            };
        };
        self.servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .map_or(ServerState::NotStarted, |e| e.state.clone())
    }

    /// The position encoding the server handling `file` negotiated, if it's
    /// currently `Ready` — `None` before that (nothing to report yet) or if
    /// no server is configured for this file type. Used to convert an LSP
    /// response's byte/UTF-16 offsets (a definition's target range, a
    /// reference's location) back into display columns; see
    /// [`crate::ui::navigation`].
    pub fn position_encoding(&self, file: &Path, git_root: &Path) -> Option<PositionEncodingKind> {
        let key = self.key_for(file, git_root)?;
        let servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
        let client = servers.get(&key)?.client.as_ref()?;
        Some(client.position_encoding().clone())
    }

    /// Requests hover information for `display_col` on `line` (0-based) of
    /// `file`, whose full text on that line is `line_text` — needed here,
    /// not just at the call site, because the UTF-8/UTF-16 column the
    /// server actually wants depends on which encoding it negotiated, which
    /// isn't known until the connection is `Ready`. Always returns
    /// immediately; every outcome (unsupported file type, no workspace
    /// root, queued-then-answered, answered now) arrives through the
    /// returned `Receiver`.
    pub fn hover(
        &self,
        file: &Path,
        git_root: &Path,
        line_text: &str,
        line: u32,
        display_col: usize,
    ) -> Receiver<Result<HoverResult, LspError>> {
        let (tx, rx) = channel();
        self.submit(
            file,
            git_root,
            line_text,
            line,
            Some(display_col),
            Op::Hover(tx),
        );
        rx
    }

    /// As [`Self::hover`], for `textDocument/definition`.
    pub fn definition(
        &self,
        file: &Path,
        git_root: &Path,
        line_text: &str,
        line: u32,
        display_col: usize,
    ) -> Receiver<Result<DefinitionResult, LspError>> {
        let (tx, rx) = channel();
        self.submit(
            file,
            git_root,
            line_text,
            line,
            Some(display_col),
            Op::Definition(tx),
        );
        rx
    }

    /// As [`Self::hover`], for `textDocument/references`.
    pub fn references(
        &self,
        file: &Path,
        git_root: &Path,
        line_text: &str,
        line: u32,
        display_col: usize,
    ) -> Receiver<Result<ReferencesResult, LspError>> {
        let (tx, rx) = channel();
        self.submit(
            file,
            git_root,
            line_text,
            line,
            Some(display_col),
            Op::References(tx),
        );
        rx
    }

    /// Proactively `didOpen`s up to [`WARM_UP_CAP`] of `files` (in the
    /// order given — callers pass diff order, so the files a reviewer sees
    /// first are the ones most likely to have diagnostics ready by the time
    /// they scroll to them) so `textDocument/publishDiagnostics`
    /// notifications start arriving without the user having to hover
    /// something first. Files with no detected language are skipped without
    /// counting against the cap; files whose language has no available
    /// server still count as "attempted" (the point of the cap is bounding
    /// how much work this call kicks off, not predicting server
    /// availability, which isn't known synchronously — see
    /// [`Self::state`]).
    pub fn warm_up(&self, files: &[PathBuf], git_root: &Path) -> WarmUpSummary {
        let eligible: Vec<&PathBuf> = files
            .iter()
            .filter(|f| Language::detect(f).is_some())
            .collect();
        let total_eligible = eligible.len();
        let mut opened = 0;
        for file in eligible.into_iter().take(WARM_UP_CAP) {
            let (tx, _rx) = channel();
            self.submit(file, git_root, "", 0, None, Op::WarmUp(tx));
            opened += 1;
        }
        WarmUpSummary {
            opened,
            total_eligible,
        }
    }

    /// Resyncs every file in `changed` that some server already has open
    /// (per [`ServerEntry::versions`]) to its current on-disk content, via
    /// `textDocument/didChange` with that document's version bumped past
    /// whatever was last sent for it. Deliberately uncapped, unlike
    /// [`Self::warm_up`]: an already-open document staying in sync with
    /// disk isn't optional the way proactively opening a *new* file is —
    /// every one of them gets resynced on every refresh, regardless of how
    /// many that is. Files nothing has opened yet are left alone; a watch
    /// refresh opens newly-appearing diff entries separately, through
    /// [`Self::warm_up`], which *is* capped.
    pub fn sync_changed_files(&self, changed: &[PathBuf], git_root: &Path) {
        for file in changed {
            let Some(key) = self.key_for(file, git_root) else {
                continue;
            };
            let next = {
                let mut servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
                let Some(entry) = servers.get_mut(&key) else {
                    continue;
                };
                let Some(client) = entry.client.clone() else {
                    continue;
                };
                let Some(version) = bump_version(&mut entry.versions, file) else {
                    continue; // no server has this file open; nothing to resync
                };
                (client, version)
            };
            let (client, version) = next;
            let file = file.clone();
            std::thread::spawn(move || {
                // A file a watch batch reported as changed may have been
                // deleted again (or be mid-write) by the time this thread
                // gets to read it; either way there's no content to send,
                // and `workspace/didChangeWatchedFiles` (see
                // `Self::notify_watched_files`) is the mechanism that tells
                // the server about deletion, not this one.
                let Ok(content) = std::fs::read_to_string(&file) else {
                    return;
                };
                let Ok(uri) = file_uri(&file) else { return };
                client.did_change(uri, version, &content);
            });
        }
    }

    /// Sends `workspace/didChangeWatchedFiles` to every running server whose
    /// workspace root contains at least one of `changed`'s paths — the
    /// mechanism a server uses to invalidate project-wide state (dependency
    /// graphs, cross-file caches) for files it was never `didOpen`'d for,
    /// which [`Self::sync_changed_files`]'s per-open-document
    /// `didChange` doesn't reach. A server whose root contains none of the
    /// changed paths hears nothing, rather than every server hearing about
    /// every change regardless of relevance.
    pub fn notify_watched_files(&self, changed: &[(PathBuf, FileChangeType)]) {
        let servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
        for (key, entry) in servers.iter() {
            let Some(client) = &entry.client else {
                continue;
            };
            let workspace_root = &key.1;
            let events: Vec<FileEvent> = changed
                .iter()
                .filter(|(path, _)| path.starts_with(workspace_root))
                .filter_map(|(path, typ)| {
                    file_uri(path).ok().map(|uri| FileEvent { uri, typ: *typ })
                })
                .collect();
            if !events.is_empty() {
                client.did_change_watched_files(events);
            }
        }
    }

    /// Gracefully shuts down every server this manager has spawned —
    /// `shutdown`/`exit`/kill-after-timeout for each (see
    /// [`crate::lsp::client::Client::shutdown`]). Callers run this once, on
    /// the way out: `ui::run` on TUI quit, the `ktmr lsp-check` CLI command
    /// before it exits. Without it, a spawned language server would be
    /// orphaned rather than terminated — `Transport`'s `Drop` only runs if
    /// something actually drops it, and nothing does on a detached
    /// supervisor thread when the process itself exits.
    pub fn shutdown_all(&self) {
        let clients: Vec<Arc<Client>> = self
            .servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter_map(|entry| entry.client.clone())
            .collect();
        for client in clients {
            client.shutdown();
        }
    }

    fn key_for(&self, file: &Path, git_root: &Path) -> Option<ServerKey> {
        let language = Language::detect(file)?;
        let workspace_root = adapter::workspace_root(file, git_root, language);
        Some((language, workspace_root))
    }

    /// The shared entry point every public request method funnels through:
    /// resolves which server `file` belongs to, then drives `op` through
    /// that server's state — dispatch immediately if `Ready`, queue if
    /// `Starting`/`NotStarted` (spawning on the way if this is the first
    /// request for this key), or fail immediately if the server is known
    /// not to work. `submit` itself decides none of the request semantics;
    /// that's entirely `op`'s job (via [`dispatch`]) once a client exists.
    fn submit(
        &self,
        file: &Path,
        git_root: &Path,
        line_text: &str,
        line: u32,
        display_col: Option<usize>,
        op: Op,
    ) {
        let Some(key) = self.key_for(file, git_root) else {
            op.fail(LspError::Io(
                "no language server configured for this file type".to_owned(),
            ));
            return;
        };

        let mut servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
        let entry = servers.entry(key.clone()).or_insert_with(ServerEntry::new);

        match entry.state.clone() {
            ServerState::Ready => {
                let client = entry
                    .client
                    .clone()
                    .expect("ServerState::Ready always has a client");
                drop(servers);
                dispatch(
                    client,
                    QueuedRequest {
                        file: file.to_path_buf(),
                        line_text: line_text.to_owned(),
                        line,
                        display_col,
                        op,
                    },
                    Arc::clone(&self.servers),
                    key,
                );
            }
            ServerState::Unavailable { reason } | ServerState::Crashed { reason } => {
                op.fail(LspError::Io(reason));
            }
            ServerState::NotStarted | ServerState::Starting => {
                let starting_now = matches!(entry.state, ServerState::NotStarted);
                entry.state = ServerState::Starting;
                enqueue(entry, file, line_text, line, display_col, op);
                drop(servers);
                if starting_now {
                    self.spawn_server(key);
                }
            }
        }
    }

    /// Spawns and initializes the server for `key` on a background thread,
    /// then keeps that thread alive for the connection's whole lifetime,
    /// pumping its notifications onto `events_tx` until it closes. Called
    /// exactly once per key, from [`Self::submit`], the moment a key's
    /// state first moves out of `NotStarted`.
    fn spawn_server(&self, key: ServerKey) {
        let servers = Arc::clone(&self.servers);
        let events_tx = self.events_tx.clone();
        let overrides = Arc::clone(&self.overrides);

        std::thread::spawn(move || {
            let (language, workspace_root) = key.clone();

            let mut command = match adapter::resolve_server(language, &workspace_root, &overrides) {
                Ok(command) => command,
                Err(unavailable) => {
                    fail_entry(
                        &servers,
                        &key,
                        ServerState::Unavailable {
                            reason: unavailable.reason,
                        },
                    );
                    return;
                }
            };
            command.current_dir(&workspace_root);

            let client = match Client::start(command, &workspace_root) {
                Ok(client) => Arc::new(client),
                Err(e) => {
                    fail_entry(
                        &servers,
                        &key,
                        ServerState::Unavailable {
                            reason: e.to_string(),
                        },
                    );
                    return;
                }
            };

            let transport_events = client
                .transport()
                .take_events()
                .expect("Transport::take_events is only ever called here, once");

            let queued = mark_ready(&servers, &key, Arc::clone(&client));
            for request in queued {
                dispatch(
                    Arc::clone(&client),
                    request,
                    Arc::clone(&servers),
                    key.clone(),
                );
            }

            for event in transport_events {
                let closed_reason = match &event {
                    LspEvent::Closed { reason } => Some(reason.clone()),
                    LspEvent::Notification { .. } => None,
                };
                let _ = events_tx.send(ServerEvent {
                    language,
                    root: workspace_root.clone(),
                    event,
                });
                if let Some(reason) = closed_reason {
                    fail_entry(
                        &servers,
                        &key,
                        ServerState::Crashed {
                            reason: reason.unwrap_or_else(|| "server process exited".to_owned()),
                        },
                    );
                    break;
                }
            }
        });
    }
}

/// Bumps `file`'s entry in a server's version map and returns the new
/// version, or `None` if `file` isn't in the map at all (no server has it
/// open, so there's nothing to resync). Split out from
/// [`LspManager::sync_changed_files`] as a plain function over the map
/// itself so the counter arithmetic is unit-testable without a running
/// language server.
fn bump_version(versions: &mut HashMap<PathBuf, i32>, file: &Path) -> Option<i32> {
    let version = versions.get_mut(file)?;
    *version += 1;
    Some(*version)
}

fn enqueue(
    entry: &mut ServerEntry,
    file: &Path,
    line_text: &str,
    line: u32,
    display_col: Option<usize>,
    op: Op,
) {
    if entry.queue.len() >= MAX_QUEUED_PER_SERVER
        && let Some(evicted) = entry.queue.pop_front()
    {
        evicted.op.fail(LspError::Io(
            "superseded by a newer request before the server was ready".to_owned(),
        ));
    }
    entry.queue.push_back(QueuedRequest {
        file: file.to_path_buf(),
        line_text: line_text.to_owned(),
        line,
        display_col,
        op,
    });
}

fn fail_entry(
    servers: &Arc<Mutex<HashMap<ServerKey, ServerEntry>>>,
    key: &ServerKey,
    state: ServerState,
) {
    let mut servers = servers.lock().unwrap_or_else(|e| e.into_inner());
    let entry = servers.entry(key.clone()).or_insert_with(ServerEntry::new);
    entry.state = state;
    entry.client = None;
    for queued in entry.queue.drain(..) {
        queued.op.fail(LspError::Io(
            "language server became unavailable".to_owned(),
        ));
    }
}

/// Marks `key`'s entry `Ready` and hands back whatever accumulated in its
/// queue while it was starting, for the caller to dispatch now that a
/// client exists.
fn mark_ready(
    servers: &Arc<Mutex<HashMap<ServerKey, ServerEntry>>>,
    key: &ServerKey,
    client: Arc<Client>,
) -> Vec<QueuedRequest> {
    let mut servers = servers.lock().unwrap_or_else(|e| e.into_inner());
    let entry = servers.entry(key.clone()).or_insert_with(ServerEntry::new);
    entry.state = ServerState::Ready;
    entry.client = Some(client);
    entry.queue.drain(..).collect()
}

/// Runs one request to completion on a dedicated short-lived thread: reads
/// the file, opens it with the server if this is the first time, converts
/// `display_col` into the server's negotiated position encoding (skipped
/// entirely for [`Op::WarmUp`], which has none to convert), and forwards
/// the result. Off the manager's supervisor thread so a slow request never
/// delays draining that server's `$/progress`/diagnostics notifications.
fn dispatch(
    client: Arc<Client>,
    request: QueuedRequest,
    servers: Arc<Mutex<HashMap<ServerKey, ServerEntry>>>,
    key: ServerKey,
) {
    std::thread::spawn(move || {
        let content = match std::fs::read_to_string(&request.file) {
            Ok(content) => content,
            Err(e) => {
                request.op.fail(LspError::Io(format!(
                    "reading {}: {e}",
                    request.file.display()
                )));
                return;
            }
        };
        let uri = match file_uri(&request.file) {
            Ok(uri) => uri,
            Err(e) => {
                request.op.fail(e);
                return;
            }
        };

        let needs_open = {
            let mut servers = servers.lock().unwrap_or_else(|e| e.into_inner());
            servers
                .entry(key.clone())
                .or_insert_with(ServerEntry::new)
                .versions
                .insert(request.file.clone(), 1)
                .is_none()
        };
        if needs_open {
            let language_id = adapter::lsp_language_id(key.0, &request.file);
            client.did_open(uri.clone(), language_id, 1, &content);
        }

        let Some(display_col) = request.display_col else {
            if let Op::WarmUp(tx) = request.op {
                let _ = tx.send(Ok(()));
            }
            return;
        };

        let columns = ColumnMap::new(&request.line_text);
        let character = if client.position_encoding().as_str() == "utf-8" {
            columns.display_to_utf8(display_col)
        } else {
            columns.display_to_utf16(display_col)
        };
        let position = Position {
            line: request.line,
            character: character as u32,
        };

        match request.op {
            Op::Hover(tx) => forward(client.hover(uri, position), tx),
            Op::Definition(tx) => {
                if client.supports_definition() {
                    forward(client.definition(uri, position), tx);
                } else {
                    let _ = tx.send(Err(LspError::Io(
                        "server does not advertise textDocument/definition support".to_owned(),
                    )));
                }
            }
            Op::References(tx) => {
                if client.supports_references() {
                    forward(client.references(uri, position), tx);
                } else {
                    let _ = tx.send(Err(LspError::Io(
                        "server does not advertise textDocument/references support".to_owned(),
                    )));
                }
            }
            Op::WarmUp(_) => unreachable!("handled above, before a position was needed"),
        }
    });
}

/// Waits (bounded by [`REQUEST_TIMEOUT`]) for `rx` to resolve and relays the
/// result to `tx`, converting a timed-out wait into the same `LspError` a
/// transport failure would produce — generic over the result type so
/// [`dispatch`]'s three real request kinds share this one relay instead of
/// each writing its own timeout/forward boilerplate.
fn forward<T>(rx: Receiver<Result<T, LspError>>, tx: Sender<Result<T, LspError>>) {
    let result = rx
        .recv_timeout(REQUEST_TIMEOUT)
        .unwrap_or(Err(LspError::Io("request timed out".to_owned())));
    let _ = tx.send(result);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_version_increments_an_already_open_files_version() {
        let mut versions = HashMap::new();
        versions.insert(PathBuf::from("/repo/src/lib.rs"), 1);
        assert_eq!(
            bump_version(&mut versions, Path::new("/repo/src/lib.rs")),
            Some(2)
        );
        assert_eq!(
            bump_version(&mut versions, Path::new("/repo/src/lib.rs")),
            Some(3)
        );
        assert_eq!(versions[Path::new("/repo/src/lib.rs")], 3);
    }

    #[test]
    fn bump_version_is_none_for_a_file_no_server_has_open() {
        let mut versions = HashMap::new();
        versions.insert(PathBuf::from("/repo/src/lib.rs"), 1);
        assert_eq!(
            bump_version(&mut versions, Path::new("/repo/src/other.rs")),
            None
        );
        // And bumping one file never touches another's counter.
        assert_eq!(versions[Path::new("/repo/src/lib.rs")], 1);
    }

    #[test]
    fn bump_version_never_reuses_or_skips_a_number_across_repeated_changes() {
        let mut versions = HashMap::new();
        versions.insert(PathBuf::from("/repo/a.rs"), 1);
        let seen: Vec<i32> = (0..5)
            .map(|_| bump_version(&mut versions, Path::new("/repo/a.rs")).unwrap())
            .collect();
        assert_eq!(seen, vec![2, 3, 4, 5, 6]);
    }
}
