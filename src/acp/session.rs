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
use std::time::{Duration, Instant};

/// Same generous bound `check::run`'s own handshake steps use — the npx
/// fallback downloads the adapter package on first use, slow once, cached
/// after. Re-declared here rather than shared: `check`'s copy is `const`
/// and private to that module, and duplicating one `Duration` literal costs
/// less than making it `pub(crate)` just to save a line.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long [`run_session`] waits for a cancelled turn's own
/// `session/prompt` response to actually land before giving up on it and
/// letting a fresh prompt through anyway — see `draining_rx`'s own docs
/// for what this guards. Must be comfortably longer than the real
/// adapter's documented ~30s force-cancel backstop (see
/// [`SessionCommand::CancelTurn`]'s docs), or a well-behaved adapter's own
/// timeout would routinely lose the race against this one and never get
/// the chance to answer.
const CANCEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(35);

/// The one message for "a `SessionCommand::Prompt` arrived while a turn was
/// already running," shared by every place that can report it: this
/// module's own reject-not-queue branch below (as a transcript system
/// line), [`AcpNotice::PromptRejected`] (so the event loop can say the same
/// thing on the status bar when *that* branch — not the UI's own
/// synchronous pre-check — is what actually caught it), and `ui::mod`'s two
/// synchronous `is_turn_running` pre-checks (`Action::PushCommentsToAgent`
/// and the ask overlay's `Save`). Kept as one constant rather than four
/// hand-typed copies of the same string so none of these call sites can
/// ever drift apart and describe the same rejection differently.
pub(crate) const AGENT_BUSY_MSG: &str = "agent: a turn is already running — wait for it to finish";

/// The one message for "there's no turn to cancel," shared by the UI's own
/// synchronous [`AgentStore::is_turn_running`] pre-check (in
/// `Action::CancelAgentTurn`'s `handle_action` arm) *and* the manager
/// thread's own defense-in-depth branch in [`SessionCommand::CancelTurn`]'s
/// handling — same TOCTOU reasoning [`AGENT_BUSY_MSG`]'s own docs spell out
/// for `Prompt`: a turn can finish on its own (or the connection can close)
/// in the window between the UI's pre-check and this command actually
/// dequeuing.
pub(crate) const AGENT_NOTHING_TO_CANCEL_MSG: &str = "agent: nothing to cancel";

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
/// ever has to say yes or no.
enum SessionCommand {
    Prompt(String),
    AnswerPermission(bool),
    /// Abandons the in-flight turn client-side and best-effort notifies the
    /// adapter (`session/cancel` — see [`AcpClient::cancel`]'s docs on why
    /// this can't be waited on). See [`run_session`]'s own handling for the
    /// full sequence: any pending permission is answered as a reject first,
    /// [`TurnState`] returns to `Idle` immediately regardless of whether the
    /// adapter ever actually stops, and every `session/update`/
    /// `session/request_permission` between now and the abandoned turn's
    /// own `session/prompt` response actually landing is swallowed rather
    /// than risk it reading as a fresh turn's own traffic (ACP v1 has no
    /// turn/request id on `session/update` to correlate against otherwise —
    /// see the module docs). A [`SessionCommand::Prompt`] arriving during
    /// that window is rejected, not queued or dispatched early — see
    /// `draining_rx`'s own docs in [`run_session`] for why a fresh prompt
    /// can't safely go out until the old one is confirmed settled.
    CancelTurn,
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
    /// A [`SessionCommand::Prompt`] reached the manager thread's own
    /// reject-not-queue branch (a turn was already running by the time it
    /// dequeued the command, even though the UI's own synchronous
    /// `is_turn_running` pre-check passed) — see that branch's docs for the
    /// up-to-[`POLL_INTERVAL`] window this closes. Without this, a second
    /// `p`/ask-`Save` landing in that window was dropped with only a
    /// transcript system line to show for it — indistinguishable, from the
    /// status bar, from the first one succeeding.
    PromptRejected,
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

    /// Cancels whatever turn is currently in flight (fire-and-forget,
    /// mirroring [`Self::send_prompt`]/[`Self::answer_permission`]'s own
    /// shape) — see [`SessionCommand::CancelTurn`]'s docs for the full
    /// sequence this triggers on the manager thread. Callers should still
    /// prefer checking [`Self::is_turn_running`] first so the UI can report
    /// "nothing to cancel" synchronously; see [`AGENT_NOTHING_TO_CANCEL_MSG`]'s
    /// own docs on the TOCTOU window this doesn't fully close.
    pub fn cancel_turn(&self) {
        let _ = self.command_tx.send(SessionCommand::CancelTurn);
    }

    /// Kills the adapter (if one was ever spawned) and stops the manager
    /// thread. Waits briefly for that to actually happen — long enough that
    /// an adapter process spawned mid-session doesn't outlive `ktmr`'s own
    /// exit (the same guarantee [`super::transport::Transport::kill`]'s own
    /// docs describe), short enough that quitting never visibly hangs. A
    /// `Shutdown` queued mid-handshake is now picked up within
    /// [`POLL_INTERVAL`] rather than only once the in-flight handshake step
    /// finishes on its own (see [`wait_handshake_step`]'s docs), so the
    /// window this wait can actually be outrun in is small; process exit
    /// reaps the thread regardless if it somehow still is, the only cost
    /// then being a possibly-orphaned adapter process in that rarer race.
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
    // Client-side stand-in for turn identity — ACP v1's `session/update`
    // carries no turn/request id to correlate against (see the module
    // docs), so this is what lets a cancelled turn's late-arriving updates
    // be told apart from the next turn's own. Set by `SessionCommand::CancelTurn`;
    // cleared only once `draining_rx` (below) confirms the abandoned turn's
    // own `session/prompt` has actually resolved, or `CANCEL_DRAIN_TIMEOUT`
    // gives up on waiting for that — never merely because a fresh prompt
    // was *requested*, which used to be the bug: a straggling permission
    // request from turn A could arrive after a follow-up `Prompt` for turn
    // B had already cleared this, and would then be misread as turn B's
    // own request instead of auto-declined.
    let mut turn_abandoned = false;
    // The cancelled turn's own `prompt_rx`, kept rather than dropped —
    // "draining" until its `session/prompt` response actually lands (or
    // `CANCEL_DRAIN_TIMEOUT` elapses waiting for it). A fresh
    // `SessionCommand::Prompt` is never dispatched while this is `Some`:
    // dispatching one early would mean two `session/prompt` calls in flight
    // on the same session at once, which ACP v1 does not allow (see
    // `AcpClient::prompt`'s own doc comment), and would also force
    // `turn_abandoned` to clear before this thread can tell whether the old
    // turn is truly done raising `session/request_permission`.
    let mut draining_rx: Option<Receiver<Result<serde_json::Value, super::transport::AcpError>>> =
        None;
    let mut draining_since: Option<Instant> = None;
    // A prompt requested while `draining_rx` is still `Some` — held here
    // rather than rejected outright, and dispatched automatically the
    // moment draining resolves (see the poll below). Without this, "ask a
    // follow-up right after C-g" would be a straight race between however
    // long the *adapter* takes to actually confirm the cancel and however
    // long the *reviewer* takes to type the next question — fine on a fast
    // local fake agent, but a real one (or a loaded CI box) can lose that
    // race, rejecting a follow-up the reviewer has every reason to expect
    // "just works" the way the panel's own footer already promises. Holds
    // at most one: a second `Prompt` arriving while this is already `Some`
    // is rejected, the same reject-not-queue-more-than-one reasoning the
    // ordinary busy check applies (see the module docs' B5).
    let mut pending_after_drain: Option<String> = None;

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
                if prompt_rx.is_some() || pending_after_drain.is_some() {
                    // A turn is genuinely running, or a previous cancel is
                    // already holding one queued follow-up in
                    // `pending_after_drain` — either way this session's
                    // undivided attention is already spoken for, and a
                    // second prompt is rejected rather than queued (see the
                    // module docs' B5 reasoning: holding *two* would risk
                    // dispatching one against context the reviewer has since
                    // scrolled away from). Also notifies the event loop
                    // (`AcpNotice::PromptRejected`), not just the transcript
                    // — the UI's own synchronous pre-check catches the
                    // common case, but this branch is what actually runs
                    // when a second prompt slips in during the
                    // up-to-`POLL_INTERVAL` window between that check and
                    // this thread dequeuing the command (e.g. two `p`
                    // presses from keyboard auto-repeat).
                    store.push_system(AGENT_BUSY_MSG.to_owned());
                    let _ = notice_tx.send(AcpNotice::PromptRejected);
                } else if draining_rx.is_some() {
                    // A just-cancelled turn is still draining its own
                    // `session/prompt` response (see that field's docs) —
                    // held rather than dispatched or rejected: dispatching
                    // now would mean two `session/prompt` calls in flight at
                    // once (ACP v1 forbids this) and would risk a straggling
                    // `session/request_permission` from the old turn being
                    // misattributed to this one; rejecting would turn "ask a
                    // follow-up right after C-g" into a race against however
                    // long the adapter takes to confirm the cancel. The poll
                    // below dispatches this the moment draining resolves.
                    pending_after_drain = Some(text);
                } else if let Some(ack) = dispatch_prompt(
                    text,
                    adapter_override.as_deref(),
                    &repo_root,
                    &command_rx,
                    &store,
                    &notice_tx,
                    &mut client,
                    &mut events,
                    &mut session_id,
                    &mut prompt_rx,
                    &mut turn_abandoned,
                ) {
                    let _ = ack.send(());
                    return;
                }
            }
            Ok(SessionCommand::CancelTurn) => {
                if prompt_rx.is_none() {
                    // Nothing running — defense in depth for the TOCTOU
                    // window between the UI's own `is_turn_running`
                    // pre-check and this command being dequeued (same
                    // reasoning as the `Prompt` reject-not-queue branch
                    // above).
                    store.push_system(AGENT_NOTHING_TO_CANCEL_MSG.to_owned());
                } else {
                    store.push_system("you: cancelled the turn".to_owned());
                    // Answer any pending permission as part of the cancel —
                    // never leave it dangling for a stale `AnswerPermission`
                    // to find later. Declined, not silently dropped: an
                    // unanswered `session/request_permission` stalls the
                    // adapter's turn forever.
                    if let Some((id, params)) = pending_permission.take()
                        && let Some(c) = &client
                    {
                        let option_id = choose_reject_option(&params);
                        let _ = c
                            .transport()
                            .respond(id, permission_outcome(option_id.as_deref()));
                        let tool_title = params
                            .get("toolCall")
                            .and_then(|t| t.get("title"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("(tool)");
                        store.push_system(format!(
                            "agent: permission declined — {tool_title} (turn cancelled)"
                        ));
                    }
                    // `session/cancel` (see `AcpClient::cancel`'s docs) —
                    // best-effort. The real adapter honors this by
                    // interrupting the SDK query and eventually resolving
                    // the pending `session/prompt` with `stopReason:
                    // "cancelled"`, but that can lag up to its own 30s
                    // force-cancel backstop (or never arrive at all, for a
                    // hand-rolled `--adapter`). Rather than block this
                    // thread on either, this is fire-and-forget — the turn
                    // is abandoned client-side below regardless of whether
                    // the adapter ever actually stops.
                    if let (Some(c), Some(sess)) = (&client, &session_id) {
                        let _ = c.cancel(sess);
                    }
                    store.set_state(TurnState::Idle);
                    // Move the pending `session/prompt` receiver into
                    // `draining_rx` rather than dropping it — a fresh
                    // `SessionCommand::Prompt` must not go out (and
                    // `turn_abandoned` must not clear) until this turn's own
                    // response actually lands, or `CANCEL_DRAIN_TIMEOUT`
                    // gives up on waiting for it (see both fields' own
                    // docs). Dropping it here, as this used to, let a
                    // follow-up prompt dispatch the instant the *next*
                    // `SessionCommand::Prompt` was merely requested — with
                    // nothing to stop this turn's own late
                    // `session/request_permission` from then being
                    // misattributed to that follow-up and answered as if it
                    // were its own.
                    draining_rx = prompt_rx.take();
                    draining_since = Some(Instant::now());
                    turn_abandoned = true;
                    let _ = notice_tx.send(AcpNotice::TurnFinished {
                        stop_reason: "cancelled".to_owned(),
                    });
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

        // Polled every pass, independent of whether a command actually
        // arrived this iteration — a cancelled turn's adapter can (and, per
        // the real one's own async `canUseTool` path, sometimes does) keep
        // talking well after `session/cancel` went out, so this has to keep
        // checking on its own rather than only in response to something
        // else waking the loop. See `draining_rx`'s own docs for why this
        // gates both the busy-check above and `turn_abandoned` below.
        let mut just_drained = false;
        if let Some(rx) = &draining_rx {
            match rx.try_recv() {
                Ok(_) | Err(TryRecvError::Disconnected) => {
                    // The abandoned turn's own `session/prompt` response
                    // landed (or its channel died, e.g. the adapter process
                    // exited without ever answering) — either way, nothing
                    // more can arrive that plausibly belongs to that turn,
                    // so it's now safe to stop swallowing `session/update`
                    // and auto-declining `session/request_permission`, and
                    // to let a fresh prompt actually go out.
                    draining_rx = None;
                    draining_since = None;
                    turn_abandoned = false;
                    just_drained = true;
                }
                Err(TryRecvError::Empty) => {
                    if draining_since.is_some_and(|since| since.elapsed() >= CANCEL_DRAIN_TIMEOUT) {
                        // The adapter never confirmed the cancel at all
                        // (a hand-rolled `--adapter` that ignores
                        // `session/cancel` entirely, say) — give up waiting
                        // rather than lock the session out of new prompts
                        // forever. Any stragglers this adapter still sends
                        // after this point are no longer distinguishable
                        // from a fresh turn's own traffic, which is exactly
                        // the risk `AGENT_BUSY_MSG`'s TOCTOU precedent
                        // elsewhere in this module already accepts for
                        // narrower windows — this is the same trade-off,
                        // just bounded at `CANCEL_DRAIN_TIMEOUT` instead of
                        // `POLL_INTERVAL`.
                        store.push_system(
                            "agent: cancelled turn never confirmed — allowing new prompts anyway"
                                .to_owned(),
                        );
                        draining_rx = None;
                        draining_since = None;
                        turn_abandoned = false;
                        just_drained = true;
                    }
                }
            }
        }
        // A follow-up asked while draining was still in progress (see
        // `pending_after_drain`'s own docs) — now that draining just
        // resolved, dispatch it exactly the way a fresh `Prompt` command
        // would have, had it arrived this late instead of during the drain.
        if just_drained
            && let Some(text) = pending_after_drain.take()
            && let Some(ack) = dispatch_prompt(
                text,
                adapter_override.as_deref(),
                &repo_root,
                &command_rx,
                &store,
                &notice_tx,
                &mut client,
                &mut events,
                &mut session_id,
                &mut prompt_rx,
                &mut turn_abandoned,
            )
        {
            let _ = ack.send(());
            return;
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
                // `turn_abandoned` swallows every update between a cancel
                // and the abandoned turn's own `session/prompt` response
                // actually landing (see `draining_rx`'s docs on why that,
                // not merely "a new prompt was requested," is what clears
                // it) — the transcript can't otherwise tell a just-cancelled
                // turn's stragglers from a fresh turn's own stream.
                if method == "session/update"
                    && let Some(line) = describe_update(&params)
                    && !turn_abandoned
                {
                    store.push_agent(line);
                }
            }
            Ok(AcpEvent::Request { id, method, params }) => {
                if method == "session/request_permission" && turn_abandoned {
                    // The adapter dispatched one more tool call before it
                    // actually honored the interrupt — auto-decline rather
                    // than resurrect a permission prompt for a turn the
                    // reviewer already cancelled. Never sets `AwaitingPermission`/
                    // `pending_permission`/`PermissionRequested`: none of
                    // those exist for a turn nobody is waiting on anymore.
                    if let Some(c) = &client {
                        let option_id = choose_reject_option(&params);
                        let _ = c
                            .transport()
                            .respond(id, permission_outcome(option_id.as_deref()));
                    }
                    let tool_title = params
                        .get("toolCall")
                        .and_then(|t| t.get("title"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("(tool)");
                    store.push_system(format!(
                        "agent: auto-declined permission — {tool_title} (turn already cancelled)"
                    ));
                } else if method == "session/request_permission" {
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
                draining_rx = None;
                draining_since = None;
                pending_permission = None;
                turn_abandoned = false;
                // A queued follow-up (see `pending_after_drain`'s docs) has
                // nothing left to wait on — the connection it was waiting
                // to reuse just closed. Dropped, not re-dispatched: unlike
                // the ordinary self-heal (the *next* prompt re-spawns from
                // scratch), silently firing off a respawn from inside this
                // event handler on the reviewer's behalf, for a question
                // they typed before the connection died, risks surprising
                // them with a turn they never re-confirmed asking for.
                if pending_after_drain.take().is_some() {
                    store.push_system(
                        "agent: queued follow-up dropped — connection closed".to_owned(),
                    );
                }
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
                draining_rx = None;
                draining_since = None;
                pending_permission = None;
                turn_abandoned = false;
                pending_after_drain = None;
            }
        }
    }
}

/// What [`spawn_and_handshake`] resolved to. Not a plain `Result` — once
/// the handshake became interruptible (see that function's own docs on
/// why), "the adapter answered" and "the adapter failed to" needed two more
/// siblings: a `SessionCommand::CancelTurn`/`Shutdown` arriving mid-flight
/// is neither of those, and each needs different handling from the caller
/// than a `Failed` handshake does (no `SpawnFailed` notice, no error line
/// phrased as if the adapter were at fault).
enum SpawnOutcome {
    Ready(AcpClient, Receiver<AcpEvent>, String, String),
    Failed(String),
    /// A [`SessionCommand::CancelTurn`] arrived before the handshake
    /// finished — the half-spawned adapter process has already been killed
    /// by the time this is returned.
    Cancelled,
    /// A [`SessionCommand::Shutdown`] arrived before the handshake
    /// finished — same treatment, plus the ack channel the caller must
    /// still signal before returning from [`run_session`] itself.
    ShuttingDown(Sender<()>),
}

/// One handshake step's outcome — what [`wait_handshake_step`] resolves a
/// single `initialize`/`session/new`/`session/set_mode` wait into.
enum StepOutcome<T> {
    Ready(T),
    TimedOut,
    Cancelled,
    ShuttingDown(Sender<()>),
}

/// Waits for one handshake response while staying receptive to
/// `command_rx` — polling both on [`POLL_INTERVAL`], the same idiom
/// [`run_session`]'s own main loop uses for its three input sources,
/// rather than a single blocking `recv_timeout(HANDSHAKE_TIMEOUT)` with no
/// way to interrupt it at all. That used to mean a C-g pressed while
/// `TurnState::Spawning` just sat in `command_rx`, unprocessed, until the
/// in-flight handshake step finished or timed out on its own — up to
/// `HANDSHAKE_TIMEOUT` (3× worst case) despite the panel's footer
/// advertising an immediate cancel the whole time.
///
/// Only `CancelTurn`/`Shutdown` are actioned here — `Prompt`/
/// `AnswerPermission` can't legitimately arrive this early (the UI's own
/// `is_turn_running` pre-check blocks a second `Prompt` while `Spawning`
/// counts as active, and there's no pending permission to answer before a
/// session even exists yet) and are silently dropped if they somehow do —
/// defense in depth for a path that shouldn't be reachable, not a real one.
fn wait_handshake_step<T>(
    rx: &Receiver<T>,
    command_rx: &Receiver<SessionCommand>,
    deadline: Instant,
) -> StepOutcome<T> {
    loop {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(v) => return StepOutcome::Ready(v),
            Err(RecvTimeoutError::Disconnected) => return StepOutcome::TimedOut,
            Err(RecvTimeoutError::Timeout) => {}
        }
        match command_rx.try_recv() {
            Ok(SessionCommand::CancelTurn) => return StepOutcome::Cancelled,
            Ok(SessionCommand::Shutdown(ack)) => return StepOutcome::ShuttingDown(ack),
            _ => {}
        }
        if Instant::now() >= deadline {
            return StepOutcome::TimedOut;
        }
    }
}

/// Spawns the adapter if needed and sends one prompt turn — the shared
/// tail end of both a fresh [`SessionCommand::Prompt`] dispatching right
/// away and a prompt that was held in `pending_after_drain` while a
/// previous cancelled turn's own response was still outstanding, once that
/// resolves (see that field's own docs in [`run_session`]). Takes every
/// piece of state it might touch by `&mut` reference rather than a bundled
/// struct — `run_session`'s own locals, not fields of one shared type, so
/// there's nothing to bundle them into without adding a type that exists
/// for this one call site.
///
/// Returns `Some` only when a [`SessionCommand::Shutdown`] interrupted the
/// spawn (see [`wait_handshake_step`]'s docs on why that can now happen
/// mid-handshake at all) — the caller must ack it and return from
/// `run_session` itself, exactly like the top-level `Shutdown` handling
/// does; every other outcome (sent, spawn failed, spawn cancelled) is
/// already fully handled internally and needs nothing further from the
/// caller.
#[allow(clippy::too_many_arguments)]
fn dispatch_prompt(
    text: String,
    adapter_override: Option<&str>,
    repo_root: &Path,
    command_rx: &Receiver<SessionCommand>,
    store: &AgentHandle,
    notice_tx: &Sender<AcpNotice>,
    client: &mut Option<AcpClient>,
    events: &mut Option<Receiver<AcpEvent>>,
    session_id: &mut Option<String>,
    prompt_rx: &mut Option<Receiver<Result<serde_json::Value, super::transport::AcpError>>>,
    turn_abandoned: &mut bool,
) -> Option<Sender<()>> {
    if client.is_none() {
        store.set_state(TurnState::Spawning);
        match spawn_and_handshake(adapter_override, repo_root, command_rx) {
            SpawnOutcome::Ready(c, ev, sess, description) => {
                store.set_adapter_description(description);
                *client = Some(c);
                *events = Some(ev);
                *session_id = Some(sess);
            }
            SpawnOutcome::Failed(e) => {
                store.push_system(format!("agent: {e}"));
                store.set_state(TurnState::Idle);
                let _ = notice_tx.send(AcpNotice::SpawnFailed(e));
                return None;
            }
            SpawnOutcome::Cancelled => {
                // C-g landed while still `Spawning` — the half-connected
                // adapter has already been killed by `spawn_and_handshake`
                // itself; this prompt was never sent anywhere, so there's
                // nothing to drain (`draining_rx` only applies once a real
                // `session/prompt` went out) and nothing further to queue —
                // any text held in `pending_after_drain` for *this* call
                // is simply dropped, same as a fresh `Prompt` cancelled
                // mid-spawn always has been.
                store.push_system("you: cancelled the turn".to_owned());
                store
                    .push_system("agent: spawn cancelled before the handshake finished".to_owned());
                store.set_state(TurnState::Idle);
                let _ = notice_tx.send(AcpNotice::TurnFinished {
                    stop_reason: "cancelled".to_owned(),
                });
                return None;
            }
            SpawnOutcome::ShuttingDown(ack) => return Some(ack),
        }
    }
    if let (Some(c), Some(sess)) = (client.as_ref(), session_id.as_ref()) {
        store.push_system(format!("you: {text}"));
        store.set_state(TurnState::Running);
        *prompt_rx = Some(c.prompt(sess, &text));
        // Defensive, not load-bearing: every caller of this function only
        // reaches here once `draining_rx` is already `None` (the busy/hold
        // checks above route anything else to `pending_after_drain` or a
        // rejection instead), which is what actually clears
        // `turn_abandoned` — see that flag's own docs. This just keeps a
        // freshly-spawned or never-cancelled session's first turn starting
        // from `false` too.
        *turn_abandoned = false;
    }
    None
}

/// The lazy first-use handshake: resolve the adapter, spawn it,
/// `initialize`, `session/new`, and opt into `acceptEdits` mode when it's
/// on offer — the same steps `check::run` takes (39-104 as of this
/// writing), minus that function's `println!` trace and its final prompt
/// send, which belong to the caller here instead. Cancellable at every
/// step via `command_rx` — see [`wait_handshake_step`]'s docs.
fn spawn_and_handshake(
    adapter_override: Option<&str>,
    repo_root: &Path,
    command_rx: &Receiver<SessionCommand>,
) -> SpawnOutcome {
    let resolution = match super::adapter::resolve(adapter_override) {
        Ok(r) => r,
        Err(e) => return SpawnOutcome::Failed(e),
    };

    let client = match AcpClient::spawn(resolution.command) {
        Ok(c) => c,
        Err(e) => return SpawnOutcome::Failed(format!("failed to spawn adapter: {e}")),
    };
    let events = client
        .transport()
        .take_events()
        .expect("first and only take_events");

    let init_rx = client.initialize();
    let init = match wait_handshake_step(&init_rx, command_rx, Instant::now() + HANDSHAKE_TIMEOUT) {
        StepOutcome::Ready(Ok(v)) => v,
        StepOutcome::Ready(Err(e)) => {
            return SpawnOutcome::Failed(handshake_failure(&client, &e.to_string()));
        }
        StepOutcome::TimedOut => {
            return SpawnOutcome::Failed(handshake_failure(
                &client,
                "initialize produced no response",
            ));
        }
        StepOutcome::Cancelled => {
            client.transport().kill();
            return SpawnOutcome::Cancelled;
        }
        StepOutcome::ShuttingDown(ack) => {
            client.transport().kill();
            return SpawnOutcome::ShuttingDown(ack);
        }
    };
    match init.get("protocolVersion").and_then(|v| v.as_i64()) {
        Some(v) if v == PROTOCOL_VERSION => {}
        Some(v) => {
            return SpawnOutcome::Failed(format!(
                "agent answered protocol v{v}; this client speaks v{PROTOCOL_VERSION}"
            ));
        }
        None => {
            return SpawnOutcome::Failed(format!(
                "initialize result had no protocolVersion: {init}"
            ));
        }
    }

    let cwd_str = repo_root.to_string_lossy().into_owned();
    let session_rx = client.new_session(&cwd_str);
    let new_session =
        match wait_handshake_step(&session_rx, command_rx, Instant::now() + HANDSHAKE_TIMEOUT) {
            StepOutcome::Ready(Ok(v)) => v,
            StepOutcome::Ready(Err(e)) => {
                return SpawnOutcome::Failed(handshake_failure(&client, &e.to_string()));
            }
            StepOutcome::TimedOut => {
                return SpawnOutcome::Failed(handshake_failure(
                    &client,
                    "session/new produced no response",
                ));
            }
            StepOutcome::Cancelled => {
                client.transport().kill();
                return SpawnOutcome::Cancelled;
            }
            StepOutcome::ShuttingDown(ack) => {
                client.transport().kill();
                return SpawnOutcome::ShuttingDown(ack);
            }
        };
    let session = match parse_new_session(&new_session) {
        Some(s) => s,
        None => {
            return SpawnOutcome::Failed(format!(
                "session/new result had no sessionId: {new_session}"
            ));
        }
    };

    if session.available_modes.iter().any(|m| m == "acceptEdits") {
        let mode_rx = client.set_mode(&session.session_id, "acceptEdits");
        // Best-effort (see the original docs): a plain timeout or an error
        // result here doesn't fail the whole handshake, mirrors
        // `check::run`'s own treatment — every edit just asks permission
        // instead. `Cancelled`/`ShuttingDown` are the exception: there's no
        // point finishing a handshake for a turn that's already being torn
        // down.
        match wait_handshake_step(&mode_rx, command_rx, Instant::now() + HANDSHAKE_TIMEOUT) {
            StepOutcome::Cancelled => {
                client.transport().kill();
                return SpawnOutcome::Cancelled;
            }
            StepOutcome::ShuttingDown(ack) => {
                client.transport().kill();
                return SpawnOutcome::ShuttingDown(ack);
            }
            StepOutcome::Ready(_) | StepOutcome::TimedOut => {}
        }
    }

    SpawnOutcome::Ready(client, events, session.session_id, resolution.description)
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
    fn is_turn_running_goes_false_after_a_cancel_like_transition_from_any_active_state() {
        // Mirrors what `SessionCommand::CancelTurn`'s handling does to
        // `TurnState` regardless of which active state it interrupts —
        // `Running` (the common case) and `AwaitingPermission` (cancelling
        // from the permission modal, which the manager thread's cancel arm
        // resolves as a reject before this same `Idle` transition).
        let store = test_store();
        store.set_state(TurnState::Running);
        assert!(store.is_turn_running());
        store.set_state(TurnState::Idle);
        assert!(!store.is_turn_running());

        store.set_state(TurnState::AwaitingPermission {
            tool_title: "Edit foo.rs".to_owned(),
        });
        assert!(store.is_turn_running());
        store.set_state(TurnState::Idle);
        assert!(
            !store.is_turn_running(),
            "cancelling while a permission is pending must still leave the session idle"
        );
    }

    #[test]
    fn cancel_turn_sends_a_command_without_panicking_even_with_no_manager_thread() {
        // Fire-and-forget, mirroring `send_prompt`/`answer_permission`'s
        // own untested-beyond-this-shape precedent — the real command
        // handling only runs on a live `run_session` manager thread, which
        // this module has no in-process test harness for (see
        // `tests/e2e/agent_panel.rs`'s cancel tests for that coverage).
        // This just pins that the fire-and-forget send itself can never
        // panic, even against a `test_store()`'s abandoned receiver.
        let store = test_store();
        store.cancel_turn();
    }

    #[test]
    fn agent_nothing_to_cancel_msg_is_distinct_from_agent_busy_msg() {
        // The two rejection messages this module reports for `Prompt`
        // (busy) vs. `CancelTurn` (nothing to cancel) must never read as
        // the same status line — see both constants' own docs.
        assert_ne!(AGENT_NOTHING_TO_CANCEL_MSG, AGENT_BUSY_MSG);
        assert!(!AGENT_NOTHING_TO_CANCEL_MSG.is_empty());
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
