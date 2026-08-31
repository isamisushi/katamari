//! JSON-RPC over a child process's stdio, framed the way ACP requires:
//! one message per line, no headers ("newline-delimited JSON-RPC" — see
//! agentclientprotocol.com's transports page). Deliberately the same
//! three-thread shape as [`crate::lsp::transport`] rather than a shared
//! abstraction over both: the two protocols differ in framing *and* in
//! what an unsolicited peer request means (LSP's server→client requests
//! are boilerplate the reader thread answers itself; ACP's — most
//! importantly `session/request_permission` — are application decisions
//! that must reach the UI), and threading those two policies plus two
//! framings through one generic transport costs more than the ~200 shared
//! lines it would save.
//!
//! Built on plain `std::thread` and `std::sync::mpsc` for the same reason
//! the LSP transport is: the rest of katamari is a synchronous render
//! loop, and one async subsystem would put two concurrency models at
//! every call site that touches agent state.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Everything that can go wrong turning a request into a result — same
/// taxonomy as [`crate::lsp::transport::LspError`], for the same reasons.
#[derive(Debug, Clone)]
pub enum AcpError {
    Io(String),
    Json(String),
    Agent {
        code: i64,
        message: String,
    },
    /// The transport shut down (process exited, or the reader thread hit
    /// an unrecoverable error) before this request's response arrived.
    Closed,
}

impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpError::Io(msg) => write!(f, "acp transport io error: {msg}"),
            AcpError::Json(msg) => write!(f, "acp message did not parse: {msg}"),
            AcpError::Agent { code, message } => write!(f, "acp agent error {code}: {message}"),
            AcpError::Closed => write!(f, "acp transport closed before a response arrived"),
        }
    }
}

impl std::error::Error for AcpError {}

impl From<io::Error> for AcpError {
    fn from(e: io::Error) -> Self {
        AcpError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for AcpError {
    fn from(e: serde_json::Error) -> Self {
        AcpError::Json(e.to_string())
    }
}

/// Anything inbound that isn't a response to one of our requests. Unlike
/// the LSP transport, agent→client *requests* surface here instead of
/// being auto-answered: ACP's whole permission model runs through
/// `session/request_permission`, which only the application can decide.
/// Whoever consumes these events owns answering every `Request` via
/// [`Transport::respond`] / [`Transport::respond_method_not_found`] — an
/// unanswered request stalls the agent's turn indefinitely.
#[derive(Debug, Clone)]
pub enum AcpEvent {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// The reader thread stopped: clean EOF (`reason: None`) or a
    /// read/parse error (`Some(..)`).
    Closed {
        reason: Option<String>,
    },
}

/// Writes one JSON-RPC message as a single line. Free function so the
/// framing is testable against a `Vec<u8>` without spawning a process.
/// `serde_json::to_vec` never emits a raw newline (it escapes them inside
/// strings), which is exactly the property ACP's framing relies on.
fn write_line<W: Write>(writer: &mut W, value: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(value)?;
    writer.write_all(&body)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Reads one newline-terminated JSON-RPC message. Returns `Ok(None)` on a
/// clean EOF before any bytes — the ordinary way the read loop learns the
/// agent exited. Blank lines are skipped rather than treated as errors so
/// an adapter that double-terminates a message doesn't kill the session.
fn read_line_message<R: BufRead>(reader: &mut R) -> io::Result<Option<Value>> {
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = serde_json::from_str(trimmed)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        return Ok(Some(value));
    }
}

/// A pending request's response handler — see the LSP transport's
/// `PendingSlot` for why this is boxed and type-erased.
type PendingSlot = Box<dyn FnOnce(Result<Value, AcpError>) + Send>;

struct Shared {
    writer: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, PendingSlot>>,
}

impl Shared {
    fn send_raw(&self, value: &Value) -> Result<(), AcpError> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        write_line(&mut *writer, value).map_err(AcpError::from)
    }
}

/// One agent connection: the spawned adapter process plus the
/// reader/stderr threads keeping its pipes drained. Exactly one
/// `Transport` per agent process.
pub struct Transport {
    shared: Arc<Shared>,
    // `Arc`, not a bare `Mutex<Child>`, so `crate::procguard` can hold a
    // `Weak` reference alongside this one — see that module's docs for why
    // the panic hook needs an independent way to reach and kill this same
    // child when this `Transport`'s own `Drop`-time `kill()` never gets a
    // chance to run.
    child: Arc<Mutex<Child>>,
    events_rx: Mutex<Option<Receiver<AcpEvent>>>,
    stderr_log: Arc<Mutex<String>>,
}

const STDERR_LOG_CAP: usize = 16 * 1024;

impl Transport {
    /// Spawns `command` with piped stdio (a transport always owns its
    /// child's pipes) and starts the reader/stderr threads. The ACP
    /// `initialize` handshake is [`crate::acp::client`]'s job.
    pub fn spawn(mut command: Command) -> io::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let shared = Arc::new(Shared {
            writer: Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
        });

        let (events_tx, events_rx) = mpsc::channel();
        spawn_reader_thread(Arc::clone(&shared), stdout, events_tx);

        let stderr_log = Arc::new(Mutex::new(String::new()));
        spawn_stderr_thread(stderr, Arc::clone(&stderr_log));

        let child = Arc::new(Mutex::new(child));
        // See the `child` field's own doc comment and `crate::procguard`'s
        // module docs: this is the panic-path safety net, not part of the
        // normal request/response flow above.
        crate::procguard::register(&child);

        Ok(Self {
            shared,
            child,
            events_rx: Mutex::new(Some(events_rx)),
            stderr_log,
        })
    }

    /// Takes the events channel's receiving half — once, like
    /// `mpsc::Receiver` requires. The taker owns answering every
    /// [`AcpEvent::Request`] it receives.
    pub fn take_events(&self) -> Option<Receiver<AcpEvent>> {
        self.events_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// The last bytes of the agent's stderr — the difference between
    /// "adapter exited" and "adapter exited: `command not found: claude`".
    pub fn stderr_tail(&self) -> String {
        self.stderr_log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Sends a request; the returned `Receiver` yields the type-checked
    /// result whenever the agent answers. Non-blocking, same contract as
    /// the LSP transport's `request`. For `session/prompt` the answer only
    /// arrives at end of turn — the caller must keep draining events (and
    /// answering permission requests) while it waits, or the turn stalls.
    pub fn request<P, R>(&self, method: &str, params: P) -> Receiver<Result<R, AcpError>>
    where
        P: Serialize,
        R: DeserializeOwned + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);

        let params_value = match serde_json::to_value(params) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(Err(AcpError::from(e)));
                return rx;
            }
        };

        let slot: PendingSlot = Box::new(move |result: Result<Value, AcpError>| {
            let mapped =
                result.and_then(|v| serde_json::from_value::<R>(v).map_err(AcpError::from));
            let _ = tx.send(mapped);
        });
        self.shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, slot);

        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params_value,
        });
        if let Err(e) = self.shared.send_raw(&message)
            && let Some(slot) = self
                .shared
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id)
        {
            slot(Err(e));
        }
        rx
    }

    /// Sends a notification (no id, no reply expected).
    pub fn notify<P: Serialize>(&self, method: &str, params: P) -> Result<(), AcpError> {
        let params_value = serde_json::to_value(params)?;
        self.shared.send_raw(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params_value,
        }))
    }

    /// Answers an agent→client request surfaced as [`AcpEvent::Request`].
    pub fn respond(&self, id: Value, result: Value) -> Result<(), AcpError> {
        self.shared.send_raw(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
    }

    /// Declines an agent→client request for a method this client doesn't
    /// implement (we advertise no `fs`/`terminal` capabilities, so a
    /// well-behaved agent never sends those — but a reply is still owed if
    /// one arrives, or the agent hangs waiting).
    pub fn respond_method_not_found(&self, id: Value, method: &str) -> Result<(), AcpError> {
        self.shared.send_raw(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("unhandled method: {method}") },
        }))
    }

    /// Kills the adapter process. Fired on drop too, so an `agent-check`
    /// that errors out never leaves an orphaned node process behind — the
    /// same guarantee the LSP manager gives its servers.
    pub fn kill(&self) {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.kill();
    }
}

fn spawn_reader_thread(
    shared: Arc<Shared>,
    stdout: std::process::ChildStdout,
    events_tx: Sender<AcpEvent>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let closed_reason;
        loop {
            match read_line_message(&mut reader) {
                Ok(Some(value)) => dispatch_inbound(&shared, value, &events_tx),
                Ok(None) => {
                    closed_reason = None;
                    break;
                }
                Err(e) => {
                    closed_reason = Some(e.to_string());
                    break;
                }
            }
        }
        // Fail every still-pending request so no caller blocks forever on
        // a response that can no longer arrive.
        let pending: Vec<PendingSlot> = {
            let mut map = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
            map.drain().map(|(_, slot)| slot).collect()
        };
        for slot in pending {
            slot(Err(AcpError::Closed));
        }
        let _ = events_tx.send(AcpEvent::Closed {
            reason: closed_reason,
        });
    })
}

/// Routes one inbound message: a response joins its pending request; a
/// request or notification becomes an [`AcpEvent`] for the application.
fn dispatch_inbound(shared: &Arc<Shared>, value: Value, events_tx: &Sender<AcpEvent>) {
    let method = value.get("method").and_then(|m| m.as_str());
    let id = value.get("id").cloned();
    match (method, id) {
        // Response (no method, has id): find the pending slot. Ids are
        // matched as i64 — the type this client always sends — but an
        // adapter that round-trips ids through its own JSON layer may
        // echo them back as strings ("3" for 3), which JSON-RPC permits;
        // coerce that shape rather than leaking the pending slot into a
        // handshake-length timeout. A genuinely uncorrelatable id still
        // has to be dropped.
        (None, Some(id)) => {
            let coerced = id
                .as_i64()
                .or_else(|| id.as_str().and_then(|s| s.parse().ok()));
            let Some(id_num) = coerced else { return };
            let slot = shared
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id_num);
            if let Some(slot) = slot {
                if let Some(err) = value.get("error") {
                    let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
                    let message = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("(no message)")
                        .to_string();
                    slot(Err(AcpError::Agent { code, message }));
                } else {
                    let result = value.get("result").cloned().unwrap_or(Value::Null);
                    slot(Ok(result));
                }
            }
        }
        // Agent→client request: surfaced, never auto-answered (see
        // [`AcpEvent`] on why this differs from the LSP transport).
        (Some(method), Some(id)) => {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            let _ = events_tx.send(AcpEvent::Request {
                id,
                method: method.to_string(),
                params,
            });
        }
        // Notification.
        (Some(method), None) => {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            let _ = events_tx.send(AcpEvent::Notification {
                method: method.to_string(),
                params,
            });
        }
        // No method and no id: not JSON-RPC; ignore rather than kill the
        // session over an adapter's stray debug print on stdout.
        (None, None) => {}
    }
}

fn spawn_stderr_thread(
    stderr: std::process::ChildStderr,
    log: Arc<Mutex<String>>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let mut log = log.lock().unwrap_or_else(|e| e.into_inner());
            log.push_str(&line);
            log.push('\n');
            if log.len() > STDERR_LOG_CAP {
                let excess = log.len() - STDERR_LOG_CAP;
                log.drain(..excess);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn write_line_produces_one_newline_terminated_json_document() {
        let mut buf = Vec::new();
        write_line(&mut buf, &serde_json::json!({"a": 1})).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.ends_with('\n'));
        assert_eq!(text.matches('\n').count(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(text.trim()).unwrap(),
            serde_json::json!({"a": 1})
        );
    }

    #[test]
    fn a_string_containing_a_newline_stays_on_one_wire_line() {
        // ACP's framing only works because serde escapes newlines inside
        // JSON strings — this pins that assumption.
        let mut buf = Vec::new();
        write_line(&mut buf, &serde_json::json!({"text": "line one\nline two"})).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.matches('\n').count(), 1);

        let mut cursor = Cursor::new(text.into_bytes());
        let back = read_line_message(&mut cursor).unwrap().unwrap();
        assert_eq!(back["text"], "line one\nline two");
    }

    #[test]
    fn read_line_message_round_trips_two_consecutive_messages() {
        let mut buf = Vec::new();
        write_line(&mut buf, &serde_json::json!({"n": 1})).unwrap();
        write_line(&mut buf, &serde_json::json!({"n": 2})).unwrap();

        let mut cursor = Cursor::new(buf);
        assert_eq!(
            read_line_message(&mut cursor).unwrap().unwrap()["n"],
            serde_json::json!(1)
        );
        assert_eq!(
            read_line_message(&mut cursor).unwrap().unwrap()["n"],
            serde_json::json!(2)
        );
        assert!(read_line_message(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn blank_lines_are_skipped_not_fatal() {
        let mut cursor = Cursor::new(b"\n\n{\"ok\":true}\n".to_vec());
        let back = read_line_message(&mut cursor).unwrap().unwrap();
        assert_eq!(back["ok"], serde_json::json!(true));
    }

    #[test]
    fn eof_between_messages_reads_as_none() {
        let mut cursor = Cursor::new(Vec::new());
        assert!(read_line_message(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn non_json_input_is_an_error_not_a_hang() {
        let mut cursor = Cursor::new(b"not json at all\n".to_vec());
        assert!(read_line_message(&mut cursor).is_err());
    }

    #[test]
    fn a_response_with_a_stringified_id_still_completes_its_pending_request() {
        // `sh` reads our request line and echoes a response whose id is a
        // JSON *string* — the round-trip shape a loosely-typed adapter
        // can produce. The pending slot must complete anyway.
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            r#"read line; printf '{"jsonrpc":"2.0","id":"1","result":{"ok":true}}\n'"#,
        ]);
        let transport = Transport::spawn(command).unwrap();
        let rx = transport.request::<_, Value>("initialize", serde_json::json!({}));
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the stringified id must still correlate");
        assert_eq!(result.unwrap()["ok"], serde_json::json!(true));
    }

    #[test]
    fn a_pending_request_fails_with_closed_when_the_agent_exits_unanswered() {
        // The child reads our request and exits without answering: the
        // reader thread sees EOF and must fail the pending slot — the
        // documented every-pending-request-fails-on-close guarantee —
        // rather than leave the caller blocking until its own timeout.
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "read line"]);
        let transport = Transport::spawn(command).unwrap();
        let rx = transport.request::<_, Value>("initialize", serde_json::json!({}));
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the slot must be completed, not leaked");
        assert!(matches!(result, Err(AcpError::Closed)), "{result:?}");
    }

    #[test]
    fn a_request_after_the_agent_died_fails_immediately_not_silently() {
        // `true` exits at once. Once the transport has reported Closed,
        // a new request's write hits a broken pipe and must fail the
        // caller's receiver right away (the send-error path in
        // `request`), not insert a slot nobody will ever drain.
        let transport = Transport::spawn(std::process::Command::new("true")).unwrap();
        let events = transport.take_events().unwrap();
        loop {
            match events.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(AcpEvent::Closed { .. }) => break,
                Ok(_) => continue,
                Err(e) => panic!("no Closed event from an exiting child: {e}"),
            }
        }
        let rx = transport.request::<_, Value>("initialize", serde_json::json!({}));
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the failed send must complete the slot");
        assert!(matches!(result, Err(AcpError::Io(_))), "{result:?}");
    }
}
