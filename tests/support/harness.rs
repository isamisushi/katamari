//! Drives the real compiled `ktmr` binary through a PTY, the same way a
//! human terminal would, and parses what it prints via `vt100::Parser` — no
//! real terminal is ever involved, so the whole suite runs under plain
//! `cargo test`. See [`Harness::spawn`]'s docs for the one ordering
//! guarantee every test in the suite leans on without having to think about
//! it: keys are never sent before the kitty-protocol probe exchange has
//! definitely finished.

use super::key::Key;
use super::mouse::MouseKey;
use base64::Engine;
use pty_process::Size;
use pty_process::blocking::{Command, Pty, open};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use vt100::Parser;

/// Captures OSC 52 clipboard payloads off the real byte stream. `vt100`
/// parses and dispatches `\x1b]52;...\x1b\\` per its own `perform.rs`, but
/// `Parser::new`'s default `CB = ()` (`impl Callbacks for ()` — every
/// method a no-op) just discards it; this is the minimal opt-in swap to
/// `Parser::new_with_callbacks` needed to keep the raw base64 instead. Only
/// `copy_to_clipboard` is overridden — every other trait method stays the
/// default no-op, since nothing else in this suite needs bell/title/
/// resize/paste-request events.
///
/// Holds an `Arc<Mutex<..>>` rather than being the storage itself: one
/// clone lives inside the `vt100::Parser` (moved in by
/// `new_with_callbacks`, called only from the reader thread's `process`),
/// the other stays on [`Harness`] for [`Harness::last_osc52_clipboard`] to
/// read from a test thread — same split as `screen`/`bytes_read` already
/// use for the same reason.
#[derive(Clone, Default)]
struct ClipboardCapture(Arc<Mutex<Option<Vec<u8>>>>);

impl vt100::Callbacks for ClipboardCapture {
    fn copy_to_clipboard(&mut self, _screen: &mut vt100::Screen, _ty: &[u8], data: &[u8]) {
        *self.0.lock().expect("clipboard mutex poisoned") = Some(data.to_vec());
    }
}

/// Whether the fake terminal answers crossterm's kitty-keyboard-protocol
/// probe (`\x1b[?u\x1b[c`) as though it supports the protocol. See
/// [`spawn_reader_thread`] for where the reply is actually sent, and
/// [`Key::encode`](super::key::Key::encode) for how a [`Key`] is encoded
/// differently depending on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyMode {
    Supported,
    Unsupported,
}

impl KittyMode {
    /// The bytes a terminal answering `\x1b[?u\x1b[c` (kitty flags query +
    /// DA1) sends back. Per the M10 task's verified research: a `Supported`
    /// terminal answers both halves; an `Unsupported` one answers only DA1
    /// — which is what makes `supports_keyboard_enhancement` return
    /// `Ok(false)` rather than block for its full 2s timeout.
    fn probe_reply(self) -> &'static [u8] {
        match self {
            KittyMode::Supported => b"\x1b[?1u\x1b[?1c",
            KittyMode::Unsupported => b"\x1b[?1c",
        }
    }
}

/// The exact bytes crossterm 0.29's `supports_keyboard_enhancement` writes
/// to probe for kitty keyboard protocol support. Watched for in the raw
/// byte stream (not the parsed screen — it's a query the child sends, never
/// something it displays) so the reader thread knows the instant to answer.
const PROBE_QUERY: &[u8] = b"\x1b[?u\x1b[c";

/// `ui::mod::STARTUP_SPLASH_TEXT`, hand-duplicated here the same way
/// [`PROBE_QUERY`] above hand-duplicates crossterm's own probe bytes —
/// there's no `[lib]` target in `Cargo.toml` for this integration test to
/// import the real constant from, so the two copies are kept in sync by
/// convention and a comment on each side pointing at the other, not by the
/// compiler.
///
/// `ktmr` now draws a splash frame containing this text immediately after
/// entering the alternate screen and *before* the kitty-keyboard-protocol
/// probe (whose synchronous stdin read can eat any key a test sends too
/// early) and before `spawn_input_thread` even starts. That splash is the
/// first non-empty frame, so [`Harness::spawn`]'s readiness wait must keep
/// waiting past a frame that still contains this marker rather than
/// treating it as "ready for keys" — see its docs.
pub(crate) const SPLASH_MARKER: &str = "katamari — starting…";

/// [`Harness::spawn`]'s configuration. Defaults to a generously-sized
/// terminal and no extra args — plain `ktmr` in a git repo defaults to `ktmr
/// diff`, which is all this suite ever needs to spawn.
pub struct SpawnOptions {
    pub cols: u16,
    pub rows: u16,
    pub kitty_mode: KittyMode,
    pub args: Vec<&'static str>,
    /// Pre-seeds `$XDG_STATE_HOME/katamari/update-check.json` with this
    /// exact JSON text before the child ever starts, overriding
    /// [`spawn`](Harness::spawn)'s own default seed — for a test that needs
    /// a specific fabricated cache (e.g. a newer `latest_version`, to make
    /// the status-bar notice appear). `None` (the default) leaves `spawn`'s
    /// own seeding in place: a cache stamped "just checked, nothing newer,"
    /// so `update::on_startup`'s staleness check never spawns a real
    /// background request against the actual GitHub API — see
    /// [`spawn`](Harness::spawn)'s docs on why that matters for every test
    /// in this suite, not just the ones about the update check itself.
    pub update_state_json: Option<String>,
    /// Extra environment variables for the spawned `ktmr` — most usefully
    /// a rewritten `PATH` that shadows a real CLI with a fake one (a fake
    /// `gh`, say). A per-child env is the only safe way to do that here:
    /// tests share one process, so mutating the test runner's own
    /// environment would race every parallel test in the suite.
    pub extra_env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    /// Holds [`spawn_reader_thread`]'s reply to the kitty-keyboard-protocol
    /// probe back by this long after spotting it on the wire. `None` (the
    /// default) replies the instant the probe bytes are seen, matching
    /// every real terminal this harness otherwise simulates. Exists only so
    /// a test can observe the real, if brief, window where `ktmr` is
    /// blocked inside `supports_keyboard_enhancement`'s synchronous read —
    /// the exact window `ui::mod::draw_startup_splash` exists to paint over
    /// — without waiting out that function's real ~2s timeout to prove it:
    /// see `kitty::splash_is_visible_while_the_kitty_probe_is_still_pending`.
    pub probe_reply_delay: Option<Duration>,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            cols: 100,
            rows: 30,
            kitty_mode: KittyMode::Supported,
            args: Vec::new(),
            update_state_json: None,
            extra_env: Vec::new(),
            probe_reply_delay: None,
        }
    }
}

/// How long [`Harness::spawn`] waits for the first rendered frame before
/// giving up — generous, since a debug build's startup (run `git diff`,
/// parse it, load config; never a language server for this suite's
/// plain-text fixtures) is the only thing racing this.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Default timeout for [`Harness::wait_for_text`] and any [`Harness::wait_until`]
/// call that doesn't need a tighter one.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(3);

/// Drives one `ktmr` session through a PTY. Owns the child process, the PTY
/// master, a background thread that continuously drains it into a
/// [`vt100::Parser`], and a per-test `$HOME` (plus `$XDG_CONFIG_HOME`/
/// `$XDG_DATA_HOME`/`$XDG_STATE_HOME`) tempdir the child's environment
/// points at, so the real `~/.config/katamari`, the katamari-managed LSP
/// install prefix, and the update-check state file are never touched by a
/// test run.
pub struct Harness {
    pty: Arc<Pty>,
    write_lock: Arc<Mutex<()>>,
    screen: Arc<Mutex<Parser<ClipboardCapture>>>,
    bytes_read: Arc<AtomicUsize>,
    clipboard: Arc<Mutex<Option<Vec<u8>>>>,
    kitty_mode: KittyMode,
    child: Child,
    reader: Option<JoinHandle<()>>,
    // Held only for its `Drop`: `ktmr` reads `$HOME`/`$XDG_*` at startup, so
    // this tempdir must outlive the child process, not just the `spawn`
    // call that set the env vars pointing at it.
    _env_home: tempfile::TempDir,
}

impl Harness {
    /// Spawns `ktmr` (via `env!("CARGO_BIN_EXE_ktmr")`) in `cwd` inside a
    /// PTY sized `opts.cols`x`opts.rows`, answers the kitty-keyboard-protocol
    /// probe as `opts.kitty_mode` dictates, and blocks until the first
    /// rendered frame that is *not* the startup splash has real content on
    /// screen before returning.
    ///
    /// That wait is load-bearing, not a convenience. `ktmr` answers the
    /// probe via a synchronous blocking read that runs *before* it spawns
    /// its own input-reading thread (see `ui::mod::run`'s ordering) — any
    /// key bytes a test sent before that exchange finished could be
    /// consumed and silently dropped by it, an easy way for a test to look
    /// flaky for a reason that has nothing to do with the feature it's
    /// testing. Blocking here until content appears means every [`Key`] a
    /// test sends via [`Harness::send`] is sent only once that window has
    /// definitely closed — test bodies never have to think about the race
    /// themselves.
    ///
    /// The splash frame (see [`SPLASH_MARKER`]) is drawn *before* that probe
    /// even runs, so "any non-empty frame" alone is no longer a safe
    /// definition of ready — it would return the moment the splash lands,
    /// which is still inside the exact window the paragraph above warns
    /// about. Skipping any frame containing the marker closes that gap: the
    /// wait only succeeds once the splash has been replaced by the real UI,
    /// which happens after both the probe and `spawn_input_thread` have
    /// already run.
    ///
    /// Also always seeds `$XDG_STATE_HOME/katamari/update-check.json` before
    /// the child starts — with `opts.update_state_json` if a test supplied
    /// one, otherwise with a cache stamped "checked right now, nothing
    /// newer than this build." Without that default seed, `update::on_startup`
    /// would see no cache at all in a fresh per-test `$HOME` and treat that
    /// exactly like a stale one: spawning a real background thread that
    /// hits the actual GitHub API from every single test in this suite,
    /// however unrelated to the update check, which is both a real network
    /// dependency this suite must never have (see `support::fixture`'s
    /// module docs) and — even fire-and-forget — enough concurrent
    /// DNS/TLS work under `cargo test`'s default parallelism to make
    /// timing-sensitive assertions elsewhere in the suite flaky.
    pub fn spawn(cwd: &Path, opts: SpawnOptions) -> Harness {
        let harness = Self::spawn_without_ready_wait(cwd, opts);
        harness.wait_until(READY_TIMEOUT, |screen| {
            let contents = screen.contents();
            !contents.trim().is_empty() && !contents.contains(SPLASH_MARKER)
        });
        harness
    }

    /// Everything [`spawn`](Self::spawn) does *except* that final ready
    /// wait — spawns the child, wires up the pty/reader thread, and returns
    /// immediately. `spawn`'s wait is there for a reason (see its docs) and
    /// every ordinary test must keep going through it; this exists only for
    /// `kitty::splash_is_visible_while_the_kitty_probe_is_still_pending`,
    /// which needs a live [`Harness`] to poll *during* the window `spawn`'s
    /// wait deliberately skips past (the splash, before the kitty probe has
    /// replied). `pub(crate)` rather than `pub`, and not part of
    /// `support::mod`'s `pub use harness::{...}` re-export list — visible to
    /// the one test file that needs it without being something an ordinary
    /// test could reach for by accident and skip the real readiness
    /// guarantee.
    pub(crate) fn spawn_without_ready_wait(cwd: &Path, opts: SpawnOptions) -> Harness {
        let env_home = tempfile::Builder::new()
            .prefix("katamari-e2e-home-")
            .tempdir()
            .expect("failed to create per-test $HOME tempdir");

        let state_home = env_home.path().join("state");
        let update_state_json = opts.update_state_json.clone().unwrap_or_else(|| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before the epoch")
                .as_secs();
            format!(
                r#"{{"last_checked":{now},"latest_version":"{}"}}"#,
                env!("CARGO_PKG_VERSION")
            )
        });
        let state_dir = state_home.join("katamari");
        std::fs::create_dir_all(&state_dir)
            .expect("failed to create fixture $XDG_STATE_HOME/katamari dir");
        std::fs::write(state_dir.join("update-check.json"), update_state_json)
            .expect("failed to write fixture update-check.json");

        let (pty, pts) = open().expect("failed to allocate a pty");
        pty.resize(Size::new(opts.rows, opts.cols))
            .expect("failed to size the pty");

        let bin = env!("CARGO_BIN_EXE_ktmr");
        // pty_process's builder consumes and returns `self`, so the
        // variable is rebound per env var rather than mutated in place.
        let mut command = Command::new(bin)
            .args(&opts.args)
            .current_dir(cwd)
            .env("TERM", "xterm-256color")
            .env("HOME", env_home.path())
            .env("XDG_CONFIG_HOME", env_home.path().join("config"))
            .env("XDG_DATA_HOME", env_home.path().join("data"))
            .env("XDG_STATE_HOME", state_home);
        for (key, value) in &opts.extra_env {
            command = command.env(key, value);
        }
        let child = command.spawn(pts).expect("failed to spawn ktmr in a pty");

        let pty = Arc::new(pty);
        let write_lock = Arc::new(Mutex::new(()));
        let clipboard: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let screen = Arc::new(Mutex::new(Parser::new_with_callbacks(
            opts.rows,
            opts.cols,
            0,
            ClipboardCapture(Arc::clone(&clipboard)),
        )));
        let bytes_read = Arc::new(AtomicUsize::new(0));

        let reader = spawn_reader_thread(
            Arc::clone(&pty),
            Arc::clone(&write_lock),
            Arc::clone(&screen),
            Arc::clone(&bytes_read),
            opts.kitty_mode,
            opts.probe_reply_delay,
        );

        Harness {
            pty,
            write_lock,
            screen,
            bytes_read,
            clipboard,
            kitty_mode: opts.kitty_mode,
            child,
            reader: Some(reader),
            _env_home: env_home,
        }
    }

    /// Sends one logical keypress, encoded for this harness's
    /// [`KittyMode`]. Always safe to call any time after `spawn` returns —
    /// see [`Harness::spawn`]'s docs on the ordering this relies on.
    pub fn send(&self, key: Key) {
        write_locked(&self.pty, &self.write_lock, &key.encode(self.kitty_mode));
    }

    /// Sends one SGR mouse-wheel event. Unlike [`Self::send`], this never
    /// depends on [`KittyMode`] — see [`MouseKey::encode`]'s docs on why
    /// SGR mouse reporting and the kitty keyboard protocol are independent
    /// wire formats.
    pub fn send_mouse(&self, key: MouseKey) {
        write_locked(&self.pty, &self.write_lock, &key.encode());
    }

    /// Polls the parsed screen every 10ms until `predicate` is true, or
    /// panics once `timeout` elapses. The panic dumps the full screen
    /// contents (via [`vt100::Screen::contents`]) and how many raw bytes
    /// have been read from the pty so far — diagnosability first, per the
    /// M10 task: a bare "timed out" tells a future debugger nothing.
    pub fn wait_until(&self, timeout: Duration, mut predicate: impl FnMut(&vt100::Screen) -> bool) {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let parser = self.screen.lock().expect("screen mutex poisoned");
                if predicate(parser.screen()) {
                    return;
                }
            }
            if Instant::now() >= deadline {
                let dump = self.screen_contents();
                panic!(
                    "wait_until timed out after {timeout:?}\n\
                     bytes read from pty so far: {}\n\
                     --- screen contents ---\n{dump}\n--- end screen contents ---",
                    self.bytes_read.load(Ordering::SeqCst),
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Convenience for the common case: wait until `text` appears anywhere
    /// on screen, with [`DEFAULT_WAIT`].
    pub fn wait_for_text(&self, text: &str) {
        self.wait_until(DEFAULT_WAIT, |screen| screen.contents().contains(text));
    }

    /// A snapshot of the current screen contents, for ad hoc assertions
    /// that don't fit `wait_for_text`'s "just wait for a substring" shape.
    pub fn screen_contents(&self) -> String {
        self.screen
            .lock()
            .expect("screen mutex poisoned")
            .screen()
            .contents()
    }

    /// The most recent OSC 52 clipboard payload the child has written,
    /// base64-decoded back to UTF-8 — `None` until a real
    /// `\x1b]52;c;<base64>\x1b\\` sequence has actually been parsed off the
    /// wire (see [`ClipboardCapture`]). Everywhere else in the suite the
    /// rendered status-bar note is witness enough that `ktmr` believes it
    /// copied something (see `yank.rs`'s module docs on why this harness
    /// otherwise never inspects raw bytes); this exists only for the one
    /// test that needs to prove the actual escape sequence and its base64
    /// payload survive a real write to a real terminal fd.
    pub fn last_osc52_clipboard(&self) -> Option<String> {
        let raw = self
            .clipboard
            .lock()
            .expect("clipboard mutex poisoned")
            .clone()?;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(raw)
            .expect("ktmr's OSC 52 payload must be valid base64");
        Some(String::from_utf8(decoded).expect("ktmr's OSC 52 payload must be valid UTF-8"))
    }

    /// Runs `f` against the current parsed screen — for assertions that
    /// need [`vt100::Screen`]'s cell-level API (`cell`, `is_wide`,
    /// `underline`, ...), which a plain string snapshot can't answer. See
    /// `support::screen` for ready-made readers built on this.
    pub fn with_screen<T>(&self, f: impl FnOnce(&vt100::Screen) -> T) -> T {
        let parser = self.screen.lock().expect("screen mutex poisoned");
        f(parser.screen())
    }

    /// Waits for the child to exit (typically after sending `q`) and
    /// returns its [`std::process::ExitStatus`]. Panics with the current
    /// screen contents if it hasn't exited within `timeout` — a session
    /// that doesn't quit on `q` is exactly the kind of hang this harness
    /// exists to catch, not something to wait out silently.
    pub fn wait_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait failed") {
                return status;
            }
            if Instant::now() >= deadline {
                let dump = self.screen_contents();
                panic!(
                    "ktmr did not exit within {timeout:?}\n--- screen contents ---\n{dump}\n--- end screen contents ---"
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// The one place any byte ever gets written to the pty master — both the
/// reader thread's reactive probe reply and every [`Harness::send`] call
/// funnel through this, serialized by `lock`, so two writers can never
/// interleave their bytes on the wire.
fn write_locked(pty: &Pty, lock: &Mutex<()>, bytes: &[u8]) {
    let _guard = lock.lock().expect("pty write lock poisoned");
    let mut writer: &Pty = pty;
    writer
        .write_all(bytes)
        .expect("failed to write to pty master");
    writer.flush().expect("failed to flush pty master");
}

/// Drains the pty master into `screen` for the harness's whole lifetime,
/// watching the raw byte stream for the kitty-keyboard-protocol probe and
/// answering it inline the moment it appears (see [`PROBE_QUERY`] and
/// [`KittyMode::probe_reply`]) — reactively, never preemptively, so a
/// terminal that never asked never gets an unsolicited reply sitting in its
/// input buffer ahead of whatever a test sends next. `probe_reply_delay`
/// (see [`SpawnOptions::probe_reply_delay`]) sleeps this thread, right here,
/// for that long after spotting the probe and before writing the reply —
/// safe to do inline rather than off on some other thread, since `ktmr`'s
/// own probe read is synchronous: it sends nothing else worth draining
/// until this reply arrives.
fn spawn_reader_thread(
    pty: Arc<Pty>,
    write_lock: Arc<Mutex<()>>,
    screen: Arc<Mutex<Parser<ClipboardCapture>>>,
    bytes_read: Arc<AtomicUsize>,
    kitty_mode: KittyMode,
    probe_reply_delay: Option<Duration>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        // Bytes seen so far that might be the start of `PROBE_QUERY` split
        // across two reads — bounded to the query's own length, since a
        // genuine split can never leave more than that unmatched.
        let mut tail: Vec<u8> = Vec::with_capacity(PROBE_QUERY.len());
        let mut probe_answered = false;

        loop {
            let mut reader: &Pty = &pty;
            let n = match reader.read(&mut buf) {
                Ok(0) | Err(_) => break, // child's pts side closed: session over
                Ok(n) => n,
            };
            let chunk = &buf[..n];

            screen.lock().expect("screen mutex poisoned").process(chunk);
            bytes_read.fetch_add(n, Ordering::SeqCst);

            if !probe_answered {
                tail.extend_from_slice(chunk);
                if find_subslice(&tail, PROBE_QUERY).is_some() {
                    if let Some(delay) = probe_reply_delay {
                        std::thread::sleep(delay);
                    }
                    write_locked(&pty, &write_lock, kitty_mode.probe_reply());
                    probe_answered = true;
                    tail.clear();
                } else if tail.len() > PROBE_QUERY.len() {
                    let drop = tail.len() - PROBE_QUERY.len();
                    tail.drain(0..drop);
                }
            }
        }
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

impl Drop for Harness {
    /// Guarantees no zombie `ktmr` process and no stuck reader thread
    /// survives a test, whether it finished cleanly or panicked partway
    /// through: kill (a no-op if it already exited via `q`), reap, then join
    /// the reader thread — which unblocks on its own the moment the child's
    /// pts side closes (see the loop in [`spawn_reader_thread`]), so this
    /// never hangs waiting for it.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
