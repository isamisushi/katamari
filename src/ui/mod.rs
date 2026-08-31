//! Owns the terminal session: entering/leaving the alternate screen, the
//! panic hook that guarantees the terminal is restored even on a crash, and
//! the draw/input loop that turns key presses into [`Action`]s via the
//! keymap trie. `main.rs` only ever calls [`run`] — everything about how a
//! screen is laid out into panes, and which screen is currently active,
//! lives here, not in the entrypoint.
//!
//! Since M3a, this is also the one place that bridges two worlds: terminal
//! input, which arrives synchronously from crossterm, and LSP activity
//! (hover/definition/references responses, `$/progress` and
//! `publishDiagnostics` notifications), which arrives asynchronously from
//! background threads owned by [`crate::lsp::LspManager`]. Both are funneled
//! onto one channel (see [`AppEvent`]) so the render loop has a single place
//! to wait, with a short timeout, for "anything worth redrawing for."

pub mod app;
pub mod clipboard;
pub mod compose;
pub mod context_menu;
pub mod diff_view;
pub mod file_tree;
pub mod file_view;
pub mod help;
pub mod hints;
pub mod hover_popup;
pub mod key_display;
pub mod log_view;
pub mod lsp_inspector;
pub mod mouse;
pub mod navigation;
mod pane;
mod pointer_hover;
pub(crate) mod probe_cache;
pub mod refresh;
pub mod refs_panel;
pub mod scope_menu;
pub mod scroll;
pub mod search;
pub mod sidebar;
pub mod status_bar;
pub mod symbols;
pub mod text;
pub(crate) mod text_input;
pub mod timeline_view;
pub mod units_panel;
pub mod units_setup;
pub mod view;

pub use app::App;
pub use file_view::FileView;
pub use refresh::{NoopPreRefreshHook, PreRefreshHook};
pub use view::{View, ViewStack};

use crate::comments::{self, Comment, CommentIndex, CommentStore};
use crate::config::{self, Config};
use crate::diff::{RenderRow, parse_unified_diff};
use crate::highlight::LineHighlighter;
use crate::keymap::{Action, KeyChord, Keymap, Resolver, StepResult, emacs_preset, vim_preset};
use crate::lsp::adapter::LangKey;
use crate::lsp::client::uri_to_path;
use crate::lsp::manager::{ActionReadiness, ServerState, classify_action_readiness};
use crate::lsp::{
    DefinitionResult, DiagnosticsStore, HoverResult, LspError, LspManager, ReferencesResult,
    ServerEvent, parse_publish_diagnostics, progress_status_text,
};
use crate::lsp::{ObservationStore, ServerIdentity};
use crate::skill;
use crate::ui::compose::{ComposeKeymap, ComposeOutcome, ComposeState};
use crate::ui::context_menu::{ContextMenuState, MenuCommand, MenuTarget};
use crate::ui::help::{HelpOutcome, HelpState};
use crate::ui::log_view::LogView;
use crate::ui::navigation::{FilesConfirmOutcome, JumpEntry, JumpStack, navigate_to, record_jump};
use crate::ui::refs_panel::RefsPanel;
use crate::ui::scope_menu::{
    RevisionInputOutcome, ScopeMenuEntry, ScopeMenuState, handle_revision_key,
};
use crate::ui::timeline_view::TimelineView;
use crate::ui::units_panel::UnitsPanel;
use crate::ui::units_setup::{SetupOutcome, UnitsSetupState};
use crate::update;
use crate::vcs::DiffSource;
use crate::vcs::LogBackend;
use crate::vcs::git::GitSource;
use crate::vcs::jj::{self, JjRepo};
use crate::watch::{self, WatchSignal};
use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseButton, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use lsp_types::{FileChangeType, PositionEncodingKind};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout as RatatuiLayout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::collections::HashSet;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

const SIDEBAR_WIDTH: u16 = 30;

/// How long the render loop's channel wait blocks before looping anyway.
/// Short enough that a hover response or a `$/progress` tick shows up
/// without perceptible lag; long enough not to busy-loop between real
/// events.
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Everything that can wake the render loop: a terminal event (from the
/// dedicated input thread) or something an LSP server sent (relayed from
/// [`LspManager`]'s internal events channel by a small forwarding thread —
/// see [`run`]). One enum, one channel, one `recv_timeout` call is what
/// lets the loop below stay a single `match` instead of juggling multiple
/// receivers.
enum AppEvent {
    Terminal(Event),
    Lsp(ServerEvent),
    Watch(WatchSignal),
    /// [`JjPreRefreshHook`] created a new jj operation this refresh cycle —
    /// forwarded so an open [`View::Timeline`] can prepend it live instead
    /// of only picking it up the next time the timeline is reopened. Not
    /// sent when [`crate::vcs::jj::JjRepo::snapshot`] ran but found nothing
    /// to snapshot (a no-op refresh cycle isn't a new timeline entry).
    JjSnapshot,
    /// `.katamari/comments.jsonl` changed on disk — forwarded by
    /// [`crate::watch::spawn_comments_watcher`], which runs for every
    /// session with a root diff regardless of root live-refresh mode.
    /// Triggers a comments-only reload (re-read the log, rebuild the
    /// [`comments::CommentIndex`]) without re-running `git diff` — an
    /// agent resolving comments via `ktmr comments resolve` shouldn't need
    /// an unrelated file edit to make the reviewer's session notice.
    CommentsChanged,
    /// Issue #8: a watched git-dir path (`HEAD`, a ref, a reflog,
    /// `packed-refs`) changed on disk — forwarded by
    /// [`crate::watch::spawn_revision_watcher`], which like
    /// [`crate::watch::spawn_comments_watcher`] runs unconditionally for
    /// every session with a root diff, regardless of watch mode. Named
    /// distinctly from "Refs" to avoid colliding with `refs_panel`'s own
    /// naming — this has nothing to do with the find-references overlay.
    /// A cheap no-op via [`handle_moving_scope_refresh`] whenever the root
    /// diff isn't currently sitting on a classified-moving revision scope
    /// (see [`MovingScopeState`]'s docs) — most ticks, in practice, since a
    /// `git add`/ordinary commit touches several of these same paths too.
    RevisionChanged,
}

/// The M5 [`PreRefreshHook`]: snapshots the working copy via jj (see
/// [`JjRepo::snapshot`]) before every watch-triggered refresh, and forwards
/// [`AppEvent::JjSnapshot`] when that actually created a new operation, so
/// an open [`View::Timeline`] updates live rather than only on next open.
/// Lives here rather than in `vcs::jj` because it needs [`AppEvent`], which
/// is private to this module by design (see `AppEvent`'s docs) — `vcs::jj`
/// has no business knowing this crate has a terminal UI at all.
///
/// `repo_root` is deliberately ignored in [`Self::before_refresh`]: this
/// hook is only ever constructed (see [`run`]) for the same repo root its
/// held [`JjRepo`] was already detected against, and watch mode never
/// refreshes any repo but the one it started with.
struct JjPreRefreshHook {
    jj_repo: JjRepo,
    tx: Sender<AppEvent>,
}

impl PreRefreshHook for JjPreRefreshHook {
    fn before_refresh(&self, _repo_root: &std::path::Path) {
        match self.jj_repo.snapshot() {
            Ok(true) => {
                let _ = self.tx.send(AppEvent::JjSnapshot);
            }
            Ok(false) => {} // nothing changed since the last snapshot
            Err(_) => {
                // A transient jj failure (e.g. racing another jj process
                // touching the same working copy) isn't fatal to the
                // refresh pipeline: the diff re-run right after this still
                // reads the working tree via `git diff` directly, so the
                // reviewer sees the edit either way — it just won't gain
                // its own timeline entry this cycle.
            }
        }
    }
}

/// Detects a colocated jj repository for `view`'s repository root, if any —
/// `None` for anything but [`View::Diff`] (a `FileView`/`TimelineView`
/// session has no "root diff repo" the timeline could relate to) or when jj
/// itself isn't detected (see [`JjRepo::detect`]'s docs on what "detected"
/// requires). Used both to decide whether watch mode's [`PreRefreshHook`]
/// should be a real [`JjPreRefreshHook`] instead of [`NoopPreRefreshHook`],
/// and — regardless of watch mode — whether `Action::ToggleTimeline` has
/// anything to open at all.
fn detect_jj_repo(view: &View) -> Option<JjRepo> {
    let View::Diff(app) = view else {
        return None;
    };
    jj::resolve_jj_bin().and_then(|bin| JjRepo::detect(&app.repo_root, bin))
}

/// Runs the full-screen UI until the view stack empties (every view has been
/// quit or popped back past the root). Installs a panic hook and enters the
/// alternate screen on the way in, and restores the terminal on every exit
/// path, including panics.
///
/// `pre_refresh_hook` doubles as the watch-mode switch: `Some` both enables
/// watching the root diff's repository for changes and supplies the M5 seam
/// (see [`PreRefreshHook`]) each refresh calls before re-running the diff;
/// `None` is a plain, non-watching session and never spawns a watcher at
/// all. Working-tree diffs pass [`crate::ui::NoopPreRefreshHook`] by default;
/// `--no-watch` passes `None` explicitly. Staged and historical scopes also
/// pass `None`.
///
/// `config` selects the keymap preset (plus any `[keys]` rebinding — see
/// [`config::apply_key_overrides`]), the LSP server command overrides
/// [`crate::lsp::LspManager`] resolves against, and watch mode's debounce
/// duration; every caller loads it once via [`config::load_merged`] before
/// reaching here. A malformed `[keys]` entry surfaces as a plain startup
/// error naming the bad entry (see `apply_key_overrides`'s docs) rather than
/// a panic or a silently-ignored override — this is the one place a bad
/// config value can actually stop the session from starting, since
/// everything else `Config` carries has a safe built-in fallback.
///
/// `show_keys` is `config.show_keys` already OR'd with the session's
/// `--show-keys` flag (see `main.rs`'s per-command flag docs) — the CLI flag
/// only ever forces this *on* for one session, never off, so callers pass
/// the already-resolved bool rather than threading `Config` and the flag
/// through separately.
pub fn run(
    stack: &mut ViewStack,
    pre_refresh_hook: Option<Box<dyn PreRefreshHook>>,
    config: &Config,
    show_keys: bool,
) -> Result<()> {
    install_panic_hook();

    // Every TUI session (this function, called only from `run_diff`/
    // `run_open`/`run_timeline`/`run_log` — never from `--dump`, a hidden
    // plumbing subcommand, or `ktmr comments`) is where a background update
    // check belongs: cheap (a small file read, and a detached background
    // thread if the cache is stale — see `update::on_startup`'s docs), so
    // it runs before the terminal is even touched. `available_update` is
    // sourced entirely from whatever was already cached before this call;
    // it's read once here and reused for both the startup status note below
    // and the on-quit stderr line, so the two agree within one session even
    // if a background refresh completes in between.
    let available_update = update::on_startup(config.update_check);

    let mut terminal = init_terminal()?;

    // Looked up before the splash draw below (not just before the probe
    // itself) so the splash can tell a genuinely slow launch — this
    // terminal's fingerprint has no cached verdict, so
    // `enable_kitty_keyboard_protocol` is about to run the real probe —
    // apart from a cache-hit one that's about to skip it entirely. See
    // `probe_cache`'s module docs for the fingerprint and staleness
    // reasoning, and `render_startup_splash`'s docs for what it does with
    // `cached_kitty_support.is_none()` below.
    //
    // `probe_cache_usable` gates both the lookup here and the recording
    // inside `enable_kitty_keyboard_protocol` below — see `probe_cache`'s
    // module docs' **Multiplexers** section: inside tmux/screen,
    // `probe_fingerprint` is still computed (a doctor report can still show
    // it) but never trusted or written, since the multiplexer has already
    // overwritten the very env vars it's built from.
    let probe_cache_path = probe_cache::cache_file_path();
    let probe_fingerprint = probe_cache::fingerprint_from_env();
    let probe_cache_usable = !probe_cache::multiplexed_from_env();
    let cached_kitty_support = probe_cache_usable
        .then(|| probe_cache::look_up(&probe_cache_path, &probe_fingerprint))
        .flatten();

    // Between `init_terminal` entering the alternate screen and the event
    // loop's first real `terminal.draw` (below, past config/keymap/LSP
    // setup and `enable_kitty_keyboard_protocol`'s own possibly-2s probe on
    // a cache miss), nothing would otherwise touch stdout — a real user hit
    // exactly that: a terminal that doesn't answer the probe leaves them
    // staring at a black screen with just a cursor, indistinguishable from
    // "stuck." See `draw_startup_splash`'s docs.
    draw_startup_splash(&mut terminal, cached_kitty_support.is_none())?;

    // Must happen before `spawn_input_thread` below starts its own
    // blocking `event::read()` loop — see `enable_kitty_keyboard_protocol`'s
    // docs on why the two would otherwise contend for crossterm's internal
    // event-reader lock. Drawing the splash just above writes to stdout
    // only, so it doesn't need to respect that ordering itself — it's safe
    // on either side of this call, and goes first so the probe's up-to-2s
    // wait (on a cache miss) never runs against a still-black screen.
    let ci_distinguishable = enable_kitty_keyboard_protocol(
        cached_kitty_support,
        &probe_cache_path,
        &probe_fingerprint,
        probe_cache_usable,
    );

    // Issue #20: unlike the kitty probe above, this is a plain write with
    // no reply to race against `spawn_input_thread`'s reader — the ordering
    // here is only about keeping "every terminal-mode write happens in one
    // place, before the input thread starts" one rule, not working around a
    // second hard race. See `enable_mouse_capture`'s docs.
    enable_mouse_capture(config.mouse);

    let preset = match config.keymap {
        config::KeymapPreset::Vim => vim_preset(ci_distinguishable),
        config::KeymapPreset::Emacs => emacs_preset(ci_distinguishable),
    };
    let bindings = config::apply_key_overrides(preset, &config.key_overrides)
        .map_err(|e| anyhow::anyhow!("config: {e}"))?;
    let keymap = Keymap::from_bindings(&bindings);
    // Issue #27: the compose overlay's own save/cancel keys, resolved once
    // here alongside the main keymap — same fail-fast-at-startup treatment
    // as `apply_key_overrides` just above, since a bad `[keys.compose]`
    // entry is just as much a startup-blocking config error as a bad
    // `[keys]` one (see `ComposeKeymap::resolve`'s docs for why it's
    // actually stricter).
    let compose_keymap = ComposeKeymap::resolve(&config.compose_key_overrides)
        .map_err(|e| anyhow::anyhow!("config: {e}"))?;
    let mut resolver = keymap.resolver();
    let mut highlighter = LineHighlighter::new();

    let (app_tx, app_rx) = mpsc::channel::<AppEvent>();
    spawn_input_thread(app_tx.clone());

    let (lsp_tx, lsp_rx) = mpsc::channel::<ServerEvent>();
    spawn_lsp_forwarder(lsp_rx, app_tx.clone());
    // Runtime observability belongs to TUI sessions only. Headless doctor
    // and lsp-check keep using the old constructor and create no journal.
    let observer = ObservationStore::start(config.lsp_logging.clone());
    let lsp_manager = LspManager::new_with_observer(
        lsp_tx,
        Arc::new(config.lsp_servers.clone()),
        config.auto_install,
        Some(observer.clone()),
    );

    // Proactively `didOpen`s the files this session starts out looking at,
    // so diagnostics gutters have something to show without the user
    // hovering first — see `LspManager::warm_up`'s docs for why hovering
    // alone isn't enough to make that happen promptly.
    let startup_status = warm_up_root(stack.top(), &lsp_manager);

    // M5: detected once, regardless of watch mode — `Action::ToggleTimeline`
    // needs this even when the root watcher isn't active, since existing jj
    // history is browsable any time, not just while katamari is actively
    // creating new snapshots. See `detect_jj_repo`'s docs.
    let jj_repo = detect_jj_repo(stack.top());

    // "Wire in ui::run when jj detected + watch mode" (the milestone plan's
    // words): a live session's hook starts as whatever the caller passed
    // (`NoopPreRefreshHook`, per `main.rs`'s docs) and is upgraded to a real
    // `JjPreRefreshHook` here, the one place that already knows both "is this
    // watching at all" (`pre_refresh_hook.is_some()`) and "is this a jj
    // repo" (`jj_repo`) — callers don't need to know jj exists.
    let pre_refresh_hook: Option<Box<dyn PreRefreshHook>> = match (&pre_refresh_hook, &jj_repo) {
        (Some(_), Some(repo)) => Some(Box::new(JjPreRefreshHook {
            jj_repo: repo.clone(),
            tx: app_tx.clone(),
        })),
        _ => pre_refresh_hook,
    };

    let debounce_quiet = Duration::from_millis(config.debounce_ms);
    let watch_startup_status = pre_refresh_hook
        .is_some()
        .then(|| start_watch(stack, app_tx.clone(), debounce_quiet))
        .flatten();

    // Issue #8: unconditional, like the comments watcher just below — see
    // `start_revision_watch`'s own docs for why this can't gate on watch
    // mode either.
    let revision_watch_status = start_revision_watch(stack.top(), app_tx.clone());

    // M6: a root diff's comments are loaded and watched unconditionally —
    // unlike the working-tree watcher above, this has nothing to do with
    // whether root live refresh is enabled. See
    // [`AppEvent::CommentsChanged`]'s docs and `watch::spawn_comments_watcher`.
    let (comments_repo_root, initial_comments, comments_startup_status) =
        start_comments(stack.top(), app_tx);
    // Lowest priority of the four: an available-update notice is
    // informational, never a problem report the way a failed watcher or a
    // capped warm-up is, so it only shows when nothing more actionable
    // claimed this session's one startup status slot.
    let startup_status = startup_status
        .or(comments_startup_status)
        .or(revision_watch_status)
        .or(available_update.as_ref().map(update::status_bar_notice));

    let result = event_loop(
        &mut terminal,
        stack,
        &mut resolver,
        &keymap,
        &compose_keymap,
        &mut highlighter,
        &app_rx,
        &lsp_manager,
        startup_status,
        pre_refresh_hook,
        watch_startup_status,
        jj_repo,
        comments_repo_root,
        initial_comments,
        show_keys,
        config.offer_install,
        observer.clone(),
        config.mouse,
        config.mouse_hover,
    );

    restore_terminal(&mut terminal)?;

    // However the loop above exited, any language server it spawned needs
    // to be told to shut down before the process does — nothing else in
    // this program ever calls this, and without it a server spawned mid
    // session would be orphaned rather than terminated (see
    // `LspManager::shutdown_all`'s docs for why `Drop` alone doesn't cover
    // this). Runs after the terminal is already restored so this brief,
    // bounded wait doesn't leave the user staring at a frozen screen.
    lsp_manager.shutdown_all();
    observer.shutdown();

    // A normal quit only — a session that exited via an `Err` already has
    // something more important to report (and `main` prints that), and
    // isn't the "you're about to walk away, here's a heads-up" moment this
    // line is for. Silent unless stderr is a real terminal — see
    // `update::print_exit_notice`'s docs.
    if result.is_ok() {
        update::print_exit_notice(available_update.as_ref());
    }

    result
}

/// Starts watching the root diff's repository and wires its events onto
/// `app_tx`, marking the root [`App`] as being in watch mode along the way.
/// Returns a status-bar note if the watcher itself failed to start (e.g.
/// the platform's file-watching backend couldn't be initialized) — a live
/// session that silently isn't watching would be a much worse failure mode
/// than one that says so up front.
fn start_watch(stack: &mut ViewStack, app_tx: Sender<AppEvent>, quiet: Duration) -> Option<String> {
    let View::Diff(app) = stack.root_mut() else {
        // `main.rs` only ever offers live refresh alongside the plain diff
        // view (not `ktmr open`'s single-file view), so this doesn't happen
        // in practice — but reaching it silently, without a watcher, would
        // be a worse failure than declining loudly.
        return Some("watch: not available for this view".to_owned());
    };
    app.watch_mode = true;
    let (watch_tx, watch_rx) = mpsc::channel::<WatchSignal>();
    match watch::spawn(app.repo_root.clone(), watch_tx, quiet) {
        Ok(()) => {
            spawn_watch_forwarder(watch_rx, app_tx);
            None
        }
        Err(e) => Some(format!("watch: failed to start: {e}")),
    }
}

/// Loads the root diff's comment log (empty if there isn't one yet or the
/// view has no comments concept at all — see below) and starts the M6
/// comments watcher against it, unconditionally, for every session with a
/// [`View::Diff`] root — see [`AppEvent::CommentsChanged`]'s docs on why
/// this doesn't gate on root live refresh. Returns the repo root comments are
/// keyed against (`None` for a [`View::File`]/[`View::Timeline`] root, which have
/// no working-tree diff for a comment to anchor into), the comments loaded
/// so far, and a status-bar note if the watcher itself failed to start —
/// the same "don't silently not-watch" principle [`start_watch`] follows.
fn start_comments(
    view: &View,
    app_tx: Sender<AppEvent>,
) -> (Option<PathBuf>, Vec<Comment>, Option<String>) {
    let View::Diff(app) = view else {
        return (None, Vec::new(), None);
    };
    let repo_root = app.repo_root.clone();
    let loaded = CommentStore::new(&repo_root).load().unwrap_or_default();

    let (tx, rx) = mpsc::channel::<()>();
    let status = match watch::spawn_comments_watcher(repo_root.clone(), tx) {
        Ok(()) => {
            spawn_comments_forwarder(rx, app_tx);
            None
        }
        Err(e) => Some(format!("comments watch: failed to start: {e}")),
    };
    (Some(repo_root), loaded, status)
}

/// As [`spawn_watch_forwarder`], relaying the comments watcher's bare `()`
/// signals onto the shared [`AppEvent`] channel as
/// [`AppEvent::CommentsChanged`].
fn spawn_comments_forwarder(rx: Receiver<()>, tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        for () in rx {
            if tx.send(AppEvent::CommentsChanged).is_err() {
                break;
            }
        }
    });
}

/// Issue #8: starts the ref-watcher behind a moving historical scope's live
/// refresh, unconditionally, for every session with a [`View::Diff`] root —
/// exactly [`start_comments`]'s own "don't gate on root live-refresh mode"
/// reasoning, and for the same underlying reason: a plain `ktmr diff -r
/// HEAD` session has no other watcher running at all (`start_watch` only
/// ever spawns for a working-tree root — see `App::disk_is_new_side`'s
/// callers in `main.rs`), so this is the only thing that can ever notice
/// `HEAD` (or any other moving scope) pointing at a different commit after
/// an amend. Spawning it regardless of whether a moving scope happens to be
/// open *right now* is deliberate too — [`handle_moving_scope_refresh`]'s
/// own signals are cheap no-ops whenever [`MovingScopeState`] is `None`, and
/// the scope menu can swap onto a moving revision at any later point in the
/// session, not just at startup.
fn start_revision_watch(view: &View, app_tx: Sender<AppEvent>) -> Option<String> {
    let View::Diff(app) = view else {
        return None;
    };
    let git = GitSource::at(app.repo_root.clone());
    let (tx, rx) = mpsc::channel::<()>();
    match watch::spawn_revision_watcher(&git, tx) {
        Ok(()) => {
            spawn_revision_forwarder(rx, app_tx);
            None
        }
        Err(e) => Some(format!("scope watch: failed to start: {e}")),
    }
}

/// As [`spawn_comments_forwarder`], relaying the revision watcher's bare
/// `()` signals onto the shared [`AppEvent`] channel as
/// [`AppEvent::RevisionChanged`].
fn spawn_revision_forwarder(rx: Receiver<()>, tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        for () in rx {
            if tx.send(AppEvent::RevisionChanged).is_err() {
                break;
            }
        }
    });
}

/// Proactively opens whichever files `view` starts out showing —
/// [`View::Diff`]'s changed files (in diff order, excluding deletions and
/// binaries, which have nothing current to open), or [`View::File`]'s
/// single file. Returns a status-bar note when [`LspManager::warm_up`]'s
/// cap meant not everything got opened, so a reviewer of an unusually large
/// diff knows why some files' diagnostics gutters stay quiet until touched.
fn warm_up_root(view: &View, lsp_manager: &LspManager) -> Option<String> {
    match view {
        View::Diff(app) if app.interactive => {
            let summary = warm_up_diff(app, lsp_manager);
            summary.capped().then(|| {
                format!(
                    "LSP: opened {} of {} changed files for diagnostics (first {} by diff order)",
                    summary.opened, summary.total_eligible, summary.opened
                )
            })
        }
        // A remote PR or historical revision need not match local files.
        // Do not start/install language servers for unrelated disk content.
        View::Diff(_) => None,
        View::File(file) => {
            if !file.highlight_skipped
                && let Some(path) = file.file_path()
            {
                lsp_manager.warm_up(std::slice::from_ref(&path.to_path_buf()), file.git_root());
            }
            None
        }
        // Read-only and LSP-free by design (see `TimelineView::hover_query`'s
        // / `LogView::hover_query`'s docs) — a timeline or log session,
        // whether reached via `ktmr timeline`/`ktmr log` or by toggling from
        // the root diff, has nothing for `LspManager` to warm up.
        View::Timeline(_) | View::Log(_) | View::LspInspector(_) => None,
    }
}

/// Reads crossterm events on a dedicated thread and forwards them, since
/// `crossterm::event::read()` blocks and the render loop needs to wait on
/// LSP activity at the same time. Exits quietly once the receiving end goes
/// away (normal shutdown) or a read fails (terminal gone).
fn spawn_input_thread(tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if tx.send(AppEvent::Terminal(ev)).is_err() {
                break;
            }
        }
    });
}

/// Relays `LspManager`'s events onto the shared [`AppEvent`] channel,
/// wrapping each one. A thin, permanent thread rather than handing
/// `LspManager` the `Sender<AppEvent>` type directly, so `lsp` stays free of
/// any dependency on `ui`'s event-loop plumbing.
fn spawn_lsp_forwarder(rx: Receiver<ServerEvent>, tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        for event in rx {
            if tx.send(AppEvent::Lsp(event)).is_err() {
                break;
            }
        }
    });
}

/// Relays the watcher's [`WatchSignal`]s onto the shared [`AppEvent`]
/// channel — the same pattern as [`spawn_lsp_forwarder`], for the same
/// reason: [`crate::watch`] stays free of any dependency on `ui`'s
/// event-loop plumbing.
fn spawn_watch_forwarder(rx: Receiver<WatchSignal>, tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        for signal in rx {
            if tx.send(AppEvent::Watch(signal)).is_err() {
                break;
            }
        }
    });
}

/// What a `textDocument/definition`/`references` request is currently
/// waiting on — kept alongside the [`JumpEntry`] the cursor was at when the
/// request was issued, since that's what a single-result definition (or a
/// references-panel selection) jumps *from*, for the jump stack.
enum PendingGoto {
    Definition {
        operation_id: u64,
        rx: Receiver<Result<DefinitionResult, LspError>>,
    },
    References {
        operation_id: u64,
        rx: Receiver<Result<ReferencesResult, LspError>>,
    },
}

type PendingHover = (u64, u64, Receiver<Result<HoverResult, LspError>>);

/// An open references (or multi-result definitions) panel, plus the
/// workspace root its entries' targets should be opened relative to —
/// `RefsPanel` itself has no notion of "workspace," only of the entries a
/// caller already resolved, so that context is kept alongside it here
/// rather than added to `RefsPanel`'s own fields for a UI-overlay concern
/// that isn't part of what a references list *is*.
struct RefsPanelState {
    git_root: PathBuf,
    panel: RefsPanel,
    operation_id: u64,
}

/// A transient status-bar note about watch-mode activity ("updated",
/// "closed: file changed", a debounce-pending hint). Unlike `goto_status`
/// (cleared the moment the next key is pressed) or `lsp_status` (superseded
/// by the next progress tick), nothing else in the event loop naturally
/// clears a watch note on its own timeline — a refresh might be the last
/// thing that happens for a while if the reviewer just sits reading — so it
/// carries its own expiry rather than lingering indefinitely.
struct WatchStatus {
    text: String,
    set_at: Instant,
}

impl WatchStatus {
    fn new(text: String) -> Self {
        Self {
            text,
            set_at: Instant::now(),
        }
    }
}

/// Issue #8: live-tracking state for a moving historical scope (`ktmr diff
/// -r HEAD`, the scope menu's "Revision…" opened onto something like
/// `HEAD`/`@`/a branch name). `Some` only while the root diff's current
/// scope is both a revision scope (`App::revision_scope`) *and* classified
/// moving (`vcs::is_moving_revision` — see [`seed_moving_scope`]); `None` is
/// deliberately the entire pause mechanism, mirroring `watch_paused`'s
/// "consumer-side no-op" shape rather than a second boolean threaded
/// alongside it — see [`handle_moving_scope_refresh`]'s early return.
/// Independent of `watch_paused`/`watch_active`: a revision scope is never
/// the working tree, so watch mode is always paused whenever this is `Some`
/// (see `watch_pause_decision`) — the two states track different concerns
/// and neither handler ever consults the other's flag.
struct MovingScopeState {
    text: String,
    via_jj: bool,
    /// The resolved commit/change-content id the scope named as of the
    /// last check — compared against a fresh resolve on every
    /// `AppEvent::RevisionChanged` tick in [`handle_moving_scope_refresh`]:
    /// unequal means the amend actually moved this scope's target; equal
    /// means some other watched ref path changed (an unrelated branch's
    /// reflog, a `git add`) and this scope's own target didn't move.
    /// `None` when the seed-time resolve itself failed (a transient
    /// repository hiccup at scope-open): tracking stays alive rather than
    /// silently disabling for the whole session, and the first successful
    /// resolve re-runs the diff once — a redundant, anchor-preserving
    /// refresh in the worst case, never a missed amend.
    last_hash: Option<String>,
}

/// Builds the event loop's live [`MovingScopeState`] for a freshly opened
/// (or swapped-to) revision scope — used both at session setup (from
/// `App::revision_scope`, seeded once before the loop starts) and by
/// `apply_scope_swap` on every later in-session scope change. `None` for
/// every scope that isn't [`crate::vcs::is_moving_revision`] (an immutable
/// commit hash, a range, the working tree/staged, or no scope at all) — the
/// entire "moving scope refresh" mechanism is a no-op for any of those, by
/// construction, since a `None` here is also [`handle_moving_scope_refresh`]'s
/// own early-return condition.
///
/// Resolves the scope's *current* target right away (rather than leaving
/// `last_hash` empty until the first watcher tick) so the first
/// `AppEvent::RevisionChanged` this session actually receives is compared
/// against a real baseline, not treated as a spurious "changed" the moment
/// this session happens to check at all. A resolve failure at seed time
/// (a momentarily locked repo, say) still seeds — with `last_hash: None` —
/// because `None` from this function is the feature's *permanent* off
/// switch, and a transient hiccup at open must not silently disable
/// live-refresh for the whole session; the caller reports it, and the
/// first successful later resolve re-runs the diff once (see
/// `MovingScopeState::last_hash`'s docs). Only a genuinely non-moving or
/// unresolvable-by-construction scope (no jj repo for a jj revset)
/// returns `None`.
fn seed_moving_scope(
    scope: Option<&app::RevisionScope>,
    git: &GitSource,
    jj_repo: Option<&JjRepo>,
) -> Option<MovingScopeState> {
    let scope = scope?;
    if !crate::vcs::is_moving_revision(&scope.text) {
        return None;
    }
    let last_hash = if scope.via_jj {
        jj_repo?.resolve_commit_id(&scope.text).ok().flatten()
    } else {
        git.resolve(&scope.text).ok().flatten()
    };
    Some(MovingScopeState {
        text: scope.text.clone(),
        via_jj: scope.via_jj,
        last_hash,
    })
}

/// How long a [`WatchStatus`] note stays in the status bar before clearing
/// itself — long enough to read at a glance, short enough not to crowd out
/// the hover/goto/LSP notes that share the same status-bar slot.
const WATCH_STATUS_FLASH: Duration = Duration::from_secs(3);

#[allow(clippy::too_many_arguments)] // mirrors `handle_action`'s: this is
// `ui::mod`'s one render/dispatch loop, already threading through every
// long-lived piece of session state (hover, jump history, diagnostics,
// LSP, and now watch-mode) that a single frame's worth of work touches;
// splitting the signature into a struct would just move the same fields
// one level down without reducing how many things one iteration needs.
fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    stack: &mut ViewStack,
    resolver: &mut Resolver<'_>,
    keymap: &Keymap,
    compose_keymap: &ComposeKeymap,
    highlighter: &mut LineHighlighter,
    app_rx: &Receiver<AppEvent>,
    lsp_manager: &LspManager,
    startup_status: Option<String>,
    pre_refresh_hook: Option<Box<dyn PreRefreshHook>>,
    initial_watch_status: Option<String>,
    jj_repo: Option<JjRepo>,
    comments_repo_root: Option<PathBuf>,
    initial_comments: Vec<Comment>,
    show_keys: bool,
    offer_skill_install: bool,
    observer: crate::lsp::ObservationHandle,
    mouse_enabled: bool,
    mouse_hover_enabled: bool,
) -> Result<()> {
    let mut key_display = key_display::KeyDisplayState::new(show_keys);
    let mut hover_state = hover_popup::HoverState::default();
    let mut pending_hover: Option<PendingHover> = None;
    // Issue #24: the passive pointer-hover analogue of `hover_state`/
    // `pending_hover` just above — a wholly separate state machine and
    // generation counter (see `pointer_hover`'s module docs on why the two
    // must never share one), but the identical "state type owns pure
    // transitions, the event loop owns the in-flight `Receiver`" split.
    let mut pointer_hover = pointer_hover::PointerHoverState::default();
    let mut pending_pointer_hover: Option<PendingHover> = None;
    let mut pending_goto: Option<(JumpEntry, PendingGoto)> = None;
    let mut refs_panel: Option<RefsPanelState> = None;
    // The semantic-units overlay and its in-flight computation. Unlike
    // `pending_hover` there's no generation counter: at most one grouping
    // request runs at a time (`ToggleUnits` refuses to start a second),
    // and staleness is handled by re-checking the grouping's `diff_key`
    // against the live diff when the result lands, not by racing counters.
    let mut units_panel: Option<UnitsPanel> = None;
    let mut units_setup: Option<UnitsSetupState> = None;
    // `.` — whether the status bar shows every curated hint or just the
    // minimal always-on subset (the default). Session-local chrome, reset
    // each launch on purpose: a collapsed-by-default bar is the feature.
    let mut hints_expanded = false;
    let mut pending_units: Option<Receiver<Result<crate::groups::Grouping, String>>> = None;
    let mut units_status: Option<String> = None;
    let mut jump_stack = JumpStack::new();
    let mut diagnostics = DiagnosticsStore::new();
    let mut lsp_status: Option<String> = startup_status;
    let mut goto_status: Option<String> = None;
    let mut watch_status: Option<WatchStatus> = initial_watch_status.map(WatchStatus::new);
    // Keyed by `(LangKey, workspace_root)` — a full server *instance*, not
    // just a language — so two independently-rooted servers of the same
    // language (e.g. two unrelated Cargo workspaces reviewed in one
    // session) each get their own one-time warning instead of the second
    // one's crash going silently unreported because the first one already
    // consumed the language-wide slot. See the `Unavailable`/`Crashed` arms
    // below and `LspManager::server_identity`'s docs for why a `LangKey`-only
    // key would be wrong.
    let mut warned_servers: HashSet<(LangKey, PathBuf)> = HashSet::new();
    let mut compose: Option<ComposeState> = None;
    let mut scope_menu: Option<ScopeMenuState> = None;
    // The "GitHub PR…" entry's background `gh` fetch and its parked
    // result, if any — see [`PrScopeFetch`]'s docs for the lifecycle.
    // Polled in the frame preamble beside `pending_units`.
    let mut pr_scope = PrScopeFetch::default();
    // Issue #23's right-click context menu. `Some` only while it's actually
    // open — like `scope_menu`/`refs_panel`, a transient layer on top of
    // whichever view is on screen, not something the view stack itself
    // owns. Re-derived every iteration (see `refresh_context_menu`, called
    // just below the loop preamble) so a readiness reason can flip live
    // while the menu sits open, per req 5.
    let mut context_menu: Option<ContextMenuState> = None;
    let mut help: Option<HelpState> = None;
    // Issue #5's `/` search prompt — `Some` only while it's actually open;
    // the confirmed search it produces on Enter lives on `App::search`
    // instead (see `search::SearchPromptState`'s docs for why the two are
    // split this way).
    let mut search_prompt: Option<search::SearchPromptState> = None;
    // Memoizes `help::build_rows(keymap, filter).len()` against the filter
    // text it was computed for — most keys the help popup's raw-key bypass
    // sees while open (every scroll/page/top/bottom key in `Browse` mode)
    // can't possibly change the row count, only `Filter` mode's
    // insert/backspace/clear can, so rebuilding the whole row list from
    // scratch on every keystroke (as opposed to just the ones that can
    // change its length) would be pure waste. `keymap` never changes
    // within a session (built once above), so the cache only needs to
    // track `filter`, not `keymap` too.
    let mut help_row_count_cache: Option<(String, usize)> = None;
    // M16's first-comment skill-install prompt: `skill_prompt_offered` is a
    // one-way session latch — set the moment the prompt is shown (whether or
    // not the reviewer ever responds), never reset, so it never appears
    // twice in one session. `awaiting_skill_prompt_key` is true only for the
    // single frame between the prompt appearing and the *next* key press,
    // which is special-cased below (see the main key-dispatch `else` arm) to
    // consume `y` as "install now" or fall through to ordinary key handling
    // for anything else — see that arm's docs for why a plain dismiss
    // doesn't swallow the key.
    let mut skill_prompt_offered = false;
    let mut awaiting_skill_prompt_key = false;
    // Only meaningful while `pre_refresh_hook.is_some()` (watch mode is
    // actually running) — see `apply_scope_swap`'s docs and
    // `handle_watch_refresh`'s early-return on this flag.
    let mut watch_paused = false;
    let watch_active = pre_refresh_hook.is_some();
    // Issue #8: seeded once here from whatever revision scope the session
    // actually started on (resolving its current target right away, not
    // waiting for the first ref-watcher tick — see `seed_moving_scope`'s
    // docs), then re-seeded by `apply_scope_swap` on every later in-session
    // scope change. Independent of `watch_paused`/`watch_active` above —
    // see `MovingScopeState`'s docs.
    let mut moving_scope: Option<MovingScopeState> = match stack.root_mut() {
        View::Diff(app) => {
            let git = GitSource::at(app.repo_root.clone());
            seed_moving_scope(app.revision_scope.as_ref(), &git, jj_repo.as_ref())
        }
        _ => None,
    };
    // A seed whose baseline resolve failed keeps tracking alive but must
    // say so — issue #8's transient-failure criterion applies at open just
    // as much as on a later tick, and this is the one failure the later
    // tick's own reporting can't cover.
    if let Some(scope) = &moving_scope
        && scope.last_hash.is_none()
    {
        watch_status = Some(WatchStatus::new(format!(
            "scope: couldn't resolve {} yet; live refresh will retry",
            scope.text
        )));
    }
    let mut comment_list: Vec<Comment> = initial_comments;
    let mut comment_index: CommentIndex = comments_repo_root
        .as_deref()
        .map(|root| comments::build_index(root, &comment_list))
        .unwrap_or_default();

    loop {
        let size = terminal.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        // `content_width` feeds `App`/`FileView`'s wrap-aware scroll math
        // (see `View::set_content_width`'s docs) — always the *unified*
        // layout's content width for a `View::Diff`, regardless of which
        // layout is actually showing (see `App::content_width`'s docs on
        // why side-by-side is a deliberately separate concern from this).
        let (content_height, content_width, files_viewport_height) = match stack.top() {
            View::Diff(app) => {
                let status_height = hints::required_height(
                    &hints::diff_view_items(keymap, hints_expanded),
                    area.width,
                );
                let areas = diff_layout(area, app.sidebar_visible, status_height);
                // Mirrors `draw`'s banner split exactly — viewport height
                // fed to scroll math must equal the rows content actually
                // gets, or the cursor could sit "visible" one banner-height
                // below the pane's real bottom edge.
                let banner_height = if app.unit_filter().is_some() {
                    units_panel::BANNER_HEIGHT.min(areas.diff.height)
                } else {
                    0
                };
                let diff_pane_area = Rect {
                    height: areas.diff.height - banner_height,
                    ..areas.diff
                };
                // `pane::inner_rect` is the exact same border-geometry
                // function `render_focusable`'s real `PaneChrome` uses (see
                // `diff_view::unified_content_width`'s docs) — req 8:
                // content geometry derived from the new pane borders
                // exactly once here, then reused for both dimensions,
                // rather than hand-re-deriving height a second way.
                let diff_inner = pane::inner_rect(diff_pane_area.width, diff_pane_area);
                let files_viewport_height = areas.sidebar.map(|sidebar_area| {
                    pane::inner_rect(sidebar_area.width, sidebar_area).height as usize
                });
                (
                    diff_inner.height,
                    diff_view::unified_content_width(diff_pane_area.width),
                    files_viewport_height,
                )
            }
            View::File(_) => {
                let status_height = hints::required_height(
                    &hints::file_view_items(keymap, hints_expanded),
                    area.width,
                );
                let content_area = file_view::layout(area, status_height).content;
                (
                    content_area.height,
                    file_view::content_width_for_pane(content_area.width),
                    None,
                )
            }
            View::Timeline(_) => {
                let status_height = hints::required_height(
                    &hints::timeline_view_items(keymap, hints_expanded),
                    area.width,
                );
                (
                    timeline_view::layout(area, status_height).diff.height,
                    0,
                    None,
                )
            }
            View::Log(_) => {
                let status_height = hints::required_height(
                    &hints::log_view_items(keymap, hints_expanded),
                    area.width,
                );
                (log_view::layout(area, status_height).list.height, 0, None)
            }
            View::LspInspector(_) => (area.height.saturating_sub(2), area.width as usize, None),
        };
        stack.top_mut().set_viewport_height(content_height as usize);
        stack.top_mut().set_content_width(content_width);
        if let (Some(height), View::Diff(app)) = (files_viewport_height, stack.top_mut()) {
            app.set_files_viewport_height(height);
        }

        if !matches!(stack.top(), View::LspInspector(_))
            && let Some((generation, operation_id, rx)) = &pending_hover
            && let Ok(result) = rx.try_recv()
        {
            let stale = *generation != hover_state.generation();
            let outcome = if stale {
                crate::lsp::EventOutcome::Superseded
            } else {
                match &result {
                    Ok(Some(_)) => crate::lsp::EventOutcome::Result,
                    Ok(None) => crate::lsp::EventOutcome::NoResult,
                    Err(error) => request_error_outcome(error),
                }
            };
            hover_state.apply(*generation, result, highlighter);
            observer.record_ui(
                Some(*operation_id),
                outcome,
                match outcome {
                    crate::lsp::EventOutcome::Result => "hover displayed",
                    crate::lsp::EventOutcome::NoResult => "hover returned no result",
                    crate::lsp::EventOutcome::Unsupported => "hover unsupported",
                    crate::lsp::EventOutcome::Superseded => "hover result superseded",
                    _ => "hover failed",
                },
            );
            pending_hover = None;
        }

        // As the `pending_hover` poll just above, for issue #24's passive
        // pointer hover — same shape, a wholly separate generation/receiver
        // pair (see `pointer_hover`'s field docs on why the two counters
        // never share). `pointer_hover.apply` folds in the extra "silent
        // Idle on `Ok(None)`/`Err`" divergence documented there; this call
        // site still records every outcome (including those) to the
        // journal, "passive"-prefixed so the inspector can tell the two
        // hover paths apart.
        if !matches!(stack.top(), View::LspInspector(_))
            && let Some((generation, operation_id, rx)) = &pending_pointer_hover
            && let Ok(result) = rx.try_recv()
        {
            let stale = *generation != pointer_hover.generation();
            let outcome = if stale {
                crate::lsp::EventOutcome::Superseded
            } else {
                match &result {
                    Ok(Some(_)) => crate::lsp::EventOutcome::Result,
                    Ok(None) => crate::lsp::EventOutcome::NoResult,
                    Err(error) => request_error_outcome(error),
                }
            };
            pointer_hover.apply(*generation, result, highlighter);
            observer.record_ui(
                Some(*operation_id),
                outcome,
                match outcome {
                    crate::lsp::EventOutcome::Result => "passive hover displayed",
                    crate::lsp::EventOutcome::NoResult => "passive hover returned no result",
                    crate::lsp::EventOutcome::Unsupported => "passive hover unsupported",
                    crate::lsp::EventOutcome::Superseded => "passive hover result superseded",
                    _ => "passive hover failed",
                },
            );
            pending_pointer_hover = None;
        }

        if let Some(rx) = &pending_units
            && let Ok(result) = rx.try_recv()
        {
            pending_units = None;
            units_status = None;
            match result {
                Ok(grouping) => {
                    if let View::Diff(app) = stack.top() {
                        let live_key = crate::groups::diff_key(&crate::groups::enumerate_hunks(
                            app.full_files(),
                        ));
                        if grouping.diff_key == live_key {
                            units_panel = Some(UnitsPanel::build(&grouping, app.full_files()));
                        } else {
                            // The diff refreshed while the agent was
                            // thinking (watch mode, a scope swap). The
                            // stale grouping is already persisted, so
                            // nothing is lost — but showing it against a
                            // diff it doesn't describe would be worse
                            // than asking for a re-run.
                            goto_status = Some(
                                "units: diff changed while grouping — press u again".to_owned(),
                            );
                        }
                    }
                }
                Err(e) => goto_status = Some(format!("units: {e}")),
            }
        }

        if let Some((number, view_token, rx)) = &pr_scope.pending
            && let Ok(result) = rx.try_recv()
        {
            let number = *number;
            let view_token = *view_token;
            pr_scope.pending = None;
            match result {
                // Never applied here directly: parked first, and the
                // block just below applies it iff the requesting view is
                // on top — same frame in the common case, later if the
                // reviewer wandered off to the log/a file view meanwhile.
                Ok(text) => pr_scope.parked = Some((number, view_token, text)),
                Err(e) => {
                    // gh's message can be multi-line; the status bar has
                    // one line, and the first non-empty one is where gh
                    // puts the substance.
                    let first = e
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("gh failed")
                        .to_string();
                    goto_status = Some(format!("scope: {first}"));
                }
            }
        }

        if let Some((_, view_token, _)) = &pr_scope.parked {
            let view_token = *view_token;
            let at_root = stack.is_at_root();
            // Only the exact view that asked — matched by token, not by
            // "is a Diff": the log view pushes fresh `App`s, so an
            // unrelated diff can be on top by the time `gh` answers, and
            // swapping *that* would overwrite a view the reviewer never
            // asked to change. Until the requester returns, the result
            // just stays parked (see `PrScopeFetch`'s docs).
            if let View::Diff(app) = stack.top_mut()
                && app.view_token == view_token
            {
                let (number, _, text) = pr_scope.parked.take().expect("checked above");
                finish_scope_swap(
                    app,
                    ScopeSwapPayload {
                        text,
                        interactive: false,
                        disk_is_new_side: false,
                        scope_label: Some(format!("PR #{number}")),
                        revision_scope: None,
                        is_working_tree: false,
                    },
                    at_root,
                    watch_active,
                    jj_repo.as_ref(),
                    lsp_manager,
                    highlighter,
                    &mut hover_state,
                    &mut refs_panel,
                    &mut context_menu,
                    &mut watch_paused,
                    &mut watch_status,
                    &mut moving_scope,
                );
                goto_status = Some(format!("scope: PR #{number}"));
            }
        }

        if !matches!(stack.top(), View::LspInspector(_))
            && let Some((from, op)) = &pending_goto
        {
            match op {
                PendingGoto::Definition { operation_id, rx } => {
                    if let Ok(result) = rx.try_recv() {
                        let from = from.clone();
                        let operation_id = *operation_id;
                        pending_goto = None;
                        apply_definition_result(
                            result,
                            from,
                            stack,
                            &mut jump_stack,
                            lsp_manager,
                            &mut refs_panel,
                            &mut goto_status,
                            &observer,
                            operation_id,
                        );
                        hover_state.invalidate();
                    }
                }
                PendingGoto::References { operation_id, rx } => {
                    if let Ok(result) = rx.try_recv() {
                        let from = from.clone();
                        let operation_id = *operation_id;
                        pending_goto = None;
                        apply_references_result(
                            result,
                            from,
                            lsp_manager,
                            &mut refs_panel,
                            &mut goto_status,
                            &observer,
                            operation_id,
                        );
                    }
                }
            }
        }

        // A status hint for the server a hover-eligible position would use,
        // in two different cadences depending on what's wrong: `Installing`
        // updates every frame (its `message` changes as an auto-install
        // progresses — a download percentage, an `npm install` starting —
        // so the status bar should track it live, not freeze on the first
        // one seen), while `Unavailable` only warns once per language (its
        // reason is static, and `lsp::adapter::resolve_server`'s error
        // messages already read like a status-bar hint — "LSP: typescript
        // ✕ — npm i -g …" — so repeating it every frame would add nothing).
        if let Some(query) = stack.top().hover_query()
            && let Some(server_key) = lsp_manager.server_identity(&query.file, &query.git_root)
        {
            match lsp_manager.state(&query.file, &query.git_root) {
                ServerState::Installing { message } => lsp_status = Some(message),
                ServerState::Unavailable { reason } if !warned_servers.contains(&server_key) => {
                    lsp_status = Some(reason);
                    warned_servers.insert(server_key);
                }
                // A server that crashed mid-session is at least as worth
                // surfacing as one that was never available in the first
                // place — before this arm, `Crashed` fell through to the
                // wildcard below and a reviewer had no in-session signal at
                // all that go-to-definition/hover had gone silent (see
                // issue #4's motivating report). Same once-per-server-instance
                // dedup as `Unavailable`, and deliberately reusing the very
                // same `warned_servers` set rather than a second one: a
                // server this session already warned about as `Unavailable`
                // and a later `Crashed` for that *same instance* are both
                // "stop repeating this," not two independent budgets. Keyed
                // by the full `(LangKey, workspace_root)` identity, not just
                // the language, so this doesn't also collapse two distinct
                // server instances of the same language into one budget —
                // see `warned_servers`' docs above.
                ServerState::Crashed { reason } if !warned_servers.contains(&server_key) => {
                    lsp_status = Some(reason);
                    warned_servers.insert(server_key);
                }
                _ => {}
            }
        }

        if watch_status
            .as_ref()
            .is_some_and(|s| s.set_at.elapsed() > WATCH_STATUS_FLASH)
        {
            watch_status = None;
        }
        let now = Instant::now();
        key_display.tick(now);
        // Issue #24: the debounce deadline is checked once per loop
        // iteration here, in the wall-clock preamble, rather than driven by
        // any dedicated timer/thread — see `pointer_hover::POINTER_HOVER_DEBOUNCE`'s
        // docs for the resulting worst-case latency. `passive_hover_suppressed`
        // is re-checked right before firing (defense-in-depth: every real
        // trigger for it already runs through one of the four cancellation
        // hooks below, so this should never actually catch anything by the
        // time a debounce fires — but it costs nothing to be sure).
        if let Some((target, anchor_row)) = pointer_hover.due(now) {
            if passive_hover_suppressed(
                &compose,
                &search_prompt,
                &help,
                &scope_menu,
                &units_setup,
                &refs_panel,
                &units_panel,
                &context_menu,
                &hover_state,
                stack.top(),
            ) {
                pointer_hover.cancel();
            } else {
                fire_pointer_hover(
                    &target,
                    anchor_row,
                    &mut pointer_hover,
                    &mut pending_pointer_hover,
                    lsp_manager,
                    &observer,
                    &diagnostics,
                    stack.top(),
                );
            }
        }
        let status_note = hover_state
            .status_hint()
            .or_else(|| pointer_hover.tree_status_hint())
            .or_else(|| goto_status.clone())
            .or_else(|| units_status.clone())
            .or_else(|| lsp_status.clone())
            .or_else(|| watch_status.as_ref().map(|s| s.text.clone()));
        // Issue #23 req 5: a disabled entry's reason (e.g. "LSP: rust-
        // analyzer is starting") must flip live while the menu just sits
        // open, not freeze on whatever it said the frame it opened —
        // re-derived fresh every iteration, same "nothing here is ever
        // carried over stale" reasoning as `geometry` just below. Closes
        // the menu outright (rather than rendering an empty popup) if its
        // target stops resolving any entries at all.
        refresh_context_menu(&mut context_menu, stack.top(), lsp_manager);
        // Issue #20: rebuilt fresh every iteration — every rect a pane/
        // overlay might have moved (resize, a toggled sidebar, a popup
        // opening) since the last frame, so nothing here is ever carried
        // over stale. `draw`'s closure borrows this mutably (never moves
        // it), so it's still populated and in scope for the wheel-routing
        // arm below, covering this same iteration's one `recv_timeout` event.
        let mut geometry = mouse::FrameGeometry::new();
        terminal.draw(|frame| {
            draw(
                frame,
                stack.top_mut(),
                keymap,
                highlighter,
                &hover_state,
                &pointer_hover,
                &diagnostics,
                refs_panel.as_ref().map(|s| &s.panel),
                units_panel.as_ref(),
                units_setup.as_ref(),
                status_note.as_deref(),
                &comment_index,
                compose.as_mut(),
                scope_menu.as_ref(),
                context_menu.as_ref(),
                help.as_ref(),
                search_prompt.as_ref(),
                jj_repo.is_some(),
                &key_display,
                hints_expanded,
                &mut geometry,
                compose_keymap,
            )
        })?;

        match app_rx.recv_timeout(EVENT_POLL_INTERVAL) {
            Ok(AppEvent::Terminal(Event::Key(key))) => {
                if key.kind != KeyEventKind::Press {
                    // no-op; falls through to the loop's bottom
                } else {
                    // Issue #24 req 5's first cancellation hook: any real key
                    // press cancels passive pointer hover unconditionally,
                    // before the compose/help/search-prompt/scope-menu/ordinary
                    // dispatch chain below even runs — a keystroke is the
                    // clearest possible signal the reviewer's attention just
                    // left wherever the pointer was resting, whether or not it
                    // resolves to an `Action` at all (`StepResult::Pending`/
                    // `Cancelled` count too, which is why this sits ahead of
                    // the resolver rather than inside its `Matched` arm).
                    cancel_pending_pointer_hover(
                        &mut pointer_hover,
                        &mut pending_pointer_hover,
                        &observer,
                    );
                    if let Some(state) = compose.as_mut() {
                        // The compose overlay wants raw characters, not
                        // `Action`s — see `ui::compose::handle_key`'s docs on
                        // why this bypasses the keymap resolver entirely rather
                        // than only intercepting a few already-resolved
                        // actions the way the hover popup/references panel do
                        // below. The key-display overlay masks this the same
                        // way — see `key_display`'s module docs on why it never
                        // echoes typed characters.
                        key_display.record_typing(Instant::now());
                        match compose::handle_key(state.buffer_mut(), key, compose_keymap) {
                            ComposeOutcome::Continue => {}
                            ComposeOutcome::Cancel => compose = None,
                            ComposeOutcome::Save => {
                                let (status, saved) = finish_compose(
                                    state,
                                    comments_repo_root.as_deref(),
                                    &mut comment_list,
                                    &mut comment_index,
                                );
                                compose = None;
                                // Issue #19's range-compose counterpart to #17's
                                // `y`: a successful save clears the selection
                                // that produced it, so the reviewer sees the new
                                // range's markers land in place of the (now
                                // stale) highlighted rows instead of both
                                // showing at once. A no-op for a plain `c` —
                                // `cancel_visual`'s own `bool` result already
                                // handles "there was nothing to cancel," so this
                                // doesn't need to distinguish the two cases.
                                if saved && let View::Diff(app) = stack.top_mut() {
                                    app.cancel_visual();
                                }

                                // The prompt only ever fires once per session
                                // (`skill_prompt_offered`), only after a comment
                                // actually persisted (not a discarded-empty or
                                // failed save), only when the reviewer hasn't
                                // opted out (`offer_skill_install`), only when
                                // this repo doesn't already have the full
                                // harness — skill, AGENTS.md, and CLAUDE.md, see
                                // `skill::harness_installed`'s docs on why *any*
                                // missing piece re-offers (M17 extended this
                                // from a skill-only check so a repo that only
                                // ran an older `ktmr skill install` still gets
                                // offered the rest) — and only when `$HOME`
                                // doesn't already carry the skill via `ktmr
                                // skill install --user`: a repo that inherits
                                // it from there is functionally already done,
                                // even though `harness_installed` (which only
                                // ever looks inside the repo) can't see that —
                                // offering a per-repo copy on top would just be
                                // redundant, not incomplete. Any one of those
                                // false means a plain status message, same as
                                // before M16.
                                let offer = saved
                                    && offer_skill_install
                                    && !skill_prompt_offered
                                    && comments_repo_root
                                        .as_deref()
                                        .is_some_and(|root| !skill::harness_installed(root))
                                    && !skill::user_skill_installed();
                                goto_status = Some(if offer {
                                    skill_prompt_offered = true;
                                    awaiting_skill_prompt_key = true;
                                    format!(
                                        "{status} \u{b7} press y to install the Claude Code review skill (ktmr skill install)"
                                    )
                                } else {
                                    status
                                });
                            }
                        }
                    } else if let Some(state) = help.as_mut() {
                        // The help popup is modal (see `ui::help`'s module
                        // docs): every key while it's open is consumed here,
                        // never falling through to the resolver below, so
                        // there's nothing more this arm needs to do once
                        // `handle_key` returns other than decide whether to
                        // close. Sits right after `compose` and before the
                        // search prompt and the scope-menu revision input in
                        // this `else if` chain — mutual exclusion between all
                        // four holds structurally, not by any extra check here:
                        // `?` only ever reaches the resolver (and so only ever
                        // produces `Action::OpenHelp`, see `handle_action`
                        // below) when none of `compose`, `search_prompt`, or
                        // `ScopeMenuState::Revision` is already claiming every
                        // keystroke as literal text, so `help` can never become
                        // `Some` while any of those is open, and all three sit
                        // ahead of this one regardless (`search_prompt`'s own
                        // arm below sits *after* this one, for the same reason
                        // in reverse: `/` while help is open filters its own
                        // row list rather than opening a second, unrelated
                        // prompt underneath it).
                        //
                        // Deliberately does *not* call `key_display.record_typing`
                        // the way `compose`/the revision input do above: that
                        // masking exists to keep private review content (a
                        // comment body, a revision string) off the
                        // `--show-keys` overlay — see `key_display`'s module
                        // docs — but `Filter` mode only ever narrows a list of
                        // public command names/descriptions/bindings, so
                        // there's nothing here worth hiding.
                        let filter = state.filter_text();
                        let total_rows = match &help_row_count_cache {
                            Some((cached_filter, count)) if cached_filter == filter => *count,
                            _ => {
                                let count = help::build_rows(keymap, filter).len();
                                help_row_count_cache = Some((filter.to_owned(), count));
                                count
                            }
                        };
                        let viewport = help::viewport_rows(area);
                        match help::handle_key(state, key, total_rows, viewport) {
                            HelpOutcome::Continue => {}
                            HelpOutcome::Close => help = None,
                        }
                    } else if let Some(prompt) = search_prompt.as_mut() {
                        // Bypasses the keymap resolver for the same reason
                        // `compose`/the revision input do below: a query like
                        // `fn main` or `TODO(` can contain characters (space,
                        // `(`) that would otherwise resolve to unrelated
                        // `Action`s. Sits between `help` and the scope-menu
                        // revision input in this `else if` chain — see `help`'s
                        // own comment above for why the four-way mutual
                        // exclusion holds structurally.
                        //
                        // Deliberately does *not* call `key_display.record_typing`
                        // the way `compose`/the revision input do — same
                        // reasoning as `help`'s own filter text above: a search
                        // query only ever narrows down to content already
                        // visible on screen (the diff itself), never private
                        // review prose, so there's nothing here worth masking
                        // from `--show-keys`.
                        match search::handle_prompt_key(&mut prompt.input, key) {
                            search::SearchPromptOutcome::Continue => {
                                if let View::Diff(app) = stack.top_mut() {
                                    app.recompute_search_live(prompt.input.text(), &prompt.origin);
                                }
                            }
                            search::SearchPromptOutcome::Cancel => {
                                let origin = prompt.origin.clone();
                                if let View::Diff(app) = stack.top_mut() {
                                    app.cancel_search(&origin);
                                }
                                search_prompt = None;
                            }
                            search::SearchPromptOutcome::Confirm => {
                                let origin = prompt.origin.clone();
                                // The prompt's own current text, not
                                // `app.search`'s prior state — see
                                // `App::confirm_search`'s docs on why a
                                // reopened prompt confirmed with nothing typed
                                // must cancel rather than silently reconfirm
                                // whatever search was already active before
                                // this `/` press.
                                let query = prompt.input.text().to_owned();
                                let jump_from = prompt.jump_from.clone();
                                if let View::Diff(app) = stack.top_mut() {
                                    goto_status = app.confirm_search(&query, &origin);
                                }
                                // `confirm_search` itself never moves the
                                // cursor — the live incremental preview
                                // already did, on every keystroke while the
                                // prompt was open — so this records the whole
                                // `/` session (from wherever it was pressed to
                                // wherever Enter leaves the cursor now) as one
                                // significant jump, not each keystroke's own
                                // preview move. A query that resolved to zero
                                // matches, or was cancelled via
                                // `App::cancel_search` above, leaves the cursor
                                // back where it started — `record_jump`'s
                                // equality check quietly declines to record
                                // that case, same as everywhere else.
                                record_jump(&mut jump_stack, jump_from, stack.top().jump_entry());
                                search_prompt = None;
                            }
                        }
                    } else if let Some(ScopeMenuState::PullRequest(input)) = scope_menu.as_mut() {
                        // Same keymap bypass and typing-mask as the revision
                        // input below — a PR number is still user-typed text.
                        key_display.record_typing(Instant::now());
                        match handle_revision_key(input, key) {
                            RevisionInputOutcome::Continue => {}
                            RevisionInputOutcome::Back => {
                                scope_menu = Some(ScopeMenuState::new_list(jj_repo.is_some()));
                            }
                            RevisionInputOutcome::Submit(text) => {
                                match scope_menu::parse_pr_number(&text) {
                                    Err(e) => {
                                        // Leave the input open to fix and
                                        // retry — the same rejected-choice
                                        // shape the revision input uses.
                                        goto_status = Some(format!("scope: {e}"));
                                    }
                                    Ok(number) => {
                                        if let View::Diff(app) = stack.top() {
                                            // A network call through `gh` —
                                            // seconds, sometimes worse — so
                                            // unlike the git/jj choices it
                                            // never runs on this thread: the
                                            // diff on screen stays put and
                                            // fully interactive until the text
                                            // lands (see the pending poll in
                                            // the frame preamble).
                                            let repo_root = app.repo_root.clone();
                                            let (tx, rx) = std::sync::mpsc::channel();
                                            std::thread::spawn(move || {
                                                let result = crate::vcs::github::pull_request_diff(
                                                    &repo_root, number,
                                                )
                                                .map_err(|e| e.to_string());
                                                let _ = tx.send(result);
                                            });
                                            pr_scope.clear();
                                            pr_scope.pending = Some((number, app.view_token, rx));
                                            goto_status = Some(format!(
                                                "scope: fetching PR #{number} via gh …"
                                            ));
                                        }
                                        scope_menu = None;
                                    }
                                }
                            }
                        }
                    } else if let Some(ScopeMenuState::Revision(input)) = scope_menu.as_mut() {
                        // Bypasses the keymap resolver for the same reason
                        // `compose` does above: a revision string can contain
                        // characters (`.`, `@`, `-`) that would otherwise
                        // resolve to unrelated `Action`s. Masked the same way
                        // too — a revision string is still user-typed text.
                        key_display.record_typing(Instant::now());
                        match handle_revision_key(input, key) {
                            RevisionInputOutcome::Continue => {}
                            RevisionInputOutcome::Back => {
                                scope_menu = Some(ScopeMenuState::new_list(jj_repo.is_some()));
                            }
                            RevisionInputOutcome::Submit(text) => {
                                let at_root = stack.is_at_root();
                                if let View::Diff(app) = stack.top_mut() {
                                    match apply_scope_swap(
                                        app,
                                        &ScopeChoice::Revision(text),
                                        at_root,
                                        watch_active,
                                        jj_repo.as_ref(),
                                        lsp_manager,
                                        highlighter,
                                        &mut hover_state,
                                        &mut refs_panel,
                                        &mut context_menu,
                                        &mut watch_paused,
                                        &mut watch_status,
                                        &mut moving_scope,
                                    ) {
                                        Ok(()) => {
                                            scope_menu = None;
                                            pr_scope.clear();
                                        }
                                        Err(e) => {
                                            goto_status = Some(format!("scope: {e}"));
                                            // Leave `scope_menu` open (still
                                            // `Revision`) so the reviewer can
                                            // fix the input and retry — an
                                            // invalid revision must never blank
                                            // the diff already on screen.
                                        }
                                    }
                                } else {
                                    // The popup only ever opens from a
                                    // `View::Diff` (see `Action::OpenScopeMenu`'s
                                    // handling below) and nothing else pops the
                                    // stack while it's open, so this shouldn't
                                    // happen in practice — closing defensively
                                    // rather than leaving an orphaned overlay.
                                    scope_menu = None;
                                }
                            }
                        }
                    } else {
                        // M16's first-comment skill-install prompt consumes
                        // exactly the one keypress that follows it, resolved
                        // *before* the keymap even sees this key — same
                        // bypass-the-resolver idea `compose`/the revision input
                        // use above, but only for this single key rather than a
                        // whole overlay's lifetime. `y` means "install now" and
                        // is swallowed entirely here, before the resolver ever
                        // gets a chance to match it against
                        // `Action::YankSelection` (issue #17): a reviewer who
                        // just saved their first comment sees the skill-install
                        // offer, not a yank status, even though `y` is a bound
                        // key everywhere else. Any other key means "dismiss" —
                        // deliberately *not* swallowed: falling through to the
                        // ordinary resolver below means a reviewer who, say,
                        // presses `j` to keep scrolling right after saving a
                        // comment gets both the dismiss and the scroll from that
                        // one keypress, rather than needing a second, wasted
                        // press just to clear the prompt first.
                        let mut consumed_by_skill_prompt = false;
                        if awaiting_skill_prompt_key {
                            awaiting_skill_prompt_key = false;
                            if key.code == KeyCode::Char('y') && key.modifiers.is_empty() {
                                consumed_by_skill_prompt = true;
                                goto_status =
                                    Some(run_skill_install_prompt(comments_repo_root.as_deref()));
                            }
                        }

                        if !consumed_by_skill_prompt {
                            let chord = KeyChord::from(key);
                            let step = resolver.feed(chord);
                            key_display.record_step(chord, step, Instant::now());
                            match step {
                                // Global quit, intercepted here rather than
                                // taught to every pushed view: reached only via
                                // this `else` branch's resolver (see the long
                                // comment chain above — compose/help/search
                                // prompt/the scope-menu's revision input all
                                // bypass the resolver entirely while they own
                                // input, so a plain `q` typed into any of them
                                // never resolves to `Action::Quit` in the first
                                // place), and unconditionally on whatever view
                                // is on top — a pushed `File`/`Timeline`/`Log`/
                                // `LspInspector`, the units-setup wizard, an
                                // open hover/references/scope-menu overlay, all
                                // included. `handle_action` never even sees
                                // this action: terminal restore, LSP shutdown,
                                // observer shutdown, and the update-exit notice
                                // all live in `run` after `event_loop` returns,
                                // so returning `Ok(())` here runs every one of
                                // them exactly as a normal exit would.
                                StepResult::Matched(Action::Quit) => return Ok(()),
                                StepResult::Matched(action) => {
                                    stack.top_mut().clear_pending_keys();
                                    goto_status = None;
                                    handle_action(
                                        action,
                                        stack,
                                        &mut hover_state,
                                        &mut pending_hover,
                                        &mut pending_goto,
                                        &mut refs_panel,
                                        &mut units_panel,
                                        &mut units_setup,
                                        &mut pending_units,
                                        &mut units_status,
                                        &mut jump_stack,
                                        &diagnostics,
                                        &mut goto_status,
                                        lsp_manager,
                                        jj_repo.as_ref(),
                                        &mut compose,
                                        &mut scope_menu,
                                        &mut pr_scope,
                                        &mut context_menu,
                                        &mut help,
                                        &mut search_prompt,
                                        highlighter,
                                        &mut watch_paused,
                                        watch_active,
                                        &mut watch_status,
                                        &mut moving_scope,
                                        &mut hints_expanded,
                                        &observer,
                                    );
                                }
                                StepResult::Pending => {
                                    stack.top_mut().set_pending_keys(resolver.pending_display());
                                }
                                StepResult::Cancelled => stack.top_mut().clear_pending_keys(),
                            }
                        }
                    }
                }
            }
            // Issue #20 wired wheel routing; issue #21 adds the files-tree
            // click; issue #23 adds the right-click context menu below.
            // `Up`/`Drag`/`Moved`/`ScrollLeft`/`ScrollRight` still fall to
            // the wildcard arm below — there's no drag/double-click in
            // scope for wheel, click, or the context menu.
            // `mouse::scroll_at`/`mouse::handle_left_click` never touch
            // keyboard focus except
            // through the same `App::click_files_row`/`confirm_row` path
            // Enter already uses, which is what makes req 5's "wheel
            // scrolling does not steal keyboard focus" (and the click
            // path's deliberate *opposite* — see `click_files_row`'s docs)
            // both true by construction.
            //
            // Gated on `mouse_enabled` (`[ui] mouse`, see
            // `Config::mouse`'s docs) even though `run` already skips
            // sending `EnableMouseCapture` when it's `false`: on a real
            // terminal that alone is enough (nothing generates SGR mouse
            // reports for the input thread to ever forward), but this is
            // belt-and-suspenders against a terminal/multiplexer that
            // reports mouse events regardless of what katamari asked for —
            // "keyboard behavior is identical with mouse disabled" means
            // *nothing* here should react to a mouse byte that arrives
            // anyway, not just "we didn't ask for it."
            Ok(AppEvent::Terminal(Event::Mouse(mouse_event))) if mouse_enabled => {
                // Issue #24 req 5's second cancellation hook: any mouse
                // event that isn't bare motion cancels passive pointer
                // hover — a click, a drag, a wheel tick, all mean the
                // reviewer's attention (or their hand on the button) is
                // doing something other than resting. `Drag` never reaches
                // this check with pointer hover still armed for the same
                // reason it's structurally free elsewhere in this module:
                // crossterm only reports `Moved` when *no* button is held
                // (see `parse_cb`), so a held button always arrives as
                // `Drag`, already caught by this `!= Moved` guard before the
                // `match` below even runs.
                if mouse_event.kind != MouseEventKind::Moved {
                    cancel_pending_pointer_hover(
                        &mut pointer_hover,
                        &mut pending_pointer_hover,
                        &observer,
                    );
                }
                match mouse_event.kind {
                    // Issue #23: while the context menu is open, every wheel
                    // tick is inert — it has no scrollable content of its
                    // own (≤6 entries), and content beneath it must never
                    // scroll out from under it (req 9). `context_menu` has
                    // no `ScrollTarget`/recorded rect for `scroll_at` to
                    // match on at all (see `FrameGeometry::context_menu_rect`'s
                    // docs on why), so it needs this explicit guard.
                    //
                    // The three fully-modal overlays need the same explicit
                    // gate, and for the mirror-image reason: they *do*
                    // record `ScrollTarget` rects, but only spanning the
                    // diff pane — the sidebar's `DiffFiles` rect (and any
                    // other strip a modal doesn't cover) stays hittable
                    // beneath them, so without this guard a wheel tick in
                    // the sidebar column would scroll the file list out
                    // from under an open scope menu/compose/units-setup, a
                    // state no keyboard sequence can produce (issue #20
                    // req 7). Exactly the click arm's gate below, minus
                    // `help`: the help popup is itself wheel-scrollable,
                    // so a blanket block here would kill its own
                    // scrolling — the finer point-check inside the arm
                    // handles it instead.
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                        if context_menu.is_none()
                            && compose.is_none()
                            && scope_menu.is_none()
                            && units_setup.is_none() =>
                    {
                        // Help's centered popup leaves margins on every
                        // side (the exact gap the click arms' `help.is_none()`
                        // guards exist for) while the sidebar/diff-pane
                        // rects beneath stay recorded — so while help is
                        // open, a tick anywhere *off* the popup is captured
                        // and discarded rather than falling through
                        // `hit()` to the pane underneath it.
                        let over_helps_uncovered_margin = help.is_some()
                            && !matches!(
                                geometry.hit(mouse_event.column, mouse_event.row),
                                Some(mouse::ScrollTarget::HelpPopup)
                            );
                        if !over_helps_uncovered_margin {
                            let delta = if mouse_event.kind == MouseEventKind::ScrollUp {
                                -mouse::WHEEL_SCROLL_ROWS
                            } else {
                                mouse::WHEEL_SCROLL_ROWS
                            };
                            mouse::scroll_at(
                                &geometry,
                                mouse_event.column,
                                mouse_event.row,
                                delta,
                                stack,
                                &mut hover_state,
                                refs_panel.as_mut().map(|s| &mut s.panel),
                                units_panel.as_mut(),
                                help.as_mut(),
                                keymap,
                                &mut help_row_count_cache,
                                area,
                            );
                        }
                    }
                    // The keyboard path's fully-modal gates apply to clicks
                    // too: compose/scope-menu/units-setup/help intercept
                    // every keystroke before it can reach the App, but
                    // their recorded rects cover less than the full frame —
                    // the modal trio only spans the diff pane, and help's
                    // centered popup leaves margins on every side — so a
                    // click in the uncovered strip would otherwise mutate
                    // selection/focus/the diff cursor beneath an open
                    // modal, a state no keyboard sequence can produce. A
                    // click while one is open falls to the `_` arm below
                    // instead.
                    // Issue #23: the context menu's own left-click handling
                    // — checked *first*, ahead of the refs/units dismiss
                    // arm and the general click-dispatch arm just below, so
                    // a click while the menu is open is always resolved
                    // against the menu, never misread as a click meant for
                    // whatever overlay/pane happens to be recorded beneath
                    // it (in practice `context_menu` and `refs_panel`/
                    // `units_panel`/`compose`/`scope_menu`/`units_setup`
                    // can never be `Some` at once — the open flow closes
                    // any of those before a menu can open at all, see
                    // `mouse::handle_right_click`'s docs — but ordering
                    // this arm first makes that guarantee explicit rather
                    // than relying on every other guard below getting an
                    // extra `&& context_menu.is_none()` added and kept in
                    // sync forever). Three outcomes, matching
                    // `context_menu::entry_at`'s own three-way split: a hit
                    // on an entry runs the *exact* same confirm dispatch
                    // `Action::Confirm` (keyboard Enter) already does — set
                    // the click's entry as selected, then let
                    // `handle_action`'s own interception block do the rest,
                    // so mouse and keyboard invocation are provably one
                    // code path, not two; the border/title row is a
                    // captured no-op; anywhere else closes the menu (req
                    // 7's close-when-invalid half, and req 9: this click
                    // must never also reach content underneath).
                    MouseEventKind::Down(MouseButton::Left) if context_menu.is_some() => {
                        let click = Position {
                            x: mouse_event.column,
                            y: mouse_event.row,
                        };
                        match geometry.context_menu_rect() {
                            Some(rect) if rect.contains(click) => {
                                if let Some(idx) = context_menu::entry_at(
                                    rect,
                                    context_menu.as_ref().unwrap().entries().len(),
                                    mouse_event.column,
                                    mouse_event.row,
                                ) {
                                    context_menu.as_mut().unwrap().set_selected(idx);
                                    handle_action(
                                        Action::Confirm,
                                        stack,
                                        &mut hover_state,
                                        &mut pending_hover,
                                        &mut pending_goto,
                                        &mut refs_panel,
                                        &mut units_panel,
                                        &mut units_setup,
                                        &mut pending_units,
                                        &mut units_status,
                                        &mut jump_stack,
                                        &diagnostics,
                                        &mut goto_status,
                                        lsp_manager,
                                        jj_repo.as_ref(),
                                        &mut compose,
                                        &mut scope_menu,
                                        &mut pr_scope,
                                        &mut context_menu,
                                        &mut help,
                                        &mut search_prompt,
                                        highlighter,
                                        &mut watch_paused,
                                        watch_active,
                                        &mut watch_status,
                                        &mut moving_scope,
                                        &mut hints_expanded,
                                        &observer,
                                    );
                                }
                                // else: the border/title row — captured, no-op.
                            }
                            _ => context_menu = None, // missed the popup entirely
                        }
                    }
                    // The references/units panels follow the keyboard's
                    // own convention for them ("any other key closes the
                    // panel"): the first click anywhere dismisses the
                    // panel and is consumed — it must not also act on the
                    // pane behind it, whose rect stays hittable above the
                    // panel's own bottom strip and would otherwise let a
                    // click silently move the cursor beneath an open
                    // panel, a state no keyboard sequence can produce.
                    MouseEventKind::Down(MouseButton::Left)
                        if refs_panel.is_some() || units_panel.is_some() =>
                    {
                        refs_panel = None;
                        units_panel = None;
                    }
                    MouseEventKind::Down(MouseButton::Left)
                        if compose.is_none()
                            && scope_menu.is_none()
                            && units_setup.is_none()
                            && help.is_none() =>
                    {
                        let identifier_hit = mouse::handle_left_click(
                            &geometry,
                            stack,
                            &mut jump_stack,
                            &mut hover_state,
                            mouse_event.column,
                            mouse_event.row,
                            mouse_event.modifiers.contains(KeyModifiers::SHIFT),
                        );
                        // Issue #22: an eligible identifier click chases
                        // go-to-definition through the exact same
                        // `handle_action` dispatch keyboard `gd` uses —
                        // `handle_left_click` already positioned the cursor
                        // and active symbol, so this call's own
                        // `stack.top().hover_query()` re-derivation resolves
                        // the identifier the click just landed on, getting
                        // readiness/supersession/jump-history/status for
                        // free rather than a second copy of any of it here.
                        if identifier_hit {
                            handle_action(
                                Action::GotoDefinition,
                                stack,
                                &mut hover_state,
                                &mut pending_hover,
                                &mut pending_goto,
                                &mut refs_panel,
                                &mut units_panel,
                                &mut units_setup,
                                &mut pending_units,
                                &mut units_status,
                                &mut jump_stack,
                                &diagnostics,
                                &mut goto_status,
                                lsp_manager,
                                jj_repo.as_ref(),
                                &mut compose,
                                &mut scope_menu,
                                &mut pr_scope,
                                &mut context_menu,
                                &mut help,
                                &mut search_prompt,
                                highlighter,
                                &mut watch_paused,
                                watch_active,
                                &mut watch_status,
                                &mut moving_scope,
                                &mut hints_expanded,
                                &observer,
                            );
                        }
                    }
                    // Issue #23: secondary click — resolves a fresh target,
                    // retargets an already-open menu onto a new one, or
                    // closes it, all through the single
                    // `mouse::handle_right_click` open/retarget/close flow
                    // (its own docs cover why compose/help/the fully-modal
                    // overlays are a no-op here rather than a close: a
                    // stray click, either mouse button, must never discard
                    // one of those). The final assignment below always
                    // replaces `context_menu` wholesale — retarget-when-
                    // valid and close-when-invalid (req 7) both fall out of
                    // that same one `match`, never a second "is this a
                    // retarget or a fresh open" branch.
                    // Same fully-modal gate as the left-click arm above,
                    // plus help: every one of these records less than the
                    // full frame (help's centered popup leaves margins,
                    // the modal trio only covers the diff pane), so
                    // without the gate a right-click in the uncovered
                    // strip could open a menu over — and then act beneath
                    // — an overlay that swallows every key.
                    MouseEventKind::Down(MouseButton::Right)
                        if compose.is_none()
                            && scope_menu.is_none()
                            && units_setup.is_none()
                            && help.is_none() =>
                    {
                        match mouse::handle_right_click(
                            &geometry,
                            stack,
                            &mut hover_state,
                            mouse_event.column,
                            mouse_event.row,
                        ) {
                            mouse::RightClickOutcome::Noop
                            | mouse::RightClickOutcome::ClosedHover => {}
                            mouse::RightClickOutcome::ClosedPanel => {
                                refs_panel = None;
                                units_panel = None;
                            }
                            mouse::RightClickOutcome::Miss => context_menu = None,
                            mouse::RightClickOutcome::Target(target) => {
                                // Req 9: opening the menu closes anything
                                // that would overlap it — a hover popup or
                                // refs/units panel open *elsewhere* on
                                // screen (a click directly on one is
                                // already handled by the outcomes above).
                                hover_state.close();
                                refs_panel = None;
                                units_panel = None;
                                let entries =
                                    context_menu_entries_for(&target, stack.top(), lsp_manager);
                                context_menu = if entries.is_empty() {
                                    goto_status = Some("menu: nothing to do here".to_owned());
                                    None
                                } else {
                                    Some(ContextMenuState::new(
                                        target,
                                        entries,
                                        (mouse_event.column, mouse_event.row),
                                    ))
                                };
                            }
                        }
                    }
                    // Req 7's hover highlight: moving over an open menu's
                    // entries moves its selection. Checked first, ahead of
                    // issue #24's own `Moved` arm just below, since the two
                    // are mutually exclusive by construction (the menu
                    // closes hover/refs/units before it can ever open — see
                    // `mouse::handle_right_click`'s docs) but a guard order
                    // that made that explicit rather than assumed is one
                    // fewer thing to keep in sync if that ever changes: menu
                    // open → menu highlight wins; menu closed → passive
                    // hover tracking (below) gets the event instead.
                    MouseEventKind::Moved if context_menu.is_some() => {
                        if let Some(rect) = geometry.context_menu_rect()
                            && let Some(idx) = context_menu::entry_at(
                                rect,
                                context_menu.as_ref().unwrap().entries().len(),
                                mouse_event.column,
                                mouse_event.row,
                            )
                        {
                            context_menu.as_mut().unwrap().set_selected(idx);
                        }
                    }
                    // Issue #24: app-wide passive hover tracking — armed
                    // only when `[ui] mouse_hover` is on (independent of
                    // `mouse`/capture itself; see `Config::mouse_hover`'s
                    // docs) and only once every fully-modal/suppressing
                    // overlay above has already had first refusal via the
                    // context-menu arm's guard. `passive_hover_suppressed`
                    // covers everything *this* arm still needs to check
                    // (compose/search/help/scope-menu/units-setup/refs/
                    // units-panel/an explicit hover already active/visual
                    // selection) — load-bearing here, not just the
                    // deadline-fire's defense-in-depth copy (see that call
                    // site's docs): this is what stops a debounce from ever
                    // arming while one of those is open in the first place.
                    // `resolve_target` returning `None` (the pointer left
                    // every eligible pane, or landed on an ineligible row)
                    // cancels exactly like an explicit trigger would — "left
                    // the pane" is req 5's own wording.
                    MouseEventKind::Moved if mouse_hover_enabled => {
                        if passive_hover_suppressed(
                            &compose,
                            &search_prompt,
                            &help,
                            &scope_menu,
                            &units_setup,
                            &refs_panel,
                            &units_panel,
                            &context_menu,
                            &hover_state,
                            stack.top(),
                        ) {
                            cancel_pending_pointer_hover(
                                &mut pointer_hover,
                                &mut pending_pointer_hover,
                                &observer,
                            );
                        } else {
                            match pointer_hover::resolve_target(
                                &geometry,
                                stack,
                                mouse_event.column,
                                mouse_event.row,
                            ) {
                                Some((target, anchor_row)) => {
                                    pointer_hover.arm(target, anchor_row, Instant::now());
                                }
                                None => cancel_pending_pointer_hover(
                                    &mut pointer_hover,
                                    &mut pending_pointer_hover,
                                    &observer,
                                ),
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(AppEvent::Terminal(Event::Resize(_, _))) => {
                // Issue #23 req 10: a resize can shift or shrink the pane
                // the menu's target row/index was resolved against —
                // rather than trying to prove a resize never invalidates
                // it, always close and say so, the same "always-close over
                // clever survival analysis" call `handle_watch_refresh`
                // makes for a watch refresh that doesn't preserve the
                // overlay (see that function's docs). The next frame
                // re-measures every pane's geometry from scratch regardless
                // (see `run`'s loop preamble), so there's nothing else a
                // resize needs here.
                if context_menu.take().is_some() {
                    goto_status = Some("menu: closed on resize".to_owned());
                }
                // Issue #24 req 5's third cancellation hook: the pointer's
                // resolved target/anchor row was computed against this
                // frame's now-stale geometry, exactly the same "don't try
                // to prove it still applies" reasoning the context-menu
                // close just above already follows for the same event.
                cancel_pending_pointer_hover(
                    &mut pointer_hover,
                    &mut pending_pointer_hover,
                    &observer,
                );
            }
            Ok(AppEvent::Terminal(_)) => {
                // Resize, focus, paste: nothing to dispatch, but the next
                // iteration re-measures the viewport and redraws
                // regardless, which is all a resize actually needs.
            }
            Ok(AppEvent::Lsp(ServerEvent {
                language, event, ..
            })) => match event {
                crate::lsp::LspEvent::Notification { method, params } => {
                    if method == "$/progress" {
                        // Tagged with which server it came from (see
                        // `ServerEvent`'s docs) so a session running more
                        // than one language server at once doesn't show a
                        // rust-analyzer indexing tick as if it might be
                        // about the TypeScript file the cursor happens to
                        // be on.
                        lsp_status =
                            progress_status_text(&params).map(|text| format!("{language}: {text}"));
                    } else if method == "textDocument/publishDiagnostics"
                        && let Some(parsed) = parse_publish_diagnostics(&params)
                        && let Some(path) = uri_to_path(&parsed.uri)
                    {
                        diagnostics.set(path, parsed.diagnostics);
                    }
                }
                crate::lsp::LspEvent::Closed { .. } => lsp_status = None,
            },
            Ok(AppEvent::Watch(WatchSignal::Pending)) => {
                // Suppressed while paused: a "pending…" note that never
                // resolves into "updated" (see `handle_watch_refresh`'s
                // early return below) would read as a stuck refresh rather
                // than the deliberate pause it actually is.
                if !watch_paused {
                    watch_status = Some(WatchStatus::new("watch: pending\u{2026}".to_owned()));
                }
            }
            Ok(AppEvent::Watch(WatchSignal::Flushed(batch))) => {
                // Only meaningful when the currently-open prompt (if any)
                // actually belongs to the *root* diff `handle_watch_refresh`
                // is about to touch — a prompt open on a pushed diff (e.g.
                // one opened from `LogView::confirm`) has nothing to do
                // with a root-diff refresh, and passing it through would
                // recompute the wrong `App`'s search entirely. Safe to
                // check once here rather than inside `handle_watch_refresh`
                // itself: nothing can change which view is on top while a
                // modal prompt has every key (this bypass chain runs before
                // the resolver ever sees one), so the stack's shape is
                // stable for as long as `search_prompt` stays `Some`.
                let live_search = stack
                    .is_at_root()
                    .then(|| search_prompt.as_ref().map(|p| (p.input.text(), &p.origin)))
                    .flatten();
                handle_watch_refresh(
                    batch,
                    stack,
                    pre_refresh_hook.as_deref(),
                    lsp_manager,
                    highlighter,
                    &mut hover_state,
                    &mut refs_panel,
                    &mut context_menu,
                    &mut watch_status,
                    watch_paused,
                    live_search,
                    &mut pointer_hover,
                    &mut pending_pointer_hover,
                    &observer,
                );
                // The working tree just changed, which can shift where
                // every comment relocates to even though the comment log
                // itself didn't — rebuild against the fresh file content
                // rather than waiting for a `.katamari/comments.jsonl`
                // write that isn't coming.
                if let Some(root) = &comments_repo_root {
                    comment_index = comments::build_index(root, &comment_list);
                }
            }
            Ok(AppEvent::RevisionChanged) => {
                // Same root-only live-search capture as the watch arm above
                // — see its comment for why the check is safe out here.
                let live_search = stack
                    .is_at_root()
                    .then(|| search_prompt.as_ref().map(|p| (p.input.text(), &p.origin)))
                    .flatten();
                handle_moving_scope_refresh(
                    &mut moving_scope,
                    stack,
                    jj_repo.as_ref(),
                    highlighter,
                    &mut hover_state,
                    &mut refs_panel,
                    &mut context_menu,
                    &mut watch_status,
                    live_search,
                    &mut pointer_hover,
                    &mut pending_pointer_hover,
                    &observer,
                );
            }
            Ok(AppEvent::CommentsChanged) => {
                // Issue #24 req 5's fifth cancellation hook, easy to miss
                // because it isn't an input event: a comment reload can
                // insert or resize an inline comment block, reflowing
                // every diff row below it under a stationary pointer —
                // the same "changing layout" case the resize hook covers,
                // arriving through this always-live watcher (`ktmr
                // comments add/resolve` from another terminal) instead of
                // any key or mouse byte.
                cancel_pending_pointer_hover(
                    &mut pointer_hover,
                    &mut pending_pointer_hover,
                    &observer,
                );
                if let Some(root) = &comments_repo_root {
                    match CommentStore::new(root).load() {
                        Ok(loaded) => {
                            comment_list = loaded;
                            comment_index = comments::build_index(root, &comment_list);
                        }
                        Err(e) => {
                            watch_status =
                                Some(WatchStatus::new(format!("comments: reload failed: {e}")));
                        }
                    }
                }
            }
            Ok(AppEvent::JjSnapshot) => {
                if let View::Timeline(timeline) = stack.top_mut() {
                    timeline.refresh_live();
                }
                // If the timeline isn't the view on top right now, there's
                // nothing to do: `TimelineView::new` always fetches fresh
                // from jj, so reopening it later picks up this snapshot
                // anyway.
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }

        if stack.top().should_quit() && !stack.pop() {
            return Ok(());
        }
    }
}

/// Persists a finished compose overlay's buffer as a new comment (`C-s`)
/// and reports what happened as a status-bar note — a blank buffer, a
/// vanished `comments_repo_root` (shouldn't happen: the overlay only ever
/// opens from `App::comment_target`, which only exists on a `View::Diff`
/// root, exactly when `comments_repo_root` is `Some`), the target file
/// having disappeared or shrunk out from under the comment since the
/// overlay was opened, or a store I/O failure are all reported the same
/// way rather than silently dropping the comment. On success, appends the
/// new [`Comment`] to `comment_list` and rebuilds `comment_index` in place
/// so the newly saved comment renders immediately, without waiting for the
/// comments watcher's own filesystem round trip.
///
/// The returned `bool` is whether a comment actually got persisted — `false`
/// for a missing repo root, a discarded-empty buffer, or a store failure,
/// `true` only on the success arm. The caller (this module's key-dispatch
/// loop) uses it to gate M16's first-comment skill-install prompt, which
/// should only ever follow a real save, never a no-op or a failure.
fn finish_compose(
    state: &ComposeState,
    repo_root: Option<&Path>,
    comment_list: &mut Vec<Comment>,
    comment_index: &mut CommentIndex,
) -> (String, bool) {
    let Some(repo_root) = repo_root else {
        return (
            "comment: no repository root to save against".to_owned(),
            false,
        );
    };
    if state.buffer().is_blank() {
        return ("comment: discarded (empty)".to_owned(), false);
    }
    match save_comment(repo_root, state) {
        Ok(comment) => {
            comment_list.push(comment);
            *comment_index = comments::build_index(repo_root, comment_list);
            ("comment: saved".to_owned(), true)
        }
        Err(e) => (format!("comment: {e}"), false),
    }
}

/// Runs [`skill::install`] in response to `y` on M16's first-comment prompt
/// and reports the result as a status-bar note — the TUI-side twin of
/// `main.rs`'s `run_skill_install`, minus the multi-line stdout report a
/// terminal command can afford: a status bar has room for one line, so this
/// names each of the three link/write outcomes (skill link, `AGENTS.md`,
/// `CLAUDE.md`) but skips the `SKILL.md`-write detail `run_skill_install`
/// also prints — redundant with the link outcome for a status-bar reader.
/// `repo_root` is `None` only if the prompt somehow fired without a
/// `View::Diff` root's comments repo (shouldn't happen — the prompt only
/// ever arms itself right after a successful [`finish_compose`], which
/// itself requires `Some`).
fn run_skill_install_prompt(repo_root: Option<&Path>) -> String {
    let Some(repo_root) = repo_root else {
        return "skill: no repository root to install into".to_owned();
    };
    match skill::install(repo_root) {
        Ok(report) => format!(
            "skill: {} \u{b7} {} \u{b7} {}",
            report.link, report.agents_md, report.claude_md
        ),
        Err(e) => format!("skill: install failed: {e}"),
    }
}

/// Builds and appends the [`Comment`] a finished compose overlay describes:
/// re-reads `state.target.file()`'s current content (rather than trusting
/// whatever it was when the overlay opened, in case it changed mid-edit) to
/// compute a fresh [`comments::Anchor`] per endpoint, then writes it through
/// [`CommentStore::append_comment`]. A [`app::CommentTarget::Range`]'s two
/// endpoints are anchored independently against that same read — issue
/// #18's `Comment::end_anchor` already carries its own content/context hash,
/// so there's no shortcut that derives one anchor from the other. A file
/// that shrinks mid-compose so only one endpoint still exists is reported
/// per-endpoint ("line N no longer exists…") rather than silently anchoring
/// half a range; a comment log corrupted by a watch refresh that moves the
/// file *between* the two `anchor_for` calls below is a pre-existing risk
/// class this doesn't newly introduce (the single-line path always had the
/// same window between its read and its one `anchor_for` call).
fn save_comment(repo_root: &Path, state: &ComposeState) -> std::result::Result<Comment, String> {
    let file = state.target.file();
    let absolute = repo_root.join(file);
    let content =
        std::fs::read_to_string(&absolute).map_err(|e| format!("couldn't re-read {file}: {e}"))?;
    let lines: Vec<&str> = content.lines().collect();

    let (anchor, end_anchor) = match &state.target {
        app::CommentTarget::Single { line, .. } => {
            let anchor = comments::anchor_for(&lines, *line)
                .ok_or_else(|| format!("line {line} no longer exists in {file}"))?;
            (anchor, None)
        }
        app::CommentTarget::Range { start, end, .. } => {
            let start_anchor = comments::anchor_for(&lines, *start)
                .ok_or_else(|| format!("line {start} no longer exists in {file}"))?;
            let end_anchor = comments::anchor_for(&lines, *end)
                .ok_or_else(|| format!("line {end} no longer exists in {file}"))?;
            (start_anchor, Some(end_anchor))
        }
    };

    let comment = Comment {
        id: comments::generate_id(),
        created_at: comments::now_unix(),
        file: file.to_owned(),
        anchor,
        end_anchor,
        body: state.buffer().text(),
        status: comments::Status::Open,
        resolved_at: None,
    };
    CommentStore::new(repo_root)
        .append_comment(&comment)
        .map_err(|e| e.to_string())?;
    Ok(comment)
}

/// `z o`'s fallible half: reads the gap's file fresh off disk and hands the
/// content to [`App::expand_gap`], mapping every way that can fail into the
/// same status-bar string [`save_comment`] does for comments — a gating
/// check, a missing path, and a read error, each `?`'d/`ok_or_else`'d in a
/// flat line rather than nested inline in `handle_action`'s `ExpandFold`
/// arm the way this used to be (six levels of `match`/`if` deep, in the
/// same function `save_comment`/`finish_compose` already show the
/// alternative for). [`App::expand_gap`]'s own three-way [`app::ExpandOutcome`]
/// is passed through as `Ok` unchanged — only *this* function's own
/// disk-access failures become `Err`.
fn try_expand_fold(
    app: &mut App,
    file_idx: usize,
    gap_idx: usize,
) -> std::result::Result<app::ExpandOutcome, String> {
    if !app.disk_is_new_side {
        return Err("fold: expanding needs a working-tree diff".to_owned());
    }
    let display = app.files[file_idx].display_path().to_owned();
    // A deleted file has no new side to read and never has gaps in the
    // first place (see `file_gaps`), so this is defensive, not reachable
    // in practice.
    let rel = app.files[file_idx]
        .new_path
        .clone()
        .ok_or_else(|| format!("fold: can't read {display}"))?;
    let content = std::fs::read_to_string(app.repo_root.join(&rel))
        .map_err(|_| format!("fold: can't read {display}"))?;
    // Must match the parser's own split convention — see
    // `parse_unified_diff` — or a CRLF file's boundary rows would never
    // validate.
    let lines: Vec<&str> = content.lines().collect();
    Ok(app.expand_gap(file_idx, gap_idx, &lines))
}

/// The shared "not ready" half of `Action::Hover`/`GotoDefinition`/
/// `FindReferences` (issue #11): classifies the server backing `query`'s
/// target via [`classify_action_readiness`] and, for anything but `Ready`,
/// returns the status line to show *instead of* dispatching — kicking off
/// [`LspManager::ensure_started`] first when nothing has asked the server
/// to start yet. `None` means `Ready`: the caller should proceed with its
/// normal dispatch exactly as it did before this milestone.
///
/// A file with no configured adapter at all (`server_identity` returns
/// `None`) is deliberately left alone here, returning `None` just like
/// `Ready` does — that's the pre-existing "unsupported file type" path,
/// which already runs all the way through `LspManager::submit` itself (see
/// its `key_for`-`None` arm) and reports `EventOutcome::Unsupported`; this
/// is a readiness question about a server that *is* configured, not a
/// substitute for that path, so the caller's ordinary dispatch is left to
/// surface it exactly as it always has.
fn check_action_readiness(
    lsp_manager: &LspManager,
    query: &hover_popup::HoverQuery,
    action_name: &str,
) -> Option<String> {
    let (lang, _root) = lsp_manager.server_identity(&query.file, &query.git_root)?;
    let readiness = classify_action_readiness(lsp_manager.state(&query.file, &query.git_root));
    if readiness == ActionReadiness::NotStarted {
        lsp_manager.ensure_started(&query.file, &query.git_root);
    }
    readiness_status_message(readiness, &lang, action_name)
}

/// As [`check_action_readiness`], minus the [`LspManager::ensure_started`]
/// call — issue #23's context menu needs to *preview* what a Hover/
/// GotoDefinition/FindReferences entry would say right now, every frame it
/// sits open (req 5: the reason must flip live), without that preview
/// itself being the thing that kicks a not-started server off or leaves a
/// journal record behind. Starting the server and recording telemetry both
/// stay exclusively [`check_action_readiness`]'s job, reached only once the
/// reviewer actually confirms an entry (`ui::mod`'s context-menu
/// interception dispatches the real [`Action`] via the ordinary
/// `handle_action` arms — see that block's docs) — this is read-only,
/// side-effect-free preview, never the real dispatch path.
fn peek_action_readiness(
    lsp_manager: &LspManager,
    query: &hover_popup::HoverQuery,
    action_name: &str,
) -> Option<String> {
    let (lang, _root) = lsp_manager.server_identity(&query.file, &query.git_root)?;
    let readiness = classify_action_readiness(lsp_manager.state(&query.file, &query.git_root));
    readiness_status_message(readiness, &lang, action_name)
}

/// The status line for each [`ActionReadiness`] — split out of
/// [`check_action_readiness`] as a pure function so every message shape
/// (notably `Installing`'s live progress pass-through and `Unavailable`'s
/// verbatim reason) is testable without contriving a manager whose server
/// is actually mid-install or crashed, which no unit test can do reliably.
fn readiness_status_message(
    readiness: ActionReadiness,
    lang: &LangKey,
    action_name: &str,
) -> Option<String> {
    match readiness {
        ActionReadiness::Ready => None,
        // `NotStarted` and `Starting` read identically on purpose: by the
        // time the reviewer sees either message, `check_action_readiness`
        // has already kicked the not-started server off, so from their
        // seat both mean the same thing — it's coming up, retry shortly.
        ActionReadiness::NotStarted | ActionReadiness::Starting => Some(format!(
            "LSP: {lang} is starting; {action_name} is not ready yet"
        )),
        ActionReadiness::Installing { message } => Some(format!(
            "LSP: {message}; {action_name} unavailable until ready"
        )),
        ActionReadiness::Unavailable { reason } => Some(reason),
    }
}

/// The three [`peek_action_readiness`] calls a diff-row/file-view-row
/// context menu needs, grouped to match [`context_menu::SymbolReadiness`]'s
/// own shape — one call site for what would otherwise be three
/// near-identical lines at both of [`context_menu_entries_for`]'s callers
/// (the open flow and the every-frame refresh).
fn symbol_readiness(
    lsp_manager: &LspManager,
    query: &hover_popup::HoverQuery,
) -> context_menu::SymbolReadiness {
    context_menu::SymbolReadiness {
        hover: peek_action_readiness(lsp_manager, query, "hover"),
        goto_definition: peek_action_readiness(lsp_manager, query, "go to definition"),
        find_references: peek_action_readiness(lsp_manager, query, "find references"),
    }
}

/// Issue #24: whether passive pointer hover must stay suppressed this
/// frame — every overlay/prompt that already claims exclusive keyboard
/// input (compose, search, help, the scope menu, the units wizard, an open
/// refs/units panel, the context menu), an *explicit* hover already
/// `Pending`/`Shown` (see [`hover_popup::HoverState::blocks_passive`]'s
/// docs on why `Message` doesn't count), or an active visual selection
/// (`View::Diff` only — no other view has one). Checked at `Moved`-arm time
/// (load-bearing: this is what keeps a mouse resting over an open overlay
/// from ever arming a debounce in the first place) and again right before a
/// debounce fires (see that call site's own docs on why the second check is
/// pure defense-in-depth). `scope_menu`/`units_setup` are included even
/// though req 7 doesn't name them individually — both already block every
/// keystroke exactly like compose/search/help do, so a resting pointer
/// showing a popup *through* one would be the same bug wearing a different
/// hat.
#[allow(clippy::too_many_arguments)] // one bool per already-`Option`
// overlay this frame's suppression depends on, the same "no struct saves
// anything here" reasoning `event_loop`'s own signature docs give.
fn passive_hover_suppressed(
    compose: &Option<ComposeState>,
    search_prompt: &Option<search::SearchPromptState>,
    help: &Option<HelpState>,
    scope_menu: &Option<ScopeMenuState>,
    units_setup: &Option<UnitsSetupState>,
    refs_panel: &Option<RefsPanelState>,
    units_panel: &Option<UnitsPanel>,
    context_menu: &Option<ContextMenuState>,
    hover_state: &hover_popup::HoverState,
    view: &View,
) -> bool {
    compose.is_some()
        || search_prompt.is_some()
        || help.is_some()
        || scope_menu.is_some()
        || units_setup.is_some()
        || refs_panel.is_some()
        || units_panel.is_some()
        || context_menu.is_some()
        || hover_state.blocks_passive()
        || matches!(view, View::Diff(app) if app.visual_active())
}

/// Fires whatever a [`pointer_hover::PointerHoverState`] debounce's deadline
/// just made due — issue #24's analogue of `Action::Hover`'s own dispatch
/// (see that arm's docs), reached only after [`passive_hover_suppressed`]
/// has already said no. `Tree` is a synchronous local lookup (no LSP
/// involved at all — req 8) and goes straight to `Shown`, cancelling
/// silently if the row vanished from the tree between arming and firing
/// (a rebuild, a scroll past the end). `Code` is peek-only against
/// readiness (req 4): [`classify_action_readiness`] without ever calling
/// [`LspManager::ensure_started`] — a drifting mouse must not be what
/// launches a language server — so anything but `Ready` cancels silently,
/// with no journal entry at all (mirroring `check_action_readiness`'s
/// design, minus the start-the-server side effect `peek_action_readiness`
/// already omits for the same "preview, not dispatch" reason). A `Ready`
/// target dispatches exactly like `Action::Hover`: `Queued` recorded,
/// `set_pending`, the request submitted, and any previous passive request
/// still in flight recorded `Superseded` (structurally at most one, since
/// arming a new target already cancelled whatever came before — see
/// `PointerHoverState::arm`'s docs — this only catches the one case a
/// `Tree`→`Code` retarget or the reverse could leave stale: a `Pending`
/// request whose `Armed` predecessor already got superseded when it was
/// *armed*, not when it *fires*, so this is a second, cheap belt-and-
/// suspenders check, not the primary mechanism).
#[allow(clippy::too_many_arguments)]
fn fire_pointer_hover(
    target: &pointer_hover::PointerTarget,
    anchor_row: u16,
    pointer_hover: &mut pointer_hover::PointerHoverState,
    pending_pointer_hover: &mut Option<PendingHover>,
    lsp_manager: &LspManager,
    observer: &crate::lsp::ObservationHandle,
    diagnostics: &DiagnosticsStore,
    view: &View,
) {
    match target {
        pointer_hover::PointerTarget::Tree(id) => {
            let View::Diff(app) = view else {
                pointer_hover.cancel();
                return;
            };
            match pointer_hover::tree_note_for(app, id) {
                Some(text) => pointer_hover.show_tree_note(id.clone(), anchor_row, text),
                None => pointer_hover.cancel(),
            }
        }
        pointer_hover::PointerTarget::Code(query) => {
            let readiness =
                classify_action_readiness(lsp_manager.state(&query.file, &query.git_root));
            if readiness != ActionReadiness::Ready {
                pointer_hover.cancel();
                return;
            }
            let operation_id = observer.next_operation_id();
            observer.record_ui(
                Some(operation_id),
                crate::lsp::EventOutcome::Queued,
                "passive hover: resolving target",
            );
            let overlapping = diagnostics.diagnostics_on_line(&query.file, query.line);
            let diagnostics_prefix = hover_popup::diagnostics_section(&overlapping);
            pointer_hover.set_pending(target.clone(), anchor_row, diagnostics_prefix);
            let generation = pointer_hover.generation();
            let rx = lsp_manager.hover_with_operation(
                &query.file,
                &query.git_root,
                &query.line_text,
                query.line,
                query.display_col,
                operation_id,
            );
            if let Some((_, superseded_id, _)) = pending_pointer_hover.take() {
                observer.record_ui(
                    Some(superseded_id),
                    crate::lsp::EventOutcome::Superseded,
                    "passive hover request superseded by a newer passive hover",
                );
            }
            *pending_pointer_hover = Some((generation, operation_id, rx));
        }
    }
}

/// Cancels issue #24's passive pointer hover unconditionally — the shared
/// body of all four of req 5's cancellation hooks (any key press, any
/// non-`Moved` mouse event, a resize, a watch refresh), mirroring
/// [`cancel_pending_lsp_requests`]'s shape for the explicit hover/goto path:
/// drop to `Idle` (bumping generation, so a still-in-flight response is
/// discarded as stale on arrival) and, only if a `Code` request was actually
/// in flight, record one `Superseded` journal entry. An `Armed` debounce
/// that hadn't fired yet gets no journal entry at all — nothing was ever
/// queued for one to supersede.
fn cancel_pending_pointer_hover(
    pointer_hover: &mut pointer_hover::PointerHoverState,
    pending_pointer_hover: &mut Option<PendingHover>,
    observer: &crate::lsp::ObservationHandle,
) {
    pointer_hover.cancel();
    if let Some((_, operation_id, _)) = pending_pointer_hover.take() {
        observer.record_ui(
            Some(operation_id),
            crate::lsp::EventOutcome::Superseded,
            "passive hover request superseded",
        );
    }
}

/// Whether the diff cursor's current row is a [`RenderRow::Line`] — the
/// same eligibility check [`app::App::toggle_visual`] itself makes before
/// starting a fresh selection (see its docs), re-derived here rather than
/// exposed as its own `App` method: it's a one-line fact about already-`pub`
/// state (`app.rows`/`app.cursor`), not new `App` behavior, and the context
/// menu is its only caller outside `App` itself.
fn can_start_visual(app: &App) -> bool {
    matches!(app.rows.get(app.cursor), Some(RenderRow::Line { .. }))
}

/// Gathers the facts a [`context_menu::MenuTarget`] needs and derives its
/// entries — the one place `LspManager`/`App::comment_target`/`App::visual_active`
/// meet `context_menu`'s pure derivation functions (see that module's own
/// docs on why gathering facts stays out of it). Shared by
/// `mouse::handle_right_click`'s open flow and [`refresh_context_menu`]'s
/// every-frame re-derivation, so there is exactly one place that knows how
/// to turn a target into entries. Returns an empty `Vec` when `target`
/// doesn't match `view` at all (defensive: nothing pops or swaps the view
/// stack while the menu holds every key) or a `FileViewRow`/`DiffRow`
/// target's symbol has nothing left to target — both callers treat an empty
/// result as "close the menu," never "render nothing."
fn context_menu_entries_for(
    target: &MenuTarget,
    view: &View,
    lsp_manager: &LspManager,
) -> Vec<context_menu::MenuEntry> {
    match (target, view) {
        (MenuTarget::TreeDir { path }, View::Diff(app)) => {
            let expanded = app.visible_rows.iter().any(|row| {
                row.id.is_directory
                    && &row.id.path == path
                    && matches!(
                        row.kind,
                        file_tree::VisibleKind::Directory { expanded: true, .. }
                    )
            });
            context_menu::tree_dir_entries(expanded, app.descendant_dir_count(path))
        }
        (MenuTarget::TreeFile, View::Diff(_)) => context_menu::tree_file_entries(),
        (MenuTarget::DiffRow, View::Diff(app)) => {
            let symbol = view
                .hover_query()
                .map(|query| symbol_readiness(lsp_manager, &query));
            context_menu::diff_row_entries(
                symbol,
                app.comment_target(),
                app.visual_active(),
                can_start_visual(app),
            )
        }
        (MenuTarget::FileViewRow, View::File(_)) => match view.hover_query() {
            Some(query) => {
                context_menu::file_view_symbol_entries(symbol_readiness(lsp_manager, &query))
            }
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Re-derives the open context menu's entries against the current frame's
/// facts (req 5) — called once per event-loop iteration, before `draw`, so
/// both the frame about to render and whatever key/click this same
/// iteration processes afterward see the same, freshly-peeked readiness.
/// Closes the menu outright once re-derivation comes back empty (the
/// target stopped making sense — see [`context_menu_entries_for`]'s docs)
/// rather than rendering an empty popup [`ContextMenuState::set_entries`]
/// would panic on.
fn refresh_context_menu(
    context_menu: &mut Option<ContextMenuState>,
    view: &View,
    lsp_manager: &LspManager,
) {
    let Some(target) = context_menu.as_ref().map(|menu| menu.target.clone()) else {
        return;
    };
    let entries = context_menu_entries_for(&target, view, lsp_manager);
    if entries.is_empty() {
        *context_menu = None;
    } else if let Some(menu) = context_menu.as_mut() {
        menu.set_entries(entries);
    }
}

/// Drops whichever definition/references request is still in flight,
/// recording it `Superseded` in the journal. Every explicit gd/gr press
/// must do this — dispatching or not: `pending_goto` has no generation
/// check the way `pending_hover` does (taking the receiver *is* the
/// invalidation), so a not-ready press that merely set `goto_status` and
/// left the old receiver alive would let that stale response resolve
/// later, navigate the view, and overwrite the very "not ready" message
/// the reviewer just read.
fn supersede_pending_goto(
    pending_goto: &mut Option<(JumpEntry, PendingGoto)>,
    observer: &crate::lsp::ObservationHandle,
    reason: &str,
) {
    if let Some((_, pending)) = pending_goto.take() {
        let superseded_id = match pending {
            PendingGoto::Definition { operation_id, .. }
            | PendingGoto::References { operation_id, .. } => operation_id,
        };
        observer.record_ui(
            Some(superseded_id),
            crate::lsp::EventOutcome::Superseded,
            reason,
        );
    }
}

/// The status-bar note for a diff-content `action` pressed while the files
/// pane has focus (issue #14) — `None` for anything that either already
/// makes sense in `Files` focus (movement, `Confirm`, pane toggles) or is
/// handled by a routing change inside `App::update` itself (hunk/file/
/// symbol navigation quietly no-op there instead of erroring — see
/// `App::update`'s `MainPaneFocus::Files` arms). Every variant this matches
/// needs a cursor position *inside the diff* to act on: a hover/definition/
/// references target, a fold to expand/collapse, a comment anchor, or a
/// search/diagnostic row to jump to — none of which `files_selection`
/// provides. Named per-action rather than one generic "focus the diff pane
/// first" so the message still says *what* is unavailable, matching this
/// codebase's own rule that status notes name the action (see the roadmap
/// issue's cross-cutting acceptance rules).
fn files_focus_blocked_message(action: Action) -> Option<&'static str> {
    Some(match action {
        Action::Hover => "hover: focus the diff pane first",
        Action::GotoDefinition => "definition: focus the diff pane first",
        Action::FindReferences => "references: focus the diff pane first",
        Action::AddComment => "comment: focus the diff pane first",
        Action::ExpandFold | Action::CollapseFold => "fold: focus the diff pane first",
        Action::OpenSearch | Action::NextMatch | Action::PrevMatch => {
            "search: focus the diff pane first"
        }
        Action::NextDiagnostic | Action::PrevDiagnostic => "diagnostic: focus the diff pane first",
        Action::ToggleVisualLine => "visual: focus the diff pane first",
        Action::YankSelection => "yank: focus the diff pane first",
        _ => return None,
    })
}

/// Applies one matched [`Action`], handling every concern the pure
/// `App`/`FileView::update` methods can't: issuing LSP requests
/// (`Hover`/`GotoDefinition`/`FindReferences`), navigating on their
/// responses, retracing the jump history, and letting an open overlay (a
/// hover popup or the references panel) intercept `j`/`k`/Enter/Esc before
/// they'd otherwise move the cursor or reopen something. Every action that
/// isn't intercepted by one of those goes through the view's own `update`,
/// with the hover popup invalidated afterward if the action actually
/// changed what's under the cursor (compared via [`View::hover_cursor_key`],
/// not by hardcoding which actions move the cursor).
#[allow(clippy::too_many_arguments)] // this is `ui::mod`'s one dispatch point
// for every concern the render loop owns; splitting the signature into a
// struct would just move the same fields one level down without reducing
// how many things a jump/hover/panel action actually needs to touch.
fn handle_action(
    action: Action,
    stack: &mut ViewStack,
    hover_state: &mut hover_popup::HoverState,
    pending_hover: &mut Option<PendingHover>,
    pending_goto: &mut Option<(JumpEntry, PendingGoto)>,
    refs_panel: &mut Option<RefsPanelState>,
    units_panel: &mut Option<UnitsPanel>,
    units_setup: &mut Option<UnitsSetupState>,
    pending_units: &mut Option<Receiver<Result<crate::groups::Grouping, String>>>,
    units_status: &mut Option<String>,
    jump_stack: &mut JumpStack,
    diagnostics: &DiagnosticsStore,
    goto_status: &mut Option<String>,
    lsp_manager: &LspManager,
    jj_repo: Option<&JjRepo>,
    compose: &mut Option<ComposeState>,
    scope_menu: &mut Option<ScopeMenuState>,
    pr_scope: &mut PrScopeFetch,
    context_menu: &mut Option<ContextMenuState>,
    help: &mut Option<HelpState>,
    search_prompt: &mut Option<search::SearchPromptState>,
    highlighter: &mut LineHighlighter,
    watch_paused: &mut bool,
    watch_active: bool,
    watch_status: &mut Option<WatchStatus>,
    moving_scope: &mut Option<MovingScopeState>,
    hints_expanded: &mut bool,
    observer: &crate::lsp::ObservationHandle,
) {
    // The inspector is a full-screen modal view.  Route every action to it
    // before hover/references/scope overlays can consume Esc or navigation
    // keys, so nothing beneath it can change while the user is inspecting.
    // `OpenHelp`/`YankSelection` are the two exceptions: help mutates
    // nothing beneath the inspector and its "opens from any view" contract
    // (see the arm below) would be silently broken by the catch-all
    // `_ => {}` in the inspector's own `update`; `YankSelection` (issue
    // #17) needs its own special case just below instead, since — unlike
    // every other action `update` handles — copying is I/O (`write_osc52`)
    // that `update`'s `()` return type has nowhere to report the outcome
    // of, the same reason `ui::mod` (not `App::update`) owns the main
    // diff's own `YankSelection` arm further down.
    if !matches!(action, Action::OpenHelp | Action::YankSelection)
        && let View::LspInspector(inspector) = stack.top_mut()
    {
        inspector.update(action);
        return;
    }

    // Issue #17: the inspector's Journal-focus `y` — formerly a raw-key
    // bypass (`LspInspectorView::handle_literal_key`), now this action's
    // one inspector-specific special case. `copy_selection` does its own
    // Journal-focus/empty-selection/bounds checks and sets its own status
    // message on every `None` outcome, so there is nothing left to report
    // here beyond turning a `Some` payload into the actual OSC 52 write —
    // byte-for-byte the same status wording `handle_literal_key` used to
    // produce.
    if action == Action::YankSelection
        && let View::LspInspector(inspector) = stack.top_mut()
    {
        if let Some(payload) = inspector.copy_selection() {
            let status = match clipboard::write_osc52(&payload.text) {
                Ok(()) => format!(
                    "sent {} journal record{} ({} bytes) via OSC 52; terminal support required",
                    payload.record_count,
                    if payload.record_count == 1 { "" } else { "s" },
                    payload.byte_count,
                ),
                Err(error) => format!("journal copy failed: {error}"),
            };
            inspector.set_copy_status(status);
        }
        return;
    }

    // Issue #23's context menu, intercepted ahead of every other overlay
    // below (the menu only ever opens once `mouse::handle_right_click`'s
    // open flow has already closed anything it would otherwise overlap —
    // see that function's docs — so this and, say, `scope_menu`'s own
    // interception just below are never both reachable for the same
    // action; this one still goes first, structurally, so a future overlay
    // added between the two could never change that by accident).
    // Deliberately built entirely out of `Action` arms, never a raw-key
    // bypass the way `compose`/the revision input are (see `run`'s event
    // loop): the resolver's own global-quit intercept
    // (`StepResult::Matched(Action::Quit) => return Ok(())`, in `run`,
    // *before* `handle_action` is ever called) sits structurally above
    // this function entirely, so `q` still quits the whole session while
    // this menu is open, exactly like every other overlay here — a raw-key
    // bypass would have hidden `q` from the resolver and broken that.
    if let Some(menu) = context_menu {
        match action {
            Action::CursorDown => return menu.move_down(),
            Action::CursorUp => return menu.move_up(),
            Action::Top => return menu.move_top(),
            Action::Bottom => return menu.move_bottom(),
            Action::Cancel => {
                *context_menu = None;
                return;
            }
            Action::Confirm => {
                let entry = menu.selected_entry().clone();
                match entry.enabled {
                    // Disabled entries teach, never invoke (issue #23 req
                    // 5) — the menu stays open so the reviewer can read the
                    // reason and pick something else, the same "stay open
                    // on a rejected choice" shape `scope_menu`'s own
                    // `Revision` input uses for a bad revset.
                    Err(reason) => {
                        *goto_status = Some(reason);
                        return;
                    }
                    Ok(()) => {
                        let target = menu.target.clone();
                        // Closed *before* dispatching, not after: every
                        // command below either recurses into this very
                        // function (whose own top-of-function check just
                        // above would otherwise see the menu as still
                        // open) or mutates the tree directly — neither
                        // should have to know or care that a menu was ever
                        // involved.
                        *context_menu = None;
                        match entry.command {
                            // Req 8 by construction: every entry but the
                            // two descendant-bulk ones below dispatches
                            // through this exact recursive call into the
                            // ordinary, unmodified `Action` arms further
                            // down this same function — readiness checks,
                            // `ensure_started`, observer telemetry, and all
                            // — never a second implementation of what the
                            // keyboard binding already does.
                            MenuCommand::Action(inner) => handle_action(
                                inner,
                                stack,
                                hover_state,
                                pending_hover,
                                pending_goto,
                                refs_panel,
                                units_panel,
                                units_setup,
                                pending_units,
                                units_status,
                                jump_stack,
                                diagnostics,
                                goto_status,
                                lsp_manager,
                                jj_repo,
                                compose,
                                scope_menu,
                                pr_scope,
                                context_menu,
                                help,
                                search_prompt,
                                highlighter,
                                watch_paused,
                                watch_active,
                                watch_status,
                                moving_scope,
                                hints_expanded,
                                observer,
                            ),
                            MenuCommand::ExpandAllDescendants
                            | MenuCommand::CollapseAllDescendants => {
                                if let (MenuTarget::TreeDir { path }, View::Diff(app)) =
                                    (&target, stack.top_mut())
                                {
                                    app.set_descendants_collapsed(
                                        path,
                                        entry.command == MenuCommand::CollapseAllDescendants,
                                    );
                                }
                            }
                        }
                    }
                }
                return;
            }
            // Any other key closes the menu, then falls through — same
            // precedent as `scope_menu`'s own wildcard arm just below.
            _ => *context_menu = None,
        }
    }

    // The scope-menu popup's *list* half intercepts its own navigation
    // keys the same way `refs_panel`/`hover_state` do below — `Revision`
    // input never reaches here at all (see `run`'s event loop: it bypasses
    // the keymap resolver entirely, the same way `compose` does), so only
    // `ScopeMenuState::List` ever needs handling in this dispatch point.
    if let Some(ScopeMenuState::List(list)) = scope_menu {
        match action {
            Action::CursorDown => return list.move_down(),
            Action::CursorUp => return list.move_up(),
            Action::Cancel | Action::OpenScopeMenu => {
                *scope_menu = None;
                return;
            }
            Action::Confirm => {
                let entry = list.selected_entry();
                match entry {
                    ScopeMenuEntry::Log => {
                        *scope_menu = None;
                        open_or_close_log(stack, goto_status);
                    }
                    ScopeMenuEntry::Timeline => {
                        *scope_menu = None;
                        open_or_close_timeline(stack, jj_repo, goto_status);
                    }
                    ScopeMenuEntry::Revision => {
                        *scope_menu = Some(ScopeMenuState::new_revision_input());
                    }
                    ScopeMenuEntry::PullRequest => {
                        *scope_menu = Some(ScopeMenuState::new_pr_input());
                    }
                    ScopeMenuEntry::WorkingTree | ScopeMenuEntry::Staged => {
                        let choice = if entry == ScopeMenuEntry::WorkingTree {
                            ScopeChoice::WorkingTree
                        } else {
                            ScopeChoice::Staged
                        };
                        let at_root = stack.is_at_root();
                        if let View::Diff(app) = stack.top_mut() {
                            match apply_scope_swap(
                                app,
                                &choice,
                                at_root,
                                watch_active,
                                jj_repo,
                                lsp_manager,
                                highlighter,
                                hover_state,
                                refs_panel,
                                context_menu,
                                watch_paused,
                                watch_status,
                                moving_scope,
                            ) {
                                Ok(()) => {
                                    *scope_menu = None;
                                    // A completed swap supersedes any PR
                                    // fetch still in flight — without this,
                                    // its late arrival would overwrite the
                                    // scope the reviewer picked more
                                    // recently.
                                    pr_scope.clear();
                                }
                                Err(e) => *goto_status = Some(format!("scope: {e}")),
                            }
                        } else {
                            *scope_menu = None; // defensive; see the Revision-input arm's identical comment in `run`
                        }
                    }
                }
                return;
            }
            _ => *scope_menu = None, // any other key closes the menu, then falls through
        }
    }

    if let Some(state) = refs_panel {
        match action {
            Action::CursorDown => return state.panel.select_next(),
            Action::CursorUp => return state.panel.select_prev(),
            Action::Cancel => {
                *refs_panel = None;
                return;
            }
            Action::Confirm => {
                if let Some(entry) = state.panel.selected_entry() {
                    let target = JumpEntry {
                        file: entry.file.clone(),
                        git_root: state.git_root.clone(),
                        line: Some(entry.line),
                        col: entry.match_range.0,
                    };
                    let from = stack.top().jump_entry();
                    let panel_title = state.panel.title.clone();
                    match navigate_to(stack, jump_stack, lsp_manager, from, target, true) {
                        Ok(()) => observer.record_ui(
                            Some(state.operation_id),
                            crate::lsp::EventOutcome::Result,
                            format!("{panel_title} navigation succeeded"),
                        ),
                        Err(error) => observer.record_ui(
                            Some(state.operation_id),
                            crate::lsp::EventOutcome::NavigationFailure,
                            format!("{panel_title} navigation failed: {error}"),
                        ),
                    }
                    hover_state.invalidate();
                }
                *refs_panel = None;
                return;
            }
            _ => *refs_panel = None, // any other key closes the panel, then falls through
        }
    }

    // The first-use units picker is fully modal (it exists to stop a
    // surprise CLI spawn, so no key may fall through past it to something
    // that could spawn one) — intercepted ahead of every other overlay.
    if let Some(setup) = units_setup {
        match action {
            Action::CursorDown => setup.move_down(),
            Action::CursorUp => setup.move_up(),
            Action::Confirm => match setup.confirm() {
                SetupOutcome::Continue => {}
                SetupOutcome::Done(selections) => {
                    *units_setup = None;
                    if let View::Diff(app) = stack.top_mut() {
                        selections.apply(&mut app.units_config);
                        app.units_prompt_needed = false;
                        // Session config is already updated above, so a
                        // failed write degrades to "asks again next
                        // session", not a broken feature — surface it and
                        // move on to the grouping the user actually asked
                        // for.
                        if let Err(e) =
                            crate::config::append_to_home_config(&selections.toml_section())
                        {
                            *goto_status = Some(format!("units: config not saved: {e}"));
                        }
                        spawn_units_generation(app, pending_units, units_status);
                    }
                }
            },
            // Esc — or any other key — abandons the wizard without
            // spawning anything and without persisting; the next `u`
            // simply asks again.
            _ => *units_setup = None,
        }
        return;
    }

    if units_panel.is_some() {
        match action {
            Action::CursorDown => {
                if let Some(panel) = units_panel {
                    panel.select_next();
                }
                return;
            }
            Action::CursorUp => {
                if let Some(panel) = units_panel {
                    panel.select_prev();
                }
                return;
            }
            Action::Cancel | Action::ToggleUnits => {
                *units_panel = None;
                return;
            }
            Action::Confirm => {
                // Scope the diff itself to the selected unit — the
                // stacked-PR reading: the unit *becomes* the diff (rows,
                // sidebar, search all narrow with it) until `Esc` widens
                // back, rather than merely jumping the cursor into the
                // full diff and leaving the reviewer to guess where the
                // unit ends.
                let filter = units_panel.as_ref().and_then(|panel| {
                    let entry = panel.selected_entry()?;
                    (entry.hunk_count > 0).then(|| app::UnitFilter {
                        label: entry.label.clone(),
                        description: entry.description.clone(),
                        index: panel.selected + 1,
                        total: panel.entries.len(),
                        hunk_ids: entry.hunk_ids.iter().cloned().collect(),
                    })
                });
                *units_panel = None;
                match filter {
                    Some(filter) => {
                        if let View::Diff(app) = stack.top_mut() {
                            app.set_unit_filter(filter);
                            hover_state.invalidate();
                        }
                    }
                    None => {
                        *goto_status = Some("units: no hunks in this unit anymore".to_owned());
                    }
                }
                return;
            }
            _ => *units_panel = None, // any other key closes the panel, then falls through
        }
    }

    if hover_state.is_open() {
        match action {
            Action::CursorDown => return hover_state.scroll_down(),
            Action::CursorUp => return hover_state.scroll_up(),
            Action::Hover | Action::Cancel => return hover_state.close(),
            _ => hover_state.close(),
        }
    }

    // Issue #14: a diff-content action (hover, go-to-definition, a fold
    // toggle, search, ...) needs a cursor position *inside the diff* to act
    // on — exactly what the files pane's own independent `files_selection`
    // doesn't provide. Gated here, ahead of the main dispatch below, rather
    // than inside `App::update` (which has no way to report a status note)
    // or piecemeal in each arm (which would mean re-deriving "is Files
    // focused" eleven separate times). Movement/paging, `Confirm`,
    // `Cancel`, pane-focus/view toggles, and `OpenHelp`/`Quit` all pass
    // through unblocked — see `files_focus_blocked_message`'s docs for the
    // exact boundary.
    if let View::Diff(app) = stack.top()
        && app.focus == app::MainPaneFocus::Files
        && let Some(message) = files_focus_blocked_message(action)
    {
        *goto_status = Some(message.to_owned());
        return;
    }

    match action {
        Action::Hover => match stack.top().hover_query() {
            Some(query) => {
                if let Some(message) = check_action_readiness(lsp_manager, &query, "hover") {
                    // Not `Queued`: nothing was queued for this press to
                    // ever answer — see `EventOutcome::NotReady`'s docs.
                    observer.record_ui(
                        Some(observer.next_operation_id()),
                        crate::lsp::EventOutcome::NotReady,
                        "hover action: server not ready",
                    );
                    hover_state.invalidate();
                    // Through `goto_status`, not `hover_state.set_message`:
                    // `status_hint` prefixes popup messages with "hover: ",
                    // which would render this line as "hover: LSP: …; hover
                    // is not ready yet". The readiness message already names
                    // the action, so it uses the same unprefixed sink gd/gr
                    // use.
                    *goto_status = Some(message);
                } else {
                    let operation_id = observer.next_operation_id();
                    observer.record_ui(
                        Some(operation_id),
                        crate::lsp::EventOutcome::Queued,
                        "hover action: resolving target",
                    );
                    hover_state.invalidate();
                    let overlapping = diagnostics.diagnostics_on_line(&query.file, query.line);
                    hover_state.set_diagnostics_prefix(&overlapping);
                    hover_state.set_pending();
                    let generation = hover_state.generation();
                    let rx = lsp_manager.hover_with_operation(
                        &query.file,
                        &query.git_root,
                        &query.line_text,
                        query.line,
                        query.display_col,
                        operation_id,
                    );
                    if let Some((_, superseded_id, _)) = pending_hover.take() {
                        observer.record_ui(
                            Some(superseded_id),
                            crate::lsp::EventOutcome::Superseded,
                            "hover request superseded by a newer hover action",
                        );
                    }
                    *pending_hover = Some((generation, operation_id, rx));
                }
            }
            None => {
                observer.record_ui(
                    Some(observer.next_operation_id()),
                    crate::lsp::EventOutcome::NoResult,
                    "hover action: no target under cursor",
                );
                hover_state.set_message("nothing to hover here");
            }
        },
        Action::GotoDefinition => match stack.top().hover_query() {
            Some(query) => {
                if let Some(message) =
                    check_action_readiness(lsp_manager, &query, "go to definition")
                {
                    observer.record_ui(
                        Some(observer.next_operation_id()),
                        crate::lsp::EventOutcome::NotReady,
                        "definition action: server not ready",
                    );
                    supersede_pending_goto(
                        pending_goto,
                        observer,
                        "navigation request superseded by a newer definition action",
                    );
                    *goto_status = Some(message);
                } else {
                    let operation_id = observer.next_operation_id();
                    observer.record_ui(
                        Some(operation_id),
                        crate::lsp::EventOutcome::Queued,
                        "definition action: resolving target",
                    );
                    let rx = lsp_manager.definition_with_operation(
                        &query.file,
                        &query.git_root,
                        &query.line_text,
                        query.line,
                        query.display_col,
                        operation_id,
                    );
                    supersede_pending_goto(
                        pending_goto,
                        observer,
                        "navigation request superseded by a newer definition action",
                    );
                    *pending_goto = Some((
                        JumpEntry::from(&query),
                        PendingGoto::Definition { operation_id, rx },
                    ));
                    *goto_status = Some("goto: \u{2026}".to_owned());
                }
            }
            None => {
                observer.record_ui(
                    Some(observer.next_operation_id()),
                    crate::lsp::EventOutcome::NoResult,
                    "definition action: no target under cursor",
                );
                *goto_status = Some("goto: nothing to jump from here".to_owned());
            }
        },
        Action::FindReferences => match stack.top().hover_query() {
            Some(query) => {
                if let Some(message) =
                    check_action_readiness(lsp_manager, &query, "find references")
                {
                    observer.record_ui(
                        Some(observer.next_operation_id()),
                        crate::lsp::EventOutcome::NotReady,
                        "references action: server not ready",
                    );
                    supersede_pending_goto(
                        pending_goto,
                        observer,
                        "navigation request superseded by a newer references action",
                    );
                    *goto_status = Some(message);
                } else {
                    let operation_id = observer.next_operation_id();
                    observer.record_ui(
                        Some(operation_id),
                        crate::lsp::EventOutcome::Queued,
                        "references action: resolving target",
                    );
                    let rx = lsp_manager.references_with_operation(
                        &query.file,
                        &query.git_root,
                        &query.line_text,
                        query.line,
                        query.display_col,
                        operation_id,
                    );
                    supersede_pending_goto(
                        pending_goto,
                        observer,
                        "navigation request superseded by a newer references action",
                    );
                    *pending_goto = Some((
                        JumpEntry::from(&query),
                        PendingGoto::References { operation_id, rx },
                    ));
                    *goto_status = Some("references: \u{2026}".to_owned());
                }
            }
            None => {
                observer.record_ui(
                    Some(observer.next_operation_id()),
                    crate::lsp::EventOutcome::NoResult,
                    "references action: no target under cursor",
                );
                *goto_status = Some("references: nothing to jump from here".to_owned());
            }
        },
        Action::AddComment => match stack.top() {
            View::Diff(app) => match app.comment_target() {
                Ok(target) => *compose = Some(ComposeState::new(target)),
                // Never touches visual state — a rejected range leaves the
                // selection exactly as it was, so the reviewer can fix the
                // status-bar-reported problem (or just try `c` again) without
                // having to reselect (req 3).
                Err(reason) => *goto_status = Some(reason.message().to_owned()),
            },
            View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => {
                *goto_status = Some("comment: only available in the diff view".to_owned());
            }
        },
        Action::ExpandFold => match stack.top_mut() {
            View::Diff(app) => match app.rows.get(app.cursor) {
                Some(RenderRow::Gap { file_idx, gap_idx }) => {
                    let (file_idx, gap_idx) = (*file_idx, *gap_idx);
                    *goto_status = match try_expand_fold(app, file_idx, gap_idx) {
                        Ok(app::ExpandOutcome::Revealed) => None,
                        Ok(app::ExpandOutcome::ProbedEmpty) => {
                            Some("fold: already at end of file".to_owned())
                        }
                        Ok(app::ExpandOutcome::Rejected) => {
                            Some("fold: diff is stale here".to_owned())
                        }
                        Err(e) => Some(e),
                    };
                }
                _ => *goto_status = Some("fold: nothing to expand here".to_owned()),
            },
            View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => {
                *goto_status = Some("fold: only available in the diff view".to_owned());
            }
        },
        Action::CollapseFold => match stack.top_mut() {
            View::Diff(app) => {
                if !app.collapse_fold_at_cursor() {
                    *goto_status = Some("fold: nothing to collapse here".to_owned());
                }
            }
            View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => {
                *goto_status = Some("fold: only available in the diff view".to_owned());
            }
        },
        // Issue #16: `V`. `App::toggle_visual` is the real, pure state
        // transition (see its docs) — this arm exists only to turn its
        // three-way `VisualToggleOutcome` into the status-bar note
        // `App::update`'s `()` return type has no room for, the same
        // reason `ExpandFold`/`CollapseFold` above call their real `App`
        // method directly rather than routing through `update`. `y` is
        // named in the `Started` note ahead of `c` (#19) actually landing
        // it — a reviewer who presses `V` today sees the whole intended
        // workflow, not just the two-thirds of it issues #16/#17 implement.
        Action::ToggleVisualLine => match stack.top_mut() {
            View::Diff(app) => {
                *goto_status = match app.toggle_visual() {
                    app::VisualToggleOutcome::Started => {
                        Some("visual: j/k extend · y copy · c comment · Esc cancel".to_owned())
                    }
                    app::VisualToggleOutcome::Cancelled => {
                        Some("visual selection cancelled".to_owned())
                    }
                    app::VisualToggleOutcome::NotSelectable => {
                        Some("visual: no selectable source line here".to_owned())
                    }
                };
            }
            View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => {
                *goto_status = Some("visual: only available in the diff view".to_owned());
            }
        },
        // Issue #17: `y`. Requires an active selection first (req 3) —
        // everything else here only makes sense once `App::selected_rows`
        // has something in it. `clipboard::resolve_selection` maps those
        // indices back to real diff content while `app.rows`/`app.files`
        // are still borrowed; `clipboard::format_diff_selection` then
        // *consumes* the resulting `Vec<SelectedLine>` (see its own docs
        // on why), which ends that borrow before `App::cancel_visual`
        // needs `&mut app` again on a successful write below.
        Action::YankSelection => match stack.top_mut() {
            View::Diff(app) => {
                if !app.visual_active() {
                    *goto_status = Some("yank: press V to select lines first".to_owned());
                } else {
                    let selected = app.selected_rows();
                    let lines = clipboard::resolve_selection(&app.rows, &app.files, &selected);
                    *goto_status = Some(match clipboard::format_diff_selection(lines) {
                        Err(clipboard::YankError::Empty) => {
                            "yank: selection has no diff lines to copy".to_owned()
                        }
                        Err(clipboard::YankError::TooLarge { byte_count }) => format!(
                            "yank selection is {byte_count} bytes; copy limit is {}",
                            clipboard::OSC52_MAX_BYTES
                        ),
                        Ok(formatted) => match clipboard::write_osc52(&formatted.text) {
                            Ok(()) => {
                                app.cancel_visual();
                                format!(
                                    "yanked {} line(s) across {} file(s) ({} bytes) via OSC 52; terminal support required",
                                    formatted.line_count,
                                    formatted.file_count,
                                    formatted.byte_count
                                )
                            }
                            // Keep the selection on failure (req 8): nothing
                            // was actually copied, so there's nothing to
                            // clear — the reviewer can retry or trim it.
                            Err(error) => format!("yank failed: {error}"),
                        },
                    });
                }
            }
            View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => {
                *goto_status = Some("yank: only available in the diff view".to_owned());
            }
        },
        // Opens the prompt (see `search::SearchPromptState`'s docs); the
        // origin `Anchor` is captured *here*, from the cursor/scroll `/`
        // was actually pressed at, rather than inside `App` itself — `App`
        // has no notion of "a prompt is opening," only of the confirmed
        // search a closed one leaves behind (see `App::search`'s docs).
        // No pending-LSP-request cancellation the way `OpenHelp` does
        // (search doesn't obscure the view the way a modal popup does), but
        // an open hover popup still closes first: this arm sits *after*
        // the `hover_state.is_open()` interception above, whose wildcard
        // arm closes-then-falls-through for any action it doesn't
        // specifically handle — see that block's docs.
        Action::OpenSearch => {
            let jump_from = stack.top().jump_entry();
            match stack.top_mut() {
                View::Diff(app) => {
                    let origin = refresh::capture_anchor(
                        &app.files,
                        &app.rows,
                        app.cursor,
                        app.scroll_offset,
                    );
                    *search_prompt = Some(search::SearchPromptState {
                        input: search::SearchInput::new(),
                        origin,
                        jump_from,
                    });
                }
                View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => {
                    *goto_status = Some("search: only available in the diff view".to_owned());
                }
            }
        }
        // `n`/`N`: real navigation lives here (an `App` method), not in
        // `App::update` — see `App::next_match`'s docs on why keeping the
        // logic out of `App::update` matters even though this arm already
        // gates on `View::Diff` on its own (defense in depth: it also keeps
        // `TimelineView`'s nested `diff_app.update(action)` fallthrough,
        // see `timeline_view::TimelineView::update`, from ever running real
        // search navigation against its embedded diff pane). `from`/`to`
        // bracket the whole step — including the no-`View::Diff`/no-active-
        // search/zero-match cases, where the cursor provably didn't move
        // and `from == to` suppresses the record on its own (see
        // `record_jump`'s docs) — so every successful `n`/`N` counts as a
        // significant jump (decision 2 of the roadmap issue) without a
        // separate "did this actually move" check here.
        Action::NextMatch | Action::PrevMatch => {
            let from = stack.top().jump_entry();
            match stack.top_mut() {
                View::Diff(app) => {
                    let before = (app.cursor, app.active_symbol);
                    let result = if action == Action::NextMatch {
                        app.next_match()
                    } else {
                        app.prev_match()
                    };
                    *goto_status = match result {
                        Some(true) => Some("search wrapped".to_owned()),
                        Some(false) => None,
                        None => Some(app.search_status_note()),
                    };
                    if (app.cursor, app.active_symbol) != before {
                        hover_state.invalidate();
                    }
                }
                View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => {
                    *goto_status = Some("search: only available in the diff view".to_owned());
                }
            }
            record_jump(jump_stack, from, stack.top().jump_entry());
        }
        // `]d`/`[d`: same `from`/`to` bracketing as `n`/`N` above, for the
        // same reason — no diagnostic to jump to leaves the cursor exactly
        // where it was, so `record_jump`'s equality check is what actually
        // decides "was this significant," not a bespoke check here.
        Action::NextDiagnostic | Action::PrevDiagnostic => {
            let forward = action == Action::NextDiagnostic;
            let before = stack.top().hover_cursor_key();
            let from = stack.top().jump_entry();
            stack.top_mut().jump_to_diagnostic(diagnostics, forward);
            if stack.top().hover_cursor_key() != before {
                hover_state.invalidate();
            }
            record_jump(jump_stack, from, stack.top().jump_entry());
        }
        Action::JumpBack => {
            let current = stack.top().jump_entry();
            match jump_stack.back(current) {
                Some(target) => {
                    let _ = navigate_to(stack, jump_stack, lsp_manager, None, target, false);
                    hover_state.invalidate();
                }
                None => *goto_status = Some("jump: no earlier position".to_owned()),
            }
        }
        Action::JumpForward => {
            let current = stack.top().jump_entry();
            match jump_stack.forward(current) {
                Some(target) => {
                    let _ = navigate_to(stack, jump_stack, lsp_manager, None, target, false);
                    hover_state.invalidate();
                }
                None => *goto_status = Some("jump: no later position".to_owned()),
            }
        }
        // With nothing else open to cancel (hover/the refs panel/the
        // scope-menu list/the units picker/the LSP inspector already
        // intercepted `Cancel` above if any of those *were* open — reaching
        // here means none was), `Esc`'s one remaining job is generic view
        // unwinding — see `cancel_diff_view`'s docs for the precedence this
        // implements. This is a different `Esc` from the search prompt's
        // own (cancel-and-restore-the-cursor, handled entirely inside
        // `run`'s raw-key bypass arm before an `Action` is ever resolved)
        // — this one only ever fires once the prompt has already closed.
        Action::Cancel => cancel_diff_view(
            stack,
            hover_state,
            pending_hover,
            pending_goto,
            observer,
            goto_status,
        ),
        Action::ToggleTimeline => open_or_close_timeline(stack, jj_repo, goto_status),
        Action::ToggleLogView => open_or_close_log(stack, goto_status),
        Action::ToggleLspInspector => {
            cancel_pending_lsp_requests(hover_state, pending_hover, pending_goto, observer);
            open_or_close_lsp_inspector(stack, observer, lsp_manager);
        }
        // Pure chrome: works identically in every view, which is why it
        // lives on the event loop rather than any `View::update`.
        Action::ToggleHints => *hints_expanded = !*hints_expanded,
        // Open (reaching here means the panel isn't open — the
        // interception block above closes it otherwise): a cached grouping
        // for this exact diff shows instantly; a miss spawns the agent CLI
        // on a background thread whose result the frame preamble picks up.
        Action::ToggleUnits => match stack.top() {
            View::Diff(app) => {
                if pending_units.is_some() {
                    *goto_status = Some("units: grouping already running".to_owned());
                } else if let Some(grouping) =
                    crate::groups::cached(&app.repo_root, app.full_files())
                {
                    let mut panel = UnitsPanel::build(&grouping, app.full_files());
                    // Reopening the panel mid-scope starts on the unit
                    // currently being read — the natural position to step
                    // to the next/previous unit from.
                    if let Some(filter) = app.unit_filter() {
                        panel.selected =
                            (filter.index - 1).min(panel.entries.len().saturating_sub(1));
                    }
                    *units_panel = Some(panel);
                } else if app.units_prompt_needed {
                    open_units_setup(units_setup, goto_status);
                } else {
                    spawn_units_generation(app, pending_units, units_status);
                }
            }
            View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => {
                *goto_status = Some("units: only available in the diff view".to_owned());
            }
        },
        // A fresh take on the same diff — the cache-first `u` can never
        // provide one (an unchanged diff always hits its cache), so this
        // is the deliberate second key rather than a modifier on the
        // first. The result lands through the same pending-units path and
        // supersedes the cache via the store's last-record-wins fold.
        Action::RegenerateUnits => match stack.top() {
            View::Diff(app) => {
                if pending_units.is_some() {
                    *goto_status = Some("units: grouping already running".to_owned());
                } else if app.units_prompt_needed {
                    // Even a regenerate is still this session's first
                    // spawn if nothing was ever configured — same gate as
                    // `u`'s cache-miss path.
                    *units_panel = None;
                    open_units_setup(units_setup, goto_status);
                } else {
                    *units_panel = None;
                    spawn_units_generation(app, pending_units, units_status);
                }
            }
            View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => {
                *goto_status = Some("units: only available in the diff view".to_owned());
            }
        },
        // Explicit, not folded into the `other` catch-all below on
        // purpose: unlike every action that bucket forwards to
        // `View::update`, this one has no view-level effect at all (see
        // `App`/`FileView::update`'s own `Action::OpenHelp` no-op arms) —
        // an explicit arm here is the only place the popup actually opens.
        // Forgetting it would still compile (the wildcard would silently
        // swallow `?` into a no-op) — this is the one item on this
        // feature's checklist that isn't compile-checked; the e2e suite is
        // what backstops it.
        //
        // Cancels any in-flight hover/goto request first — see
        // `cancel_pending_lsp_requests`'s docs for why a response
        // that arrived while the modal was up would otherwise be able to
        // mutate `stack`/`refs_panel` or draw a hover popup with the
        // reviewer unable to even see it happen, let alone react.
        Action::OpenHelp => {
            cancel_pending_lsp_requests(hover_state, pending_hover, pending_goto, observer);
            *help = Some(HelpState::new());
        }
        Action::OpenScopeMenu => match stack.top() {
            View::Diff(_) => *scope_menu = Some(ScopeMenuState::new_list(jj_repo.is_some())),
            View::File(_) | View::Timeline(_) | View::Log(_) | View::LspInspector(_) => {
                *goto_status = Some("scope: only available in the diff view".to_owned());
            }
        },
        // Issue #14 (extended by #15's tree): Enter while `Files` has focus
        // either opens the selected file (jumps the diff cursor to its
        // header, hands focus to `Diff`, and records the jump in the
        // general history exactly like every other significant navigation
        // does — `from` captured *before* `confirm_files_selection` moves
        // the cursor, so a header/`Del`-row starting point that has no
        // source location of its own naturally records nothing, see
        // `record_jump`'s docs) or toggles the selected directory, which
        // records no jump and invalidates no hover popup at all — nothing
        // under the diff cursor changed, so there's nothing for an open
        // hover to have drifted from. Checked ahead of the `stack.top_mut()`
        // match below rather than as another arm inside it, since this
        // needs an immutable `jump_entry()` read of the *pre-jump* cursor
        // before the mutable call that moves it.
        Action::Confirm if matches!(stack.top(), View::Diff(app) if app.focus == app::MainPaneFocus::Files) =>
        {
            let from = stack.top().jump_entry();
            let outcome = match stack.top_mut() {
                View::Diff(app) => app.confirm_files_selection(),
                // unreachable — the guard above already matched View::Diff
                _ => FilesConfirmOutcome::NoSelection,
            };
            if let FilesConfirmOutcome::Opened(to) = outcome {
                record_jump(jump_stack, from, Some(to));
                hover_state.invalidate();
            }
        }
        Action::Confirm => match stack.top_mut() {
            // Computing the diff is a real backend call that can fail, so
            // `LogView::confirm` is called directly here rather than
            // forwarded through `View::update` — see `log_view`'s module
            // docs on why `Action::Confirm` is deliberately absent from
            // `LogView::update`'s own match.
            View::Log(log) => match log.confirm() {
                Ok(Some(app)) => stack.push(View::Diff(app)),
                Ok(None) => {} // empty list, or a blocked range — see `LogView::confirm`
                Err(e) => *goto_status = Some(format!("log: {e}")),
            },
            _ => {
                let before = stack.top().hover_cursor_key();
                stack.top_mut().update(action);
                if stack.top().hover_cursor_key() != before {
                    hover_state.invalidate();
                }
            }
        },
        other => {
            let before = stack.top().hover_cursor_key();
            stack.top_mut().update(other);
            if stack.top().hover_cursor_key() != before {
                hover_state.invalidate();
            }
            // Tabbing off the Diff pane supersedes an in-flight
            // definition/references request the same way a newer press
            // would (see `supersede_pending_goto`): the files-focus gate
            // only guards *initiation*, so a `gd` dispatched from Diff
            // focus could otherwise resolve seconds later — after the
            // reviewer deliberately moved to the sidebar — and yank focus
            // back to Diff via `jump_cursor_to`, clobbering the sidebar
            // browsing position through `sync_files_selection`. Same
            // no-surprise-navigation principle as issue #11's readiness
            // gate, applied at response-apply time. A pending hover is
            // covered by the generation bump.
            if matches!(other, Action::FocusNextPane | Action::FocusPrevPane)
                && let View::Diff(app) = stack.top()
                && app.focus == app::MainPaneFocus::Files
            {
                supersede_pending_goto(
                    pending_goto,
                    observer,
                    "navigation request superseded by focusing the files pane",
                );
                hover_state.invalidate();
            }
        }
    }
}

/// `Action::Cancel`'s handling once every earlier-precedence overlay
/// (compose/search/help/hover/references panel/scope menu/units picker —
/// all intercepted above in `handle_action`, and the LSP inspector earlier
/// still, via its own local `Cancel` handling) has already had first
/// refusal and declined to consume it. Three outcomes:
///
/// - On *any* [`View::Diff`], root or pushed: an active issue #16 visual
///   selection cancels first, ahead of even the at-root checks below —
///   see [`crate::ui::app::App::cancel_visual`]'s docs. It's the most
///   local transient state a diff view can carry (a handful of rows the
///   reviewer was mid-selection over, versus an active unit scope or
///   search, both of which describe the *whole* diff), and a pushed
///   `Diff` (opened via [`crate::ui::log_view::LogView::confirm`]) can
///   have its own selection exactly as easily as the root one can — this
///   must cancel it in place, not also pop the view out from under the
///   reviewer.
/// - At the root, on the root [`View::Diff`], once no selection remained to
///   cancel: clears an active unit scope first (widening back to the full
///   diff), or failing that clears a confirmed search's highlight (vim's
///   `:noh`) — unchanged from before this issue. Every other root view
///   (`File`/`Timeline`/`Log` reached via `ktmr open`/`ktmr timeline`/`ktmr
///   log`) has nothing local left to cancel, so `Esc` there is simply a
///   no-op (tier 6 of the issue's precedence list) — [`ViewStack::pop`]'s
///   own refusal to pop the last view makes that fall out for free rather
///   than needing a separate check here.
/// - Anywhere else — a pushed `File`/nested `Diff`/`Timeline`/`Log`,
///   *including* a pushed `Diff` that itself has an active search or unit
///   scope (a selection, if any, was already handled by the first arm
///   above) — pops exactly that one view, revealing whatever was
///   underneath. A pushed diff's own search/unit-filter state is
///   deliberately *not* cleared first: the issue's precedence list reserves
///   that clearing role for "at the root," and popping already discards the
///   pushed view's state wholesale.
///
/// Cancels any hover/goto-definition/references request still in flight
/// only when a pop actually happened — a response answering a request
/// issued from the view that just disappeared has nowhere sensible left to
/// navigate or draw a popup over.
fn cancel_diff_view(
    stack: &mut ViewStack,
    hover_state: &mut hover_popup::HoverState,
    pending_hover: &mut Option<PendingHover>,
    pending_goto: &mut Option<(JumpEntry, PendingGoto)>,
    observer: &crate::lsp::ObservationHandle,
    goto_status: &mut Option<String>,
) {
    // Checked ahead of the match below, not as a guard on its first arm —
    // a pattern guard only ever gets a shared borrow of what it matched
    // (`app.cancel_visual()` needs `&mut`), so this has to be its own
    // statement rather than folded into the `match`.
    if let View::Diff(app) = stack.top_mut()
        && app.cancel_visual()
    {
        *goto_status = Some("visual selection cancelled".to_owned());
        return;
    }
    let at_root = stack.is_at_root();
    match stack.top_mut() {
        View::Diff(app) if at_root => {
            if app.clear_unit_filter() {
                hover_state.invalidate();
            } else {
                app.clear_search();
            }
        }
        View::Diff(_) | View::File(_) | View::Timeline(_) | View::Log(_) => {
            if stack.pop() {
                cancel_pending_lsp_requests(hover_state, pending_hover, pending_goto, observer);
            }
        }
        View::LspInspector(_) => unreachable!(
            "Cancel on an open LspInspector is routed to inspector.update() \
             earlier in handle_action, before this arm is ever reached"
        ),
    }
}

/// Cancels whatever hover/goto-definition/find-references request is still
/// in flight, so a response arriving after the reviewer has already moved
/// on can never mutate `stack`/`refs_panel` or draw a hover popup out from
/// under something else. Two callers: the instant the help modal opens (it
/// intercepts every keystroke until it closes — see `run`'s `help` arm —
/// so the reviewer would have no way to even notice, let alone dismiss,
/// whatever changed underneath), and the instant `Esc` actually pops a
/// pushed view (see [`cancel_diff_view`]) — a request issued from a `gd`/
/// `gr`/hover press against the view that's about to disappear has nowhere
/// sensible left to land once it does.
///
/// The manager keeps working after a receiver is dropped, so each discarded
/// operation gets an explicit cancellation event while its eventual server
/// response can still be recorded independently. Dropping the receivers is
/// what prevents a late answer from mutating whatever view is on top once
/// the modal closes or the pop has happened.
fn cancel_pending_lsp_requests(
    hover_state: &mut hover_popup::HoverState,
    pending_hover: &mut Option<PendingHover>,
    pending_goto: &mut Option<(JumpEntry, PendingGoto)>,
    observer: &crate::lsp::ObservationHandle,
) {
    hover_state.invalidate();
    if let Some((_, operation_id, _)) = pending_hover.take() {
        observer.record_ui(
            Some(operation_id),
            crate::lsp::EventOutcome::Cancellation,
            "hover result discarded by modal view",
        );
    }
    if let Some((_, pending)) = pending_goto.take() {
        let operation_id = match pending {
            PendingGoto::Definition { operation_id, .. }
            | PendingGoto::References { operation_id, .. } => operation_id,
        };
        observer.record_ui(
            Some(operation_id),
            crate::lsp::EventOutcome::Cancellation,
            "navigation result discarded by modal view",
        );
    }
}

/// `Action::ToggleTimeline`'s handling, factored out so the scope-menu
/// popup's `Timeline (jj)` entry (see `handle_action`'s scope-menu
/// interception) triggers exactly the same behavior `t` already does — the
/// milestone spec calls for the popup entry to be a discoverability alias
/// for the key, not a second implementation of it.
fn open_or_close_timeline(
    stack: &mut ViewStack,
    jj_repo: Option<&JjRepo>,
    goto_status: &mut Option<String>,
) {
    match stack.top_mut() {
        View::Timeline(timeline) => timeline.should_quit = true, // closes back to the diff
        View::Diff(_) => match jj_repo {
            Some(repo) => {
                match TimelineView::new(repo.clone(), timeline_view::DEFAULT_OP_LOG_LIMIT) {
                    Ok(timeline) => stack.push(View::Timeline(timeline)),
                    Err(e) => *goto_status = Some(format!("timeline: {e}")),
                }
            }
            None => {
                *goto_status = Some("jj not detected — timeline unavailable".to_owned());
            }
        },
        // The timeline only relates to the root diff; a `FileView`/
        // `LogView` pushed on top of it (via goto-definition or `L`) has
        // nothing for `t` to open.
        View::File(_) | View::Log(_) | View::LspInspector(_) => {}
    }
}

/// `Action::ToggleLogView`'s handling, factored out the same way and for
/// the same reason as [`open_or_close_timeline`] — the scope-menu popup's
/// `Log` entry.
fn open_or_close_log(stack: &mut ViewStack, goto_status: &mut Option<String>) {
    match stack.top_mut() {
        View::Log(log) => log.should_quit = true, // closes back to the diff
        View::Diff(app) => {
            let backend = LogBackend::detect(&app.repo_root);
            match LogView::new(backend, log_view::DEFAULT_LOG_LIMIT) {
                Ok(log) => stack.push(View::Log(log)),
                Err(e) => *goto_status = Some(format!("log: {e}")),
            }
        }
        View::File(_) | View::Timeline(_) | View::LspInspector(_) => {}
    }
}

fn open_or_close_lsp_inspector(
    stack: &mut ViewStack,
    observer: &crate::lsp::ObservationHandle,
    lsp_manager: &LspManager,
) {
    if let View::LspInspector(inspector) = stack.top_mut() {
        inspector.should_quit = true;
        return;
    }
    let preferred = stack
        .top()
        .hover_query()
        .and_then(|query| lsp_manager.server_identity(&query.file, &query.git_root))
        .map(|(language, root)| ServerIdentity::new(language, root));
    stack.push(View::LspInspector(lsp_inspector::LspInspectorView::new(
        observer.clone(),
        preferred,
    )));
}

/// What the scope-menu popup can swap the current [`View::Diff`]'s content
/// to *in place* — unlike `Log`/`Timeline` (handled by
/// [`open_or_close_log`]/[`open_or_close_timeline`] instead), which push a
/// new view rather than replacing this one's content, so they never become
/// a `ScopeChoice`.
/// The scope menu's background `gh pr diff` machinery: at most one fetch
/// in flight, and at most one fetched-but-unapplied result *parked*
/// because the view that asked wasn't on top when the text arrived. Both
/// halves carry the requesting diff's [`App::view_token`] — the result
/// must land on that exact view, never just "whichever `Diff` is on top
/// when it arrives" (the log view pushes fresh `App`s, so those can
/// differ) — and a parked result applies automatically the moment its
/// view is back on top, rather than asking the reviewer to retype the
/// request. Any completed scope swap, or a newer fetch, clears both: a
/// more recent choice always supersedes an older fetch, however far that
/// fetch got. A parked result whose view was popped for good simply sits
/// until then — it's one string, and applying it anywhere else would be
/// worse than the memory.
type PendingPrFetch = (
    std::num::NonZeroU64,
    u64,
    std::sync::mpsc::Receiver<Result<String, String>>,
);

#[derive(Default)]
struct PrScopeFetch {
    pending: Option<PendingPrFetch>,
    parked: Option<(std::num::NonZeroU64, u64, String)>,
}

impl PrScopeFetch {
    fn clear(&mut self) {
        self.pending = None;
        self.parked = None;
    }
}

enum ScopeChoice {
    WorkingTree,
    Staged,
    /// The free-form text a "Revision…" submission carried, untrimmed —
    /// [`apply_scope_swap`] trims it once, at the point it's actually used.
    Revision(String),
}

/// Whether a scope swap that resolves at the session's root diff should
/// pause (`Some(true)`) or resume (`Some(false)`) watch mode's refresh loop
/// — `None` when watch mode isn't running at all, or the swap happened on
/// a *pushed* diff rather than the root (see [`ViewStack::is_at_root`]'s
/// docs), in which case nothing about watch mode changes. Split out as its
/// own pure function — no `App`, no I/O — so this decision is unit
/// testable without constructing an [`LspManager`] or a real watcher, per
/// this milestone's convention of factoring state transitions out of the
/// glue that has to run them.
fn watch_pause_decision(watch_active: bool, at_root: bool, is_working_tree: bool) -> Option<bool> {
    (watch_active && at_root).then_some(!is_working_tree)
}

/// Proactively opens `app`'s current files with the LSP — shared by
/// [`warm_up_root`]'s [`View::Diff`] handling and [`apply_scope_swap`] (a
/// mid-session scope swap onto an interactive scope warms up exactly the
/// same way the session's initial root view does), so both call sites stay
/// in agreement about which files qualify rather than maintaining the
/// filter twice.
fn warm_up_diff(app: &App, lsp_manager: &LspManager) -> crate::lsp::manager::WarmUpSummary {
    let max_lines = crate::config::highlight_max_lines();
    let files: Vec<PathBuf> = app
        .files
        .iter()
        .filter(|f| !f.is_deleted && !f.is_binary && !f.skip_highlighting(max_lines))
        .filter_map(|f| f.new_path.as_deref())
        .map(|relative| app.repo_root.join(relative))
        .collect();
    lsp_manager.warm_up(&files, &app.repo_root)
}

/// Fetches `choice`'s diff text and, on success, swaps it into `app`
/// ([`App::apply_scope_swap`]) and runs every side effect a scope change
/// needs: closing a now-stale hover/references overlay, bounding the
/// highlight cache the same way a watch refresh already does (see
/// `handle_watch_refresh`'s docs), warming up the LSP for an interactive
/// scope (`Working tree`/`Staged` — never `Revision`, matching
/// [`crate::ui::log_view::LogView`]-opened diffs' existing "historical
/// content isn't LSP-eligible" precedent, see `App::interactive`'s docs),
/// and — only when this swap happened on the session's root diff, the one
/// view watch mode actually refreshes (`at_root`) — pausing or resuming
/// watch mode to match the new scope via [`watch_pause_decision`].
///
/// On failure, `app` is left *completely* untouched: every branch below
/// only calls [`App::apply_scope_swap`] after its own VCS call already
/// succeeded, so a bad revision or a transient git/jj failure can never
/// leave a blank or half-updated diff on screen — the error is returned for
/// the caller to show as a status-bar note instead, per the milestone spec.
///
/// Issue #8: also records `app.revision_scope` (`Some` for `Revision`,
/// `None` for `WorkingTree`/`Staged`) directly as a field assignment rather
/// than through [`App::apply_scope_swap`]'s own parameter list (see that
/// method's docs for why), and — only `at_root`, the same gate
/// [`watch_pause_decision`] uses just below, since [`MovingScopeState`]
/// only ever tracks the root diff — re-seeds `moving_scope` from the new
/// scope via [`seed_moving_scope`]. A swap on a *pushed* diff still records
/// `revision_scope` on that diff's own `app` (for display/introspection
/// consistency), but never touches `moving_scope`, exactly the way it never
/// touches `watch_paused` either.
#[allow(clippy::too_many_arguments)] // one function, every side effect a
// scope change needs — see `handle_action`'s identical justification for
// why splitting this into a struct wouldn't reduce how many independent
// pieces of session state it has to touch.
fn apply_scope_swap(
    app: &mut App,
    choice: &ScopeChoice,
    at_root: bool,
    watch_active: bool,
    jj_repo: Option<&JjRepo>,
    lsp_manager: &LspManager,
    highlighter: &mut LineHighlighter,
    hover_state: &mut hover_popup::HoverState,
    refs_panel: &mut Option<RefsPanelState>,
    context_menu: &mut Option<ContextMenuState>,
    watch_paused: &mut bool,
    watch_status: &mut Option<WatchStatus>,
    moving_scope: &mut Option<MovingScopeState>,
) -> Result<(), String> {
    let git = GitSource::at(app.repo_root.clone());
    let (result, interactive, scope_label): (anyhow::Result<String>, bool, Option<String>) =
        match choice {
            ScopeChoice::WorkingTree => (git.working_tree_diff(), true, None),
            ScopeChoice::Staged => (git.staged_diff(), true, None),
            ScopeChoice::Revision(input) => {
                let revset = input.trim();
                if revset.is_empty() {
                    return Err("enter a revision first".to_owned());
                }
                // jj mode passes the whole string through as one revset to
                // `jj diff -r` unvalidated, letting jj's own operators
                // (`a..b`, `@-`, ...) work exactly as they do on the
                // command line — see `crate::vcs::jj::JjRepo::revision_diff`'s
                // docs, and this milestone's task notes for the empirical
                // check that a DAG-range revset like `a..b` resolves to the
                // combined diff from `a`'s side to `b`'s, not something
                // unhelpful. git mode reuses `GitSource::range_diff`, which
                // already accepts a plain rev or an `A..B`/`A...B` range
                // (see `git::plan_range`) — no second parser needed here.
                let result = match jj_repo {
                    Some(repo) => repo.revision_diff(revset),
                    None => git.range_diff(revset),
                };
                (result, false, Some(format!("r: {revset}")))
            }
        };
    let text = result.map_err(|e| e.to_string())?;
    // Only `WorkingTree`'s new side is genuinely the live working tree —
    // `Staged`'s is the index, and `Revision`'s is historical (see
    // `App::disk_is_new_side`'s docs for why this can't just reuse
    // `interactive`, which is `true` for `Staged` too).
    let payload = ScopeSwapPayload {
        text,
        interactive,
        disk_is_new_side: matches!(choice, ScopeChoice::WorkingTree),
        scope_label,
        revision_scope: match choice {
            ScopeChoice::Revision(input) => Some(app::RevisionScope {
                text: input.trim().to_owned(),
                via_jj: jj_repo.is_some(),
            }),
            ScopeChoice::WorkingTree | ScopeChoice::Staged => None,
        },
        is_working_tree: matches!(choice, ScopeChoice::WorkingTree),
    };
    finish_scope_swap(
        app,
        payload,
        at_root,
        watch_active,
        jj_repo,
        lsp_manager,
        highlighter,
        hover_state,
        refs_panel,
        context_menu,
        watch_paused,
        watch_status,
        moving_scope,
    );
    Ok(())
}

/// Everything [`finish_scope_swap`] needs to know about the diff a scope
/// swap resolved to — split from the resolution itself so a swap whose
/// text arrives asynchronously (the "GitHub PR…" entry's background `gh`
/// fetch) applies through the exact same code as the synchronous git/jj
/// choices, instead of a drifting copy of it.
struct ScopeSwapPayload {
    text: String,
    interactive: bool,
    disk_is_new_side: bool,
    scope_label: Option<String>,
    revision_scope: Option<app::RevisionScope>,
    is_working_tree: bool,
}

/// The application half of a scope swap: parse and install the new diff,
/// drop every overlay anchored to the old one, and settle watch/moving-
/// scope state. Infallible by design — by the time a payload exists, the
/// only fallible work (resolving a diff out of git/jj/`gh`) already
/// happened.
#[expect(
    clippy::too_many_arguments,
    reason = "threads the run loop's overlay/watch locals through, same as apply_scope_swap"
)]
fn finish_scope_swap(
    app: &mut App,
    payload: ScopeSwapPayload,
    at_root: bool,
    watch_active: bool,
    jj_repo: Option<&JjRepo>,
    lsp_manager: &LspManager,
    highlighter: &mut LineHighlighter,
    hover_state: &mut hover_popup::HoverState,
    refs_panel: &mut Option<RefsPanelState>,
    context_menu: &mut Option<ContextMenuState>,
    watch_paused: &mut bool,
    watch_status: &mut Option<WatchStatus>,
    moving_scope: &mut Option<MovingScopeState>,
) {
    let git = GitSource::at(app.repo_root.clone());
    let ScopeSwapPayload {
        text,
        interactive,
        disk_is_new_side,
        scope_label,
        revision_scope,
        is_working_tree,
    } = payload;
    app.apply_scope_swap(
        parse_unified_diff(&text),
        interactive,
        disk_is_new_side,
        scope_label,
    );
    app.revision_scope = revision_scope;

    hover_state.invalidate();
    *refs_panel = None;
    // A scope swap replaces the diff/tree wholesale — the row/directory the
    // menu targeted (a `MenuTarget::DiffRow`'s cursor position, a
    // `TreeDir`'s path) has no guaranteed correspondence in the new one, so
    // this closes it the same way `refs_panel` does just above rather than
    // re-deriving against content the menu was never opened on.
    *context_menu = None;
    highlighter.clear_cache();
    if interactive {
        warm_up_diff(app, lsp_manager);
    }

    if let Some(should_pause) = watch_pause_decision(watch_active, at_root, is_working_tree) {
        let was_paused = *watch_paused;
        *watch_paused = should_pause;
        if should_pause && !was_paused {
            *watch_status = Some(WatchStatus::new(
                "watch paused (historical scope)".to_owned(),
            ));
        } else if !should_pause && was_paused {
            *watch_status = Some(WatchStatus::new("watch resumed".to_owned()));
        }
    }
    if at_root {
        *moving_scope = seed_moving_scope(app.revision_scope.as_ref(), &git, jj_repo);
        // Same unresolved-baseline note the session-start seed emits — see
        // that call site's comment.
        if let Some(scope) = &moving_scope
            && scope.last_hash.is_none()
        {
            *watch_status = Some(WatchStatus::new(format!(
                "scope: couldn't resolve {} yet; live refresh will retry",
                scope.text
            )));
        }
    }
}

/// Applies one flushed watch batch: runs the M5 pre-refresh hook, re-runs
/// the working-tree diff, re-parses it, and swaps it into the root diff via
/// [`App::apply_refresh`] (which owns cursor/scroll preservation — see its
/// docs), then resyncs every language server that has anything to say about
/// the files that changed. Closes an open hover/references overlay whose
/// anchored row didn't survive the swap unchanged, and always bumps the
/// hover generation counter regardless, so any request issued before the
/// refresh can never land after it (mirrors what a cursor move already does
/// via [`hover_popup::HoverState::invalidate`] — see
/// [`hover_popup::HoverState::bump_generation_for_refresh`]'s docs for why
/// this refresh path needs the non-closing variant instead).
///
/// A transient failure re-running or re-parsing the diff (e.g. `git diff`
/// racing an in-progress `git rebase`) is left as a status note rather than
/// treated as fatal — the diff already on screen stays put, and the next
/// flush gets another chance.
///
/// M12: a no-op entirely while `watch_paused` (the scope-menu popup swapped
/// the root diff to something other than the working tree — see
/// [`apply_scope_swap`]/[`watch_pause_decision`]). Watch mode's own
/// filesystem watcher thread keeps running regardless — nothing here tears
/// it down — so the moment the reviewer swaps back to `Working tree`,
/// refreshes resume on the very next flush with no restart needed.
///
/// `live_search` is `Some((query_text, origin))` exactly when Issue #5's
/// `/` prompt is currently open *on the root diff* (see this function's one
/// call site for why any other case passes `None`) — [`App::apply_refresh`]
/// already recomputes a *confirmed* search's matches on its own (anchored
/// on the cursor, which is right for the ordinary no-prompt case), but a
/// live prompt's incremental preview must stay anchored on where `/` was
/// first pressed, not wherever the cursor happens to be mid-typing — see
/// [`crate::ui::search::compute_search`]'s docs. When `Some`,
/// [`App::recompute_search_live`] re-runs after [`App::apply_refresh`] and
/// simply overwrites whatever that call's own recompute produced with the
/// origin-correct result — a small amount of redundant work on an
/// infrequent, already-debounced event, not a correctness concern.
#[allow(clippy::too_many_arguments)] // each parameter is a distinct piece
// of session state one refresh cycle touches — see `handle_action`'s
// comment for why bundling into a struct wouldn't reduce this.
fn handle_watch_refresh(
    batch: watch::WatchBatch,
    stack: &mut ViewStack,
    pre_refresh_hook: Option<&dyn PreRefreshHook>,
    lsp_manager: &LspManager,
    highlighter: &mut LineHighlighter,
    hover_state: &mut hover_popup::HoverState,
    refs_panel: &mut Option<RefsPanelState>,
    context_menu: &mut Option<ContextMenuState>,
    watch_status: &mut Option<WatchStatus>,
    watch_paused: bool,
    live_search: Option<(&str, &refresh::Anchor)>,
    pointer_hover: &mut pointer_hover::PointerHoverState,
    pending_pointer_hover: &mut Option<PendingHover>,
    observer: &crate::lsp::ObservationHandle,
) {
    if watch_paused {
        return;
    }

    let View::Diff(app) = stack.root_mut() else {
        return; // watch mode only ever runs against the root diff view
    };

    // Issue #24 req 5's fourth cancellation hook: unconditional, unlike
    // `hover_state`/`refs_panel` just below, which only close when
    // `overlay_survives` says the cursor's own row didn't make it through
    // the refresh — a passive target's row could easily survive while its
    // *content* (and therefore what a stale response would render) did not,
    // and correctness here matters more than occasionally re-requesting a
    // hover that would have come back identical (see the issue's own
    // implementation notes).
    cancel_pending_pointer_hover(pointer_hover, pending_pointer_hover, observer);

    if let Some(hook) = pre_refresh_hook {
        hook.before_refresh(&app.repo_root);
    }

    let files = match GitSource::discover(&app.repo_root).and_then(|s| s.working_tree_diff()) {
        Ok(diff_text) => parse_unified_diff(&diff_text),
        Err(e) => {
            *watch_status = Some(WatchStatus::new(format!("watch: refresh failed: {e}")));
            return;
        }
    };

    let overlay_survives = app.apply_refresh(files);
    if let Some((query, origin)) = live_search {
        app.recompute_search_live(query, origin);
    }
    hover_state.bump_generation_for_refresh();
    // Bounds the highlight cache's memory to roughly one diff's worth of
    // content rather than accumulating across every refresh of a long
    // watch session — see `LineHighlighter::clear_cache`'s docs on why this
    // is a memory-management call, not a correctness one.
    highlighter.clear_cache();

    let mut overlay_closed = false;
    if !overlay_survives {
        if hover_state.is_open() {
            hover_state.close();
            overlay_closed = true;
        }
        if refs_panel.is_some() {
            *refs_panel = None;
            overlay_closed = true;
        }
    }
    // Issue #23 req 10: the menu closes on *every* refresh, not only when
    // `overlay_survives` flipped — that flag tracks the diff cursor's own
    // row, which says nothing about a `TreeDir`/`TreeFile` target: a
    // refresh re-flattens `visible_rows` and re-anchors `files_selection`
    // wholesale, so a tree menu opened before it can silently point at a
    // different row while its entries look unchanged (`tree_file_entries`
    // structurally can't go empty to signal staleness). Always-close over
    // clever survival analysis, the same call the `Event::Resize` arm
    // makes — reopening costs one right-click.
    if context_menu.is_some() {
        *context_menu = None;
        overlay_closed = true;
    }

    sync_lsp_after_refresh(app, lsp_manager, &batch.changes);

    *watch_status = Some(WatchStatus::new(
        if overlay_closed {
            "closed: file changed"
        } else {
            "updated"
        }
        .to_owned(),
    ));
}

/// Issue #8: applies one `AppEvent::RevisionChanged` tick — sibling of
/// [`handle_watch_refresh`], re-resolving the root diff's moving scope (see
/// [`MovingScopeState`]) and, only if its target commit actually changed
/// since the last check, re-running the *exact* diff call the scope was
/// opened with and swapping it in via [`App::apply_refresh`] — the anchor-
/// preserving path, not [`apply_scope_swap`]'s reset-to-top-and-relabel one,
/// since this is a later version of the *same* scope, not a switch to a
/// different one. The scope label itself is untouched by construction (see
/// [`App::apply_refresh`]'s docs), so `"r: HEAD"` stays exactly that even
/// though the commit it names just changed underneath it.
///
/// `None` is the entire pause mechanism (see [`MovingScopeState`]'s docs):
/// every ref-watcher tick this session ever receives is a cheap no-op the
/// moment the root diff isn't sitting on a classified-moving revision
/// scope, which covers both "never was" (working tree/staged/an immutable
/// hash) and "isn't anymore" (swapped away — see [`apply_scope_swap`]'s
/// reseed). Every failure path (a bad resolve, a failed re-diff) leaves
/// `app`/`moving_scope` completely untouched and reports a transient status
/// note instead — the same "never blank the screen over a flaky VCS call"
/// posture [`apply_scope_swap`]/[`handle_watch_refresh`] both already take.
#[allow(clippy::too_many_arguments)] // mirrors `handle_watch_refresh`'s
// identical justification: each parameter is a distinct piece of session
// state one refresh cycle touches.
fn handle_moving_scope_refresh(
    moving_scope: &mut Option<MovingScopeState>,
    stack: &mut ViewStack,
    jj_repo: Option<&JjRepo>,
    highlighter: &mut LineHighlighter,
    hover_state: &mut hover_popup::HoverState,
    refs_panel: &mut Option<RefsPanelState>,
    context_menu: &mut Option<ContextMenuState>,
    watch_status: &mut Option<WatchStatus>,
    live_search: Option<(&str, &refresh::Anchor)>,
    pointer_hover: &mut pointer_hover::PointerHoverState,
    pending_pointer_hover: &mut Option<PendingHover>,
    observer: &crate::lsp::ObservationHandle,
) {
    let Some(scope) = moving_scope else {
        return;
    };
    let View::Diff(app) = stack.root_mut() else {
        return; // the moving-scope refresh only ever targets the root diff
    };
    let git = GitSource::at(app.repo_root.clone());

    let resolved = if scope.via_jj {
        match jj_repo {
            Some(repo) => repo.resolve_commit_id(&scope.text),
            None => Ok(None),
        }
    } else {
        git.resolve(&scope.text)
    };
    let new_hash = match resolved {
        Ok(Some(hash)) => hash,
        Ok(None) => {
            *watch_status = Some(WatchStatus::new(format!(
                "scope: refresh check failed: {} no longer resolves",
                scope.text
            )));
            return;
        }
        Err(e) => {
            *watch_status = Some(WatchStatus::new(format!(
                "scope: refresh check failed: {e}"
            )));
            return;
        }
    };
    if scope.last_hash.as_deref() == Some(new_hash.as_str()) {
        return; // some other watched ref path changed; this scope's own target didn't move
    }

    // The same diff call the scope was opened with — see
    // `apply_scope_swap`'s `ScopeChoice::Revision` arm, which this mirrors.
    let diff_result = if scope.via_jj {
        match jj_repo {
            Some(repo) => repo.revision_diff(&scope.text),
            None => Err(anyhow::anyhow!("no jj repository detected")),
        }
    } else {
        git.range_diff(&scope.text)
    };
    let files = match diff_result {
        Ok(text) => parse_unified_diff(&text),
        Err(e) => {
            *watch_status = Some(WatchStatus::new(format!("scope: refresh failed: {e}")));
            return;
        }
    };

    // Issue #24 req 5's fourth cancellation hook, unconditional the same
    // way `handle_watch_refresh` applies it — see that function's docs.
    cancel_pending_pointer_hover(pointer_hover, pending_pointer_hover, observer);

    let overlay_survives = app.apply_refresh(files);
    // Same live-search recompute `handle_watch_refresh` does: an open,
    // unconfirmed `/` prompt's matches were computed against the rows this
    // refresh just replaced.
    if let Some((query, origin)) = live_search {
        app.recompute_search_live(query, origin);
    }
    hover_state.bump_generation_for_refresh();
    highlighter.clear_cache();
    if !overlay_survives {
        if hover_state.is_open() {
            hover_state.close();
        }
        if refs_panel.is_some() {
            *refs_panel = None;
        }
    }
    // Always closes, matching `handle_watch_refresh`'s own req-10 reasoning:
    // a tree/diff-row menu target has no guaranteed correspondence in the
    // refreshed content regardless of whether the cursor's own row survived.
    *context_menu = None;

    scope.last_hash = Some(new_hash);
    *watch_status = Some(WatchStatus::new(format!("updated: {} moved", scope.text)));
}

/// Keeps every language server in sync with the files a watch batch
/// touched: already-open documents get a full-text `textDocument/didChange`
/// (see [`LspManager::sync_changed_files`] on why that's uncapped), every
/// running server whose workspace root contains a changed path gets
/// `workspace/didChangeWatchedFiles` so it can invalidate project state for
/// files it never opened, and newly-appearing diff entries get `didOpen`'d
/// through the same capped [`LspManager::warm_up`] machinery startup uses —
/// already-open files are cheap no-ops there, so calling it with the full
/// current file list on every refresh is simpler than tracking "which files
/// are new" separately.
fn sync_lsp_after_refresh(app: &App, lsp_manager: &LspManager, changes: &[watch::ChangedPath]) {
    let changed_paths: Vec<PathBuf> = changes.iter().map(|c| c.path.clone()).collect();
    lsp_manager.sync_changed_files(&changed_paths, &app.repo_root);

    let watched: Vec<(PathBuf, FileChangeType)> = changes
        .iter()
        .map(|c| (c.path.clone(), file_change_type(c.kind)))
        .collect();
    lsp_manager.notify_watched_files(&watched);

    let max_lines = crate::config::highlight_max_lines();
    let current_files: Vec<PathBuf> = app
        .files
        .iter()
        .filter(|f| !f.is_deleted && !f.is_binary && !f.skip_highlighting(max_lines))
        .filter_map(|f| f.new_path.as_deref())
        .map(|relative| app.repo_root.join(relative))
        .collect();
    lsp_manager.warm_up(&current_files, &app.repo_root);
}

/// Maps a filesystem watcher's [`watch::ChangeKind`] onto LSP's
/// `FileChangeType` — the vocabulary conversion between "what `notify` saw"
/// and "what `workspace/didChangeWatchedFiles` expects," which belongs at
/// this integration layer rather than in either `watch` (no LSP knowledge)
/// or `lsp` (no filesystem-watcher knowledge).
fn file_change_type(kind: watch::ChangeKind) -> FileChangeType {
    match kind {
        watch::ChangeKind::Created => FileChangeType::CREATED,
        watch::ChangeKind::Changed => FileChangeType::CHANGED,
        watch::ChangeKind::Deleted => FileChangeType::DELETED,
    }
}

/// Resolves the position encoding to convert a definition/references
/// response's LSP coordinates with — the encoding negotiated by whichever
/// server answered, looked up by the request's origin file/root. Falls back
/// to UTF-16 (LSP's mandated default) on the (practically impossible, since
/// a response implies a `Ready` server) chance it's not known.
fn response_encoding(lsp_manager: &LspManager, from: &JumpEntry) -> PositionEncodingKind {
    lsp_manager
        .position_encoding(&from.file, &from.git_root)
        .unwrap_or(PositionEncodingKind::UTF16)
}

fn request_error_outcome(error: &LspError) -> crate::lsp::EventOutcome {
    match error {
        LspError::Server { .. } => crate::lsp::EventOutcome::ServerError,
        LspError::Io(message) if message.contains("does not advertise") => {
            crate::lsp::EventOutcome::Unsupported
        }
        LspError::Io(message) if message == crate::lsp::manager::REQUEST_TIMED_OUT => {
            crate::lsp::EventOutcome::Timeout
        }
        LspError::Closed | LspError::Io(_) | LspError::Json(_) => {
            crate::lsp::EventOutcome::TransportFailure
        }
    }
}

/// Applies a `textDocument/definition` result: navigates straight there for
/// a single candidate (the common case), opens the references panel
/// (labeled "Definitions") for several, or leaves a status-bar note for
/// "none"/an error.
#[allow(clippy::too_many_arguments)]
fn apply_definition_result(
    result: Result<DefinitionResult, LspError>,
    from: JumpEntry,
    stack: &mut ViewStack,
    jump_stack: &mut JumpStack,
    lsp_manager: &LspManager,
    refs_panel: &mut Option<RefsPanelState>,
    goto_status: &mut Option<String>,
    observer: &crate::lsp::ObservationHandle,
    operation_id: u64,
) {
    let response = match result {
        Ok(Some(response)) => response,
        Ok(None) => {
            observer.record_ui(
                Some(operation_id),
                crate::lsp::EventOutcome::NoResult,
                "definition returned no result",
            );
            *goto_status = Some("goto: no definition found".to_owned());
            return;
        }
        Err(e) => {
            observer.record_ui(
                Some(operation_id),
                request_error_outcome(&e),
                format!("definition failed: {e}"),
            );
            *goto_status = Some(format!("goto: {e}"));
            return;
        }
    };
    let locations = navigation::definition_locations(response);
    if locations.is_empty() {
        observer.record_ui(
            Some(operation_id),
            crate::lsp::EventOutcome::NoResult,
            "definition returned zero locations",
        );
        *goto_status = Some("goto: no definition found".to_owned());
        return;
    }
    let encoding = response_encoding(lsp_manager, &from);
    if locations.len() == 1 {
        match navigation::location_to_target(&locations[0], &from.git_root, &encoding) {
            Some(target) => {
                let navigation =
                    navigate_to(stack, jump_stack, lsp_manager, Some(from), target, true);
                if let Err(error) = navigation {
                    observer.record_ui(
                        Some(operation_id),
                        crate::lsp::EventOutcome::NavigationFailure,
                        format!("definition navigation failed: {error}"),
                    );
                } else {
                    observer.record_ui(
                        Some(operation_id),
                        crate::lsp::EventOutcome::Result,
                        "definition navigated (1 location)",
                    );
                }
            }
            None => {
                observer.record_ui(
                    Some(operation_id),
                    crate::lsp::EventOutcome::NavigationFailure,
                    "definition points at an unreadable file",
                );
                *goto_status = Some("goto: definition points at an unreadable file".to_owned())
            }
        }
        return;
    }
    let (entries, truncated) = refs_panel::build_entries(&locations, &from.git_root, &encoding);
    *refs_panel = Some(RefsPanelState {
        git_root: from.git_root,
        panel: RefsPanel::new("Definitions", entries, truncated),
        operation_id,
    });
    observer.record_ui(
        Some(operation_id),
        crate::lsp::EventOutcome::Result,
        format!("definition returned {} locations", locations.len()),
    );
}

/// As [`apply_definition_result`], for `textDocument/references` — always
/// opens the panel (labeled "References") rather than auto-navigating a
/// single result, since a reviewer asking "where else is this used" wants
/// the list even when there's only one other use.
fn apply_references_result(
    result: Result<ReferencesResult, LspError>,
    from: JumpEntry,
    lsp_manager: &LspManager,
    refs_panel: &mut Option<RefsPanelState>,
    goto_status: &mut Option<String>,
    observer: &crate::lsp::ObservationHandle,
    operation_id: u64,
) {
    match result {
        Ok(Some(locations)) if !locations.is_empty() => {
            let encoding = response_encoding(lsp_manager, &from);
            let (entries, truncated) =
                refs_panel::build_entries(&locations, &from.git_root, &encoding);
            *refs_panel = Some(RefsPanelState {
                git_root: from.git_root,
                panel: RefsPanel::new("References", entries, truncated),
                operation_id,
            });
            observer.record_ui(
                Some(operation_id),
                crate::lsp::EventOutcome::Result,
                format!("references returned {} locations", locations.len()),
            );
        }
        Ok(_) => {
            observer.record_ui(
                Some(operation_id),
                crate::lsp::EventOutcome::NoResult,
                "references returned no locations",
            );
            *goto_status = Some("references: none found".to_owned())
        }
        Err(e) => {
            observer.record_ui(
                Some(operation_id),
                request_error_outcome(&e),
                format!("references failed: {e}"),
            );
            *goto_status = Some(format!("references: {e}"))
        }
    }
}

/// Opens the first-use setup wizard — or reports the one condition it
/// can't help with (no agent CLI installed at all, where a picker with
/// zero rows would just be a more confusing version of this message).
fn open_units_setup(units_setup: &mut Option<UnitsSetupState>, goto_status: &mut Option<String>) {
    let detected = crate::groups::agent::detect_all();
    if detected.is_empty() {
        *goto_status = Some(
            "units: no agent CLI found — grouping needs `claude` or `codex` on PATH".to_owned(),
        );
    } else {
        *units_setup = Some(UnitsSetupState::new(detected));
    }
}

/// Kicks off one background grouping run against the *full* diff (see
/// [`App::full_files`] — never the unit-filtered view) and parks its
/// receiver in `pending_units` for the frame preamble to collect. Clones
/// the inputs because the agent call outlives this frame by seconds to
/// minutes; the diff it groups is pinned at request time and staleness is
/// re-checked when the result arrives.
fn spawn_units_generation(
    app: &App,
    pending_units: &mut Option<Receiver<Result<crate::groups::Grouping, String>>>,
    units_status: &mut Option<String>,
) {
    let repo_root = app.repo_root.clone();
    let files = app.full_files().to_vec();
    let units_config = app.units_config.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::groups::generate(&repo_root, &files, &units_config));
    });
    *pending_units = Some(rx);
    *units_status = Some("units: asking the agent CLI \u{2026}".to_owned());
}

#[allow(clippy::too_many_arguments)] // one render pass threading through
// every overlay/lookaside table the frame might need to draw; see
// `handle_action`'s comment for why a struct wouldn't reduce this.
fn draw(
    frame: &mut Frame,
    view: &mut View,
    keymap: &Keymap,
    highlighter: &mut LineHighlighter,
    hover_state: &hover_popup::HoverState,
    pointer_hover_state: &pointer_hover::PointerHoverState,
    diagnostics: &DiagnosticsStore,
    refs_panel: Option<&RefsPanel>,
    units_panel: Option<&UnitsPanel>,
    units_setup: Option<&UnitsSetupState>,
    status_note: Option<&str>,
    comments: &CommentIndex,
    compose: Option<&mut ComposeState>,
    scope_menu: Option<&ScopeMenuState>,
    context_menu: Option<&ContextMenuState>,
    help: Option<&HelpState>,
    search_prompt: Option<&search::SearchPromptState>,
    jj_available: bool,
    key_display: &key_display::KeyDisplayState,
    hints_expanded: bool,
    geometry: &mut mouse::FrameGeometry,
    compose_keymap: &ComposeKeymap,
) {
    match view {
        View::Diff(app) => {
            let hint_items = hints::diff_view_items(keymap, hints_expanded);
            let status_height = hints::required_height(&hint_items, frame.area().width);
            let areas = diff_layout(frame.area(), app.sidebar_visible, status_height);
            if let Some(sidebar_area) = areas.sidebar {
                // Recorded here rather than inside `sidebar::render` — the
                // rect is already exactly what this call site computed and
                // is about to hand it, no different from `diff_area` a few
                // lines below; see this module's `mouse` docs on reusing
                // the layout calculation rather than re-deriving it.
                geometry.record(sidebar_area, mouse::ScrollTarget::DiffFiles);
                sidebar::render(frame, sidebar_area, app, keymap);
            }
            // While a unit scope is active, the top of the diff pane is
            // given to its banner and everything else renders into what
            // remains — the same subtraction the frame preamble's
            // `content_height` applies, so scroll math and pixels agree
            // (see `units_panel::BANNER_HEIGHT`'s docs).
            let mut diff_area = areas.diff;
            if let Some(filter) = app.unit_filter() {
                let banner = Rect {
                    height: units_panel::BANNER_HEIGHT.min(diff_area.height),
                    ..diff_area
                };
                units_panel::render_banner(frame, banner, filter);
                diff_area.y += banner.height;
                diff_area.height -= banner.height;
            }
            // Recorded post-banner (after the subtraction above) so a wheel
            // over the banner itself — which shows a unit's label/rationale,
            // not diff content — doesn't scroll content it isn't over.
            geometry.record(diff_area, mouse::ScrollTarget::DiffPane);
            let effective_layout = diff_view::effective_layout(app.layout, diff_area.width);
            let diff_hits = diff_view::render_focusable(
                frame,
                diff_area,
                app,
                highlighter,
                effective_layout,
                diagnostics,
                comments,
                app.focus == app::MainPaneFocus::Diff,
                &hints::diff_pane_hints(keymap, app.sidebar_visible),
                keymap,
            );
            // Issue #22: the *content* rect (inside `render_focusable`'s own
            // border) paired with the `HitRow`s it just drew, for click
            // resolution — `pane::inner_rect` reproduces exactly the same
            // inner rect `render_focusable`'s own `PaneChrome::block().inner(area)`
            // computed internally (see that function's docs), so this is
            // the one border-geometry function, not a second hand-counted
            // rect independently at risk of drifting from the real one.
            geometry.record_diff_content(pane::inner_rect(diff_area.width, diff_area), diff_hits);
            status_bar::render(
                frame,
                areas.status,
                app,
                effective_layout,
                status_note,
                search_prompt.map(|p| (p.input.text(), p.input.cursor())),
                &hint_items,
            );
            // Issue #24 render precedence: an explicit (keyboard-cursor)
            // hover, once open, always wins over a passive pointer popup —
            // `passive_hover_suppressed` already keeps the two from *arming*
            // at once, but an already-`Shown` pointer popup must still yield
            // the instant an explicit hover opens on top of it, rather than
            // both trying to occupy the same anchored rect.
            if hover_state.is_open() {
                if let Some(row) = view.cursor_screen_row() {
                    hover_popup::render(frame, diff_area, row, hover_state, geometry);
                }
            } else {
                pointer_hover::render(frame, diff_area, pointer_hover_state);
            }
            if let Some(panel) = refs_panel {
                refs_panel::render(frame, diff_area, panel, geometry);
            }
            if let Some(panel) = units_panel {
                units_panel::render(frame, diff_area, panel, geometry);
            }
            if let Some(state) = units_setup {
                units_setup::render(frame, diff_area, state);
                // Recorded over the whole pane, not the popup's own rect:
                // these overlays are fully modal for keys, so a wheel tick
                // anywhere over the content they block must be captured and
                // discarded, not scroll what's underneath — see
                // `mouse::ScrollTarget`'s docs on the modal variants.
                geometry.record(diff_area, mouse::ScrollTarget::UnitsSetupModal);
            }
            if let Some(state) = compose
                && let Some(row) = view.cursor_screen_row()
            {
                compose::render(frame, diff_area, row, state, compose_keymap);
                geometry.record(diff_area, mouse::ScrollTarget::ComposeModal);
            }
            if let Some(state) = scope_menu {
                scope_menu::render(frame, diff_area, state, jj_available);
                geometry.record(diff_area, mouse::ScrollTarget::ScopeMenuModal);
            }
            if let Some(state) = context_menu {
                context_menu::render(frame, diff_area, state, geometry);
            }
            key_display::render(frame, diff_area, key_display);
        }
        View::File(file) => {
            let hint_items = hints::file_view_items(keymap, hints_expanded);
            let status_height = hints::required_height(&hint_items, frame.area().width);
            let areas = file_view::layout(frame.area(), status_height);
            let file_hits = file_view::render(frame, areas.content, file, diagnostics, geometry);
            // As the diff pane's `record_diff_content` above —
            // `file_view::content_rect` reproduces the exact inner rect
            // `render` itself carved out of `areas.content`.
            geometry.record_file_content(file_view::content_rect(areas.content), file_hits);
            file_view::render_status(frame, areas.status, file, status_note, &hint_items);
            // Same explicit-wins-over-passive precedence as `View::Diff`
            // above.
            if hover_state.is_open() {
                if let Some(row) = view.cursor_screen_row() {
                    hover_popup::render(frame, areas.content, row, hover_state, geometry);
                }
            } else {
                pointer_hover::render(frame, areas.content, pointer_hover_state);
            }
            if let Some(panel) = refs_panel {
                refs_panel::render(frame, areas.content, panel, geometry);
            }
            if let Some(state) = context_menu {
                context_menu::render(frame, areas.content, state, geometry);
            }
            key_display::render(frame, areas.content, key_display);
        }
        View::Timeline(timeline) => {
            let area = frame.area();
            timeline_view::render(
                frame,
                area,
                timeline,
                highlighter,
                keymap,
                key_display,
                hints_expanded,
                geometry,
            );
        }
        View::Log(log) => {
            let area = frame.area();
            log_view::render(
                frame,
                area,
                log,
                keymap,
                key_display,
                hints_expanded,
                geometry,
            );
        }
        View::LspInspector(inspector) => {
            inspector.render(frame, frame.area(), geometry);
        }
    }

    // Rendered once, unconditionally, *outside* the match above rather than
    // nested in `View::Diff`'s arm the way `compose`/`scope_menu` are —
    // those two only ever open from a live `View::Diff` session, but
    // `Action::OpenHelp` opens from any view (see its docs), and sizing
    // this against `frame.area()` rather than `areas.diff` is what makes
    // that true on screen, not just in `handle_action`'s dispatch. Recorded
    // last, for the same reason: nothing else in this function can possibly
    // draw on top of it, so it must win `FrameGeometry::hit`'s
    // last-recorded-wins scan over literally everything above.
    if let Some(state) = help {
        help::render(frame, frame.area(), state, keymap, geometry);
    }
}

struct DiffAreas {
    sidebar: Option<Rect>,
    diff: Rect,
    status: Rect,
}

/// `status_height` is [`hints::required_height`] applied to
/// [`hints::diff_view_items`] and `area`'s width — see
/// `file_view::layout`'s docs for why the caller computes this rather than
/// a fixed constant.
fn diff_layout(area: Rect, sidebar_visible: bool, status_height: u16) -> DiffAreas {
    let rows = RatatuiLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(status_height)])
        .split(area);
    let (main, status) = (rows[0], rows[1]);

    if sidebar_visible {
        let cols = RatatuiLayout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(0)])
            .split(main);
        DiffAreas {
            sidebar: Some(cols[0]),
            diff: cols[1],
            status,
        }
    } else {
        DiffAreas {
            sidebar: None,
            diff: main,
            status,
        }
    }
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

/// The text [`draw_startup_splash`] renders. Pulled out to a constant
/// (rather than inlined in that function) because it doubles as the marker
/// `tests/support/harness.rs` watches for: that harness blocks
/// `Harness::spawn` on "first non-empty frame," and the splash is now the
/// first non-empty frame, drawn before `spawn_input_thread` even starts —
/// so the harness must keep waiting past a frame containing this exact text
/// rather than treating it as "ready for keys" (a key sent that early could
/// be eaten by `enable_kitty_keyboard_protocol`'s synchronous stdin read).
/// There's no `[lib]` target for an integration test to import this
/// constant from, so `SPLASH_MARKER` in `harness.rs` hand-duplicates the
/// string instead — the same by-hand sharing that file's own `PROBE_QUERY`
/// already uses for crossterm's probe bytes. Keep the two in sync.
const STARTUP_SPLASH_TEXT: &str = "katamari — starting…";

/// The splash's second, dimmer line — drawn only when `run`'s
/// `probe_cache::look_up` missed, i.e. `enable_kitty_keyboard_protocol` is
/// about to run crossterm's real (possibly ~2s) probe rather than skip it.
/// Every other launch in this same terminal, past this first one, never
/// shows this line at all: the plain [`STARTUP_SPLASH_TEXT`] frame still
/// flashes by (a real `terminal.draw` call before the event loop's own
/// first one, however fast the rest of startup is — see `draw_startup_splash`'s
/// docs), but it's replaced by the real UI too quickly to read.
const STARTUP_SPLASH_PROBE_PENDING_TEXT: &str =
    "checking terminal keyboard support (first run in this terminal)…";

/// Renders [`STARTUP_SPLASH_TEXT`] centered and dimmed onto `frame` — split
/// out from [`draw_startup_splash`] the same way every other pane in this
/// module keeps its cell-drawing logic in a plain `render(frame, ...)`
/// function separate from whatever calls `terminal.draw` around it, so it
/// can be exercised in a unit test against a [`ratatui::backend::TestBackend`]
/// without a real terminal. `probe_pending` adds
/// [`STARTUP_SPLASH_PROBE_PENDING_TEXT`] as a second line beneath the first
/// — see that constant's docs for when `run` passes `true`.
fn render_startup_splash(frame: &mut Frame, probe_pending: bool) {
    let area = frame.area();
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    let mut lines = vec![Line::from(Span::styled(STARTUP_SPLASH_TEXT, dim))];
    if probe_pending {
        lines.push(Line::from(Span::styled(
            STARTUP_SPLASH_PROBE_PENDING_TEXT,
            dim,
        )));
    }
    // Same "shrink to the text's own height, center via `Alignment` for
    // the horizontal half" split `render_empty_state` in `diff_view.rs`
    // uses — `lines.len()` rows here (one or two, depending on
    // `probe_pending`) rather than the fixed `1` a single-line splash could
    // hardcode, so `top_pad` still centers the whole block vertically.
    let height = lines.len() as u16;
    let top_pad = area.height.saturating_sub(height) / 2;
    let text_area = Rect {
        x: area.x,
        y: area.y + top_pad,
        width: area.width,
        height: area.height.min(height),
    };
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        text_area,
    );
}

/// Draws one [`render_startup_splash`] frame straight onto `terminal`.
/// Called from `run` immediately after `init_terminal` and before
/// `enable_kitty_keyboard_protocol`, whose crossterm-internal probe blocks
/// on a real synchronous tty read that it only bounds at 2s *on a cache
/// miss* (see `probe_cache`'s module docs) — on a terminal with no cached
/// verdict that also never answers the probe (stock Terminal.app, some tmux
/// setups), nothing else would touch the alternate screen `init_terminal`
/// just entered until the event loop's first real `terminal.draw` call,
/// well past config/keymap/LSP setup, so a user on such a terminal was
/// staring at a black screen with just a blinking cursor for up to 2s with
/// no way to tell "starting" from "stuck." This is cheap enough (one or two
/// small paragraph lines, no layout beyond centering) to draw
/// unconditionally rather than only on terminals suspected of being slow —
/// `probe_pending` (see [`STARTUP_SPLASH_PROBE_PENDING_TEXT`]) is what
/// tells a cache-miss launch (about to actually wait) apart from a
/// cache-hit one (about to draw this frame and move straight past it).
///
/// Takes no [`Keymap`]/[`config::KeymapPreset`] and renders no key hints —
/// both depend on `ci_distinguishable`, which only exists once the very
/// probe this function draws ahead of has returned (see `vim_preset`/
/// `emacs_preset`'s callers in `run`), so a splash that needed either would
/// have to wait for the probe first, defeating the point of drawing before
/// it. Drawn once and never refreshed; the real UI takes over at the event
/// loop's own first `terminal.draw`.
fn draw_startup_splash(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    probe_pending: bool,
) -> Result<()> {
    terminal.draw(|frame| render_startup_splash(frame, probe_pending))?;
    Ok(())
}

/// Probes for the kitty keyboard protocol and, if the terminal answers
/// that it supports it, enables its escape-code disambiguation — the
/// mechanism this session needs to tell a literal Tab from `Ctrl-i` apart
/// on the wire (see [`Action::JumpBack`](crate::keymap::Action::JumpBack)'s
/// docs for why that distinction decides `JumpForward`'s canonical
/// binding). Returns whether it ended up active; `run` threads that
/// straight into [`vim_preset`]/[`emacs_preset`].
///
/// Requests only [`KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES`] —
/// not `REPORT_EVENT_TYPES` (this app has no use for key-release/repeat
/// events; every [`Action`] fires on a plain press, matched by
/// `KeyEventKind::Press` in the event loop) and not `REPORT_ALTERNATE_KEYS`
/// (nothing here reads a keyboard-layout alternate keycode) — so the
/// contract this session asks the terminal for is exactly as wide as what
/// it actually acts on.
///
/// `cached` is `run`'s `probe_cache::look_up(cache_path, fingerprint)` —
/// this terminal's previously-recorded verdict, if any (see
/// [`probe_cache`]'s module docs). A hit skips crossterm's real probe
/// entirely: `Some(true)` pushes the enhancement flags directly with no
/// query round trip at all, which is safe to do unconditionally — an
/// unrecognized CSI final byte is conventionally a no-op per ECMA-48, so a
/// terminal that stops understanding it between runs (see the module docs'
/// staleness note) just silently ignores the write, same as it always did
/// for `restore_terminal`'s unconditional pop on the way out — and a cached
/// verdict came from this exact fingerprint answering the real probe `yes`
/// before, so there is nothing left to ask. `Some(false)` returns `false`
/// immediately, same reasoning in the other direction. Only `None` (no
/// cached verdict yet — a first launch in this terminal, or one since
/// `ktmr reset --cache`, or *every* launch inside a multiplexer, since `run`
/// never looks one up there) runs the real probe below.
///
/// `record_verdict` is `run`'s `probe_cache_usable`
/// (`!probe_cache::multiplexed_from_env()`) — when true, the real probe's
/// answer is recorded via `probe_cache::record` before this terminal is
/// ever asked again; when false (inside tmux/screen), the answer is used
/// for this session but never written, since `fingerprint` has already lost
/// the identity a later session under a *different* outer terminal would
/// need it to carry — see [`probe_cache`]'s module docs' **Multiplexers**
/// section.
///
/// Must run after [`enable_raw_mode`] (crossterm's probe blocks on a real
/// synchronous read from the tty — it doesn't work otherwise) and before
/// [`spawn_input_thread`] starts its own blocking `event::read()` loop on a
/// background thread; both read from the same underlying tty through
/// crossterm's internal event-reader lock, so overlapping them risks the
/// probe's response bytes being stolen by the input thread instead (or a
/// deadlock, depending on timing) rather than a hang or a crash, which is
/// worse to debug. `supports_keyboard_enhancement` already bounds its own
/// wait (2s — crossterm 0.29.0's `Duration::from_millis(2000)` in
/// `terminal/sys/unix.rs::query_keyboard_enhancement_flags_raw`, verified
/// against the vendored source this build actually compiles against) and
/// turns "no response," "not a tty" (e.g. output piped in a test harness),
/// or any other I/O hiccup into `Ok(false)`/`Err` — both treated
/// identically here as "not supported," so a plain terminal or a pipe never
/// makes startup fail or hang, it just keeps `M-Right` as the sole working
/// binding for `JumpForward` (see [`Action::JumpBack`]'s docs) — and, as of
/// the probe cache, never pays that 2s bound more than once per terminal
/// either.
fn enable_kitty_keyboard_protocol(
    cached: Option<bool>,
    cache_path: &Path,
    fingerprint: &str,
    record_verdict: bool,
) -> bool {
    if let Some(supported) = cached {
        return supported && push_kitty_enhancement_flags();
    }
    let Ok(true) = supports_keyboard_enhancement() else {
        if record_verdict {
            probe_cache::record(cache_path, fingerprint, false);
        }
        return false;
    };
    if record_verdict {
        probe_cache::record(cache_path, fingerprint, true);
    }
    push_kitty_enhancement_flags()
}

/// The actual `PushKeyboardEnhancementFlags` write, shared by
/// [`enable_kitty_keyboard_protocol`]'s freshly-probed-`true` path and its
/// cached-`true` path — both need the identical write, and neither needs to
/// know which one the other took.
fn push_kitty_enhancement_flags() -> bool {
    execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok()
}

/// Issue #20: best-effort `EnableMouseCapture` — called after
/// [`enable_kitty_keyboard_protocol`] (see `run`'s docs on the ordering,
/// which is about "one place, before the input thread starts," not a probe
/// race the way kitty's ordering is) and before [`spawn_input_thread`]
/// starts reading events, so wheel events arrive on
/// [`AppEvent::Terminal`] from the very first `event::read()` rather than a
/// window where some early scroll silently does nothing. `enabled` is
/// `false` for `[ui] mouse = false` (see [`crate::config::Config::mouse`]'s
/// docs on the native-terminal-selection trade-off this exists for) — a
/// session that opts out never sends the enabling escape sequence at all,
/// matching "keyboard behavior is identical with mouse disabled" (the
/// input thread still forwards whatever bytes arrive; there's just nothing
/// generating SGR mouse sequences for it to forward). Errors are ignored
/// the same way [`enable_kitty_keyboard_protocol`]'s own write is treated —
/// a terminal that can't take this write has bigger problems than a
/// missing scroll feature, and failing startup over it would be worse.
fn enable_mouse_capture(enabled: bool) {
    if enabled {
        let _ = execute!(io::stdout(), EnableMouseCapture);
    }
}

/// The one definition of `restore_terminal`'s ANSI teardown ordering,
/// shared with the composition test below so the test exercises the exact
/// command list production sends rather than a hand-copied duplicate that
/// could silently drift (the test's whole point is the relative order of
/// mouse-off vs leave-alternate-screen).
macro_rules! write_restore_sequence {
    ($writer:expr) => {
        execute!(
            $writer,
            PopKeyboardEnhancementFlags,
            DisableMouseCapture,
            LeaveAlternateScreen
        )
    };
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    // Popping/disabling unconditionally — not gated on whether
    // `enable_kitty_keyboard_protocol`/`enable_mouse_capture` actually did
    // anything on the way in — mirrors `disable_raw_mode`/`LeaveAlternateScreen`
    // right alongside them: every one of these runs every time regardless
    // of what this particular session did on the way in. It's safe to do:
    // a terminal that never understood the enabling sequence in the first
    // place ignores the disabling one the same way (unrecognized CSI final
    // bytes are conventionally no-ops per ECMA-48), the kitty protocol
    // itself specifies popping an empty flag stack as a no-op, and
    // `DisableMouseCapture` is the same shape of paired on/off CSI toggle.
    write_restore_sequence!(terminal.backend_mut())?;
    terminal.show_cursor()?;
    Ok(())
}

/// Ensures a panicking render doesn't leave the user's terminal stuck in
/// raw mode / the alternate screen / the kitty keyboard protocol's escape
/// mode. Installed once, before the terminal is touched, so it's active
/// for the whole session — including the window between
/// `enable_kitty_keyboard_protocol` pushing the flag and `restore_terminal`
/// popping it on a clean exit, which a panic mid-session would otherwise
/// skip entirely.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        // Issue #20: same best-effort, unconditional `DisableMouseCapture`
        // `restore_terminal` sends on a clean exit — a panic mid-session
        // must leave the terminal exactly as un-stuck either way, and this
        // is cheap enough (and safe enough on a terminal that never enabled
        // capture at all — see `restore_terminal`'s docs) to just always
        // send rather than plumbing whether `[ui] mouse` was even on
        // through to the panic hook.
        let _ = execute!(io::stdout(), DisableMouseCapture);
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_root_does_not_start_a_language_server_for_local_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = root.join("a.rs");
        std::fs::write(&file, "fn unrelated_local_code() {}\n").unwrap();
        let mut app = selectable_diff_app();
        app.repo_root = root.clone();
        app.interactive = false;
        let (tx, _rx) = std::sync::mpsc::channel();
        let overrides = std::collections::HashMap::from([(
            "rust".to_owned(),
            crate::config::ServerOverride {
                command: root.join("missing-test-server").display().to_string(),
                ..Default::default()
            },
        )]);
        let manager = LspManager::new(tx, std::sync::Arc::new(overrides), false);

        assert!(warm_up_root(&View::Diff(app), &manager).is_none());
        assert_eq!(manager.state(&file, &root), ServerState::NotStarted);
    }

    // ---- watch_pause_decision ---------------------------------------------

    #[test]
    fn watch_inactive_never_changes_the_pause_flag() {
        assert_eq!(watch_pause_decision(false, true, true), None);
        assert_eq!(watch_pause_decision(false, true, false), None);
    }

    #[test]
    fn a_swap_on_a_pushed_diff_never_changes_the_pause_flag() {
        // `at_root == false`: the scope menu was opened on a diff `L`/`t`
        // (or goto-definition) pushed on top of the root, not the root
        // itself — watch mode only ever refreshes the root, so this has
        // nothing to do with it.
        assert_eq!(watch_pause_decision(true, false, true), None);
        assert_eq!(watch_pause_decision(true, false, false), None);
    }

    #[test]
    fn swapping_the_root_to_working_tree_resumes() {
        assert_eq!(watch_pause_decision(true, true, true), Some(false));
    }

    #[test]
    fn swapping_the_root_to_a_non_working_tree_scope_pauses() {
        // `is_working_tree = false` covers both `Staged` and `Revision` —
        // this function only distinguishes "the working tree" from
        // "anything else," matching the milestone spec's "non-working-tree
        // scope" wording exactly.
        assert_eq!(watch_pause_decision(true, true, false), Some(true));
    }

    // ---- cancel_diff_view -------------------------------------------------

    fn empty_diff_app() -> App {
        App::new("test-repo".to_owned(), PathBuf::from("/repo"), Vec::new())
    }

    /// One context row (`flat_idx` 2 after the file/hunk headers) with the
    /// cursor already on it — the minimum an `App` needs for
    /// `toggle_visual` to start a selection in a `cancel_diff_view` test.
    fn selectable_diff_app() -> App {
        let file = crate::diff::DiffFile {
            old_path: Some("a.rs".to_owned()),
            new_path: Some("a.rs".to_owned()),
            hunks: vec![crate::diff::DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                header: String::new(),
                known_eof: true,
                rows: vec![crate::diff::DiffRow {
                    kind: crate::diff::DiffLineKind::Context,
                    text: "one".to_owned(),
                    old_line: Some(1),
                    new_line: Some(1),
                }],
            }],
            ..Default::default()
        };
        let mut app = App::new("test-repo".to_owned(), PathBuf::from("/repo"), vec![file]);
        app.cursor = 2;
        app
    }

    fn empty_file_view() -> FileView {
        FileView::with_hover_target("f.txt".to_owned(), "hello\n", None)
    }

    #[test]
    fn cancel_clears_an_active_selection_at_root_before_anything_else() {
        let mut app = selectable_diff_app();
        assert_eq!(app.toggle_visual(), app::VisualToggleOutcome::Started);
        let mut stack = ViewStack::new(View::Diff(app));
        let mut hover_state = hover_popup::HoverState::default();
        let mut pending_hover: Option<PendingHover> = None;
        let mut pending_goto: Option<(JumpEntry, PendingGoto)> = None;
        let mut goto_status: Option<String> = None;

        cancel_diff_view(
            &mut stack,
            &mut hover_state,
            &mut pending_hover,
            &mut pending_goto,
            &crate::lsp::ObservationStore::in_memory(),
            &mut goto_status,
        );

        let View::Diff(app) = stack.top() else {
            panic!("still the root diff");
        };
        assert!(!app.visual_active(), "the first Esc spends itself here");
        assert_eq!(goto_status.as_deref(), Some("visual selection cancelled"));
    }

    #[test]
    fn cancel_clears_a_selection_on_a_pushed_diff_instead_of_popping() {
        // The selection is the most local transient layer, so one Esc must
        // consume it and leave the pushed view in place; only the *next*
        // Esc pops — exactly one layer per press.
        let mut pushed = selectable_diff_app();
        assert_eq!(pushed.toggle_visual(), app::VisualToggleOutcome::Started);
        let mut stack = ViewStack::new(View::Diff(empty_diff_app()));
        stack.push(View::Diff(pushed));
        let mut hover_state = hover_popup::HoverState::default();
        let mut pending_hover: Option<PendingHover> = None;
        let mut pending_goto: Option<(JumpEntry, PendingGoto)> = None;
        let mut goto_status: Option<String> = None;

        cancel_diff_view(
            &mut stack,
            &mut hover_state,
            &mut pending_hover,
            &mut pending_goto,
            &crate::lsp::ObservationStore::in_memory(),
            &mut goto_status,
        );
        assert!(
            !stack.is_at_root(),
            "the first Esc cancels the selection, not the view"
        );
        let View::Diff(app) = stack.top() else {
            panic!("the pushed diff is still on top");
        };
        assert!(!app.visual_active());

        cancel_diff_view(
            &mut stack,
            &mut hover_state,
            &mut pending_hover,
            &mut pending_goto,
            &crate::lsp::ObservationStore::in_memory(),
            &mut goto_status,
        );
        assert!(stack.is_at_root(), "the second Esc pops the pushed diff");
    }

    #[test]
    fn cancel_pops_a_pushed_file_view_and_reveals_the_diff_below() {
        let mut stack = ViewStack::new(View::Diff(empty_diff_app()));
        stack.push(View::File(empty_file_view()));
        let mut hover_state = hover_popup::HoverState::default();
        let mut pending_hover: Option<PendingHover> = None;
        let mut pending_goto: Option<(JumpEntry, PendingGoto)> = None;
        let mut goto_status: Option<String> = None;

        cancel_diff_view(
            &mut stack,
            &mut hover_state,
            &mut pending_hover,
            &mut pending_goto,
            &crate::lsp::ObservationStore::in_memory(),
            &mut goto_status,
        );

        assert!(stack.is_at_root(), "exactly one view must be popped");
        assert!(matches!(stack.top(), View::Diff(_)));
    }

    #[test]
    fn cancel_pops_exactly_one_pushed_nested_diff() {
        // A `Diff` pushed on top of another `Diff` — the shape `LogView`'s
        // `Confirm` produces (see `ui::mod::handle_action`'s `Action::Confirm`
        // arm) — pops the same way a pushed `File`/`Timeline`/`Log` does,
        // without first clearing its own search/unit filter: that clearing
        // role is reserved for the root (see `cancel_diff_view`'s docs).
        let mut stack = ViewStack::new(View::Diff(empty_diff_app()));
        stack.push(View::Diff(empty_diff_app()));
        let mut hover_state = hover_popup::HoverState::default();
        let mut pending_hover: Option<PendingHover> = None;
        let mut pending_goto: Option<(JumpEntry, PendingGoto)> = None;
        let mut goto_status: Option<String> = None;

        cancel_diff_view(
            &mut stack,
            &mut hover_state,
            &mut pending_hover,
            &mut pending_goto,
            &crate::lsp::ObservationStore::in_memory(),
            &mut goto_status,
        );

        assert!(stack.is_at_root());
    }

    #[test]
    fn cancel_at_the_root_file_view_is_a_no_op() {
        let mut stack = ViewStack::new(View::File(empty_file_view()));
        let mut hover_state = hover_popup::HoverState::default();
        let mut pending_hover: Option<PendingHover> = None;
        let mut pending_goto: Option<(JumpEntry, PendingGoto)> = None;
        let mut goto_status: Option<String> = None;

        cancel_diff_view(
            &mut stack,
            &mut hover_state,
            &mut pending_hover,
            &mut pending_goto,
            &crate::lsp::ObservationStore::in_memory(),
            &mut goto_status,
        );

        // `ViewStack::pop` refuses to pop the last remaining view — the
        // root `File` is still there, and there was never anything else
        // this arm could do with it (no search/unit-filter concept
        // outside `View::Diff`).
        assert!(stack.is_at_root());
        assert!(matches!(stack.top(), View::File(_)));
    }

    #[test]
    fn cancel_at_the_root_diff_clears_the_search_instead_of_popping() {
        let mut app = empty_diff_app();
        // A confirmed (if immediately match-less, on an empty diff) search
        // is enough to exercise `App::clear_search`'s guard without needing
        // real diff content — `cancel_diff_view` only needs to prove it
        // *tried* to clear something at the root rather than popping.
        let origin = refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);
        app.recompute_search_live("anything", &origin);
        assert!(app.search.as_ref().unwrap().highlight_visible);

        let mut stack = ViewStack::new(View::Diff(app));
        let mut hover_state = hover_popup::HoverState::default();
        let mut pending_hover: Option<PendingHover> = None;
        let mut pending_goto: Option<(JumpEntry, PendingGoto)> = None;
        let mut goto_status: Option<String> = None;

        cancel_diff_view(
            &mut stack,
            &mut hover_state,
            &mut pending_hover,
            &mut pending_goto,
            &crate::lsp::ObservationStore::in_memory(),
            &mut goto_status,
        );

        assert!(stack.is_at_root(), "the root view is never popped");
        let View::Diff(app) = stack.top() else {
            panic!("expected the root diff");
        };
        assert!(
            !app.search.as_ref().unwrap().highlight_visible,
            "Esc at the root must clear the confirmed search, not pop anything"
        );
    }

    #[test]
    fn cancel_that_pops_also_cancels_an_in_flight_hover_and_goto_request() {
        let mut stack = ViewStack::new(View::Diff(empty_diff_app()));
        stack.push(View::File(empty_file_view()));

        let mut hover_state = hover_popup::HoverState::default();
        hover_state.set_pending();
        let generation_before = hover_state.generation();

        let (_tx, rx) = mpsc::channel();
        let mut pending_hover: Option<PendingHover> = None;
        let mut pending_goto: Option<(JumpEntry, PendingGoto)> = Some((
            JumpEntry {
                file: PathBuf::from("a.rs"),
                git_root: PathBuf::from("."),
                line: Some(0),
                col: 0,
            },
            PendingGoto::Definition {
                operation_id: 1,
                rx,
            },
        ));

        let mut goto_status: Option<String> = None;
        cancel_diff_view(
            &mut stack,
            &mut hover_state,
            &mut pending_hover,
            &mut pending_goto,
            &crate::lsp::ObservationStore::in_memory(),
            &mut goto_status,
        );

        assert!(stack.is_at_root(), "the pushed FileView must have popped");
        assert_ne!(generation_before, hover_state.generation());
        assert!(pending_goto.is_none());
    }

    // ---- cancel_pending_lsp_requests ------------------------------

    #[test]
    fn opening_help_invalidates_a_pending_hover_and_drops_a_pending_goto() {
        let mut hover_state = hover_popup::HoverState::default();
        hover_state.set_pending();
        let generation_before = hover_state.generation();

        let (_tx, rx) = mpsc::channel();
        let mut pending_hover: Option<PendingHover> = None;
        let mut pending_goto: Option<(JumpEntry, PendingGoto)> = Some((
            JumpEntry {
                file: PathBuf::from("a.rs"),
                git_root: PathBuf::from("."),
                line: Some(0),
                col: 0,
            },
            PendingGoto::Definition {
                operation_id: 1,
                rx,
            },
        ));

        cancel_pending_lsp_requests(
            &mut hover_state,
            &mut pending_hover,
            &mut pending_goto,
            &crate::lsp::ObservationStore::in_memory(),
        );

        // The pending hover is discarded by bumping the generation
        // (mirroring a cursor move), not by touching `Status::Pending`
        // directly — a late response tagged with the old generation is
        // simply ignored by `apply` whenever it does arrive.
        assert_ne!(generation_before, hover_state.generation());
        assert!(!hover_state.is_open());
        // `pending_goto` has no generation to lean on, so it's dropped
        // outright — its `Receiver` (and whatever answer arrives on it)
        // goes with it.
        assert!(pending_goto.is_none());
    }

    // ---- check_action_readiness --------------------------------------------

    #[test]
    fn check_action_readiness_leaves_an_unconfigured_file_type_alone() {
        let (events_tx, _events_rx) = mpsc::channel();
        let lsp_manager =
            LspManager::new(events_tx, Arc::new(std::collections::HashMap::new()), false);
        let query = hover_popup::HoverQuery {
            file: PathBuf::from("/repo/README.md"),
            git_root: PathBuf::from("/repo"),
            line: 0,
            line_text: "hello".to_owned(),
            display_col: 0,
        };
        // No adapter claims `.md` — this is the pre-existing "unsupported
        // file type" path through `LspManager::submit` itself (see
        // `check_action_readiness`'s docs), not a readiness question, so
        // it must return `None` here and let the caller's ordinary
        // dispatch surface that the same way it always has.
        assert_eq!(check_action_readiness(&lsp_manager, &query, "hover"), None);
    }

    #[test]
    fn check_action_readiness_reports_starting_and_kicks_off_a_not_started_server() {
        let (events_tx, _events_rx) = mpsc::channel();
        let lsp_manager =
            LspManager::new(events_tx, Arc::new(std::collections::HashMap::new()), false);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let file = dir.path().join("main.rs");
        let query = hover_popup::HoverQuery {
            file: file.clone(),
            git_root: dir.path().to_path_buf(),
            line: 0,
            line_text: "fn main() {}".to_owned(),
            display_col: 3,
        };

        assert_eq!(
            lsp_manager.state(&file, dir.path()),
            ServerState::NotStarted
        );

        let message = check_action_readiness(&lsp_manager, &query, "go to definition")
            .expect("a NotStarted server must never dispatch");
        assert!(message.contains("is starting"), "{message}");
        assert!(
            message.contains("go to definition is not ready yet"),
            "{message}"
        );

        // The whole point of routing `NotStarted` through here: the server
        // actually got kicked off (via `ensure_started`), even though
        // nothing was dispatched for it to answer.
        assert_ne!(
            lsp_manager.state(&file, dir.path()),
            ServerState::NotStarted
        );
    }

    // ---- peek_action_readiness (issue #23) -----------------------------------

    /// The whole point of this function existing separately from
    /// [`check_action_readiness`]: a context menu can peek a `NotStarted`
    /// server's readiness every frame it sits open without that ever being
    /// the thing that kicks the server off — only actually confirming the
    /// entry (through the real `handle_action` arm) does that.
    #[test]
    fn peek_action_readiness_reports_not_started_without_starting_the_server() {
        let (events_tx, _events_rx) = mpsc::channel();
        let lsp_manager =
            LspManager::new(events_tx, Arc::new(std::collections::HashMap::new()), false);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let file = dir.path().join("main.rs");
        let query = hover_popup::HoverQuery {
            file: file.clone(),
            git_root: dir.path().to_path_buf(),
            line: 0,
            line_text: "fn main() {}".to_owned(),
            display_col: 3,
        };
        assert_eq!(
            lsp_manager.state(&file, dir.path()),
            ServerState::NotStarted
        );

        let message = peek_action_readiness(&lsp_manager, &query, "go to definition")
            .expect("a NotStarted server must never report Ready");
        assert!(message.contains("is starting"), "{message}");

        assert_eq!(
            lsp_manager.state(&file, dir.path()),
            ServerState::NotStarted,
            "peeking must never kick the server off — see `check_action_readiness` for that"
        );
    }

    #[test]
    fn peek_action_readiness_leaves_an_unconfigured_file_type_alone() {
        let (events_tx, _events_rx) = mpsc::channel();
        let lsp_manager =
            LspManager::new(events_tx, Arc::new(std::collections::HashMap::new()), false);
        let query = hover_popup::HoverQuery {
            file: PathBuf::from("/repo/README.md"),
            git_root: PathBuf::from("/repo"),
            line: 0,
            line_text: "hello".to_owned(),
            display_col: 0,
        };
        assert_eq!(peek_action_readiness(&lsp_manager, &query, "hover"), None);
    }

    // ---- fire_pointer_hover / cancel_pending_pointer_hover (issue #24) -----

    #[test]
    fn fire_pointer_hover_on_a_not_ready_server_never_populates_pending_or_starts_it() {
        let (events_tx, _events_rx) = mpsc::channel();
        let lsp_manager =
            LspManager::new(events_tx, Arc::new(std::collections::HashMap::new()), false);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let file = dir.path().join("main.rs");
        let query = hover_popup::HoverQuery {
            file: file.clone(),
            git_root: dir.path().to_path_buf(),
            line: 0,
            line_text: "fn main() {}".to_owned(),
            display_col: 3,
        };
        assert_eq!(
            lsp_manager.state(&file, dir.path()),
            ServerState::NotStarted
        );

        let target = pointer_hover::PointerTarget::Code(query);
        let mut pointer_hover_state = pointer_hover::PointerHoverState::default();
        let mut pending: Option<PendingHover> = None;
        let observer = crate::lsp::ObservationStore::in_memory();
        let view = View::Diff(App::new(
            "test-repo".to_owned(),
            dir.path().to_path_buf(),
            Vec::new(),
        ));
        let diagnostics = DiagnosticsStore::new();

        fire_pointer_hover(
            &target,
            0,
            &mut pointer_hover_state,
            &mut pending,
            &lsp_manager,
            &observer,
            &diagnostics,
            &view,
        );

        assert!(
            pending.is_none(),
            "a not-ready server must never populate a pending passive request"
        );
        assert_eq!(
            lsp_manager.state(&file, dir.path()),
            ServerState::NotStarted,
            "passive hover must never call ensure_started — a drifting mouse must not launch a server"
        );
        assert!(
            observer.events().is_empty(),
            "req 4: passive hover stays silent for a not-ready state, no journal entry either"
        );
    }

    #[test]
    fn fire_pointer_hover_on_a_tree_target_shows_the_tooltip_synchronously_with_no_lsp_involved() {
        let file = crate::diff::DiffFile {
            old_path: Some("src/a.rs".to_owned()),
            new_path: Some("src/a.rs".to_owned()),
            ..Default::default()
        };
        let app = App::new("test-repo".to_owned(), PathBuf::from("/repo"), vec![file]);
        let id = app.visible_rows[0].id.clone();
        let view = View::Diff(app);

        let (events_tx, _events_rx) = mpsc::channel();
        let lsp_manager =
            LspManager::new(events_tx, Arc::new(std::collections::HashMap::new()), false);
        let observer = crate::lsp::ObservationStore::in_memory();
        let diagnostics = DiagnosticsStore::new();
        let mut pointer_hover_state = pointer_hover::PointerHoverState::default();
        let mut pending: Option<PendingHover> = None;

        let target = pointer_hover::PointerTarget::Tree(id);
        fire_pointer_hover(
            &target,
            3,
            &mut pointer_hover_state,
            &mut pending,
            &lsp_manager,
            &observer,
            &diagnostics,
            &view,
        );

        assert!(
            pending.is_none(),
            "a tree lookup is synchronous — it never goes through the async pending-request path"
        );
        assert!(
            pointer_hover_state.tree_status_hint().is_some(),
            "a resolvable tree target shows its tooltip immediately"
        );
        assert!(
            observer.events().is_empty(),
            "a tree lookup has nothing to do with LSP — no journal entry"
        );
    }

    #[test]
    fn cancel_pending_pointer_hover_with_nothing_in_flight_records_no_journal_entry() {
        let mut pointer_hover_state = pointer_hover::PointerHoverState::default();
        pointer_hover_state.arm(
            pointer_hover::PointerTarget::Tree(file_tree::NodeId {
                path: "src".to_owned(),
                is_directory: true,
            }),
            0,
            std::time::Instant::now(),
        );
        let mut pending: Option<PendingHover> = None;
        let observer = crate::lsp::ObservationStore::in_memory();

        cancel_pending_pointer_hover(&mut pointer_hover_state, &mut pending, &observer);

        assert!(pending.is_none());
        // The `Armed` debounce is gone — even a deadline far in the future
        // is no longer due for it.
        assert_eq!(
            pointer_hover_state.due(std::time::Instant::now() + Duration::from_secs(3600)),
            None
        );
        assert!(
            observer.events().is_empty(),
            "nothing was ever queued for a cancel to supersede"
        );
    }

    #[test]
    fn cancel_pending_pointer_hover_drops_the_receiver_and_records_superseded() {
        let mut pointer_hover_state = pointer_hover::PointerHoverState::default();
        let (_tx, rx) = mpsc::channel();
        let mut pending: Option<PendingHover> = Some((0, 42, rx));
        let observer = crate::lsp::ObservationStore::in_memory();

        cancel_pending_pointer_hover(&mut pointer_hover_state, &mut pending, &observer);

        assert!(pending.is_none(), "the in-flight receiver must be dropped");
        let events = observer.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].operation_id, Some(42));
        assert_eq!(events[0].outcome, crate::lsp::EventOutcome::Superseded);
    }

    // ---- passive_hover_suppressed (issue #24) -------------------------------

    /// Every argument defaulted to "not suppressing" — each test below flips
    /// exactly one to prove that condition alone is enough, without having
    /// to restate every other parameter each time.
    #[allow(clippy::too_many_arguments)]
    fn suppressed(
        compose: bool,
        search_prompt: bool,
        help: bool,
        scope_menu: bool,
        units_setup: bool,
        refs_panel: bool,
        units_panel: bool,
        context_menu: bool,
        hover_blocks: bool,
        visual_active: bool,
    ) -> bool {
        let compose = compose.then(|| {
            ComposeState::new(app::CommentTarget::Single {
                file: "a.rs".to_owned(),
                line: 1,
            })
        });
        let search_prompt = search_prompt.then(|| search::SearchPromptState {
            input: search::SearchInput::new(),
            origin: refresh::capture_anchor(&[], &[], 0, 0),
            jump_from: None,
        });
        let help = help.then(HelpState::default);
        let scope_menu = scope_menu.then(|| ScopeMenuState::new_list(false));
        let units_setup = units_setup.then(|| {
            UnitsSetupState::new(vec![crate::groups::agent::AgentCli {
                kind: crate::groups::agent::AgentKind::Claude,
                path: PathBuf::from("/bin/claude"),
            }])
        });
        let refs_panel = refs_panel.then(|| RefsPanelState {
            git_root: PathBuf::from("/repo"),
            panel: RefsPanel::new("test".to_owned(), Vec::new(), 0),
            operation_id: 0,
        });
        let units_panel = units_panel.then(|| {
            UnitsPanel::build(
                &crate::groups::Grouping {
                    diff_key: String::new(),
                    agent: "claude".to_owned(),
                    created_at: 0,
                    units: Vec::new(),
                },
                &[],
            )
        });
        let context_menu = context_menu.then(|| {
            ContextMenuState::new(
                MenuTarget::DiffRow,
                vec![context_menu::MenuEntry {
                    label: "hover".to_owned(),
                    enabled: Ok(()),
                    command: MenuCommand::Action(Action::Hover),
                }],
                (0, 0),
            )
        });
        let mut hover_state = hover_popup::HoverState::default();
        if hover_blocks {
            hover_state.invalidate();
            hover_state.set_pending();
        }
        let mut app = App::new(
            "test-repo".to_owned(),
            PathBuf::from("/repo"),
            vec![crate::diff::DiffFile {
                old_path: Some("a.rs".to_owned()),
                new_path: Some("a.rs".to_owned()),
                hunks: vec![crate::diff::DiffHunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 1,
                    header: String::new(),
                    known_eof: true,
                    rows: vec![crate::diff::DiffRow {
                        kind: crate::diff::DiffLineKind::Context,
                        text: "alpha".to_owned(),
                        old_line: Some(1),
                        new_line: Some(1),
                    }],
                }],
                ..Default::default()
            }],
        );
        if visual_active {
            app.cursor = 2; // the one `RenderRow::Line` in this fixture
            let outcome = app.toggle_visual();
            debug_assert!(matches!(outcome, app::VisualToggleOutcome::Started));
        }
        let view = View::Diff(app);

        passive_hover_suppressed(
            &compose,
            &search_prompt,
            &help,
            &scope_menu,
            &units_setup,
            &refs_panel,
            &units_panel,
            &context_menu,
            &hover_state,
            &view,
        )
    }

    #[test]
    fn nothing_open_never_suppresses() {
        assert!(!suppressed(
            false, false, false, false, false, false, false, false, false, false
        ));
    }

    #[test]
    fn compose_suppresses() {
        assert!(suppressed(
            true, false, false, false, false, false, false, false, false, false
        ));
    }

    #[test]
    fn search_prompt_suppresses() {
        assert!(suppressed(
            false, true, false, false, false, false, false, false, false, false
        ));
    }

    #[test]
    fn help_suppresses() {
        assert!(suppressed(
            false, false, true, false, false, false, false, false, false, false
        ));
    }

    #[test]
    fn scope_menu_suppresses() {
        assert!(suppressed(
            false, false, false, true, false, false, false, false, false, false
        ));
    }

    #[test]
    fn units_setup_suppresses() {
        assert!(suppressed(
            false, false, false, false, true, false, false, false, false, false
        ));
    }

    #[test]
    fn refs_panel_suppresses() {
        assert!(suppressed(
            false, false, false, false, false, true, false, false, false, false
        ));
    }

    #[test]
    fn units_panel_suppresses() {
        assert!(suppressed(
            false, false, false, false, false, false, true, false, false, false
        ));
    }

    #[test]
    fn context_menu_suppresses() {
        assert!(suppressed(
            false, false, false, false, false, false, false, true, false, false
        ));
    }

    #[test]
    fn an_explicit_hover_pending_or_shown_suppresses() {
        assert!(suppressed(
            false, false, false, false, false, false, false, false, true, false
        ));
    }

    #[test]
    fn an_active_visual_selection_suppresses() {
        assert!(suppressed(
            false, false, false, false, false, false, false, false, false, true
        ));
    }

    // ---- readiness_status_message ------------------------------------------

    #[test]
    fn readiness_status_message_is_none_only_for_ready() {
        let lang = LangKey::Custom("stubls".to_owned());
        assert_eq!(
            readiness_status_message(ActionReadiness::Ready, &lang, "hover"),
            None
        );
    }

    #[test]
    fn readiness_status_message_names_the_language_and_action_while_starting() {
        let lang = LangKey::Custom("stubls".to_owned());
        for readiness in [ActionReadiness::NotStarted, ActionReadiness::Starting] {
            assert_eq!(
                readiness_status_message(readiness, &lang, "go to definition").as_deref(),
                Some("LSP: stubls is starting; go to definition is not ready yet")
            );
        }
    }

    #[test]
    fn readiness_status_message_preserves_the_live_install_progress_line() {
        // The `Installing` message is the auto-installer's own live
        // progress line — swallowing it here would replace real progress
        // ("npm install …") with a generic shrug, exactly what issue #11's
        // "not overwritten by a generic ellipsis" criterion forbids.
        let lang = LangKey::Custom("stubls".to_owned());
        assert_eq!(
            readiness_status_message(
                ActionReadiness::Installing {
                    message: "npm install pyright@1.1.411".to_owned(),
                },
                &lang,
                "hover",
            )
            .as_deref(),
            Some("LSP: npm install pyright@1.1.411; hover unavailable until ready")
        );
    }

    #[test]
    fn readiness_status_message_passes_an_unavailable_reason_through_verbatim() {
        // `Unavailable`/`Crashed` reasons already read like actionable
        // status-bar hints ("LSP: typescript ✕ — npm i -g …"), so any
        // decoration here would just bury the fix instruction.
        let lang = LangKey::Custom("stubls".to_owned());
        let reason = "LSP: rust \u{2715} \u{2014} rust-analyzer not found".to_owned();
        assert_eq!(
            readiness_status_message(
                ActionReadiness::Unavailable {
                    reason: reason.clone(),
                },
                &lang,
                "hover",
            ),
            Some(reason)
        );
    }

    // ---- supersede_pending_goto --------------------------------------------

    #[test]
    fn supersede_pending_goto_drops_the_in_flight_request_outright() {
        // Regression guard for the not-ready path specifically: a gd/gr
        // press that can't dispatch still must drop an older in-flight
        // navigation, or that stale response would fire later and jump the
        // view / overwrite the "not ready" status the press just showed.
        let (_tx, rx) = mpsc::channel();
        let mut pending_goto: Option<(JumpEntry, PendingGoto)> = Some((
            JumpEntry {
                file: PathBuf::from("a.rs"),
                git_root: PathBuf::from("."),
                line: Some(0),
                col: 0,
            },
            PendingGoto::Definition {
                operation_id: 7,
                rx,
            },
        ));

        supersede_pending_goto(
            &mut pending_goto,
            &crate::lsp::ObservationStore::in_memory(),
            "navigation request superseded by a newer definition action",
        );

        assert!(pending_goto.is_none());
    }

    // ---- files_focus_blocked_message (issue #14) ---------------------------

    #[test]
    fn every_diff_content_action_is_blocked_with_a_named_status() {
        for action in [
            Action::Hover,
            Action::GotoDefinition,
            Action::FindReferences,
            Action::AddComment,
            Action::ExpandFold,
            Action::CollapseFold,
            Action::OpenSearch,
            Action::NextMatch,
            Action::PrevMatch,
            Action::NextDiagnostic,
            Action::PrevDiagnostic,
            Action::ToggleVisualLine,
            Action::YankSelection,
        ] {
            let message = files_focus_blocked_message(action)
                .unwrap_or_else(|| panic!("{action:?} must be blocked while Files is focused"));
            assert!(
                message.ends_with("focus the diff pane first"),
                "{action:?}: {message}"
            );
        }
    }

    #[test]
    fn movement_confirm_and_pane_toggles_pass_through_unblocked() {
        // The req 5/§9 boundary: these all either already make sense while
        // `Files` is focused (movement routes inside `App::update` itself),
        // or need to keep working regardless of focus (`Cancel`/jump
        // history/pane-focus cycling/quitting/help) — none of them are
        // diff-content actions this gate exists to shield.
        for action in [
            Action::CursorDown,
            Action::CursorUp,
            Action::Top,
            Action::Bottom,
            Action::HalfPageDown,
            Action::HalfPageUp,
            Action::NextHunk,
            Action::PrevHunk,
            Action::NextFile,
            Action::PrevFile,
            Action::NextSymbol,
            Action::PrevSymbol,
            Action::Confirm,
            Action::Cancel,
            Action::JumpBack,
            Action::JumpForward,
            Action::FocusNextPane,
            Action::FocusPrevPane,
            Action::ToggleSidebar,
            Action::ToggleLayout,
            Action::ToggleComments,
            Action::ToggleTimeline,
            Action::ToggleLogView,
            Action::ToggleLspInspector,
            Action::ToggleUnits,
            Action::RegenerateUnits,
            Action::ToggleHints,
            Action::ToggleRangeSelect,
            Action::OpenScopeMenu,
            Action::OpenHelp,
            Action::Quit,
        ] {
            assert_eq!(
                files_focus_blocked_message(action),
                None,
                "{action:?} must not be blocked by the files-focus gate"
            );
        }
    }

    // ---- terminal-restore sequence composition (issue #20) ---------------

    #[test]
    fn restore_terminal_disables_mouse_capture_before_leaving_the_alternate_screen() {
        // Drives the same `write_restore_sequence!` invocation
        // `restore_terminal` itself runs (see the macro's docs — that
        // sharing is the point: a hand-copied command list here could
        // silently drift from production), against an in-memory writer
        // instead of a real PTY or a panicking-process E2E test, both of
        // which destabilize the test terminal.
        let mut written: Vec<u8> = Vec::new();
        write_restore_sequence!(&mut written).unwrap();
        let written = String::from_utf8(written).unwrap();

        let mouse_off = written
            .find("\x1b[?1006l")
            .expect("DisableMouseCapture must emit the SGR-mouse-off sequence");
        let leave_alt_screen = written
            .find("\x1b[?1049l")
            .expect("LeaveAlternateScreen must emit its own sequence");
        assert!(
            mouse_off < leave_alt_screen,
            "mouse capture must be disabled before leaving the alternate \
             screen, not after — a terminal that only honors the disable \
             sequence while still in the alternate screen would otherwise \
             leave capture stuck on; got:\n{written:?}"
        );
    }

    // ---- startup splash ----------------------------------------------------

    #[test]
    fn startup_splash_renders_the_marker_text_centered() {
        use ratatui::backend::TestBackend;

        let width = 40;
        let height = 10;
        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        // `probe_pending: false` — the cache-hit case, and the one every
        // pre-existing assertion below (single centered row) was written
        // against; the two-line, `probe_pending: true` case gets its own
        // test just below.
        terminal
            .draw(|frame| render_startup_splash(frame, false))
            .unwrap();
        let buffer = terminal.backend().buffer();

        // Same "concatenate one row's cells" reader `diff_view`'s own
        // `row_text` uses — needed here (rather than a whole-buffer
        // substring check) to also assert *which* row and that it's
        // horizontally centered on it, not just that the text exists
        // somewhere on screen.
        let row_text = |y: u16| -> String {
            (0..width)
                .map(|x| buffer.cell((x, y)).map_or(" ", |c| c.symbol()))
                .collect()
        };

        let marker_row = (0..height)
            .find(|&y| row_text(y).contains(STARTUP_SPLASH_TEXT))
            .expect("draw_startup_splash must render its marker text somewhere on screen");

        // `harness.rs`'s readiness wait treats this as "not a real frame
        // yet" precisely because the marker is what it greps for — a
        // regression here (wrong text, or text that never lands on screen
        // at all) would silently break that contract without tripping any
        // other test, since the harness would just wait past a frame it
        // should have kept waiting past anyway. See `STARTUP_SPLASH_TEXT`'s
        // docs on the two sides staying in sync.
        assert_eq!(
            marker_row,
            (height - 1) / 2,
            "one line of text on a height-{height} screen must land on the \
             vertically centered row (matching render_startup_splash's own \
             top_pad formula)"
        );

        let text = row_text(marker_row);
        let start = text
            .find(STARTUP_SPLASH_TEXT)
            .expect("already confirmed present on this row");
        let leading = text[..start].chars().filter(|c| *c != ' ').count();
        assert_eq!(
            leading, 0,
            "no non-space cell should precede the marker text on its row"
        );
        let end = start + STARTUP_SPLASH_TEXT.chars().count();
        let trailing_text: String = text.chars().skip(end).collect();
        assert!(
            trailing_text.trim().is_empty(),
            "no non-space cell should follow the marker text on its row: {trailing_text:?}"
        );

        // Horizontal centering: the gap on the left and right of the text
        // must be within one cell of each other (an odd leftover column
        // rounds to one side or the other, same as `render_empty_state`'s
        // own `Alignment::Center` everywhere else in the UI).
        let left_gap = start as i32;
        let right_gap = (width as usize - end) as i32;
        assert!(
            (left_gap - right_gap).abs() <= 1,
            "left gap {left_gap} and right gap {right_gap} must differ by at \
             most one column for text centered via Alignment::Center"
        );
    }

    #[test]
    fn startup_splash_adds_a_second_line_only_when_the_probe_is_pending() {
        use ratatui::backend::TestBackend;

        // Wide enough for `STARTUP_SPLASH_PROBE_PENDING_TEXT` (64 chars) to
        // render on one row without truncating — `Paragraph` clips rather
        // than wraps here (no `.wrap(...)` call), so a narrower width would
        // make the `contains` check below fail for a reason that has
        // nothing to do with the claim this test exists to prove.
        let width = 80;
        let height = 10;
        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_startup_splash(frame, true))
            .unwrap();
        let buffer = terminal.backend().buffer();

        let row_text = |y: u16| -> String {
            (0..width)
                .map(|x| buffer.cell((x, y)).map_or(" ", |c| c.symbol()))
                .collect()
        };
        let contents: Vec<String> = (0..height).map(row_text).collect();

        let marker_row = contents
            .iter()
            .position(|row| row.contains(STARTUP_SPLASH_TEXT))
            .expect("the first line must still render on a cache miss");
        let pending_row = contents
            .iter()
            .position(|row| row.contains(STARTUP_SPLASH_PROBE_PENDING_TEXT))
            .expect(
                "probe_pending: true must render the second, \
                 first-run-in-this-terminal line",
            );

        // Two lines, block-centered: the pending line sits directly beneath
        // the marker line, and together they occupy the same vertically
        // centered pair of rows `render_startup_splash`'s `top_pad` formula
        // computes for a height-2 block — not the single-line row the
        // `probe_pending: false` test above lands on.
        assert_eq!(
            pending_row,
            marker_row + 1,
            "the pending line must render immediately below the marker line"
        );
        assert_eq!(
            marker_row,
            ((height - 2) / 2) as usize,
            "a two-line block must start one row above where the single-line \
             splash centers, per top_pad = (area.height - lines.len()) / 2"
        );
    }

    // ---- context menu: handle_action's interception block (issue #23) -----

    /// Calls the real [`handle_action`] with every param this suite's
    /// context-menu tests don't need to individually observe filled in with
    /// an inert default — `context_menu`/`compose`/`goto_status` stay as
    /// out-params since those are exactly what each test asserts on.
    #[allow(clippy::too_many_arguments)]
    fn call_handle_action(
        action: Action,
        stack: &mut ViewStack,
        context_menu: &mut Option<ContextMenuState>,
        compose: &mut Option<ComposeState>,
        goto_status: &mut Option<String>,
        lsp_manager: &LspManager,
        observer: &crate::lsp::ObservationHandle,
    ) {
        let mut hover_state = hover_popup::HoverState::default();
        let mut pending_hover: Option<PendingHover> = None;
        let mut pending_goto: Option<(JumpEntry, PendingGoto)> = None;
        let mut refs_panel: Option<RefsPanelState> = None;
        let mut units_panel: Option<UnitsPanel> = None;
        let mut units_setup: Option<UnitsSetupState> = None;
        let mut pending_units: Option<Receiver<Result<crate::groups::Grouping, String>>> = None;
        let mut units_status: Option<String> = None;
        let mut jump_stack = JumpStack::new();
        let diagnostics = DiagnosticsStore::new();
        let mut scope_menu: Option<ScopeMenuState> = None;
        let mut pr_scope = PrScopeFetch::default();
        let mut help: Option<HelpState> = None;
        let mut search_prompt: Option<search::SearchPromptState> = None;
        let mut highlighter = LineHighlighter::new();
        let mut watch_paused = false;
        let mut watch_status: Option<WatchStatus> = None;
        let mut moving_scope: Option<MovingScopeState> = None;
        let mut hints_expanded = false;

        handle_action(
            action,
            stack,
            &mut hover_state,
            &mut pending_hover,
            &mut pending_goto,
            &mut refs_panel,
            &mut units_panel,
            &mut units_setup,
            &mut pending_units,
            &mut units_status,
            &mut jump_stack,
            &diagnostics,
            goto_status,
            lsp_manager,
            None,
            compose,
            &mut scope_menu,
            &mut pr_scope,
            context_menu,
            &mut help,
            &mut search_prompt,
            &mut highlighter,
            &mut watch_paused,
            false,
            &mut watch_status,
            &mut moving_scope,
            &mut hints_expanded,
            observer,
        );
    }

    fn inert_lsp_manager() -> LspManager {
        let (events_tx, _events_rx) = mpsc::channel();
        LspManager::new(events_tx, Arc::new(std::collections::HashMap::new()), false)
    }

    #[test]
    fn context_menu_confirm_on_an_enabled_entry_closes_and_dispatches_add_comment() {
        let app = selectable_diff_app();
        let mut stack = ViewStack::new(View::Diff(app));
        let entry = context_menu::MenuEntry {
            label: "Add comment".to_owned(),
            enabled: Ok(()),
            command: MenuCommand::Action(Action::AddComment),
        };
        let mut context_menu = Some(ContextMenuState::new(
            MenuTarget::DiffRow,
            vec![entry],
            (0, 0),
        ));
        let mut compose: Option<ComposeState> = None;
        let mut goto_status: Option<String> = None;
        let lsp_manager = inert_lsp_manager();

        call_handle_action(
            Action::Confirm,
            &mut stack,
            &mut context_menu,
            &mut compose,
            &mut goto_status,
            &lsp_manager,
            &crate::lsp::ObservationStore::in_memory(),
        );

        assert!(
            context_menu.is_none(),
            "confirming an enabled entry always closes the menu first"
        );
        assert!(
            compose.is_some(),
            "AddComment must dispatch through the real Action::AddComment arm \
             (req 8) — this is req 8 proven by observation, not by reading code"
        );
    }

    #[test]
    fn context_menu_confirm_dispatches_toggle_visual_line_and_starts_a_selection() {
        let app = selectable_diff_app();
        let mut stack = ViewStack::new(View::Diff(app));
        let entry = context_menu::MenuEntry {
            label: "Start visual selection".to_owned(),
            enabled: Ok(()),
            command: MenuCommand::Action(Action::ToggleVisualLine),
        };
        let mut context_menu = Some(ContextMenuState::new(
            MenuTarget::DiffRow,
            vec![entry],
            (0, 0),
        ));
        let mut compose: Option<ComposeState> = None;
        let mut goto_status: Option<String> = None;
        let lsp_manager = inert_lsp_manager();

        call_handle_action(
            Action::Confirm,
            &mut stack,
            &mut context_menu,
            &mut compose,
            &mut goto_status,
            &lsp_manager,
            &crate::lsp::ObservationStore::in_memory(),
        );

        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert!(
            app.visual_active(),
            "ToggleVisualLine must dispatch through the real Action arm and \
             flip the visual anchor on"
        );
        assert!(context_menu.is_none());
    }

    #[test]
    fn context_menu_confirm_on_a_disabled_entry_stays_open_and_reports_the_reason() {
        let app = selectable_diff_app();
        let mut stack = ViewStack::new(View::Diff(app));
        let reason = "LSP: rust is starting; go to definition is not ready yet".to_owned();
        let entry = context_menu::MenuEntry {
            label: "Go to definition".to_owned(),
            enabled: Err(reason.clone()),
            command: MenuCommand::Action(Action::GotoDefinition),
        };
        let mut context_menu = Some(ContextMenuState::new(
            MenuTarget::DiffRow,
            vec![entry],
            (0, 0),
        ));
        let mut compose: Option<ComposeState> = None;
        let mut goto_status: Option<String> = None;
        let lsp_manager = inert_lsp_manager();

        call_handle_action(
            Action::Confirm,
            &mut stack,
            &mut context_menu,
            &mut compose,
            &mut goto_status,
            &lsp_manager,
            &crate::lsp::ObservationStore::in_memory(),
        );

        assert!(
            context_menu.is_some(),
            "a disabled entry must never close the menu or invoke anything"
        );
        assert_eq!(goto_status, Some(reason));
    }

    /// A two-directory-deep fixture (`src` > `src/nested`) for
    /// `ExpandAllDescendants`'s dispatch test — `selectable_diff_app` puts
    /// its one file at the repo root, too flat to have any directory at all.
    fn nested_files_app() -> App {
        let make = |name: &str| crate::diff::DiffFile {
            old_path: Some(name.to_owned()),
            new_path: Some(name.to_owned()),
            hunks: vec![crate::diff::DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                header: String::new(),
                known_eof: true,
                rows: vec![crate::diff::DiffRow {
                    kind: crate::diff::DiffLineKind::Context,
                    text: "x".to_owned(),
                    old_line: Some(1),
                    new_line: Some(1),
                }],
            }],
            ..Default::default()
        };
        App::new(
            "repo".to_owned(),
            PathBuf::from("/repo"),
            vec![make("src/nested/a.rs"), make("src/nested/b.rs")],
        )
    }

    #[test]
    fn context_menu_confirm_dispatches_expand_all_descendants_via_app_method() {
        let mut app = nested_files_app();
        app.set_descendants_collapsed("src", true); // setup: start collapsed
        assert!(
            !app.visible_rows
                .iter()
                .any(|r| r.id.path == "src/nested/a.rs"),
            "setup sanity: collapsing must hide the nested file rows"
        );
        let mut stack = ViewStack::new(View::Diff(app));
        let entry = context_menu::MenuEntry {
            label: "Expand all descendants".to_owned(),
            enabled: Ok(()),
            command: MenuCommand::ExpandAllDescendants,
        };
        let mut context_menu = Some(ContextMenuState::new(
            MenuTarget::TreeDir {
                path: "src".to_owned(),
            },
            vec![entry],
            (0, 0),
        ));
        let mut compose: Option<ComposeState> = None;
        let mut goto_status: Option<String> = None;
        let lsp_manager = inert_lsp_manager();

        call_handle_action(
            Action::Confirm,
            &mut stack,
            &mut context_menu,
            &mut compose,
            &mut goto_status,
            &lsp_manager,
            &crate::lsp::ObservationStore::in_memory(),
        );

        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert!(
            app.visible_rows
                .iter()
                .any(|r| r.id.path == "src/nested/a.rs"),
            "ExpandAllDescendants must reach App::set_descendants_collapsed"
        );
        assert!(context_menu.is_none());
    }

    #[test]
    fn context_menu_cursor_and_top_bottom_actions_navigate_without_invoking_anything() {
        let app = selectable_diff_app();
        let mut stack = ViewStack::new(View::Diff(app));
        let entries = vec![
            context_menu::MenuEntry {
                label: "a".to_owned(),
                enabled: Ok(()),
                command: MenuCommand::Action(Action::Hover),
            },
            context_menu::MenuEntry {
                label: "b".to_owned(),
                enabled: Ok(()),
                command: MenuCommand::Action(Action::Hover),
            },
            context_menu::MenuEntry {
                label: "c".to_owned(),
                enabled: Ok(()),
                command: MenuCommand::Action(Action::Hover),
            },
        ];
        let mut context_menu = Some(ContextMenuState::new(MenuTarget::DiffRow, entries, (0, 0)));
        let mut compose: Option<ComposeState> = None;
        let mut goto_status: Option<String> = None;
        let lsp_manager = inert_lsp_manager();

        call_handle_action(
            Action::Bottom,
            &mut stack,
            &mut context_menu,
            &mut compose,
            &mut goto_status,
            &lsp_manager,
            &crate::lsp::ObservationStore::in_memory(),
        );
        assert_eq!(context_menu.as_ref().unwrap().selected(), 2);

        call_handle_action(
            Action::CursorUp,
            &mut stack,
            &mut context_menu,
            &mut compose,
            &mut goto_status,
            &lsp_manager,
            &crate::lsp::ObservationStore::in_memory(),
        );
        assert_eq!(context_menu.as_ref().unwrap().selected(), 1);

        call_handle_action(
            Action::Top,
            &mut stack,
            &mut context_menu,
            &mut compose,
            &mut goto_status,
            &lsp_manager,
            &crate::lsp::ObservationStore::in_memory(),
        );
        assert_eq!(context_menu.as_ref().unwrap().selected(), 0);
        assert!(
            context_menu.is_some(),
            "pure navigation must never close or invoke anything"
        );
    }

    #[test]
    fn context_menu_cancel_closes_the_menu() {
        let app = selectable_diff_app();
        let mut stack = ViewStack::new(View::Diff(app));
        let entry = context_menu::MenuEntry {
            label: "a".to_owned(),
            enabled: Ok(()),
            command: MenuCommand::Action(Action::Hover),
        };
        let mut context_menu = Some(ContextMenuState::new(
            MenuTarget::DiffRow,
            vec![entry],
            (0, 0),
        ));
        let mut compose: Option<ComposeState> = None;
        let mut goto_status: Option<String> = None;
        let lsp_manager = inert_lsp_manager();

        call_handle_action(
            Action::Cancel,
            &mut stack,
            &mut context_menu,
            &mut compose,
            &mut goto_status,
            &lsp_manager,
            &crate::lsp::ObservationStore::in_memory(),
        );
        assert!(context_menu.is_none());
    }

    /// The structural guarantee the context-menu interception block's own
    /// docs describe: `q` is intercepted by the resolver in `run` — via
    /// `StepResult::Matched(Action::Quit) => return Ok(())` — *before*
    /// `handle_action` (and therefore this menu's own interception) is ever
    /// reached, on whatever key sequence a real keypress produces. Proven
    /// here at the resolver level, which has no notion of `context_menu` (or
    /// any other overlay) at all — that's what makes this true regardless of
    /// what's open, not a fact this test has to construct a menu to check.
    #[test]
    fn q_resolves_to_quit_through_the_real_keymap_resolver() {
        let keymap = Keymap::from_bindings(&vim_preset(true));
        let mut resolver = keymap.resolver();
        let chord = KeyChord::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(resolver.feed(chord), StepResult::Matched(Action::Quit));
    }

    // ---- apply_scope_swap seeds moving_scope; handle_moving_scope_refresh
    // ---- picks up a real amend (issue #8) -----------------------------

    /// End-to-end at the `App`/free-function level (a real `git` repo and a
    /// real `git commit --amend` subprocess, but no terminal/PTY — that's
    /// `tests/e2e/moving_scope.rs`'s job): swapping onto the `HEAD` revision
    /// scope seeds a `MovingScopeState`, and a subsequent
    /// `handle_moving_scope_refresh` call after a real amend picks up the
    /// new content via the anchor-preserving `App::apply_refresh` path
    /// (never `apply_scope_swap`'s reset-to-top one) while leaving the
    /// scope label untouched.
    #[test]
    fn apply_scope_swap_seeds_moving_scope_and_a_later_amend_is_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(path.join("a.txt"), "one\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "first"]);

        let app = App::new("repo".to_owned(), path.to_path_buf(), Vec::new());
        let mut stack = ViewStack::new(View::Diff(app));
        let mut hover_state = hover_popup::HoverState::default();
        let mut refs_panel: Option<RefsPanelState> = None;
        let mut context_menu: Option<ContextMenuState> = None;
        let mut watch_paused = false;
        let mut watch_status: Option<WatchStatus> = None;
        let mut moving_scope: Option<MovingScopeState> = None;
        let mut highlighter = LineHighlighter::new();
        let lsp_manager = inert_lsp_manager();

        let at_root = stack.is_at_root();
        let View::Diff(app) = stack.top_mut() else {
            unreachable!()
        };
        apply_scope_swap(
            app,
            &ScopeChoice::Revision("HEAD".to_owned()),
            at_root,
            false,
            None,
            &lsp_manager,
            &mut highlighter,
            &mut hover_state,
            &mut refs_panel,
            &mut context_menu,
            &mut watch_paused,
            &mut watch_status,
            &mut moving_scope,
        )
        .expect("swapping onto HEAD must succeed");
        assert!(
            moving_scope.is_some(),
            "HEAD is classified moving, so the swap must seed a MovingScopeState"
        );
        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert_eq!(app.scope_label.as_deref(), Some("r: HEAD"));

        std::fs::write(path.join("a.txt"), "one\ntwo\n").unwrap();
        git(&["commit", "-q", "-a", "--amend", "--no-edit"]);

        let mut pointer_hover = pointer_hover::PointerHoverState::default();
        let mut pending_pointer_hover: Option<PendingHover> = None;
        let observer = crate::lsp::ObservationStore::in_memory();
        handle_moving_scope_refresh(
            &mut moving_scope,
            &mut stack,
            None,
            &mut highlighter,
            &mut hover_state,
            &mut refs_panel,
            &mut context_menu,
            &mut watch_status,
            None,
            &mut pointer_hover,
            &mut pending_pointer_hover,
            &observer,
        );

        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert!(
            app.files.iter().any(|f| f
                .hunks
                .iter()
                .any(|h| h.rows.iter().any(|r| r.text == "two"))),
            "the amended content must appear after the refresh; files: {:?}",
            app.files
        );
        // The scope label is untouched by `apply_refresh` — stays symbolic
        // even though the commit HEAD names has just changed.
        assert_eq!(app.scope_label.as_deref(), Some("r: HEAD"));
        assert!(
            watch_status
                .as_ref()
                .is_some_and(|s| s.text == "updated: HEAD moved"),
            "watch_status: {:?}",
            watch_status.map(|s| s.text)
        );
    }

    /// Issue #8's fourth acceptance criterion — "a transient VCS failure
    /// leaves the current view intact and reports the failure" — covered at
    /// this unit level rather than end to end: reliably forcing a *transient*
    /// `git`/`jj` failure from outside (a mid-rebase repo, a racing lock) in
    /// a PTY-driven E2E test would be exactly the kind of flaky
    /// test-infrastructure engineering this suite avoids elsewhere (see
    /// `support::fixture`'s module docs), whereas a scope whose text simply
    /// no longer resolves (a deleted branch, same code path as a resolve
    /// erroring) is trivial to construct directly. Either way,
    /// `handle_moving_scope_refresh` can't distinguish "resolve returned
    /// `Ok(None)`" from "resolve errored" in what it does next — see that
    /// function's own docs — so this exercises the representative case.
    #[test]
    fn handle_moving_scope_refresh_leaves_the_view_untouched_on_a_resolve_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(path.join("a.txt"), "one\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "first"]);

        let mut app = App::new("repo".to_owned(), path.to_path_buf(), Vec::new());
        // A scope pointed at a branch name that doesn't exist in this repo
        // — `git.resolve` (or `resolve_commit_id`, for a jj scope) returns
        // `Ok(None)`, the same "doesn't resolve" outcome a transient failure
        // funnels into (see this test's own docs).
        app.scope_label = Some("r: nonexistent-branch".to_owned());
        let original_files = app.files.clone();
        let mut stack = ViewStack::new(View::Diff(app));
        let mut moving_scope = Some(MovingScopeState {
            text: "nonexistent-branch".to_owned(),
            via_jj: false,
            last_hash: Some("deadbeef".to_owned()),
        });
        let mut hover_state = hover_popup::HoverState::default();
        let mut refs_panel: Option<RefsPanelState> = None;
        let mut context_menu: Option<ContextMenuState> = None;
        let mut watch_status: Option<WatchStatus> = None;
        let mut highlighter = LineHighlighter::new();
        let mut pointer_hover = pointer_hover::PointerHoverState::default();
        let mut pending_pointer_hover: Option<PendingHover> = None;
        let observer = crate::lsp::ObservationStore::in_memory();

        handle_moving_scope_refresh(
            &mut moving_scope,
            &mut stack,
            None,
            &mut highlighter,
            &mut hover_state,
            &mut refs_panel,
            &mut context_menu,
            &mut watch_status,
            None,
            &mut pointer_hover,
            &mut pending_pointer_hover,
            &observer,
        );

        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert_eq!(
            app.files, original_files,
            "a resolve failure must leave the diff completely untouched"
        );
        assert_eq!(
            app.scope_label.as_deref(),
            Some("r: nonexistent-branch"),
            "the label must also stay exactly as it was"
        );
        assert!(
            watch_status
                .as_ref()
                .is_some_and(|s| s.text.starts_with("scope: refresh check failed")),
            "watch_status: {:?}",
            watch_status.map(|s| s.text)
        );
        // `moving_scope` itself is untouched too — `last_hash` never
        // advances past a failed check, so the very next real amend is
        // still correctly detected as "changed" rather than silently
        // absorbed into a stale baseline.
        assert_eq!(moving_scope.unwrap().last_hash.as_deref(), Some("deadbeef"));
    }

    /// The other half of `MovingScopeState::last_hash` being an `Option`:
    /// a scope whose *seed* resolve failed (`last_hash: None` — say, the
    /// view opened mid-rebase) must self-heal on the first tick where the
    /// resolve succeeds — `None != Some(_)` reads as "moved", so the view
    /// re-diffs once and the baseline is finally established. Without this,
    /// a bad first resolve would leave live refresh permanently inert.
    #[test]
    fn handle_moving_scope_refresh_recovers_once_a_failed_seed_finally_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(path.join("a.txt"), "one\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "first"]);

        let mut app = App::new("repo".to_owned(), path.to_path_buf(), Vec::new());
        app.scope_label = Some("r: HEAD".to_owned());
        let mut stack = ViewStack::new(View::Diff(app));
        let mut moving_scope = Some(MovingScopeState {
            text: "HEAD".to_owned(),
            via_jj: false,
            last_hash: None,
        });
        let mut hover_state = hover_popup::HoverState::default();
        let mut refs_panel: Option<RefsPanelState> = None;
        let mut context_menu: Option<ContextMenuState> = None;
        let mut watch_status: Option<WatchStatus> = None;
        let mut highlighter = LineHighlighter::new();
        let mut pointer_hover = pointer_hover::PointerHoverState::default();
        let mut pending_pointer_hover: Option<PendingHover> = None;
        let observer = crate::lsp::ObservationStore::in_memory();

        handle_moving_scope_refresh(
            &mut moving_scope,
            &mut stack,
            None,
            &mut highlighter,
            &mut hover_state,
            &mut refs_panel,
            &mut context_menu,
            &mut watch_status,
            None,
            &mut pointer_hover,
            &mut pending_pointer_hover,
            &observer,
        );

        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert!(
            app.files.iter().any(|f| f
                .hunks
                .iter()
                .any(|h| h.rows.iter().any(|r| r.text == "one"))),
            "the first successful resolve after a failed seed must re-diff; files: {:?}",
            app.files
        );
        assert!(
            moving_scope.as_ref().is_some_and(|s| s.last_hash.is_some()),
            "the baseline must be established by the recovery tick"
        );
        // A second tick with nothing changed is back to the cheap no-op —
        // recovery re-diffs exactly once, not on every subsequent tick.
        watch_status = None;
        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        let settled_files = app.files.clone();
        handle_moving_scope_refresh(
            &mut moving_scope,
            &mut stack,
            None,
            &mut highlighter,
            &mut hover_state,
            &mut refs_panel,
            &mut context_menu,
            &mut watch_status,
            None,
            &mut pointer_hover,
            &mut pending_pointer_hover,
            &observer,
        );
        let View::Diff(app) = stack.top() else {
            unreachable!()
        };
        assert_eq!(app.files, settled_files);
        assert!(
            watch_status.is_none(),
            "an unchanged tick must not report anything; watch_status: {:?}",
            watch_status.map(|s| s.text)
        );
    }
}
