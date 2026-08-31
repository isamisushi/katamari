//! A persistent ACP session, owned by the event loop the way
//! [`crate::lsp::LspManager`] owns language servers and
//! [`crate::lsp::observe::ObservationStore`] owns the LSP journal — the TUI
//! counterpart to [`super::check`]'s headless single-turn spike. The two
//! differ in every way a "run once and exit" tool and a "sit resident across
//! a whole review session" one must:
//!
//! - `check::run` sends exactly one prompt and returns; [`run_session`] loops
//!   forever, accepting a new [`SessionCommand::Prompt`] any time the
//!   previous turn has finished.
//! - `check::run` auto-approves every `session/request_permission` (see
//!   [`super::client::choose_allow_option`]'s own call site there); this
//!   module **never** does — a permission request only ever gets answered in
//!   response to an explicit [`SessionCommand::AnswerPermission`], which
//!   only ever originates from a human keypress in `ui::mod`'s permission
//!   modal. If a future refactor shares more code between the two call
//!   sites, this invariant — no auto-allow here, ever — must survive it.
//! - `check::run` spawns the adapter immediately; [`run_session`] spawns
//!   nothing until the first [`SessionCommand::Prompt`] arrives (a session
//!   opened on a diff nobody ever asks a question of never shells out to
//!   `npx`/`claude-agent-acp` at all), and self-heals after the adapter
//!   closes — the *next* prompt re-spawns from scratch rather than wedging
//!   the session permanently dead.
//!
//! State is intentionally split the same way [`ObservationStore`] splits
//! it: bulk content (the transcript) lives in [`Inner`], pulled by whichever
//! view renders it on its own redraw cadence (see
//! [`crate::ui::agent_panel::AgentPanelView`]'s poll-by-revision refresh,
//! mirroring [`crate::ui::lsp_inspector::LspInspectorView`]'s own idiom);
//! only the handful of things the event loop must react to even while the
//! panel is closed (a turn finishing, a permission request arriving) cross
//! [`AcpNotice`]'s thin push channel.
//!
//! [`ObservationStore`]: crate::lsp::observe::ObservationStore

use super::client::{
    AcpClient, PROTOCOL_VERSION, choose_allow_option, choose_reject_option, describe_update,
    parse_new_session, permission_outcome,
};
use super::transport::AcpEvent;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Same generous bound `check::run`'s own handshake steps use — the npx
/// fallback downloads the adapter package on first use, slow once, cached
/// after. Re-declared here rather than shared: `check`'s copy is `const`
/// and private to that module, and duplicating one `Duration` literal costs
/// less than making it `pub(crate)` just to save a line.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);

/// How often [`run_session`] polls its three input sources when nothing is
/// pending — short enough that a permission answer or a new prompt reaches
/// the agent promptly, long enough not to busy-loop the manager thread.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Ring-buffer caps, mirroring [`crate::lsp::observe`]'s `events`/
/// `events_bytes` bound (count *and* bytes, whichever is hit first) — a
/// transcript is memory-only for this v1 (see the module docs' "Scope not
/// covered" list in the design this implements), so it must stay bounded
/// regardless of how long a review session runs.
const MAX_TRANSCRIPT_LINES: usize = 4000;
const MAX_TRANSCRIPT_BYTES: usize = 1024 * 1024;

/// One line of the agent panel's transcript. `Agent` is
/// [`describe_update`]'s rendered `session/update` text; `System` is
/// everything this module itself reports (a sent prompt, a turn finishing,
/// a permission decision, a closed connection) — kept as a separate variant
/// so [`crate::ui::agent_panel`] can dim it distinctly rather than the
/// transcript reading as one undifferentiated stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptLine {
    Agent(String),
    System(String),
}

impl TranscriptLine {
    pub fn text(&self) -> &str {
        match self {
            TranscriptLine::Agent(s) | TranscriptLine::System(s) => s,
        }
    }

    pub fn is_system(&self) -> bool {
        matches!(self, TranscriptLine::System(_))
    }
}

/// What the session is doing right now — read by
/// [`crate::ui::agent_panel::AgentPanelView`] for its footer line and by
/// [`AgentStore::is_turn_running`] to decide whether a new ask should be
/// rejected (see [`super`]'s design docs on why a second prompt is refused
/// rather than queued).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TurnState {
    #[default]
    Idle,
    /// The adapter process hasn't answered `initialize`/`session/new` yet —
    /// distinct from `Running` so a slow first-use `npx` download reads as
    /// "starting up," not "stuck."
    Spawning,
    Running,
    /// The agent raised `session/request_permission` and is waiting on a
    /// human decision — never auto-resolved (see the module docs).
    AwaitingPermission {
        tool_title: String,
    },
}

impl TurnState {
    /// Whether a new [`SessionCommand::Prompt`] should be rejected rather
    /// than started — anything other than `Idle` means a turn (or the
    /// handshake before one) already has the session's undivided attention.
    pub fn is_active(&self) -> bool {
        !matches!(self, TurnState::Idle)
    }

    /// The panel footer's one-line status text.
    pub fn status_text(&self) -> String {
        match self {
            TurnState::Idle => "idle".to_owned(),
            TurnState::Spawning => "spawning adapter…".to_owned(),
            TurnState::Running => "running…".to_owned(),
            TurnState::AwaitingPermission { tool_title } => {
                format!("awaiting permission — {tool_title}")
            }
        }
    }
}

#[derive(Default)]
struct Inner {
    transcript: VecDeque<TranscriptLine>,
    transcript_bytes: usize,
    revision: u64,
    state: TurnState,
    adapter_description: Option<String>,
}

/// Appends `line` to `inner`'s transcript and evicts from the front until
/// both caps are satisfied (or exactly one line remains — never evict the
/// last line just pushed, mirroring [`crate::lsp::observe`]'s identical
/// `events.len() > 1` guard). A free function over `&mut Inner` rather than
/// an `AgentStore` method so the ring-buffer behavior is unit-testable
/// without a `Mutex`/channel in the way.
fn append_transcript_line(inner: &mut Inner, line: TranscriptLine) {
    inner.transcript_bytes += line.text().len();
    inner.transcript.push_back(line);
    while inner.transcript.len() > 1
        && (inner.transcript.len() > MAX_TRANSCRIPT_LINES
            || inner.transcript_bytes > MAX_TRANSCRIPT_BYTES)
    {
        if let Some(evicted) = inner.transcript.pop_front() {
            inner.transcript_bytes = inner.transcript_bytes.saturating_sub(evicted.text().len());
        }
    }
    inner.revision = inner.revision.saturating_add(1);
}

/// Commands the UI thread feeds to [`run_session`] — the external command
/// queue `check::run` has no equivalent of (it only ever has one turn to
/// run, decided before its loop starts). `AnswerPermission` carries no
/// id/params: the session thread already holds the one pending request
/// locally (see [`run_session`]'s own `pending_permission`), so the UI only
/// ever has to say yes or no. There is deliberately no `CancelTurn` variant
/// yet — wiring `AcpClient::cancel` to a keypress is a natural next
/// increment, explicitly scoped out of this pass to keep the surface area
/// (and this enum's reachable-variant set) bounded.
enum SessionCommand {
    Prompt(String),
    AnswerPermission(bool),
    /// Carries a rendezvous `Sender` so [`AgentStore::shutdown`] can wait
    /// (briefly) for the transport to actually die before the process
    /// exits — see that method's docs.
    Shutdown(Sender<()>),
}

/// What the session thread pushes out to the event loop — deliberately thin
/// (see the module docs' "hybrid pull-store + thin push-channel" framing):
/// ordinary streaming text/tool lines never cross this channel at all, only
/// the handful of things that must be able to wake up a loop whose redraw
/// cadence already covers everything else.
#[derive(Debug, Clone)]
pub enum AcpNotice {
    TurnFinished {
        stop_reason: String,
    },
    /// Wake-only — the tool title and full request live in the store's
    /// [`TurnState::AwaitingPermission`], read from there once this arrives.
    PermissionRequested,
    SpawnFailed(String),
    Closed(String),
}

/// Shared handle the event loop holds for the session's whole lifetime —
/// constructed once by [`start`], cloned into the manager thread, and never
/// reconstructed even across a `Closed`/re-spawn cycle (see [`run_session`]'s
/// docs on self-healing).
pub struct AgentStore {
    inner: Mutex<Inner>,
    command_tx: Sender<SessionCommand>,
}

pub type AgentHandle = Arc<AgentStore>;

impl AgentStore {
    fn push_agent(&self, text: impl Into<String>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        append_transcript_line(&mut inner, TranscriptLine::Agent(text.into()));
    }

    fn push_system(&self, text: impl Into<String>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        append_transcript_line(&mut inner, TranscriptLine::System(text.into()));
    }

    fn set_state(&self, state: TurnState) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.state = state;
        inner.revision = inner.revision.saturating_add(1);
    }

    fn set_adapter_description(&self, description: String) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.adapter_description = Some(description);
        inner.revision = inner.revision.saturating_add(1);
    }

    /// Poll-by-revision key for [`crate::ui::agent_panel::AgentPanelView`],
    /// exactly [`crate::ui::lsp_inspector::LspInspectorView`]'s own
    /// `last_event_revision` idiom: bumped on every mutation, so a caller
    /// only needs to re-read [`Self::transcript`]/[`Self::state`] when this
    /// has changed since its last check.
    pub fn revision(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .revision
    }

    pub fn state(&self) -> TurnState {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state
            .clone()
    }

    /// `Some` once the lazy first spawn has resolved an adapter — `None`
    /// before that (a session nobody has asked anything of yet).
    pub fn adapter_description(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .adapter_description
            .clone()
    }

    /// Every transcript line recorded so far, oldest first. Cloned wholesale
    /// rather than paginated — bounded by [`MAX_TRANSCRIPT_LINES`]/
    /// [`MAX_TRANSCRIPT_BYTES`], so this is at most a few thousand short
    /// `String`s, cheap enough to clone on the rare frames the revision
    /// actually changed.
    pub fn transcript(&self) -> Vec<TranscriptLine> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .transcript
            .iter()
            .cloned()
            .collect()
    }

    /// Whether a new ask should be rejected rather than started (see the
    /// design's B5: a second prompt is refused with a status note, never
    /// silently queued, since the *context* a queued ask captured may no
    /// longer match wherever the reviewer has since scrolled to).
    pub fn is_turn_running(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state
            .is_active()
    }

    /// Queues a prompt turn. Fire-and-forget: the manager thread applies
    /// the "reject, don't queue, while a turn is already running" rule
    /// itself (defense in depth — see [`run_session`]), but callers should
    /// still prefer checking [`Self::is_turn_running`] first so the UI can
    /// report the rejection synchronously rather than waiting on this
    /// round trip (see `ui::ask`'s `Save` handler).
    pub fn send_prompt(&self, text: String) {
        let _ = self.command_tx.send(SessionCommand::Prompt(text));
    }

    /// Answers the one pending `session/request_permission`, if any is
    /// still outstanding by the time this is processed — see
    /// [`run_session`]'s own handling for what happens when it isn't
    /// (the agent closed the connection between the request and this
    /// keypress).
    pub fn answer_permission(&self, allow: bool) {
        let _ = self
            .command_tx
            .send(SessionCommand::AnswerPermission(allow));
    }

    /// Kills the adapter (if one was ever spawned) and stops the manager
    /// thread. Waits briefly for that to actually happen — long enough that
    /// an adapter process spawned mid-session doesn't outlive `ktmr`'s own
    /// exit (the same guarantee [`super::transport::Transport::kill`]'s own
    /// docs describe), short enough that quitting never visibly hangs. A
    /// manager thread stuck deep inside a slow handshake (spawn already
    /// under way, past 3s) can outlive this wait — a bounded risk accepted
    /// here the same way `check::run`'s own handshake has no external
    /// cancellation either; process exit reaps the thread regardless, the
    /// only cost is a possibly-orphaned adapter process in that one rare
    /// race.
    pub fn shutdown(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self
            .command_tx
            .send(SessionCommand::Shutdown(ack_tx))
            .is_err()
        {
            return; // manager thread already gone
        }
        let _ = ack_rx.recv_timeout(Duration::from_secs(3));
    }
}

/// A handle with no manager thread behind it — for [`crate::ui::agent_panel`]'s
/// own unit tests, which only exercise view-local scroll/state behavior
/// against a handle's public reads, never a real prompt round trip. Avoids
/// [`start`]'s permanent background thread, which a test has no clean way
/// to join (the same reason [`session`](self)'s own tests build an
/// [`AgentStore`] by hand rather than through `start` too).
#[cfg(test)]
pub(crate) fn for_test() -> AgentHandle {
    let (command_tx, _command_rx) = mpsc::channel();
    Arc::new(AgentStore {
        inner: Mutex::new(Inner::default()),
        command_tx,
    })
}

/// Starts the manager thread and returns the handle the event loop keeps
/// for the rest of the session, plus the notice channel a thin forwarder
/// (mirroring `ui::mod::spawn_lsp_forwarder`) relays onto `AppEvent`. No
/// adapter process exists yet — see the module docs on lazy spawn.
pub fn start(
    adapter_override: Option<String>,
    repo_root: PathBuf,
) -> (AgentHandle, Receiver<AcpNotice>) {
    let (command_tx, command_rx) = mpsc::channel();
    let (notice_tx, notice_rx) = mpsc::channel();
    let store = Arc::new(AgentStore {
        inner: Mutex::new(Inner::default()),
        command_tx,
    });
    let thread_store = Arc::clone(&store);
    std::thread::spawn(move || {
        run_session(
            adapter_override,
            repo_root,
            thread_store,
            command_rx,
            notice_tx,
        )
    });
    (store, notice_rx)
}

/// The manager thread's whole life: interleave three input sources with
/// plain `mpsc` (no `select!` — this crate is tokio-free) across an
/// unbounded number of prompt turns on one session, re-spawning the
/// adapter from scratch after a `Closed` rather than wedging permanently.
///
/// Each pass through the loop: drain at most one queued [`SessionCommand`]
/// (never block on it — a turn in flight needs this thread free to keep
/// draining `events` too), check whether the current turn's response has
/// landed, then wait up to [`POLL_INTERVAL`] on the transport's event
/// channel (this doubles as the loop's idle sleep). `events`/`client`/
/// `session_id` are `None` until the first `Prompt` spawns them, and go
/// back to `None` on `Closed` — the self-healing the module docs describe.
fn run_session(
    adapter_override: Option<String>,
    repo_root: PathBuf,
    store: AgentHandle,
    command_rx: Receiver<SessionCommand>,
    notice_tx: Sender<AcpNotice>,
) {
    let mut client: Option<AcpClient> = None;
    let mut events: Option<Receiver<AcpEvent>> = None;
    let mut session_id: Option<String> = None;
    let mut prompt_rx: Option<Receiver<Result<serde_json::Value, super::transport::AcpError>>> =
        None;
    // The one `session/request_permission` this session can have in flight
    // at a time (ACP allows only one active prompt per session, so there is
    // never more than one pending permission either) — held here, not in
    // `Inner`, because only this thread ever needs the full request; the UI
    // only needs to know *that* one is pending and its tool title, which
    // `TurnState::AwaitingPermission` already carries.
    let mut pending_permission: Option<(serde_json::Value, serde_json::Value)> = None;

    loop {
        match command_rx.try_recv() {
            Ok(SessionCommand::Shutdown(ack)) => {
                if let Some(c) = client.take() {
                    c.transport().kill();
                }
                let _ = ack.send(());
                return;
            }
            Ok(SessionCommand::Prompt(text)) => {
                if prompt_rx.is_some() {
                    // A turn (or the handshake ahead of one) already has
                    // this session's undivided attention — reject rather
                    // than queue (see the module docs' B5 reasoning).
                    store.push_system(
                        "agent: a turn is already running — wait for it to finish".to_owned(),
                    );
                } else {
                    if client.is_none() {
                        store.set_state(TurnState::Spawning);
                        match spawn_and_handshake(adapter_override.as_deref(), &repo_root) {
                            Ok((c, ev, sess, description)) => {
                                store.set_adapter_description(description);
                                client = Some(c);
                                events = Some(ev);
                                session_id = Some(sess);
                            }
                            Err(e) => {
                                store.push_system(format!("agent: {e}"));
                                store.set_state(TurnState::Idle);
                                let _ = notice_tx.send(AcpNotice::SpawnFailed(e));
                            }
                        }
                    }
                    if let (Some(c), Some(sess)) = (&client, &session_id) {
                        store.push_system(format!("you: {text}"));
                        store.set_state(TurnState::Running);
                        prompt_rx = Some(c.prompt(sess, &text));
                    }
                }
            }
            Ok(SessionCommand::AnswerPermission(allow)) => {
                if let Some((id, params)) = pending_permission.take() {
                    if let Some(c) = &client {
                        let option_id = if allow {
                            choose_allow_option(&params)
                        } else {
                            choose_reject_option(&params)
                        };
                        let granted = allow && option_id.is_some();
                        let _ = c
                            .transport()
                            .respond(id, permission_outcome(option_id.as_deref()));
                        let tool_title = params
                            .get("toolCall")
                            .and_then(|t| t.get("title"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("(tool)");
                        store.push_system(format!(
                            "agent: permission {} — {tool_title}",
                            if granted { "granted" } else { "declined" }
                        ));
                    }
                    store.set_state(TurnState::Running);
                } else {
                    // Stale: the agent closed the connection (or the turn
                    // otherwise ended) between the request and this
                    // keypress. Nothing to answer — a no-op, not a panic,
                    // per the design's explicit "must never leave an
                    // AnswerPermission unanswered against a request that
                    // already went stale" requirement.
                    store.push_system("agent: no pending permission request to answer".to_owned());
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return, // every AgentHandle dropped
        }

        if let Some(rx) = &prompt_rx
            && let Ok(result) = rx.try_recv()
        {
            prompt_rx = None;
            let stop_reason = match result {
                Ok(value) => value
                    .get("stopReason")
                    .and_then(|s| s.as_str())
                    .unwrap_or("(none)")
                    .to_owned(),
                Err(e) => {
                    store.push_system(format!("agent: prompt failed: {e}"));
                    "error".to_owned()
                }
            };
            store.push_system(format!("agent: turn finished ({stop_reason})"));
            store.set_state(TurnState::Idle);
            let _ = notice_tx.send(AcpNotice::TurnFinished { stop_reason });
        }

        let Some(ev_rx) = &events else {
            // No adapter spawned yet (or it just closed) — nothing to
            // drain; this sleep is the idle branch's equivalent of the
            // `recv_timeout` below.
            std::thread::sleep(POLL_INTERVAL);
            continue;
        };
        match ev_rx.recv_timeout(POLL_INTERVAL) {
            Ok(AcpEvent::Notification { method, params }) => {
                if method == "session/update"
                    && let Some(line) = describe_update(&params)
                {
                    store.push_agent(line);
                }
            }
            Ok(AcpEvent::Request { id, method, params }) => {
                if method == "session/request_permission" {
                    let tool_title = params
                        .get("toolCall")
                        .and_then(|t| t.get("title"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("(tool)")
                        .to_owned();
                    store.push_system(format!("agent: requesting permission — {tool_title}"));
                    store.set_state(TurnState::AwaitingPermission { tool_title });
                    pending_permission = Some((id, params));
                    let _ = notice_tx.send(AcpNotice::PermissionRequested);
                } else if let Some(c) = &client {
                    // Owed a reply regardless (see `Transport::respond_method_not_found`'s
                    // docs) — we advertise no `fs`/`terminal` capabilities,
                    // so a well-behaved agent shouldn't send anything else,
                    // but an unanswered request stalls its turn forever.
                    let _ = c.transport().respond_method_not_found(id, &method);
                }
            }
            Ok(AcpEvent::Closed { reason }) => {
                let reason_text = reason.unwrap_or_else(|| "clean eof".to_owned());
                store.push_system(format!("agent: connection closed ({reason_text})"));
                store.set_state(TurnState::Idle);
                client = None;
                events = None;
                session_id = None;
                prompt_rx = None;
                pending_permission = None;
                let _ = notice_tx.send(AcpNotice::Closed(reason_text));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // Shouldn't happen in practice — the reader thread always
                // sends `Closed` before its sender drops — but treat it the
                // same way defensively rather than spinning on a dead
                // receiver forever.
                client = None;
                events = None;
                session_id = None;
                prompt_rx = None;
                pending_permission = None;
            }
        }
    }
}

/// The lazy first-use handshake: resolve the adapter, spawn it,
/// `initialize`, `session/new`, and opt into `acceptEdits` mode when it's
/// on offer — the same steps `check::run` takes (39-104 as of this
/// writing), minus that function's `println!` trace and its final prompt
/// send, which belong to the caller here instead.
fn spawn_and_handshake(
    adapter_override: Option<&str>,
    repo_root: &Path,
) -> Result<(AcpClient, Receiver<AcpEvent>, String, String), String> {
    let resolution = super::adapter::resolve(adapter_override)?;

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
        Some(v) if v == PROTOCOL_VERSION => {}
        Some(v) => {
            return Err(format!(
                "agent answered protocol v{v}; this client speaks v{PROTOCOL_VERSION}"
            ));
        }
        None => return Err(format!("initialize result had no protocolVersion: {init}")),
    }

    let cwd_str = repo_root.to_string_lossy().into_owned();
    let new_session = client
        .new_session(&cwd_str)
        .recv_timeout(HANDSHAKE_TIMEOUT)
        .map_err(|_| handshake_failure(&client, "session/new produced no response"))?
        .map_err(|e| handshake_failure(&client, &e.to_string()))?;
    let session = parse_new_session(&new_session)
        .ok_or_else(|| format!("session/new result had no sessionId: {new_session}"))?;

    if session.available_modes.iter().any(|m| m == "acceptEdits") {
        // Best-effort: a refusal still leaves the turn usable (every edit
        // just asks permission instead), so this doesn't fail the whole
        // handshake — mirrors `check::run`'s own treatment.
        let _ = client
            .set_mode(&session.session_id, "acceptEdits")
            .recv_timeout(HANDSHAKE_TIMEOUT);
    }

    Ok((client, events, session.session_id, resolution.description))
}

fn handshake_failure(client: &AcpClient, what: &str) -> String {
    let tail = client.transport().stderr_tail();
    let tail = if tail.trim().is_empty() {
        "(empty)".to_owned()
    } else {
        tail.trim_end().to_owned()
    };
    format!("{what}; stderr: {tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> AgentStore {
        let (command_tx, _command_rx) = mpsc::channel();
        AgentStore {
            inner: Mutex::new(Inner::default()),
            command_tx,
        }
    }

    #[test]
    fn transcript_evicts_the_oldest_line_once_the_count_cap_is_exceeded() {
        let store = test_store();
        for i in 0..(MAX_TRANSCRIPT_LINES + 10) {
            store.push_agent(format!("line {i}"));
        }
        let transcript = store.transcript();
        assert_eq!(transcript.len(), MAX_TRANSCRIPT_LINES);
        assert_eq!(
            transcript.first().unwrap().text(),
            "line 10",
            "the oldest 10 lines must have been evicted, not the newest"
        );
        assert_eq!(
            transcript.last().unwrap().text(),
            format!("line {}", MAX_TRANSCRIPT_LINES + 9)
        );
    }

    #[test]
    fn transcript_evicts_on_the_byte_cap_even_under_the_line_count_cap() {
        let store = test_store();
        let big = "x".repeat(MAX_TRANSCRIPT_BYTES / 3 + 1);
        // Four lines at just over a third of the byte cap each: the fourth
        // push must evict at least the first before it fits, even though
        // four lines is nowhere near `MAX_TRANSCRIPT_LINES`.
        for _ in 0..4 {
            store.push_system(big.clone());
        }
        let transcript = store.transcript();
        assert!(
            transcript.len() < 4,
            "byte cap must evict even though the line-count cap wasn't hit: {} lines",
            transcript.len()
        );
    }

    #[test]
    fn every_mutation_bumps_the_revision() {
        let store = test_store();
        let start = store.revision();
        store.push_agent("hello");
        assert!(store.revision() > start);
        let after_push = store.revision();
        store.set_state(TurnState::Running);
        assert!(store.revision() > after_push);
    }

    #[test]
    fn turn_state_is_active_is_false_only_for_idle() {
        assert!(!TurnState::Idle.is_active());
        assert!(TurnState::Spawning.is_active());
        assert!(TurnState::Running.is_active());
        assert!(
            TurnState::AwaitingPermission {
                tool_title: "Edit foo.rs".to_owned()
            }
            .is_active()
        );
    }

    #[test]
    fn turn_state_status_text_names_the_pending_tool() {
        let state = TurnState::AwaitingPermission {
            tool_title: "Edit foo.rs".to_owned(),
        };
        assert_eq!(state.status_text(), "awaiting permission — Edit foo.rs");
    }

    #[test]
    fn is_turn_running_tracks_state() {
        let store = test_store();
        assert!(!store.is_turn_running());
        store.set_state(TurnState::Running);
        assert!(store.is_turn_running());
        store.set_state(TurnState::Idle);
        assert!(!store.is_turn_running());
    }

    #[test]
    fn adapter_description_is_none_until_set() {
        let store = test_store();
        assert!(store.adapter_description().is_none());
        store.set_adapter_description("claude-agent-acp (on PATH)".to_owned());
        assert_eq!(
            store.adapter_description().as_deref(),
            Some("claude-agent-acp (on PATH)")
        );
    }

    #[test]
    fn transcript_line_is_system_distinguishes_the_two_kinds() {
        assert!(TranscriptLine::System("x".to_owned()).is_system());
        assert!(!TranscriptLine::Agent("x".to_owned()).is_system());
    }
}
