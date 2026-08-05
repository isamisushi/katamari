//! Owns every language server this session spawns: lazily starts one per
//! `(language, workspace root)` the first time something needs it, keeps it
//! alive for the rest of the session, and queues requests made before it's
//! ready instead of making the caller wait or retry. [`LspManager::hover`]
//! is the only entry point the UI calls — it always returns immediately
//! with a `Receiver`, whether the server is already `Ready`, still
//! `Starting` (the request is queued), or will never work at all (the
//! `Receiver` gets its one `Err` right away, no spawning required).

use crate::diff::ColumnMap;
use crate::lsp::adapter::{self, Language};
use crate::lsp::client::{Client, HoverResult, file_uri};
use crate::lsp::transport::{LspError, LspEvent};
use lsp_types::Position;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a single hover round-trip is allowed to take before this
/// manager gives up on it and reports a timeout — long enough to cover a
/// busy server still catching up on indexing, short enough that a truly
/// wedged server doesn't leave a hover request pending for the rest of the
/// session.
const HOVER_TIMEOUT: Duration = Duration::from_secs(15);

/// The most a single server's not-yet-ready queue will hold. Bounded
/// because a user tapping `K` repeatedly while a server is still starting
/// shouldn't accumulate unbounded queued work — only the most recent
/// requests are worth answering anyway, since [`crate::ui`]'s generation
/// counter discards stale ones on arrival regardless.
const MAX_QUEUED_PER_SERVER: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    NotStarted,
    Starting,
    Ready,
    Unavailable { reason: String },
    Crashed { reason: String },
}

type ServerKey = (Language, PathBuf);

struct QueuedHover {
    file: PathBuf,
    line_text: String,
    line: u32,
    display_col: usize,
    respond: Sender<Result<HoverResult, LspError>>,
}

struct ServerEntry {
    state: ServerState,
    client: Option<Arc<Client>>,
    queue: VecDeque<QueuedHover>,
    /// Files already announced to this server via `textDocument/didOpen`.
    /// Re-sending `didOpen` for the same URI without a `didClose` in
    /// between is a protocol violation most servers won't like, so every
    /// hover dispatch checks this before opening.
    opened: HashSet<PathBuf>,
}

impl ServerEntry {
    fn new() -> Self {
        Self {
            state: ServerState::NotStarted,
            client: None,
            queue: VecDeque::new(),
            opened: HashSet::new(),
        }
    }
}

pub struct LspManager {
    /// Forwards every spawned server's notifications (`$/progress`, most
    /// importantly — it drives the status bar's indexing spinner) onto one
    /// shared bus. M3a only ever runs one server at a time, so events are
    /// forwarded unlabeled; a second concurrent language in a later
    /// milestone will need to tag these by server, noted where `ui::mod`
    /// wires this channel up.
    events_tx: Sender<LspEvent>,
    servers: Arc<Mutex<HashMap<ServerKey, ServerEntry>>>,
}

impl LspManager {
    pub fn new(events_tx: Sender<LspEvent>) -> Self {
        Self {
            events_tx,
            servers: Arc::new(Mutex::new(HashMap::new())),
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
        let Some(key) = self.key_for(file, git_root) else {
            let _ = tx.send(Err(LspError::Io(
                "no language server configured for this file type".to_owned(),
            )));
            return rx;
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
                dispatch_hover(
                    client,
                    QueuedHover {
                        file: file.to_path_buf(),
                        line_text: line_text.to_owned(),
                        line,
                        display_col,
                        respond: tx,
                    },
                    Arc::clone(&self.servers),
                    key,
                );
            }
            ServerState::Unavailable { reason } | ServerState::Crashed { reason } => {
                let _ = tx.send(Err(LspError::Io(reason)));
            }
            ServerState::NotStarted | ServerState::Starting => {
                let starting_now = matches!(entry.state, ServerState::NotStarted);
                entry.state = ServerState::Starting;
                enqueue(entry, file, line_text, line, display_col, tx);
                drop(servers);
                if starting_now {
                    self.spawn_server(key);
                }
            }
        }
        rx
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
        let workspace_root = adapter::workspace_root(file, git_root)?;
        Some((language, workspace_root))
    }

    /// Spawns and initializes the server for `key` on a background thread,
    /// then keeps that thread alive for the connection's whole lifetime,
    /// pumping its notifications onto `events_tx` until it closes. Called
    /// exactly once per key, from [`Self::hover`], the moment a key's state
    /// first moves out of `NotStarted`.
    fn spawn_server(&self, key: ServerKey) {
        let servers = Arc::clone(&self.servers);
        let events_tx = self.events_tx.clone();

        std::thread::spawn(move || {
            let (language, workspace_root) = key.clone();

            let mut command = match adapter::resolve_server(language) {
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
                dispatch_hover(
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
                let _ = events_tx.send(event);
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

fn enqueue(
    entry: &mut ServerEntry,
    file: &Path,
    line_text: &str,
    line: u32,
    display_col: usize,
    respond: Sender<Result<HoverResult, LspError>>,
) {
    if entry.queue.len() >= MAX_QUEUED_PER_SERVER
        && let Some(evicted) = entry.queue.pop_front()
    {
        let _ = evicted.respond.send(Err(LspError::Io(
            "superseded by a newer hover request before the server was ready".to_owned(),
        )));
    }
    entry.queue.push_back(QueuedHover {
        file: file.to_path_buf(),
        line_text: line_text.to_owned(),
        line,
        display_col,
        respond,
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
        let _ = queued.respond.send(Err(LspError::Io(
            "language server became unavailable".to_owned(),
        )));
    }
}

/// Marks `key`'s entry `Ready` and hands back whatever accumulated in its
/// queue while it was starting, for the caller to dispatch now that a
/// client exists.
fn mark_ready(
    servers: &Arc<Mutex<HashMap<ServerKey, ServerEntry>>>,
    key: &ServerKey,
    client: Arc<Client>,
) -> Vec<QueuedHover> {
    let mut servers = servers.lock().unwrap_or_else(|e| e.into_inner());
    let entry = servers.entry(key.clone()).or_insert_with(ServerEntry::new);
    entry.state = ServerState::Ready;
    entry.client = Some(client);
    entry.queue.drain(..).collect()
}

/// Runs one hover request to completion on a dedicated short-lived thread:
/// reads the file, opens it with the server if this is the first time,
/// converts `display_col` into the server's negotiated position encoding,
/// and forwards the result. Off the manager's supervisor thread so a slow
/// hover never delays draining that server's `$/progress` notifications.
fn dispatch_hover(
    client: Arc<Client>,
    request: QueuedHover,
    servers: Arc<Mutex<HashMap<ServerKey, ServerEntry>>>,
    key: ServerKey,
) {
    std::thread::spawn(move || {
        let content = match std::fs::read_to_string(&request.file) {
            Ok(content) => content,
            Err(e) => {
                let _ = request.respond.send(Err(LspError::Io(format!(
                    "reading {}: {e}",
                    request.file.display()
                ))));
                return;
            }
        };
        let uri = match file_uri(&request.file) {
            Ok(uri) => uri,
            Err(e) => {
                let _ = request.respond.send(Err(e));
                return;
            }
        };

        let needs_open = {
            let mut servers = servers.lock().unwrap_or_else(|e| e.into_inner());
            servers
                .entry(key.clone())
                .or_insert_with(ServerEntry::new)
                .opened
                .insert(request.file.clone())
        };
        if needs_open {
            client.did_open(uri.clone(), key.0.lsp_id(), 1, &content);
        }

        let columns = ColumnMap::new(&request.line_text);
        let character = if client.position_encoding().as_str() == "utf-8" {
            columns.display_to_utf8(request.display_col)
        } else {
            columns.display_to_utf16(request.display_col)
        };
        let position = Position {
            line: request.line,
            character: character as u32,
        };

        let hover_rx = client.hover(uri, position);
        let result = hover_rx
            .recv_timeout(HOVER_TIMEOUT)
            .unwrap_or(Err(LspError::Io("hover request timed out".to_owned())));
        let _ = request.respond.send(result);
    });
}
