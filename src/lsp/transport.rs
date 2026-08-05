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
    child: Mutex<Child>,
    events_rx: Mutex<Option<Receiver<LspEvent>>>,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    stderr_handle: Mutex<Option<JoinHandle<()>>>,
    /// The last several KiB of the server's stderr, for surfacing in an
    /// `Unavailable`/`Crashed` reason when the process dies before or
    /// during initialization. Bounded by [`STDERR_LOG_CAP`] so a chatty
    /// server can't grow this without limit over a long session.
    stderr_log: Arc<Mutex<String>>,
}

const STDERR_LOG_CAP: usize = 16 * 1024;

impl Transport {
    /// Spawns `command` with piped stdio (overriding whatever the caller may
    /// have configured — a transport always owns its child's pipes) and
    /// starts the reader/stderr threads. Returns once the process is
    /// spawned; the LSP `initialize` handshake itself is
    /// [`crate::lsp::client::Client`]'s job, not the transport's.
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
        let reader_handle = spawn_reader_thread(Arc::clone(&shared), stdout, events_tx);

        let stderr_log = Arc::new(Mutex::new(String::new()));
        let stderr_handle = spawn_stderr_thread(stderr, Arc::clone(&stderr_log));

        Ok(Self {
            shared,
            child: Mutex::new(child),
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
    /// unexpectedly. Not yet surfaced anywhere in the UI — M3b's
    /// `Unavailable`/`Crashed` status messages are the natural place for
    /// it, once there's a status bar indicator to put them in.
    #[allow(dead_code)]
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
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let mut log = log.lock().unwrap_or_else(|e| e.into_inner());
                    log.push_str(&line);
                    if log.len() > STDERR_LOG_CAP {
                        let excess = log.len() - STDERR_LOG_CAP;
                        log.drain(..excess);
                    }
                }
            }
        }
    })
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
}
