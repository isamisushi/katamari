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
pub mod compose;
pub mod diff_view;
pub mod file_view;
pub mod help;
pub mod hints;
pub mod hover_popup;
pub mod key_display;
pub mod log_view;
pub mod navigation;
pub mod refresh;
pub mod refs_panel;
pub mod scope_menu;
pub mod scroll;
pub mod search;
pub mod sidebar;
pub mod status_bar;
pub mod symbols;
pub mod text;
pub mod timeline_view;
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
use crate::lsp::manager::ServerState;
use crate::lsp::{
    DefinitionResult, DiagnosticsStore, HoverResult, LspError, LspManager, ReferencesResult,
    ServerEvent, parse_publish_diagnostics, progress_status_text,
};
use crate::skill;
use crate::ui::compose::{ComposeOutcome, ComposeState};
use crate::ui::help::{HelpOutcome, HelpState};
use crate::ui::log_view::LogView;
use crate::ui::navigation::{JumpEntry, JumpStack, navigate_to};
use crate::ui::refs_panel::RefsPanel;
use crate::ui::scope_menu::{
    RevisionInputOutcome, ScopeMenuEntry, ScopeMenuState, handle_revision_key,
};
use crate::ui::timeline_view::TimelineView;
use crate::update;
use crate::vcs::DiffSource;
use crate::vcs::LogBackend;
use crate::vcs::git::GitSource;
use crate::vcs::jj::{self, JjRepo};
use crate::watch::{self, WatchSignal};
use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
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
use ratatui::layout::{Constraint, Direction, Layout as RatatuiLayout, Rect};
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

    // Must happen before `spawn_input_thread` below starts its own
    // blocking `event::read()` loop — see `enable_kitty_keyboard_protocol`'s
    // docs on why the two would otherwise contend for crossterm's internal
    // event-reader lock.
    let ci_distinguishable = enable_kitty_keyboard_protocol();

    let preset = match config.keymap {
        config::KeymapPreset::Vim => vim_preset(ci_distinguishable),
        config::KeymapPreset::Emacs => emacs_preset(ci_distinguishable),
    };
    let bindings = config::apply_key_overrides(preset, &config.key_overrides)
        .map_err(|e| anyhow::anyhow!("config: {e}"))?;
    let keymap = Keymap::from_bindings(&bindings);
    let mut resolver = keymap.resolver();
    let mut highlighter = LineHighlighter::new();

    let (app_tx, app_rx) = mpsc::channel::<AppEvent>();
    spawn_input_thread(app_tx.clone());

    let (lsp_tx, lsp_rx) = mpsc::channel::<ServerEvent>();
    spawn_lsp_forwarder(lsp_rx, app_tx.clone());
    let lsp_manager = LspManager::new(
        lsp_tx,
        Arc::new(config.lsp_servers.clone()),
        config.auto_install,
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

    // M6: a root diff's comments are loaded and watched unconditionally —
    // unlike the working-tree watcher above, this has nothing to do with
    // whether root live refresh is enabled. See
    // [`AppEvent::CommentsChanged`]'s docs and `watch::spawn_comments_watcher`.
    let (comments_repo_root, initial_comments, comments_startup_status) =
        start_comments(stack.top(), app_tx);
    // Lowest priority of the three: an available-update notice is
    // informational, never a problem report the way a failed watcher or a
    // capped warm-up is, so it only shows when nothing more actionable
    // claimed this session's one startup status slot.
    let startup_status = startup_status
        .or(comments_startup_status)
        .or(available_update.as_ref().map(update::status_bar_notice));

    let result = event_loop(
        &mut terminal,
        stack,
        &mut resolver,
        &keymap,
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

/// Proactively opens whichever files `view` starts out showing —
/// [`View::Diff`]'s changed files (in diff order, excluding deletions and
/// binaries, which have nothing current to open), or [`View::File`]'s
/// single file. Returns a status-bar note when [`LspManager::warm_up`]'s
/// cap meant not everything got opened, so a reviewer of an unusually large
/// diff knows why some files' diagnostics gutters stay quiet until touched.
fn warm_up_root(view: &View, lsp_manager: &LspManager) -> Option<String> {
    match view {
        View::Diff(app) => {
            let summary = warm_up_diff(app, lsp_manager);
            summary.capped().then(|| {
                format!(
                    "LSP: opened {} of {} changed files for diagnostics (first {} by diff order)",
                    summary.opened, summary.total_eligible, summary.opened
                )
            })
        }
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
        View::Timeline(_) | View::Log(_) => None,
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
    Definition(Receiver<Result<DefinitionResult, LspError>>),
    References(Receiver<Result<ReferencesResult, LspError>>),
}

/// An open references (or multi-result definitions) panel, plus the
/// workspace root its entries' targets should be opened relative to —
/// `RefsPanel` itself has no notion of "workspace," only of the entries a
/// caller already resolved, so that context is kept alongside it here
/// rather than added to `RefsPanel`'s own fields for a UI-overlay concern
/// that isn't part of what a references list *is*.
struct RefsPanelState {
    git_root: PathBuf,
    panel: RefsPanel,
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
) -> Result<()> {
    let mut key_display = key_display::KeyDisplayState::new(show_keys);
    let mut hover_state = hover_popup::HoverState::default();
    let mut pending_hover: Option<(u64, Receiver<Result<HoverResult, LspError>>)> = None;
    let mut pending_goto: Option<(JumpEntry, PendingGoto)> = None;
    let mut refs_panel: Option<RefsPanelState> = None;
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
        let (content_height, content_width) = match stack.top() {
            View::Diff(app) => {
                let status_height =
                    hints::required_height(&hints::diff_view_items(keymap), area.width);
                let diff_area = diff_layout(area, app.sidebar_visible, status_height).diff;
                (
                    diff_area.height,
                    diff_view::unified_content_width(diff_area.width),
                )
            }
            View::File(_) => {
                let status_height =
                    hints::required_height(&hints::file_view_items(keymap), area.width);
                let content_area = file_view::layout(area, status_height).content;
                (
                    content_area.height,
                    file_view::content_width_for_pane(content_area.width),
                )
            }
            View::Timeline(_) => {
                let status_height =
                    hints::required_height(&hints::timeline_view_items(keymap), area.width);
                (timeline_view::layout(area, status_height).diff.height, 0)
            }
            View::Log(_) => {
                let status_height =
                    hints::required_height(&hints::log_view_items(keymap), area.width);
                (log_view::layout(area, status_height).list.height, 0)
            }
        };
        stack.top_mut().set_viewport_height(content_height as usize);
        stack.top_mut().set_content_width(content_width);

        if let Some((generation, rx)) = &pending_hover
            && let Ok(result) = rx.try_recv()
        {
            hover_state.apply(*generation, result, highlighter);
            pending_hover = None;
        }

        if let Some((from, op)) = &pending_goto {
            match op {
                PendingGoto::Definition(rx) => {
                    if let Ok(result) = rx.try_recv() {
                        let from = from.clone();
                        pending_goto = None;
                        apply_definition_result(
                            result,
                            from,
                            stack,
                            &mut jump_stack,
                            lsp_manager,
                            &mut refs_panel,
                            &mut goto_status,
                        );
                        hover_state.invalidate();
                    }
                }
                PendingGoto::References(rx) => {
                    if let Ok(result) = rx.try_recv() {
                        let from = from.clone();
                        pending_goto = None;
                        apply_references_result(
                            result,
                            from,
                            lsp_manager,
                            &mut refs_panel,
                            &mut goto_status,
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
        key_display.tick(Instant::now());
        let status_note = hover_state
            .status_hint()
            .or_else(|| goto_status.clone())
            .or_else(|| lsp_status.clone())
            .or_else(|| watch_status.as_ref().map(|s| s.text.clone()));
        terminal.draw(|frame| {
            draw(
                frame,
                stack.top(),
                keymap,
                highlighter,
                &hover_state,
                &diagnostics,
                refs_panel.as_ref().map(|s| &s.panel),
                status_note.as_deref(),
                &comment_index,
                compose.as_ref(),
                scope_menu.as_ref(),
                help.as_ref(),
                search_prompt.as_ref(),
                jj_repo.is_some(),
                &key_display,
            )
        })?;

        match app_rx.recv_timeout(EVENT_POLL_INTERVAL) {
            Ok(AppEvent::Terminal(Event::Key(key))) => {
                if key.kind != KeyEventKind::Press {
                    // no-op; falls through to the loop's bottom
                } else if let Some(state) = compose.as_mut() {
                    // The compose overlay wants raw characters, not
                    // `Action`s — see `ui::compose::handle_key`'s docs on
                    // why this bypasses the keymap resolver entirely rather
                    // than only intercepting a few already-resolved
                    // actions the way the hover popup/references panel do
                    // below. The key-display overlay masks this the same
                    // way — see `key_display`'s module docs on why it never
                    // echoes typed characters.
                    key_display.record_typing(Instant::now());
                    match compose::handle_key(state.buffer_mut(), key) {
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

                            // The prompt only ever fires once per session
                            // (`skill_prompt_offered`), only after a comment
                            // actually persisted (not a discarded-empty or
                            // failed save), only when the reviewer hasn't
                            // opted out (`offer_skill_install`), and only
                            // when this repo doesn't already have the full
                            // harness — skill, AGENTS.md, and CLAUDE.md, see
                            // `skill::harness_installed`'s docs on why *any*
                            // missing piece re-offers (M17 extended this
                            // from a skill-only check so a repo that only
                            // ran an older `ktmr skill install` still gets
                            // offered the rest) — any one of those false
                            // means a plain status message, same as before
                            // M16.
                            let offer = saved
                                && offer_skill_install
                                && !skill_prompt_offered
                                && comments_repo_root
                                    .as_deref()
                                    .is_some_and(|root| !skill::harness_installed(root));
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
                            if let View::Diff(app) = stack.top_mut() {
                                goto_status = app.confirm_search(&query, &origin);
                            }
                            search_prompt = None;
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
                                    &mut watch_paused,
                                    &mut watch_status,
                                ) {
                                    Ok(()) => scope_menu = None,
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
                    // is swallowed entirely (there's nothing sensible for
                    // the keymap to additionally do with a bare `y` — it has
                    // no default binding). Any other key means "dismiss" —
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
                                    &mut jump_stack,
                                    &diagnostics,
                                    &mut goto_status,
                                    lsp_manager,
                                    jj_repo.as_ref(),
                                    &mut compose,
                                    &mut scope_menu,
                                    &mut help,
                                    &mut search_prompt,
                                    highlighter,
                                    &mut watch_paused,
                                    watch_active,
                                    &mut watch_status,
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
            Ok(AppEvent::Terminal(_)) => {
                // Resize, mouse, focus, paste: nothing to dispatch, but the
                // next iteration re-measures the viewport and redraws
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
                    &mut watch_status,
                    watch_paused,
                    live_search,
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
            Ok(AppEvent::CommentsChanged) => {
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
/// re-reads `state.file`'s current content (rather than trusting whatever
/// it was when the overlay opened, in case it changed mid-edit) to compute
/// a fresh [`comments::Anchor`], then writes it through
/// [`CommentStore::append_comment`].
fn save_comment(repo_root: &Path, state: &ComposeState) -> std::result::Result<Comment, String> {
    let absolute = repo_root.join(&state.file);
    let content = std::fs::read_to_string(&absolute)
        .map_err(|e| format!("couldn't re-read {}: {e}", state.file))?;
    let lines: Vec<&str> = content.lines().collect();
    let anchor = comments::anchor_for(&lines, state.line)
        .ok_or_else(|| format!("line {} no longer exists in {}", state.line, state.file))?;

    let comment = Comment {
        id: comments::generate_id(),
        created_at: comments::now_unix(),
        file: state.file.clone(),
        anchor,
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
    pending_hover: &mut Option<(u64, Receiver<Result<HoverResult, LspError>>)>,
    pending_goto: &mut Option<(JumpEntry, PendingGoto)>,
    refs_panel: &mut Option<RefsPanelState>,
    jump_stack: &mut JumpStack,
    diagnostics: &DiagnosticsStore,
    goto_status: &mut Option<String>,
    lsp_manager: &LspManager,
    jj_repo: Option<&JjRepo>,
    compose: &mut Option<ComposeState>,
    scope_menu: &mut Option<ScopeMenuState>,
    help: &mut Option<HelpState>,
    search_prompt: &mut Option<search::SearchPromptState>,
    highlighter: &mut LineHighlighter,
    watch_paused: &mut bool,
    watch_active: bool,
    watch_status: &mut Option<WatchStatus>,
) {
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
                                watch_paused,
                                watch_status,
                            ) {
                                Ok(()) => *scope_menu = None,
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
                        line: entry.line,
                        col: entry.match_range.0,
                    };
                    let from = stack.top().jump_entry();
                    let _ = navigate_to(stack, jump_stack, lsp_manager, from, target, true);
                    hover_state.invalidate();
                }
                *refs_panel = None;
                return;
            }
            _ => *refs_panel = None, // any other key closes the panel, then falls through
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

    match action {
        Action::Hover => match stack.top().hover_query() {
            Some(query) => {
                hover_state.invalidate();
                let overlapping = diagnostics.diagnostics_on_line(&query.file, query.line);
                hover_state.set_diagnostics_prefix(&overlapping);
                hover_state.set_pending();
                let generation = hover_state.generation();
                let rx = lsp_manager.hover(
                    &query.file,
                    &query.git_root,
                    &query.line_text,
                    query.line,
                    query.display_col,
                );
                *pending_hover = Some((generation, rx));
            }
            None => hover_state.set_message("nothing to hover here"),
        },
        Action::GotoDefinition => match stack.top().hover_query() {
            Some(query) => {
                let rx = lsp_manager.definition(
                    &query.file,
                    &query.git_root,
                    &query.line_text,
                    query.line,
                    query.display_col,
                );
                *pending_goto = Some((JumpEntry::from(&query), PendingGoto::Definition(rx)));
                *goto_status = Some("goto: \u{2026}".to_owned());
            }
            None => *goto_status = Some("goto: nothing to jump from here".to_owned()),
        },
        Action::FindReferences => match stack.top().hover_query() {
            Some(query) => {
                let rx = lsp_manager.references(
                    &query.file,
                    &query.git_root,
                    &query.line_text,
                    query.line,
                    query.display_col,
                );
                *pending_goto = Some((JumpEntry::from(&query), PendingGoto::References(rx)));
                *goto_status = Some("references: \u{2026}".to_owned());
            }
            None => *goto_status = Some("references: nothing to jump from here".to_owned()),
        },
        Action::AddComment => match stack.top() {
            View::Diff(app) => match app.comment_target() {
                Some((file, line)) => *compose = Some(ComposeState::new(file, line)),
                None => *goto_status = Some("comment: nothing to annotate on this row".to_owned()),
            },
            View::File(_) | View::Timeline(_) | View::Log(_) => {
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
            View::File(_) | View::Timeline(_) | View::Log(_) => {
                *goto_status = Some("fold: only available in the diff view".to_owned());
            }
        },
        Action::CollapseFold => match stack.top_mut() {
            View::Diff(app) => {
                if !app.collapse_fold_at_cursor() {
                    *goto_status = Some("fold: nothing to collapse here".to_owned());
                }
            }
            View::File(_) | View::Timeline(_) | View::Log(_) => {
                *goto_status = Some("fold: only available in the diff view".to_owned());
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
        Action::OpenSearch => match stack.top_mut() {
            View::Diff(app) => {
                let origin =
                    refresh::capture_anchor(&app.files, &app.rows, app.cursor, app.scroll_offset);
                *search_prompt = Some(search::SearchPromptState {
                    input: search::SearchInput::new(),
                    origin,
                });
            }
            View::File(_) | View::Timeline(_) | View::Log(_) => {
                *goto_status = Some("search: only available in the diff view".to_owned());
            }
        },
        // `n`/`N`: real navigation lives here (an `App` method), not in
        // `App::update` — see `App::next_match`'s docs on why keeping the
        // logic out of `App::update` matters even though this arm already
        // gates on `View::Diff` on its own (defense in depth: it also keeps
        // `TimelineView`'s nested `diff_app.update(action)` fallthrough,
        // see `timeline_view::TimelineView::update`, from ever running real
        // search navigation against its embedded diff pane).
        Action::NextMatch | Action::PrevMatch => match stack.top_mut() {
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
            View::File(_) | View::Timeline(_) | View::Log(_) => {
                *goto_status = Some("search: only available in the diff view".to_owned());
            }
        },
        Action::NextDiagnostic | Action::PrevDiagnostic => {
            let forward = action == Action::NextDiagnostic;
            let before = stack.top().hover_cursor_key();
            stack.top_mut().jump_to_diagnostic(diagnostics, forward);
            if stack.top().hover_cursor_key() != before {
                hover_state.invalidate();
            }
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
        // vim's `:noh`: with nothing else open to cancel (hover/the refs
        // panel/the scope-menu list already intercepted `Cancel` above if
        // any of those *were* open — reaching here means none was), a
        // plain `Esc` in the ordinary diff view clears an active
        // *confirmed* search's highlight, if any. This is a different
        // `Esc` from the prompt's own (cancel-and-restore-the-cursor,
        // handled entirely inside `run`'s raw-key bypass arm before an
        // `Action` is ever resolved) — this one only ever fires once the
        // prompt has already closed, on whatever highlight Enter left
        // behind. A no-op on any other view, and a no-op with nothing
        // active either way (`App::clear_search`'s own guard).
        Action::Cancel => {
            if let View::Diff(app) = stack.top_mut() {
                app.clear_search();
            }
        }
        Action::ToggleTimeline => open_or_close_timeline(stack, jj_repo, goto_status),
        Action::ToggleLogView => open_or_close_log(stack, goto_status),
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
        // `cancel_pending_lsp_requests_for_help`'s docs for why a response
        // that arrived while the modal was up would otherwise be able to
        // mutate `stack`/`refs_panel` or draw a hover popup with the
        // reviewer unable to even see it happen, let alone react.
        Action::OpenHelp => {
            cancel_pending_lsp_requests_for_help(hover_state, pending_goto);
            *help = Some(HelpState::new());
        }
        Action::OpenScopeMenu => match stack.top() {
            View::Diff(_) => *scope_menu = Some(ScopeMenuState::new_list(jj_repo.is_some())),
            View::File(_) | View::Timeline(_) | View::Log(_) => {
                *goto_status = Some("scope: only available in the diff view".to_owned());
            }
        },
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
        }
    }
}

/// Cancels whatever hover/goto-definition/find-references request is still
/// in flight the instant the help modal opens, so a response arriving
/// while (or after) it's up can never mutate `stack`/`refs_panel` or draw a
/// hover popup out from under it. This matters specifically because the
/// modal intercepts every keystroke until it closes (see `run`'s `help`
/// arm) — the reviewer would have no way to even notice, let alone
/// dismiss, whatever changed underneath.
///
/// Mirrors what a cursor move already does to a pending hover, via
/// [`hover_popup::HoverState::invalidate`]: its generation bump makes
/// [`hover_popup::HoverState::apply`] a no-op for a response tagged with
/// the now-stale generation, so `pending_hover` itself is left alone here,
/// same as at every other `invalidate()` call site in this file — no need
/// to also drop its `Receiver`. `pending_goto` has no generation to lean
/// on (`apply_definition_result`/`apply_references_result` run
/// unconditionally on whatever they're given), so it's dropped outright
/// instead, discarding the `Receiver` along with whatever answer the
/// request eventually produces.
fn cancel_pending_lsp_requests_for_help(
    hover_state: &mut hover_popup::HoverState,
    pending_goto: &mut Option<(JumpEntry, PendingGoto)>,
) {
    hover_state.invalidate();
    *pending_goto = None;
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
        View::File(_) | View::Log(_) => {}
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
        View::File(_) | View::Timeline(_) => {}
    }
}

/// What the scope-menu popup can swap the current [`View::Diff`]'s content
/// to *in place* — unlike `Log`/`Timeline` (handled by
/// [`open_or_close_log`]/[`open_or_close_timeline`] instead), which push a
/// new view rather than replacing this one's content, so they never become
/// a `ScopeChoice`.
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
    watch_paused: &mut bool,
    watch_status: &mut Option<WatchStatus>,
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
    let disk_is_new_side = matches!(choice, ScopeChoice::WorkingTree);
    app.apply_scope_swap(
        parse_unified_diff(&text),
        interactive,
        disk_is_new_side,
        scope_label,
    );

    hover_state.invalidate();
    *refs_panel = None;
    highlighter.clear_cache();
    if interactive {
        warm_up_diff(app, lsp_manager);
    }

    let is_working_tree = matches!(choice, ScopeChoice::WorkingTree);
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
    Ok(())
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
    watch_status: &mut Option<WatchStatus>,
    watch_paused: bool,
    live_search: Option<(&str, &refresh::Anchor)>,
) {
    if watch_paused {
        return;
    }

    let View::Diff(app) = stack.root_mut() else {
        return; // watch mode only ever runs against the root diff view
    };

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

/// Applies a `textDocument/definition` result: navigates straight there for
/// a single candidate (the common case), opens the references panel
/// (labeled "Definitions") for several, or leaves a status-bar note for
/// "none"/an error.
fn apply_definition_result(
    result: Result<DefinitionResult, LspError>,
    from: JumpEntry,
    stack: &mut ViewStack,
    jump_stack: &mut JumpStack,
    lsp_manager: &LspManager,
    refs_panel: &mut Option<RefsPanelState>,
    goto_status: &mut Option<String>,
) {
    let response = match result {
        Ok(Some(response)) => response,
        Ok(None) => {
            *goto_status = Some("goto: no definition found".to_owned());
            return;
        }
        Err(e) => {
            *goto_status = Some(format!("goto: {e}"));
            return;
        }
    };
    let locations = navigation::definition_locations(response);
    if locations.is_empty() {
        *goto_status = Some("goto: no definition found".to_owned());
        return;
    }
    let encoding = response_encoding(lsp_manager, &from);
    if locations.len() == 1 {
        match navigation::location_to_target(&locations[0], &from.git_root, &encoding) {
            Some(target) => {
                let _ = navigate_to(stack, jump_stack, lsp_manager, Some(from), target, true);
            }
            None => *goto_status = Some("goto: definition points at an unreadable file".to_owned()),
        }
        return;
    }
    let (entries, truncated) = refs_panel::build_entries(&locations, &from.git_root, &encoding);
    *refs_panel = Some(RefsPanelState {
        git_root: from.git_root,
        panel: RefsPanel::new("Definitions", entries, truncated),
    });
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
) {
    match result {
        Ok(Some(locations)) if !locations.is_empty() => {
            let encoding = response_encoding(lsp_manager, &from);
            let (entries, truncated) =
                refs_panel::build_entries(&locations, &from.git_root, &encoding);
            *refs_panel = Some(RefsPanelState {
                git_root: from.git_root,
                panel: RefsPanel::new("References", entries, truncated),
            });
        }
        Ok(_) => *goto_status = Some("references: none found".to_owned()),
        Err(e) => *goto_status = Some(format!("references: {e}")),
    }
}

#[allow(clippy::too_many_arguments)] // one render pass threading through
// every overlay/lookaside table the frame might need to draw; see
// `handle_action`'s comment for why a struct wouldn't reduce this.
fn draw(
    frame: &mut Frame,
    view: &View,
    keymap: &Keymap,
    highlighter: &mut LineHighlighter,
    hover_state: &hover_popup::HoverState,
    diagnostics: &DiagnosticsStore,
    refs_panel: Option<&RefsPanel>,
    status_note: Option<&str>,
    comments: &CommentIndex,
    compose: Option<&ComposeState>,
    scope_menu: Option<&ScopeMenuState>,
    help: Option<&HelpState>,
    search_prompt: Option<&search::SearchPromptState>,
    jj_available: bool,
    key_display: &key_display::KeyDisplayState,
) {
    match view {
        View::Diff(app) => {
            let hint_items = hints::diff_view_items(keymap);
            let status_height = hints::required_height(&hint_items, frame.area().width);
            let areas = diff_layout(frame.area(), app.sidebar_visible, status_height);
            if let Some(sidebar_area) = areas.sidebar {
                sidebar::render(frame, sidebar_area, app);
            }
            let effective_layout = diff_view::effective_layout(app.layout, areas.diff.width);
            diff_view::render(
                frame,
                areas.diff,
                app,
                highlighter,
                effective_layout,
                diagnostics,
                comments,
            );
            status_bar::render(
                frame,
                areas.status,
                app,
                effective_layout,
                status_note,
                search_prompt.map(|p| (p.input.text(), p.input.cursor())),
                &hint_items,
            );
            if let Some(row) = view.cursor_screen_row() {
                hover_popup::render(frame, areas.diff, row, hover_state);
            }
            if let Some(panel) = refs_panel {
                refs_panel::render(frame, areas.diff, panel);
            }
            if let Some(state) = compose
                && let Some(row) = view.cursor_screen_row()
            {
                compose::render(frame, areas.diff, row, state);
            }
            if let Some(state) = scope_menu {
                scope_menu::render(frame, areas.diff, state, jj_available);
            }
            key_display::render(frame, areas.diff, key_display);
        }
        View::File(file) => {
            let hint_items = hints::file_view_items(keymap);
            let status_height = hints::required_height(&hint_items, frame.area().width);
            let areas = file_view::layout(frame.area(), status_height);
            file_view::render(frame, areas.content, file, diagnostics);
            file_view::render_status(frame, areas.status, file, status_note, &hint_items);
            if let Some(row) = view.cursor_screen_row() {
                hover_popup::render(frame, areas.content, row, hover_state);
            }
            if let Some(panel) = refs_panel {
                refs_panel::render(frame, areas.content, panel);
            }
            key_display::render(frame, areas.content, key_display);
        }
        View::Timeline(timeline) => {
            let area = frame.area();
            timeline_view::render(frame, area, timeline, highlighter, keymap, key_display);
        }
        View::Log(log) => {
            let area = frame.area();
            log_view::render(frame, area, log, keymap, key_display);
        }
    }

    // Rendered once, unconditionally, *outside* the match above rather than
    // nested in `View::Diff`'s arm the way `compose`/`scope_menu` are —
    // those two only ever open from a live `View::Diff` session, but
    // `Action::OpenHelp` opens from any view (see its docs), and sizing
    // this against `frame.area()` rather than `areas.diff` is what makes
    // that true on screen, not just in `handle_action`'s dispatch.
    if let Some(state) = help {
        help::render(frame, frame.area(), state, keymap);
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
/// Must run after [`enable_raw_mode`] (crossterm's probe blocks on a real
/// synchronous read from the tty — it doesn't work otherwise) and before
/// [`spawn_input_thread`] starts its own blocking `event::read()` loop on a
/// background thread; both read from the same underlying tty through
/// crossterm's internal event-reader lock, so overlapping them risks the
/// probe's response bytes being stolen by the input thread instead (or a
/// deadlock, depending on timing) rather than a hang or a crash, which is
/// worse to debug. `supports_keyboard_enhancement` already bounds its own
/// wait (2s) and turns "no response," "not a tty" (e.g. output piped in a
/// test harness), or any other I/O hiccup into `Ok(false)`/`Err` — both
/// treated identically here as "not supported," so a plain terminal or a
/// pipe never makes startup fail or hang, it just keeps `C-t` as the sole
/// working binding.
fn enable_kitty_keyboard_protocol() -> bool {
    let Ok(true) = supports_keyboard_enhancement() else {
        return false;
    };
    execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok()
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    // Popping unconditionally — not gated on whether `enable_kitty_keyboard_protocol`
    // actually pushed anything — mirrors `disable_raw_mode`/`LeaveAlternateScreen`
    // right alongside it: both run every time regardless of what this
    // particular session did on the way in. It's safe to do: a terminal
    // that never understood the push in the first place ignores this CSI
    // sequence the same way it ignored that one (unrecognized CSI final
    // bytes are conventionally no-ops per ECMA-48), and the kitty protocol
    // itself specifies popping an empty flag stack as a no-op too.
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen
    )?;
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
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ---- cancel_pending_lsp_requests_for_help ------------------------------

    #[test]
    fn opening_help_invalidates_a_pending_hover_and_drops_a_pending_goto() {
        let mut hover_state = hover_popup::HoverState::default();
        hover_state.set_pending();
        let generation_before = hover_state.generation();

        let (_tx, rx) = mpsc::channel();
        let mut pending_goto: Option<(JumpEntry, PendingGoto)> = Some((
            JumpEntry {
                file: PathBuf::from("a.rs"),
                git_root: PathBuf::from("."),
                line: 0,
                col: 0,
            },
            PendingGoto::Definition(rx),
        ));

        cancel_pending_lsp_requests_for_help(&mut hover_state, &mut pending_goto);

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
}
