//! Runtime language-server observability.
//!
//! This module is deliberately independent of the terminal UI.  The manager
//! and transport publish typed events here, while the inspector reads a
//! bounded snapshot/event stream.  A separate, best-effort writer thread
//! persists the same events for the lifetime of one TUI session; a slow or
//! broken journal can therefore never stall an LSP request.

use crate::lsp::adapter::LangKey;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Maximum number of in-memory events retained for the inspector.  The
/// journal is a diagnostic aid, not a second unbounded copy of protocol data.
pub const MEMORY_EVENT_CAP: usize = 4096;
/// Individual messages are capped before they enter either the inspector or
/// the disk queue.  This keeps one malformed stderr/log/progress notification
/// from turning a bounded event count into an unbounded memory or disk cost.
pub const MAX_OBSERVATION_MESSAGE_BYTES: usize = 16 * 1024;
/// A server can begin progress for arbitrary tokens; keep the current-state
/// view bounded even when a server never sends matching `end` notifications.
pub const MAX_ACTIVE_PROGRESS_TOKENS: usize = 64;
const WRITER_QUEUE_CAP: usize = 512;
const DEFAULT_SEGMENT_BYTES: u64 = 5_242_880;
const DEFAULT_SEGMENTS_PER_SESSION: usize = 4;
const DEFAULT_TOTAL_BYTES: u64 = 104_857_600;
const DEFAULT_MAX_AGE_DAYS: u64 = 2;

/// User-facing logging configuration under `[lsp.logging]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggingConfig {
    pub enabled: bool,
    pub segment_bytes: u64,
    pub segments_per_session: usize,
    pub total_bytes: u64,
    pub max_age_days: u64,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            segment_bytes: DEFAULT_SEGMENT_BYTES,
            segments_per_session: DEFAULT_SEGMENTS_PER_SESSION,
            total_bytes: DEFAULT_TOTAL_BYTES,
            max_age_days: DEFAULT_MAX_AGE_DAYS,
        }
    }
}

/// Full identity of a live server process.  The root is part of the display
/// identity because one session can review two independent workspaces using
/// the same language.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerIdentity {
    pub language: LangKey,
    pub workspace_root: PathBuf,
}

impl ServerIdentity {
    pub fn new(language: LangKey, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            language,
            workspace_root: workspace_root.into(),
        }
    }

    pub fn label(&self) -> String {
        format!("{} / {}", self.language, self.workspace_root.display())
    }
}

impl fmt::Display for ServerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} / {}", self.language, self.workspace_root.display())
    }
}

/// The coarse lifecycle shown by the inspector.  `Running` means initialize
/// completed; it intentionally does not imply indexing/project readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerPhase {
    NotStarted,
    Resolving,
    Installing,
    Initializing,
    Running,
    Unavailable,
    Crashed,
    Stopped,
}

impl fmt::Display for ServerPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::NotStarted => "not started",
            Self::Resolving => "resolving",
            Self::Installing => "installing",
            Self::Initializing => "initializing",
            Self::Running => "running",
            Self::Unavailable => "unavailable",
            Self::Crashed => "crashed",
            Self::Stopped => "stopped/evicted",
        };
        f.write_str(text)
    }
}

/// A currently active `$/progress` token.  Message text is intentionally
/// limited to the server-provided title/message; no source/document content
/// is captured here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressSnapshot {
    pub token: String,
    pub title: Option<String>,
    pub message: Option<String>,
    pub percentage: Option<u64>,
    pub started_at_ms: u128,
}

/// Capabilities useful to explain why a request was unsupported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilitySnapshot {
    pub hover: bool,
    pub definition: bool,
    pub references: bool,
    pub diagnostics: bool,
}

/// Read-only state for one logical server key and its current process
/// generation.  The inspector receives this as a point-in-time snapshot.
#[derive(Debug, Clone)]
pub struct ServerSnapshot {
    pub identity: ServerIdentity,
    pub generation: u64,
    pub phase: ServerPhase,
    pub state_age_ms: u128,
    pub program: Option<String>,
    pub args: Vec<String>,
    pub pid: Option<u32>,
    pub exit_status: Option<String>,
    pub server_name: Option<String>,
    pub server_version: Option<String>,
    pub capabilities: CapabilitySnapshot,
    pub position_encoding: Option<String>,
    pub open_documents: usize,
    pub queued_requests: usize,
    pub in_flight_requests: usize,
    pub active_progress: Vec<ProgressSnapshot>,
    pub last_activity_ms: Option<u128>,
    pub last_error: Option<String>,
    state_changed_ms: u128,
}

impl ServerSnapshot {
    fn new(identity: ServerIdentity) -> Self {
        Self {
            identity,
            generation: 0,
            phase: ServerPhase::NotStarted,
            state_age_ms: 0,
            program: None,
            args: Vec::new(),
            pid: None,
            exit_status: None,
            server_name: None,
            server_version: None,
            capabilities: CapabilitySnapshot::default(),
            position_encoding: None,
            open_documents: 0,
            queued_requests: 0,
            in_flight_requests: 0,
            active_progress: Vec::new(),
            last_activity_ms: None,
            last_error: None,
            state_changed_ms: now_ms(),
        }
    }
}

/// Where an event came from.  `Stderr` is not automatically an error: a
/// language server commonly writes harmless startup notices there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    Lifecycle,
    Adapter,
    Install,
    Stderr,
    LogMessage,
    ShowMessage,
    Trace,
    Server,
    Progress,
    Transport,
    Request,
    Ui,
    Journal,
}

impl fmt::Display for EventSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Lifecycle => "lifecycle",
            Self::Adapter => "adapter",
            Self::Install => "install",
            Self::Stderr => "stderr",
            Self::LogMessage => "window/logMessage",
            Self::ShowMessage => "window/showMessage",
            Self::Trace => "$/logTrace",
            Self::Server => "server",
            Self::Progress => "progress",
            Self::Transport => "transport",
            Self::Request => "request",
            Self::Ui => "ui",
            Self::Journal => "journal",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventLevel {
    Info,
    Warn,
    Error,
    Debug,
}

impl fmt::Display for EventLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Debug => "debug",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOutcome {
    None,
    Result,
    NoResult,
    Unsupported,
    Queued,
    Superseded,
    Timeout,
    ServerError,
    TransportFailure,
    Cancellation,
    NavigationFailure,
    Dropped,
}

impl fmt::Display for EventOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "",
            Self::Result => "result",
            Self::NoResult => "no result",
            Self::Unsupported => "unsupported",
            Self::Queued => "queued",
            Self::Superseded => "superseded",
            Self::Timeout => "timeout",
            Self::ServerError => "server error",
            Self::TransportFailure => "transport failure",
            Self::Cancellation => "cancelled",
            Self::NavigationFailure => "navigation failure",
            Self::Dropped => "dropped",
        })
    }
}

/// One privacy-filtered journal entry.  Producers should pass summaries and
/// method/position metadata, never source lines, hover bodies, diagnostics,
/// initialization options, environment variables, or raw JSON-RPC payloads.
#[derive(Debug, Clone)]
pub struct JournalEvent {
    pub sequence: u64,
    pub at_ms: u128,
    pub elapsed_ms: u128,
    pub identity: Option<ServerIdentity>,
    pub generation: Option<u64>,
    pub source: EventSource,
    pub level: EventLevel,
    pub method: Option<String>,
    pub operation_id: Option<u64>,
    pub outcome: EventOutcome,
    pub message: String,
}

impl JournalEvent {
    pub fn simple(
        source: EventSource,
        level: EventLevel,
        identity: Option<ServerIdentity>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            sequence: 0,
            at_ms: now_ms(),
            elapsed_ms: 0,
            identity,
            generation: None,
            source,
            level,
            method: None,
            operation_id: None,
            outcome: EventOutcome::None,
            message: message.into(),
        }
    }

    /// Human-readable combined-log line.  Newlines are flattened so one
    /// malformed stderr line cannot forge or corrupt multiple journal rows.
    pub fn format_line(&self, session_start_ms: u128) -> String {
        // `0` is a convenient UI-only sentinel: inspector events already
        // carry their session-relative elapsed value, so rendering them does
        // not need to clone or expose the store's start timestamp.
        let elapsed = if session_start_ms == 0 {
            self.elapsed_ms
        } else {
            self.at_ms.saturating_sub(session_start_ms)
        };
        let identity = match (&self.identity, self.generation) {
            (Some(identity), Some(generation)) => format!(" [{identity} #{generation}]"),
            (Some(identity), None) => format!(" [{identity}]"),
            (None, Some(generation)) => format!(" [#{generation}]"),
            (None, None) => String::new(),
        };
        let method = self
            .method
            .as_deref()
            .map(|method| format!(" {method}"))
            .unwrap_or_default();
        let outcome = self.outcome.to_string();
        let outcome = if outcome.is_empty() {
            String::new()
        } else {
            format!(" ({outcome})")
        };
        let source = format!("{}[{}]", self.source, self.level);
        let operation = self
            .operation_id
            .map_or_else(String::new, |operation_id| format!(" op#{operation_id}"));
        format!(
            "{}.{:03} +{}ms{}{} {}{}{} {}\n",
            clock_hms(self.at_ms),
            self.at_ms % 1000,
            elapsed,
            identity,
            operation,
            source,
            method,
            outcome,
            sanitize_message(&self.message),
        )
    }
}

#[derive(Default)]
struct Inner {
    revision: u64,
    event_revision: u64,
    next_sequence: u64,
    next_operation: u64,
    events: VecDeque<JournalEvent>,
    servers: HashMap<ServerIdentity, ServerSnapshot>,
    setup_error: Option<String>,
    dropped: u64,
}

struct DiskWriter {
    tx: SyncSender<DiskCommand>,
    handle: Option<JoinHandle<()>>,
}

enum DiskCommand {
    Event(JournalEvent),
    Flush,
    Stop,
}

struct SessionLock {
    path: PathBuf,
    file: File,
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        unlock_file(&self.file);
        let _ = fs::remove_file(&self.path);
    }
}

/// Shared store/handle used by manager threads and the UI.
pub struct ObservationStore {
    inner: Mutex<Inner>,
    session_start_ms: u128,
    disk: Mutex<Option<DiskWriter>>,
    disk_error: Arc<Mutex<Option<String>>>,
    pending_disk_drops: AtomicU64,
    _session_lock: Mutex<Option<SessionLock>>,
    cleanup: Option<(PathBuf, LoggingConfig)>,
    journal_dir: Option<PathBuf>,
}

pub type ObservationHandle = Arc<ObservationStore>;

impl ObservationStore {
    /// Starts an in-memory store and best-effort persistent journal.  A
    /// failure to choose/create the state directory is retained in the store
    /// for the inspector and does not fail TUI startup.
    pub fn start(config: LoggingConfig) -> ObservationHandle {
        Self::start_in(config, default_state_root())
    }

    /// Testable variant that never touches the user's real state directory.
    pub fn start_in(config: LoggingConfig, state_root: impl Into<PathBuf>) -> ObservationHandle {
        let session_start_ms = now_ms();
        let mut inner = Inner::default();
        let state_root = state_root.into();
        let disk_error = Arc::new(Mutex::new(None));
        let (disk, session_lock, setup_error, journal_dir) = if config.enabled {
            setup_disk(
                &config,
                &state_root,
                session_start_ms,
                Arc::clone(&disk_error),
            )
        } else {
            (None, None, None, None)
        };
        inner.setup_error = setup_error;
        let store = Arc::new(Self {
            inner: Mutex::new(inner),
            session_start_ms,
            disk: Mutex::new(disk),
            disk_error,
            pending_disk_drops: AtomicU64::new(0),
            _session_lock: Mutex::new(session_lock),
            cleanup: config.enabled.then_some((state_root, config.clone())),
            journal_dir,
        });
        if let Some(error) = store.setup_error() {
            store.record(JournalEvent::simple(
                EventSource::Journal,
                EventLevel::Error,
                None,
                format!("persistent journal unavailable: {error}"),
            ));
        }
        store
    }

    #[allow(
        dead_code,
        reason = "test/headless callers need an isolated in-memory store"
    )]
    pub fn in_memory() -> ObservationHandle {
        Self::start_in(
            LoggingConfig {
                enabled: false,
                ..LoggingConfig::default()
            },
            std::env::temp_dir().join("katamari-observer-disabled"),
        )
    }

    pub fn setup_error(&self) -> Option<String> {
        let disk_error = self
            .disk_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        disk_error.or_else(|| {
            self.inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .setup_error
                .clone()
        })
    }

    /// Returns the per-session directory containing `events-*.log`, when
    /// persistent logging was enabled and session setup reached that point.
    /// The directory is intentionally exposed rather than a guessed global
    /// path: each Katamari process owns a unique locked session directory.
    pub fn journal_dir(&self) -> Option<PathBuf> {
        self.journal_dir.clone()
    }

    pub fn next_operation_id(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.next_operation = inner.next_operation.saturating_add(1);
        inner.next_operation
    }

    /// Append a bounded event and enqueue its disk representation without
    /// waiting for filesystem I/O.
    pub fn record(&self, mut event: JournalEvent) {
        self.record_disk_error_marker();
        event.message = truncate_message(&event.message);
        // Keep the sequence assignment and the non-blocking enqueue under the
        // same short lock.  Without this, producer A could assign sequence N,
        // producer B could enqueue N+1 first, and the combined journal would
        // no longer reflect the order shown by the inspector.
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let disk = self.disk.lock().unwrap_or_else(|e| e.into_inner());
        let disk_tx = disk.as_ref().map(|writer| &writer.tx);

        // A full writer queue is expected under a bursty server.  Once it has
        // room again, put one coalesced marker in the queue before the next
        // real event so persisted journals reveal the gap too.
        let pending = self.pending_disk_drops.load(Ordering::Acquire);
        if pending > 0
            && let Some(tx) = disk_tx
        {
            let mut marker = JournalEvent::simple(
                EventSource::Journal,
                EventLevel::Warn,
                None,
                format!("{pending} journal entries dropped while writer queue was full"),
            );
            marker.outcome = EventOutcome::Dropped;
            marker.sequence = inner.next_sequence.saturating_add(1);
            marker.elapsed_ms = marker.at_ms.saturating_sub(self.session_start_ms);
            match tx.try_send(DiskCommand::Event(marker.clone())) {
                Ok(()) => {
                    self.pending_disk_drops.store(0, Ordering::Release);
                    inner.next_sequence = marker.sequence;
                    append_memory_event(&mut inner, marker);
                }
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => {}
            }
        }

        inner.next_sequence = inner.next_sequence.saturating_add(1);
        event.sequence = inner.next_sequence;
        event.elapsed_ms = event.at_ms.saturating_sub(self.session_start_ms);
        append_memory_event(&mut inner, event.clone());
        if let Some(tx) = disk_tx {
            match tx.try_send(DiskCommand::Event(event)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                    inner.dropped = inner.dropped.saturating_add(1);
                    self.pending_disk_drops.fetch_add(1, Ordering::AcqRel);
                }
            }
        }
    }

    fn record_disk_error_marker(&self) {
        let Some(error) = self
            .disk_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            return;
        };
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.events.iter().any(|event| {
            event.source == EventSource::Journal && event.message.starts_with("journal error:")
        }) {
            return;
        }
        inner.next_sequence = inner.next_sequence.saturating_add(1);
        let mut marker = JournalEvent::simple(
            EventSource::Journal,
            EventLevel::Error,
            None,
            format!("journal error: {error}"),
        );
        marker.sequence = inner.next_sequence;
        marker.elapsed_ms = marker.at_ms.saturating_sub(self.session_start_ms);
        if inner.events.len() == MEMORY_EVENT_CAP {
            inner.events.pop_front();
        }
        inner.events.push_back(marker);
        inner.revision = inner.revision.saturating_add(1);
        inner.event_revision = inner.event_revision.saturating_add(1);
    }

    #[allow(
        dead_code,
        reason = "inspector/tests expose journal backpressure diagnostics"
    )]
    pub fn dropped_count(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).dropped
    }

    #[allow(dead_code, reason = "typed consumers may observe snapshot revisions")]
    pub fn revision(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .revision
    }

    /// Revision of the journal stream only. Snapshot updates (for example a
    /// progress percentage or in-flight count) do not change this counter, so
    /// the inspector can refresh live server state without cloning its
    /// bounded event buffer on every frame.
    pub fn event_revision(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .event_revision
    }

    pub fn events(&self) -> Vec<JournalEvent> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .events
            .iter()
            .cloned()
            .collect()
    }

    #[allow(
        dead_code,
        reason = "incremental typed consumers can request journal deltas"
    )]
    pub fn events_since(&self, sequence: u64) -> Vec<JournalEvent> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .events
            .iter()
            .filter(|event| event.sequence > sequence)
            .cloned()
            .collect()
    }

    pub fn snapshots(&self) -> Vec<ServerSnapshot> {
        let now = now_ms();
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .servers
            .values()
            .cloned()
            .map(|mut snapshot| {
                snapshot.state_age_ms = now.saturating_sub(snapshot.state_changed_ms);
                snapshot
            })
            .collect()
    }

    #[allow(
        dead_code,
        reason = "typed consumers can inspect one server without cloning all"
    )]
    pub fn snapshot(&self, identity: &ServerIdentity) -> Option<ServerSnapshot> {
        self.snapshots()
            .into_iter()
            .find(|snapshot| &snapshot.identity == identity)
    }

    /// Begins a new process generation.  Old-generation events cannot mutate
    /// the new snapshot after this returns.
    #[allow(
        dead_code,
        reason = "tests and alternate manager integrations start generations"
    )]
    pub fn begin_generation(
        &self,
        identity: ServerIdentity,
        program: Option<String>,
        args: Vec<String>,
    ) -> u64 {
        let generation = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .servers
            .get(&identity)
            .map_or(0, |snapshot| snapshot.generation)
            .saturating_add(1);
        self.begin_generation_at(identity, generation, program, args);
        generation
    }

    /// Starts the generation selected by the manager.  The manager owns the
    /// process-generation counter because it also guards supervisor events;
    /// accepting that number here prevents a stale spawn thread from
    /// incrementing the observer past a newer process before its first
    /// lifecycle event arrives.
    pub(crate) fn begin_generation_at(
        &self,
        identity: ServerIdentity,
        generation: u64,
        program: Option<String>,
        args: Vec<String>,
    ) -> bool {
        let (generation, event_identity) = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let snapshot = inner
                .servers
                .entry(identity.clone())
                .or_insert_with(|| ServerSnapshot::new(identity.clone()));
            if snapshot.generation > generation {
                return false;
            }
            if snapshot.generation == generation {
                return true;
            }
            snapshot.generation = generation;
            snapshot.phase = ServerPhase::Resolving;
            snapshot.state_changed_ms = now_ms();
            snapshot.program = program;
            snapshot.args = args;
            snapshot.pid = None;
            snapshot.exit_status = None;
            snapshot.server_name = None;
            snapshot.server_version = None;
            snapshot.capabilities = CapabilitySnapshot::default();
            snapshot.position_encoding = None;
            snapshot.open_documents = 0;
            snapshot.queued_requests = 0;
            snapshot.in_flight_requests = 0;
            snapshot.active_progress.clear();
            snapshot.last_error = None;
            snapshot.last_activity_ms = Some(now_ms());
            inner.revision = inner.revision.saturating_add(1);
            (generation, identity)
        };
        self.record_generation_event(
            event_identity,
            generation,
            EventLevel::Info,
            "new process generation",
        );
        true
    }

    pub fn transition(
        &self,
        identity: &ServerIdentity,
        generation: u64,
        phase: ServerPhase,
        message: impl Into<String>,
    ) -> bool {
        let message = message.into();
        let mut applied = false;
        let changed = self.with_current(identity, generation, |snapshot| {
            if snapshot.phase == ServerPhase::Stopped && phase != ServerPhase::Stopped {
                return;
            }
            applied = true;
            if snapshot.phase != phase {
                snapshot.state_changed_ms = now_ms();
            }
            snapshot.phase = phase;
            snapshot.last_activity_ms = Some(now_ms());
            if matches!(
                phase,
                ServerPhase::Unavailable | ServerPhase::Crashed | ServerPhase::Stopped
            ) {
                snapshot.active_progress.clear();
                snapshot.queued_requests = 0;
                snapshot.in_flight_requests = 0;
            }
            if matches!(phase, ServerPhase::Unavailable | ServerPhase::Crashed) {
                snapshot.last_error = Some(truncate_message(&message));
            }
        });
        if changed && applied {
            let level = match phase {
                ServerPhase::Crashed => EventLevel::Error,
                ServerPhase::Unavailable => EventLevel::Warn,
                _ => EventLevel::Info,
            };
            self.record_generation_event(identity.clone(), generation, level, message);
        }
        changed && applied
    }

    pub fn set_process(&self, identity: &ServerIdentity, generation: u64, pid: u32) -> bool {
        self.with_current(identity, generation, |snapshot| {
            snapshot.pid = Some(pid);
            snapshot.last_activity_ms = Some(now_ms());
        })
    }

    pub fn set_exit_status(
        &self,
        identity: &ServerIdentity,
        generation: u64,
        status: impl Into<String>,
    ) -> bool {
        let status = status.into();
        self.with_current_including_stopped(identity, generation, |snapshot| {
            snapshot.exit_status = Some(status);
            snapshot.last_activity_ms = Some(now_ms());
        })
    }

    pub fn set_command(
        &self,
        identity: &ServerIdentity,
        generation: u64,
        program: Option<String>,
        args: Vec<String>,
    ) -> bool {
        self.with_current(identity, generation, |snapshot| {
            snapshot.program = program;
            snapshot.args = args;
            snapshot.last_activity_ms = Some(now_ms());
        })
    }

    pub fn set_capabilities(
        &self,
        identity: &ServerIdentity,
        generation: u64,
        server_name: Option<String>,
        server_version: Option<String>,
        capabilities: CapabilitySnapshot,
        position_encoding: Option<String>,
    ) -> bool {
        self.with_current(identity, generation, |snapshot| {
            if snapshot.phase == ServerPhase::Stopped {
                return;
            }
            snapshot.server_name = server_name;
            snapshot.server_version = server_version;
            snapshot.capabilities = capabilities;
            snapshot.position_encoding = position_encoding;
            if snapshot.phase != ServerPhase::Running {
                snapshot.state_changed_ms = now_ms();
            }
            snapshot.phase = ServerPhase::Running;
            snapshot.last_activity_ms = Some(now_ms());
        })
    }

    pub fn mark_diagnostics_capability(&self, identity: &ServerIdentity, generation: u64) -> bool {
        self.with_current(identity, generation, |snapshot| {
            snapshot.capabilities.diagnostics = true;
            snapshot.last_activity_ms = Some(now_ms());
        })
    }

    #[allow(
        dead_code,
        reason = "typed producers can update all current-state counters"
    )]
    pub fn set_counts(
        &self,
        identity: &ServerIdentity,
        generation: u64,
        open: usize,
        queued: usize,
        in_flight: usize,
    ) -> bool {
        self.with_current(identity, generation, |snapshot| {
            snapshot.open_documents = open;
            snapshot.queued_requests = queued;
            snapshot.in_flight_requests = in_flight;
            snapshot.last_activity_ms = Some(now_ms());
        })
    }

    pub fn set_queue_count(
        &self,
        identity: &ServerIdentity,
        generation: u64,
        queued: usize,
    ) -> bool {
        self.with_current(identity, generation, |snapshot| {
            snapshot.queued_requests = queued;
            snapshot.last_activity_ms = Some(now_ms());
        })
    }

    pub fn set_document_counts(
        &self,
        identity: &ServerIdentity,
        generation: u64,
        open: usize,
        queued: usize,
    ) -> bool {
        self.with_current(identity, generation, |snapshot| {
            snapshot.open_documents = open;
            snapshot.queued_requests = queued;
            snapshot.last_activity_ms = Some(now_ms());
        })
    }

    pub fn increment_in_flight(&self, identity: &ServerIdentity, generation: u64) -> bool {
        self.with_current(identity, generation, |snapshot| {
            snapshot.in_flight_requests = snapshot.in_flight_requests.saturating_add(1);
            snapshot.last_activity_ms = Some(now_ms());
        })
    }

    pub fn decrement_in_flight(&self, identity: &ServerIdentity, generation: u64) -> bool {
        self.with_current(identity, generation, |snapshot| {
            snapshot.in_flight_requests = snapshot.in_flight_requests.saturating_sub(1);
            snapshot.last_activity_ms = Some(now_ms());
        })
    }

    pub fn progress(
        &self,
        identity: &ServerIdentity,
        generation: u64,
        token: String,
        params: &serde_json::Value,
    ) -> bool {
        let kind = params
            .get("value")
            .and_then(|value| value.get("kind"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("report");
        let token = truncate_message(&token);
        let mut evicted_token = false;
        let changed = self.with_current(identity, generation, |snapshot| {
            if kind == "end" {
                snapshot
                    .active_progress
                    .retain(|progress| progress.token != token);
            } else {
                let value = params.get("value").unwrap_or(&serde_json::Value::Null);
                let previous = snapshot
                    .active_progress
                    .iter()
                    .find(|progress| progress.token == token);
                let started_at_ms = previous.map_or_else(now_ms, |progress| progress.started_at_ms);
                let progress = ProgressSnapshot {
                    token: token.clone(),
                    title: value
                        .get("title")
                        .and_then(serde_json::Value::as_str)
                        .map(truncate_message)
                        .or_else(|| previous.and_then(|progress| progress.title.clone())),
                    message: value
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .map(truncate_message)
                        .or_else(|| previous.and_then(|progress| progress.message.clone())),
                    percentage: value
                        .get("percentage")
                        .and_then(serde_json::Value::as_u64)
                        .or_else(|| previous.and_then(|progress| progress.percentage)),
                    started_at_ms,
                };
                if let Some(old) = snapshot
                    .active_progress
                    .iter_mut()
                    .find(|old| old.token == token)
                {
                    *old = progress;
                } else {
                    if snapshot.active_progress.len() >= MAX_ACTIVE_PROGRESS_TOKENS {
                        snapshot.active_progress.remove(0);
                        evicted_token = true;
                    }
                    snapshot.active_progress.push(progress);
                }
            }
            snapshot.last_activity_ms = Some(now_ms());
        });
        if changed {
            let mut event = JournalEvent::simple(
                EventSource::Progress,
                EventLevel::Info,
                Some(identity.clone()),
                if kind == "end" {
                    format!("progress {token} ended")
                } else {
                    format!("progress {token} {kind}")
                },
            );
            event.generation = Some(generation);
            self.record(event);
            if evicted_token {
                let mut event = JournalEvent::simple(
                    EventSource::Progress,
                    EventLevel::Warn,
                    Some(identity.clone()),
                    format!(
                        "active progress token cap ({MAX_ACTIVE_PROGRESS_TOKENS}) reached; oldest token discarded"
                    ),
                );
                event.generation = Some(generation);
                self.record(event);
            }
        }
        changed
    }

    pub fn record_generation_event(
        &self,
        identity: ServerIdentity,
        generation: u64,
        level: EventLevel,
        message: impl Into<String>,
    ) {
        let mut event =
            JournalEvent::simple(EventSource::Lifecycle, level, Some(identity), message);
        event.generation = Some(generation);
        self.record(event);
    }

    pub fn record_server_text(
        &self,
        identity: ServerIdentity,
        generation: u64,
        source: EventSource,
        level: EventLevel,
        message: impl Into<String>,
    ) {
        let message = truncate_message(&message.into());
        let _ = self.with_current(&identity, generation, |snapshot| {
            snapshot.last_activity_ms = Some(now_ms());
            if level == EventLevel::Error {
                snapshot.last_error = Some(message.clone());
            }
        });
        let mut event = JournalEvent::simple(source, level, Some(identity), message);
        event.generation = Some(generation);
        self.record(event);
    }

    pub fn record_request(
        &self,
        identity: ServerIdentity,
        generation: u64,
        operation_id: u64,
        method: impl Into<String>,
        outcome: EventOutcome,
        message: impl Into<String>,
    ) {
        let method = method.into();
        let message = truncate_message(&message.into());
        let _ = self.with_current(&identity, generation, |snapshot| {
            snapshot.last_activity_ms = Some(now_ms());
            if matches!(
                outcome,
                EventOutcome::ServerError | EventOutcome::TransportFailure | EventOutcome::Timeout
            ) {
                snapshot.last_error = Some(message.clone());
            }
        });
        let level = level_for_outcome(outcome);
        let mut event = JournalEvent::simple(EventSource::Request, level, Some(identity), message);
        event.generation = Some(generation);
        event.operation_id = Some(operation_id);
        event.method = Some(method);
        event.outcome = outcome;
        self.record(event);
    }

    pub fn record_ui(
        &self,
        operation_id: Option<u64>,
        outcome: EventOutcome,
        message: impl Into<String>,
    ) {
        let mut event =
            JournalEvent::simple(EventSource::Ui, level_for_outcome(outcome), None, message);
        event.operation_id = operation_id;
        event.outcome = outcome;
        self.record(event);
    }

    fn with_current(
        &self,
        identity: &ServerIdentity,
        generation: u64,
        update: impl FnOnce(&mut ServerSnapshot),
    ) -> bool {
        self.with_current_if(
            identity,
            generation,
            |snapshot| snapshot.phase != ServerPhase::Stopped,
            update,
        )
    }

    fn with_current_including_stopped(
        &self,
        identity: &ServerIdentity,
        generation: u64,
        update: impl FnOnce(&mut ServerSnapshot),
    ) -> bool {
        self.with_current_if(identity, generation, |_| true, update)
    }

    fn with_current_if(
        &self,
        identity: &ServerIdentity,
        generation: u64,
        accept: impl FnOnce(&ServerSnapshot) -> bool,
        update: impl FnOnce(&mut ServerSnapshot),
    ) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(snapshot) = inner.servers.get_mut(identity) else {
            return false;
        };
        if snapshot.generation != generation || !accept(snapshot) {
            return false;
        }
        update(snapshot);
        inner.revision = inner.revision.saturating_add(1);
        true
    }

    /// Flush and stop the writer, allowing normal-close retention cleanup and
    /// releasing the active lock. Dropping the store also performs this best
    /// effort shutdown, but an explicit call is useful before terminal restore.
    pub fn shutdown(&self) {
        let mut disk = self.disk.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut writer) = disk.take() {
            let _ = writer.tx.send(DiskCommand::Flush);
            let _ = writer.tx.send(DiskCommand::Stop);
            if let Some(handle) = writer.handle.take() {
                let _ = handle.join();
            }
        }
        drop(disk);
        // Unlock before retention so this just-finished session is treated as
        // inactive and can be counted against the global budget normally.
        let _ = self
            ._session_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some((root, config)) = &self.cleanup {
            cleanup_retention(root, config);
        }
    }
}

impl Drop for ObservationStore {
    fn drop(&mut self) {
        if let Some(mut writer) = self.disk.get_mut().ok().and_then(Option::take) {
            let _ = writer.tx.send(DiskCommand::Stop);
            if let Some(handle) = writer.handle.take() {
                let _ = handle.join();
            }
        }
        let _ = self._session_lock.get_mut().ok().and_then(Option::take);
    }
}

fn default_state_root() -> PathBuf {
    crate::update::state_dir().join("lsp")
}

fn setup_disk(
    config: &LoggingConfig,
    root: &Path,
    start_ms: u128,
    disk_error: Arc<Mutex<Option<String>>>,
) -> (
    Option<DiskWriter>,
    Option<SessionLock>,
    Option<String>,
    Option<PathBuf>,
) {
    let session_root = root.join(format!(
        "{}-{}-{}",
        sortable_utc_id(start_ms),
        std::process::id(),
        unique_suffix()
    ));
    if let Err(error) = fs::create_dir_all(root) {
        return (None, None, Some(error.to_string()), None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(root, fs::Permissions::from_mode(0o700));
    }
    if let Err(error) = fs::create_dir_all(&session_root) {
        return (None, None, Some(error.to_string()), Some(session_root));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&session_root, fs::Permissions::from_mode(0o700));
    }
    let lock_path = session_root.join("active.lock");
    let file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(file) => file,
        Err(error) => return (None, None, Some(error.to_string()), Some(session_root)),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600));
    }
    if !try_lock_file(&file) {
        return (
            None,
            None,
            Some("session lock is already held".to_owned()),
            Some(session_root),
        );
    }
    // Retention only considers directories carrying this marker.  Creating
    // it *after* the lock is held closes the mkdir-to-lock race: another
    // Katamari can see the directory, but cannot mistake the half-initialized
    // directory for an inactive session.
    let marker = session_root.join(SESSION_MARKER);
    if let Err(error) = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        unlock_file(&file);
        return (None, None, Some(error.to_string()), Some(session_root));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&marker, fs::Permissions::from_mode(0o600));
    }
    let session_lock = SessionLock {
        path: lock_path,
        file,
    };
    let (tx, rx) = mpsc::sync_channel(WRITER_QUEUE_CAP);
    let session = session_root.clone();
    let cfg = config.clone();
    let writer_error = Arc::clone(&disk_error);
    let handle = thread::Builder::new()
        .name("katamari-lsp-journal".to_owned())
        .spawn(move || writer_loop(rx, session, start_ms, cfg, writer_error))
        .map_err(|error| error.to_string());
    let writer = match handle {
        Ok(handle) => Some(DiskWriter {
            tx,
            handle: Some(handle),
        }),
        Err(error) => return (None, Some(session_lock), Some(error), Some(session_root)),
    };
    cleanup_retention(root, config);
    (writer, Some(session_lock), None, Some(session_root))
}

fn writer_loop(
    rx: Receiver<DiskCommand>,
    session: PathBuf,
    start_ms: u128,
    config: LoggingConfig,
    disk_error: Arc<Mutex<Option<String>>>,
) {
    let mut segment = 1usize;
    let mut file = match open_segment(&session, segment) {
        Ok(file) => file,
        Err(error) => {
            set_disk_error(&disk_error, error);
            return;
        }
    };
    let mut bytes = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    while let Ok(command) = rx.recv() {
        match command {
            DiskCommand::Event(event) => {
                let line = event.format_line(start_ms);
                if bytes.saturating_add(line.len() as u64) > config.segment_bytes && bytes > 0 {
                    let marker = format!("{} journal segment rotated\n", clock_hms(now_ms()));
                    if let Err(error) = file.write_all(marker.as_bytes()) {
                        set_disk_error(&disk_error, error);
                        break;
                    }
                    if let Err(error) = file.flush() {
                        set_disk_error(&disk_error, error);
                        break;
                    }
                    segment = segment.saturating_add(1);
                    if segment > config.segments_per_session.max(1) {
                        let old = segment - config.segments_per_session.max(1);
                        let _ = fs::remove_file(session.join(format!("events-{old:04}.log")));
                    }
                    match open_segment(&session, segment) {
                        Ok(next) => {
                            file = next;
                            let marker =
                                format!("{} journal segment rotated\n", clock_hms(now_ms()));
                            if let Err(error) = file.write_all(marker.as_bytes()) {
                                set_disk_error(&disk_error, error);
                                break;
                            }
                            bytes = marker.len() as u64;
                            if let Err(error) = file.flush() {
                                set_disk_error(&disk_error, error);
                                break;
                            }
                        }
                        Err(error) => {
                            set_disk_error(&disk_error, error);
                            break;
                        }
                    }
                }
                if let Err(error) = file.write_all(line.as_bytes()) {
                    set_disk_error(&disk_error, error);
                    break;
                }
                bytes = bytes.saturating_add(line.len() as u64);
                if let Err(error) = file.flush() {
                    set_disk_error(&disk_error, error);
                    break;
                }
            }
            DiskCommand::Flush => {
                if let Err(error) = file.flush() {
                    set_disk_error(&disk_error, error);
                    break;
                }
            }
            DiskCommand::Stop => {
                if let Err(error) = file.flush() {
                    set_disk_error(&disk_error, error);
                }
                break;
            }
        }
    }
}

fn set_disk_error(error_slot: &Arc<Mutex<Option<String>>>, error: impl fmt::Display) {
    let mut slot = error_slot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_none() {
        *slot = Some(error.to_string());
    }
}

fn open_segment(session: &Path, segment: usize) -> io::Result<File> {
    let path = session.join(format!("events-{segment:04}.log"));
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

fn cleanup_retention(root: &Path, config: &LoggingConfig) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let now = SystemTime::now();
    let max_age = Duration::from_secs(config.max_age_days.saturating_mul(86_400));
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || !looks_like_session(&path) {
            continue;
        }
        let active =
            path.join("active.lock").exists() && !can_acquire_lock(&path.join("active.lock"));
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(UNIX_EPOCH);
        let age = now.duration_since(modified).unwrap_or_default();
        let bytes = directory_bytes(&path);
        sessions.push((path, active, modified, age, bytes));
    }
    for (path, active, _, age, _) in &sessions {
        if !active && *age > max_age {
            let _ = fs::remove_dir_all(path);
        }
    }
    let mut inactive: Vec<_> = sessions
        .into_iter()
        .filter(|(_, active, _, _, _)| !*active)
        .collect();
    inactive.sort_by_key(|(_, _, modified, _, _)| *modified);
    let mut total: u64 = inactive.iter().map(|(_, _, _, _, bytes)| *bytes).sum();
    for (path, _, _, _, bytes) in inactive {
        if total <= config.total_bytes {
            break;
        }
        let _ = fs::remove_dir_all(path);
        total = total.saturating_sub(bytes);
    }
}

fn looks_like_session(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let mut pieces = name.split('-');
    let valid_name = pieces
        .next()
        .is_some_and(|part| part.len() == 20 && part.bytes().all(|byte| byte.is_ascii_digit()))
        && pieces
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        && pieces
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        && pieces.next().is_none();
    valid_name && path.join(SESSION_MARKER).is_file()
}

fn directory_bytes(path: &Path) -> u64 {
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.metadata().ok().map(|metadata| metadata.len()))
        .sum()
}

const SESSION_MARKER: &str = ".katamari-session";

fn can_acquire_lock(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
        return false;
    };
    let acquired = try_lock_file(&file);
    if acquired {
        unlock_file(&file);
    }
    acquired
}

fn try_lock_file(file: &File) -> bool {
    match file.try_lock() {
        Ok(()) => true,
        Err(std::fs::TryLockError::WouldBlock) | Err(std::fs::TryLockError::Error(_)) => false,
    }
}

fn unlock_file(file: &File) {
    let _ = file.unlock();
}

fn sortable_utc_id(ms: u128) -> String {
    // Milliseconds since epoch sort chronologically and avoid depending on a
    // time/date crate solely for a directory name.
    format!("{ms:020}")
}

fn unique_suffix() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let counter = NEXT.fetch_add(1, Ordering::Relaxed);
    now_ms() as u64 ^ ((std::process::id() as u64) << 16) ^ counter
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn clock_hms(ms: u128) -> String {
    let total = (ms / 1000) % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        (total / 60) % 60,
        total % 60
    )
}

fn append_memory_event(inner: &mut Inner, event: JournalEvent) {
    if inner.events.len() == MEMORY_EVENT_CAP {
        inner.events.pop_front();
    }
    inner.events.push_back(event);
    inner.revision = inner.revision.saturating_add(1);
    inner.event_revision = inner.event_revision.saturating_add(1);
}

fn level_for_outcome(outcome: EventOutcome) -> EventLevel {
    match outcome {
        EventOutcome::ServerError
        | EventOutcome::TransportFailure
        | EventOutcome::Timeout
        | EventOutcome::NavigationFailure => EventLevel::Error,
        EventOutcome::Unsupported
        | EventOutcome::NoResult
        | EventOutcome::Superseded
        | EventOutcome::Cancellation
        | EventOutcome::Dropped => EventLevel::Warn,
        EventOutcome::None | EventOutcome::Result | EventOutcome::Queued => EventLevel::Info,
    }
}

/// Truncates at a UTF-8 character boundary and leaves a visible marker.  The
/// cap is measured in bytes because it bounds allocations and segment size,
/// while the resulting `String` remains valid UTF-8.
pub fn truncate_message(message: &str) -> String {
    if message.len() <= MAX_OBSERVATION_MESSAGE_BYTES {
        return message.to_owned();
    }
    const MARKER: &str = " … [truncated]";
    let limit = MAX_OBSERVATION_MESSAGE_BYTES.saturating_sub(MARKER.len());
    let mut end = limit.min(message.len());
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &message[..end], MARKER)
}

fn sanitize_message(message: &str) -> String {
    truncate_message(&message.replace(['\r', '\n'], " "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn identity() -> ServerIdentity {
        ServerIdentity::new(LangKey::Custom("rust".to_owned()), "/repo")
    }

    #[test]
    fn bounded_memory_events_drop_oldest() {
        let store = ObservationStore::in_memory();
        for i in 0..(MEMORY_EVENT_CAP + 10) {
            store.record(JournalEvent::simple(
                EventSource::Ui,
                EventLevel::Info,
                None,
                i.to_string(),
            ));
        }
        let events = store.events();
        assert_eq!(events.len(), MEMORY_EVENT_CAP);
        assert_eq!(events.first().unwrap().message, "10");
    }

    #[test]
    fn stale_generation_cannot_overwrite_new_process() {
        let store = ObservationStore::in_memory();
        let id = identity();
        let first = store.begin_generation(id.clone(), Some("old".to_owned()), vec![]);
        let second = store.begin_generation(id.clone(), Some("new".to_owned()), vec![]);
        assert!(!store.transition(&id, first, ServerPhase::Crashed, "old exited"));
        assert!(store.transition(&id, second, ServerPhase::Running, "new ready"));
        assert_eq!(store.snapshot(&id).unwrap().phase, ServerPhase::Running);
    }

    #[test]
    fn manager_generation_start_rejects_a_stale_spawn_thread() {
        let store = ObservationStore::in_memory();
        let id = identity();
        assert!(store.begin_generation_at(id.clone(), 2, Some("new".to_owned()), vec![]));
        assert!(!store.begin_generation_at(id.clone(), 1, Some("old".to_owned()), vec![]));
        let snapshot = store.snapshot(&id).unwrap();
        assert_eq!(snapshot.generation, 2);
        assert_eq!(snapshot.program.as_deref(), Some("new"));
    }

    #[test]
    fn stopped_generation_cannot_be_reclassified_as_a_crash() {
        let store = ObservationStore::in_memory();
        let id = identity();
        let generation = store.begin_generation(id.clone(), None, vec![]);
        assert!(store.transition(&id, generation, ServerPhase::Stopped, "evicted"));
        assert!(!store.transition(&id, generation, ServerPhase::Crashed, "late exit"));
        assert_eq!(store.snapshot(&id).unwrap().phase, ServerPhase::Stopped);
    }

    #[test]
    fn stale_notifications_cannot_mutate_a_stopped_snapshot() {
        let store = ObservationStore::in_memory();
        let id = identity();
        let generation = store.begin_generation(id.clone(), None, vec![]);
        assert!(store.transition(&id, generation, ServerPhase::Stopped, "evicted"));
        store.progress(
            &id,
            generation,
            "late-index".to_owned(),
            &serde_json::json!({"value":{"kind":"begin","title":"late"}}),
        );
        store.set_counts(&id, generation, 4, 5, 6);
        let snapshot = store.snapshot(&id).unwrap();
        assert!(snapshot.active_progress.is_empty());
        assert_eq!(snapshot.open_documents, 0);
        assert_eq!(snapshot.queued_requests, 0);
        assert_eq!(snapshot.in_flight_requests, 0);
    }

    #[test]
    fn progress_tokens_are_tracked_and_removed() {
        let store = ObservationStore::in_memory();
        let id = identity();
        let generation = store.begin_generation(id.clone(), None, vec![]);
        store.progress(
            &id,
            generation,
            "index".to_owned(),
            &serde_json::json!({"value":{"kind":"begin","title":"Index"}}),
        );
        assert_eq!(store.snapshot(&id).unwrap().active_progress.len(), 1);
        store.progress(
            &id,
            generation,
            "index".to_owned(),
            &serde_json::json!({"value":{"kind":"end"}}),
        );
        assert!(store.snapshot(&id).unwrap().active_progress.is_empty());
    }

    #[test]
    fn message_truncation_preserves_utf8_and_marks_the_loss() {
        let message = "界".repeat(MAX_OBSERVATION_MESSAGE_BYTES);
        let truncated = truncate_message(&message);
        assert!(truncated.len() <= MAX_OBSERVATION_MESSAGE_BYTES);
        assert!(truncated.ends_with("… [truncated]"));
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn active_progress_tokens_are_bounded() {
        let store = ObservationStore::in_memory();
        let id = identity();
        let generation = store.begin_generation(id.clone(), None, vec![]);
        for token in 0..(MAX_ACTIVE_PROGRESS_TOKENS + 10) {
            store.progress(
                &id,
                generation,
                format!("token-{token}"),
                &serde_json::json!({"value":{"kind":"begin"}}),
            );
        }
        assert_eq!(
            store.snapshot(&id).unwrap().active_progress.len(),
            MAX_ACTIVE_PROGRESS_TOKENS
        );
    }

    #[test]
    fn unrelated_hyphenated_directories_are_not_retention_candidates() {
        let root = tempfile::tempdir().unwrap();
        let unrelated = root.path().join("archive-with-hyphens");
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("important.txt"), "keep").unwrap();
        let store = ObservationStore::start_in(
            LoggingConfig {
                total_bytes: 0,
                ..LoggingConfig::default()
            },
            root.path(),
        );
        assert!(unrelated.exists());
        assert!(unrelated.join("important.txt").exists());
        store.shutdown();
    }

    #[test]
    fn snapshot_updates_do_not_advance_the_event_revision() {
        let store = ObservationStore::in_memory();
        let id = identity();
        let generation = store.begin_generation(id.clone(), None, vec![]);
        let events_revision = store.event_revision();
        store.set_counts(&id, generation, 1, 2, 3);
        assert_eq!(store.event_revision(), events_revision);
        assert!(store.revision() > events_revision);
    }

    #[test]
    fn session_journal_is_unique_and_rotates() {
        let root = tempfile::tempdir().unwrap();
        let config = LoggingConfig {
            segment_bytes: 80,
            segments_per_session: 2,
            ..LoggingConfig::default()
        };
        let store = ObservationStore::start_in(config, root.path());
        for _ in 0..20 {
            store.record(JournalEvent::simple(
                EventSource::Ui,
                EventLevel::Info,
                None,
                "hello",
            ));
        }
        store.shutdown();
        let dirs: Vec<_> = fs::read_dir(root.path()).unwrap().flatten().collect();
        assert_eq!(dirs.len(), 1);
        let logs: Vec<_> = fs::read_dir(dirs[0].path())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
            .collect();
        assert!(logs.len() <= 2);
    }

    #[test]
    fn concurrent_sessions_get_different_directories() {
        let root = tempfile::tempdir().unwrap();
        let one = ObservationStore::start_in(LoggingConfig::default(), root.path());
        let two = ObservationStore::start_in(LoggingConfig::default(), root.path());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
        one.shutdown();
        two.shutdown();
    }

    #[test]
    fn retention_removes_inactive_sessions_but_keeps_the_active_lock_holder() {
        let root = tempfile::tempdir().unwrap();
        let inactive = root.path().join("00000000000000000001-1-1");
        fs::create_dir_all(&inactive).unwrap();
        fs::write(inactive.join(SESSION_MARKER), "katamari\n").unwrap();
        fs::write(inactive.join("events-0001.log"), "old\n").unwrap();
        let store = ObservationStore::start_in(
            LoggingConfig {
                total_bytes: 0,
                ..LoggingConfig::default()
            },
            root.path(),
        );
        assert!(!inactive.exists());
        let sessions: Vec<_> = fs::read_dir(root.path()).unwrap().flatten().collect();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].path().is_dir());
        store.shutdown();
    }

    #[test]
    fn record_does_not_block_when_writer_queue_is_full() {
        let root = tempfile::tempdir().unwrap();
        let store = ObservationStore::start_in(
            LoggingConfig {
                segment_bytes: 1,
                ..LoggingConfig::default()
            },
            root.path(),
        );
        let clone = Arc::clone(&store);
        let join = thread::spawn(move || {
            for _ in 0..10_000 {
                clone.record(JournalEvent::simple(
                    EventSource::Ui,
                    EventLevel::Info,
                    None,
                    "x",
                ));
            }
        });
        join.join().unwrap();
        store.shutdown();
    }

    #[test]
    fn recovered_drop_marker_and_following_event_have_strict_sequence_order() {
        let root = tempfile::tempdir().unwrap();
        let store = ObservationStore::start_in(LoggingConfig::default(), root.path());
        store.pending_disk_drops.store(3, Ordering::Release);
        store.record(JournalEvent::simple(
            EventSource::Ui,
            EventLevel::Info,
            None,
            "after recovery",
        ));
        let events = store.events();
        let marker = events
            .iter()
            .find(|event| event.message.contains("entries dropped"))
            .expect("recovered drop marker in memory");
        let following = events
            .iter()
            .find(|event| event.message == "after recovery")
            .expect("following event in memory");
        assert!(marker.sequence < following.sequence);
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            events.len()
        );
        store.shutdown();
    }
}
