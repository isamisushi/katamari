//! JSON-RPC over a child process's stdio, framed the way LSP requires
//! (`Content-Length` headers, no other framing). Built on plain
//! `std::thread` and `std::sync::mpsc` rather than an async runtime — the
//! rest of `katamari` is a synchronous, single-threaded render loop, and
//! adding tokio just for this one subsystem would mean two different
//! concurrency models meeting at every call site that touches LSP state.
//! Three threads do the work an async runtime would otherwise schedule
//! inside one: [`Transport::spawn`]'s caller thread (writes requests as
//! they're made), a reader thread (parses framed messages off the child's
//! stdout and dispatches them), and a stderr-draining thread (so a chatty
//! server's stderr pipe never fills up and blocks the server itself).
//!
//! Callers never see a raw [`serde_json::Value`] response: [`Transport::request`]
//! is generic over the expected result type and deserializes into it before
//! the caller's [`Receiver`] ever gets a value.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Everything that can go wrong turning a request into a result: transport
/// failure (the pipe closed, a read/write errored), a response that didn't
/// parse as the type the caller expected, or the server itself reporting a
/// JSON-RPC error object.
#[derive(Debug, Clone)]
pub enum LspError {
    Io(String),
    Json(String),
    Server {
        code: i64,
        message: String,
    },
    /// The transport shut down (process exited, or the reader thread hit an
    /// unrecoverable error) before this request's response arrived.
    Closed,
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LspError::Io(msg) => write!(f, "lsp transport io error: {msg}"),
            LspError::Json(msg) => write!(f, "lsp message did not parse: {msg}"),
            LspError::Server { code, message } => write!(f, "lsp server error {code}: {message}"),
            LspError::Closed => write!(f, "lsp transport closed before a response arrived"),
        }
    }
}

impl std::error::Error for LspError {}

impl From<io::Error> for LspError {
    fn from(e: io::Error) -> Self {
        LspError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for LspError {
    fn from(e: serde_json::Error) -> Self {
        LspError::Json(e.to_string())
    }
}

/// Something the server sent that wasn't a response to one of our requests:
/// a notification (most importantly `$/progress`, which drives the status
/// bar's indexing spinner) or the transport's own end-of-life signal.
/// Server-to-client *requests* (`client/registerCapability` and the couple
/// of others rust-analyzer sends during startup) are answered automatically
/// by the reader thread instead of surfacing here — they need a reply, not
/// application logic, and every server we talk to in M3a expects the same
/// canned replies.
#[derive(Debug, Clone)]
pub enum LspEvent {
    Notification {
        method: String,
        params: Value,
    },
    /// The reader thread stopped: the process's stdout closed (clean exit
    /// or a crash) or a frame failed to parse. `reason` is `None` for a
    /// clean EOF, `Some(..)` when a read/parse error caused the stop.
    Closed {
        reason: Option<String>,
    },
}

/// Writes one JSON-RPC message with an LSP `Content-Length` header. A free
/// function (not a method) so the framing logic is testable against a plain
/// `Vec<u8>` without spawning a process.
fn write_framed<W: Write>(writer: &mut W, value: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

/// Reads one JSON-RPC message: a block of `Header: value\r\n` lines ended by
/// a blank line, then exactly `Content-Length` bytes of JSON body. Returns
/// `Ok(None)` for a clean EOF encountered before any header bytes (i.e. the
/// peer closed the pipe between messages, not mid-message) — the ordinary
/// way a transport's read loop learns the child process exited.
fn read_framed<R: BufRead>(reader: &mut R) -> io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut saw_any_header_bytes = false;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return if saw_any_header_bytes {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "eof mid-message header",
                ))
            } else {
                Ok(None)
            };
        }
        saw_any_header_bytes = true;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            let len = value
                .trim()
                .parse::<usize>()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad Content-Length"))?;
            content_length = Some(len);
        }
        // Any other header (e.g. Content-Type) is accepted and ignored.
    }
    let len = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "message had no Content-Length")
    })?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    let value =
        serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(value))
}

/// A pending request's response handler: converts the raw JSON result (or
/// transport error) into the caller's expected type and sends it down the
/// `Receiver` `Transport::request` handed back. Boxed and type-erased
/// because a single `HashMap` holds every in-flight request regardless of
/// what type each one expects back.
type PendingSlot = Box<dyn FnOnce(Result<Value, LspError>) + Send>;

struct Shared {
    writer: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, PendingSlot>>,
}

impl Shared {
    fn send_raw(&self, value: &Value) -> Result<(), LspError> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        write_framed(&mut *writer, value).map_err(LspError::from)
    }

    /// Replies to a server-to-client request. Used both by the public API
    /// (none, in M3a) and by the reader thread's automatic handling of the
    /// handful of requests rust-analyzer needs answered during startup.
    fn respond(&self, id: Value, result: Value) {
        let _ = self.send_raw(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }));
    }

    fn respond_method_not_found(&self, id: Value, method: &str) {
        let _ = self.send_raw(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("unhandled method: {method}") },
        }));
    }
}

/// One server connection: a spawned child process plus the reader/stderr
/// threads that keep its pipes drained. Cloning is not supported — there is
/// exactly one `Transport` per server process, matching
/// [`crate::lsp::client::Client`] one level up.
pub struct Transport {
    shared: Arc<Shared>,
    // `Arc`, not a bare `Mutex<Child>`, so `crate::procguard` can hold a
    // `Weak` reference alongside this one — see that module's docs for why
    // the panic hook needs an independent way to reach and kill this same
    // child when this `Transport`'s own `Drop`-time `kill()` never gets a
    // chance to run.
    child: Arc<Mutex<Child>>,
    events_rx: Mutex<Option<Receiver<LspEvent>>>,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    stderr_handle: Mutex<Option<JoinHandle<()>>>,
    /// The last several KiB of the server's stderr, for surfacing in an
    /// `Unavailable`/`Crashed` reason when the process dies before or
    /// during initialization. Bounded by [`STDERR_LOG_CAP`] so a chatty
    /// server can't grow this without limit over a long session.
    stderr_log: Arc<Mutex<String>>,
}

pub type StderrSink = Arc<dyn Fn(String) + Send + Sync + 'static>;

const STDERR_LOG_CAP: usize = 16 * 1024;

impl Transport {
    /// Spawns `command` with piped stdio (overriding whatever the caller may
    /// have configured — a transport always owns its child's pipes) and
    /// starts the reader/stderr threads. Returns once the process is
    /// spawned; the LSP `initialize` handshake itself is
    /// [`crate::lsp::client::Client`]'s job, not the transport's.
    #[expect(
        dead_code,
        reason = "headless callers use the transport without observability"
    )]
    pub fn spawn(command: Command) -> io::Result<Self> {
        Self::spawn_with_stderr(command, None)
    }

    /// As [`Self::spawn`], forwarding each complete stderr line to an
    /// optional observer in addition to retaining the initialize-failure
    /// tail. The callback runs on the drain thread and must be non-blocking.
    pub fn spawn_with_stderr(
        mut command: Command,
        stderr_sink: Option<StderrSink>,
    ) -> io::Result<Self> {
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
        let reader_handle = spawn_reader_thread(Arc::clone(&shared), stdout, events_tx);

        let stderr_log = Arc::new(Mutex::new(String::new()));
        let stderr_handle = spawn_stderr_thread(stderr, Arc::clone(&stderr_log), stderr_sink);

        let child = Arc::new(Mutex::new(child));
        // See the `child` field's own doc comment and `crate::procguard`'s
        // module docs: this is the panic-path safety net, not part of the
        // normal request/response flow above.
        crate::procguard::register(&child);

        Ok(Self {
            shared,
            child,
            events_rx: Mutex::new(Some(events_rx)),
            reader_handle: Mutex::new(Some(reader_handle)),
            stderr_handle: Mutex::new(Some(stderr_handle)),
            stderr_log,
        })
    }

    /// Takes the events channel's receiving half. Callers hand this to
    /// whatever thread multiplexes LSP events with the rest of the app (see
    /// `ui::mod`'s event loop) — it can only be taken once, since an
    /// `mpsc::Receiver` has exactly one consumer.
    pub fn take_events(&self) -> Option<Receiver<LspEvent>> {
        self.events_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// The last bytes of the server's stderr, for diagnostics when it exits
    /// unexpectedly or fails to answer `initialize` in time. Read by
    /// [`crate::lsp::client::Client::start`]'s failure path, which appends
    /// this into the error message a user actually sees — the fix for a
    /// documented class of bug where a language server dies almost
    /// immediately for an environment reason (jdtls printing "requires at
    /// least Java 21" and exiting, say) and a naive caller reports a plain
    /// 30s timeout instead, with no hint why.
    pub fn stderr_tail(&self) -> String {
        self.stderr_log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Sends a request and returns a `Receiver` the caller can poll or
    /// select on for the (type-checked, already-deserialized) result.
    /// Non-blocking: the send happens on the calling thread, but nothing
    /// waits for a reply here — that's the whole point of handing back a
    /// `Receiver` instead of the result itself.
    pub fn request<P, R>(&self, method: &str, params: P) -> Receiver<Result<R, LspError>>
    where
        P: Serialize,
        R: DeserializeOwned + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);

        let params_value = match serde_json::to_value(params) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(Err(LspError::from(e)));
                return rx;
            }
        };

        let slot: PendingSlot = Box::new(move |result: Result<Value, LspError>| {
            let mapped =
                result.and_then(|v| serde_json::from_value::<R>(v).map_err(LspError::from));
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

    /// Sends a notification (no response expected, no id assigned).
    pub fn notify<P: Serialize>(&self, method: &str, params: P) {
        let params_value = serde_json::to_value(params).unwrap_or(Value::Null);
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params_value,
        });
        let _ = self.shared.send_raw(&message);
    }

    /// Kills the child process if it's still running and joins the reader
    /// thread (which exits promptly once the killed process's stdout
    /// closes). Idempotent — calling this more than once, or after the
    /// process already exited on its own, is a no-op rather than an error.
    /// [`crate::lsp::client::Client::shutdown`] calls this only as a
    /// fallback after the graceful `shutdown`/`exit` protocol sequence
    /// times out, and `Drop` calls it as a last-resort safety net so a
    /// forgotten `Transport` can never leave a zombie server process behind.
    pub fn kill(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self
            .reader_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = handle.join();
        }
        if let Some(handle) = self
            .stderr_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = handle.join();
        }
    }

    /// Whether the child process has exited, without blocking.
    pub fn has_exited(&self) -> bool {
        matches!(self.child.lock().map(|mut c| c.try_wait()), Ok(Ok(Some(_))))
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.lock().ok().map(|child| child.id())
    }

    pub fn exit_status(&self) -> Option<String> {
        self.child
            .lock()
            .ok()
            .and_then(|mut child| child.try_wait().ok().flatten())
            .map(|status| status.to_string())
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.kill();
    }
}

fn spawn_reader_thread(
    shared: Arc<Shared>,
    stdout: impl Read + Send + 'static,
    events_tx: Sender<LspEvent>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_framed(&mut reader) {
                Ok(Some(value)) => dispatch_incoming(&shared, &events_tx, value),
                Ok(None) => {
                    let _ = events_tx.send(LspEvent::Closed { reason: None });
                    break;
                }
                Err(e) => {
                    let _ = events_tx.send(LspEvent::Closed {
                        reason: Some(e.to_string()),
                    });
                    break;
                }
            }
        }
        fail_all_pending(&shared, LspError::Closed);
    })
}

/// Routes one parsed message to wherever it belongs: a response completes a
/// pending request, a server-to-client request gets an automatic reply, and
/// a notification is forwarded to the events channel.
fn dispatch_incoming(shared: &Arc<Shared>, events_tx: &Sender<LspEvent>, value: Value) {
    let has_id = value.get("id").is_some();
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);

    match (has_id, method) {
        (true, None) => complete_pending(shared, value),
        (true, Some(method)) => handle_server_request(shared, value, &method),
        (false, Some(method)) => {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            let _ = events_tx.send(LspEvent::Notification { method, params });
        }
        (false, None) => {
            // Not a well-formed JSON-RPC message; nothing sensible to do
            // with it but drop it — a malformed frame from the server is
            // not this client's error to raise into the UI.
        }
    }
}

fn complete_pending(shared: &Arc<Shared>, value: Value) {
    let Some(id) = value.get("id").and_then(Value::as_i64) else {
        return;
    };
    let Some(slot) = shared
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id)
    else {
        return;
    };
    if let Some(error) = value.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_owned();
        slot(Err(LspError::Server { code, message }));
    } else {
        slot(Ok(value.get("result").cloned().unwrap_or(Value::Null)));
    }
}

/// The minimal set of server-to-client requests rust-analyzer needs
/// answered to proceed past startup. Anything else gets a `MethodNotFound`
/// reply rather than silence, so a server that (correctly) expects an
/// answer to every request it sends never blocks waiting for one we were
/// never going to give.
fn handle_server_request(shared: &Arc<Shared>, value: Value, method: &str) {
    let Some(id) = value.get("id").cloned() else {
        return;
    };
    match method {
        "client/registerCapability" | "window/workDoneProgress/create" => {
            shared.respond(id, Value::Null);
        }
        "workspace/configuration" => {
            let count = value
                .get("params")
                .and_then(|p| p.get("items"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            shared.respond(id, Value::Array(vec![Value::Null; count]));
        }
        other => shared.respond_method_not_found(id, other),
    }
}

fn fail_all_pending(shared: &Arc<Shared>, error: LspError) {
    let pending = std::mem::take(&mut *shared.pending.lock().unwrap_or_else(|e| e.into_inner()));
    for (_, slot) in pending {
        slot(Err(error.clone()));
    }
}

fn spawn_stderr_thread(
    stderr: impl Read + Send + 'static,
    log: Arc<Mutex<String>>,
    sink: Option<StderrSink>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut chunk = [0_u8; 4096];
        let mut line = Vec::with_capacity(4096);
        let mut truncated = false;
        while let Ok(read) = reader.read(&mut chunk) {
            if read == 0 {
                if !line.is_empty() || truncated {
                    emit_stderr_line(&line, truncated, &log, &sink);
                }
                break;
            }
            for byte in &chunk[..read] {
                if *byte == b'\n' {
                    emit_stderr_line(&line, truncated, &log, &sink);
                    line.clear();
                    truncated = false;
                } else if line.len() < STDERR_LOG_CAP {
                    line.push(*byte);
                } else {
                    // Keep draining the pipe without retaining the rest of a
                    // pathological line. The next newline starts a fresh,
                    // independently bounded message.
                    truncated = true;
                }
            }
        }
    })
}

fn emit_stderr_line(
    bytes: &[u8],
    truncated: bool,
    log: &Arc<Mutex<String>>,
    sink: &Option<StderrSink>,
) {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    text = truncate_stderr_line(&text, truncated);
    let text = text.trim_end_matches('\r').to_owned();
    if let Some(sink) = sink {
        sink(text.clone());
    }
    let mut log = log.lock().unwrap_or_else(|e| e.into_inner());
    log.push_str(&text);
    log.push('\n');
    trim_stderr_tail(&mut log);
}

fn truncate_stderr_line(line: &str, was_truncated: bool) -> String {
    const MARKER: &str = " … [truncated]";
    if !was_truncated && line.len() <= STDERR_LOG_CAP {
        return line.to_owned();
    }
    let mut end = STDERR_LOG_CAP.saturating_sub(MARKER.len());
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &line[..end], MARKER)
}

fn trim_stderr_tail(log: &mut String) {
    if log.len() <= STDERR_LOG_CAP {
        return;
    }
    let excess = log.len() - STDERR_LOG_CAP;
    let boundary = log
        .char_indices()
        .find(|(index, _)| *index >= excess)
        .map_or(log.len(), |(index, _)| index);
    log.drain(..boundary);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn write_framed_produces_a_valid_content_length_header() {
        let mut buf = Vec::new();
        write_framed(&mut buf, &serde_json::json!({"a": 1})).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let body = serde_json::to_string(&serde_json::json!({"a": 1})).unwrap();
        assert!(text.starts_with(&format!("Content-Length: {}\r\n\r\n", body.len())));
        assert!(text.ends_with(&body));
    }

    #[test]
    fn read_framed_round_trips_what_write_framed_wrote() {
        let mut buf = Vec::new();
        let original = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        write_framed(&mut buf, &original).unwrap();

        let mut cursor = Cursor::new(buf);
        let read_back = read_framed(&mut cursor).unwrap().unwrap();
        assert_eq!(read_back, original);
    }

    #[test]
    fn read_framed_handles_multibyte_utf8_bodies_by_byte_length_not_char_count() {
        // Content-Length must be the UTF-8 *byte* length; a naive
        // char-count would under-read this body and truncate it.
        let mut buf = Vec::new();
        let original = serde_json::json!({"text": "日本語のホバー"});
        write_framed(&mut buf, &original).unwrap();

        let mut cursor = Cursor::new(buf);
        let read_back = read_framed(&mut cursor).unwrap().unwrap();
        assert_eq!(read_back, original);
    }

    #[test]
    fn read_framed_parses_two_consecutive_messages_from_one_stream() {
        let mut buf = Vec::new();
        write_framed(&mut buf, &serde_json::json!({"n": 1})).unwrap();
        write_framed(&mut buf, &serde_json::json!({"n": 2})).unwrap();

        let mut cursor = Cursor::new(buf);
        let first = read_framed(&mut cursor).unwrap().unwrap();
        let second = read_framed(&mut cursor).unwrap().unwrap();
        assert_eq!(first, serde_json::json!({"n": 1}));
        assert_eq!(second, serde_json::json!({"n": 2}));
    }

    #[test]
    fn read_framed_ignores_unrelated_headers_before_the_blank_line() {
        let body = serde_json::json!({"ok": true});
        let body_bytes = serde_json::to_vec(&body).unwrap();
        let mut buf = Vec::new();
        write!(
            buf,
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
            body_bytes.len()
        )
        .unwrap();
        buf.extend_from_slice(&body_bytes);

        let mut cursor = Cursor::new(buf);
        assert_eq!(read_framed(&mut cursor).unwrap().unwrap(), body);
    }

    #[test]
    fn read_framed_returns_none_on_clean_eof_between_messages() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        assert_eq!(read_framed(&mut cursor).unwrap(), None);
    }

    #[test]
    fn read_framed_errors_on_eof_in_the_middle_of_a_header_block() {
        let mut cursor = Cursor::new(b"Content-Length: 10\r\n".to_vec());
        assert!(read_framed(&mut cursor).is_err());
    }

    #[test]
    fn read_framed_errors_on_missing_content_length() {
        let mut cursor = Cursor::new(b"Content-Type: text\r\n\r\n".to_vec());
        assert!(read_framed(&mut cursor).is_err());
    }

    #[test]
    fn stderr_capture_is_utf8_safe_and_forwards_complete_lines() {
        let log = Arc::new(Mutex::new(String::new()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let handle = spawn_stderr_thread(
            Cursor::new(vec![b'a', 0xff, b'\n', b'\xe6', b'\x97', b'\xa5', b'\n']),
            Arc::clone(&log),
            Some(Arc::new(move |line| {
                sink_seen.lock().unwrap().push(line);
            })),
        );
        handle.join().unwrap();
        let captured = log.lock().unwrap().clone();
        assert!(captured.contains("a�"));
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn stderr_tail_truncation_keeps_utf8_boundaries() {
        let mut log = String::new();
        log.push_str(&"界".repeat(STDERR_LOG_CAP));
        trim_stderr_tail(&mut log);
        assert!(log.len() <= STDERR_LOG_CAP);
        assert!(std::str::from_utf8(log.as_bytes()).is_ok());
    }

    #[test]
    fn ascii_stderr_line_that_reaches_the_cap_still_gets_a_marker() {
        let log = Arc::new(Mutex::new(String::new()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let handle = spawn_stderr_thread(
            Cursor::new(format!("{}\n", "x".repeat(STDERR_LOG_CAP + 1)).into_bytes()),
            Arc::clone(&log),
            Some(Arc::new(move |line| {
                sink_seen.lock().unwrap().push(line);
            })),
        );
        handle.join().unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].ends_with("… [truncated]"));
        assert!(seen[0].len() <= STDERR_LOG_CAP);
    }

    #[test]
    fn enormous_stderr_line_is_drained_and_capped_with_a_marker() {
        let log = Arc::new(Mutex::new(String::new()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let input = format!("{}\nnext\n", "界".repeat(STDERR_LOG_CAP));
        let handle = spawn_stderr_thread(
            Cursor::new(input.into_bytes()),
            Arc::clone(&log),
            Some(Arc::new(move |line| {
                sink_seen.lock().unwrap().push(line);
            })),
        );
        handle.join().unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].ends_with("… [truncated]"));
        assert!(seen[0].len() <= STDERR_LOG_CAP);
        assert_eq!(seen[1], "next");
        let captured = log.lock().unwrap().clone();
        assert!(std::str::from_utf8(captured.as_bytes()).is_ok());
    }
}
