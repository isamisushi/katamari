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
pub mod hover_popup;
pub mod navigation;
pub mod refresh;
pub mod refs_panel;
pub mod scroll;
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
use crate::diff::parse_unified_diff;
use crate::highlight::LineHighlighter;
use crate::keymap::{Action, KeyChord, Keymap, Resolver, StepResult, vim_preset};
use crate::lsp::adapter::Language;
use crate::lsp::client::uri_to_path;
use crate::lsp::manager::ServerState;
use crate::lsp::{
    DefinitionResult, DiagnosticsStore, HoverResult, LspError, LspManager, ReferencesResult,
    ServerEvent, parse_publish_diagnostics, progress_status_text,
};
use crate::ui::compose::{ComposeOutcome, ComposeState};
use crate::ui::navigation::{JumpEntry, JumpStack, navigate_to};
use crate::ui::refs_panel::RefsPanel;
use crate::ui::timeline_view::TimelineView;
use crate::vcs::DiffSource;
use crate::vcs::git::GitSource;
use crate::vcs::jj::{self, JjRepo};
use crate::watch::{self, WatchSignal};
use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use lsp_types::{FileChangeType, PositionEncodingKind};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout as RatatuiLayout, Rect};
use std::collections::HashSet;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

const SIDEBAR_WIDTH: u16 = 30;
const STATUS_BAR_HEIGHT: u16 = 1;

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
    /// session with a root diff regardless of `--watch`. Triggers a
    /// comments-only reload (re-read the log, rebuild the
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
/// all. `ktmr diff --watch` passes [`crate::ui::NoopPreRefreshHook`]; every
/// other command passes `None`.
pub fn run(stack: &mut ViewStack, pre_refresh_hook: Option<Box<dyn PreRefreshHook>>) -> Result<()> {
    install_panic_hook();
    let mut terminal = init_terminal()?;

    let keymap = Keymap::from_bindings(&vim_preset());
    let mut resolver = keymap.resolver();
    let mut highlighter = LineHighlighter::new();

    let (app_tx, app_rx) = mpsc::channel::<AppEvent>();
    spawn_input_thread(app_tx.clone());

    let (lsp_tx, lsp_rx) = mpsc::channel::<ServerEvent>();
    spawn_lsp_forwarder(lsp_rx, app_tx.clone());
    let lsp_manager = LspManager::new(lsp_tx);

    // Proactively `didOpen`s the files this session starts out looking at,
    // so diagnostics gutters have something to show without the user
    // hovering first — see `LspManager::warm_up`'s docs for why hovering
    // alone isn't enough to make that happen promptly.
    let startup_status = warm_up_root(stack.top(), &lsp_manager);

    // M5: detected once, regardless of watch mode — `Action::ToggleTimeline`
    // needs this even in a plain (non-`--watch`) session, since existing jj
    // history is browsable any time, not just while katamari is actively
    // creating new snapshots. See `detect_jj_repo`'s docs.
    let jj_repo = detect_jj_repo(stack.top());

    // "Wire in ui::run when jj detected + watch mode" (the milestone plan's
    // words): a `--watch` session's hook starts as whatever the caller
    // passed (`NoopPreRefreshHook`, per `main.rs`'s docs) and is upgraded to
    // a real `JjPreRefreshHook` here, the one place that already knows both
    // "is this watching at all" (`pre_refresh_hook.is_some()`) and "is this
    // a jj repo" (`jj_repo`) — callers don't need to know jj exists.
    let pre_refresh_hook: Option<Box<dyn PreRefreshHook>> = match (&pre_refresh_hook, &jj_repo) {
        (Some(_), Some(repo)) => Some(Box::new(JjPreRefreshHook {
            jj_repo: repo.clone(),
            tx: app_tx.clone(),
        })),
        _ => pre_refresh_hook,
    };

    let watch_startup_status = pre_refresh_hook
        .is_some()
        .then(|| start_watch(stack, app_tx.clone()))
        .flatten();

    // M6: a root diff's comments are loaded and watched unconditionally —
    // unlike the working-tree watcher above, this has nothing to do with
    // `--watch`. See [`AppEvent::CommentsChanged`]'s docs and
    // `watch::spawn_comments_watcher`.
    let (comments_repo_root, initial_comments, comments_startup_status) =
        start_comments(stack.top(), app_tx);
    let startup_status = startup_status.or(comments_startup_status);

    let result = event_loop(
        &mut terminal,
        stack,
        &mut resolver,
        &mut highlighter,
        &app_rx,
        &lsp_manager,
        startup_status,
        pre_refresh_hook,
        watch_startup_status,
        jj_repo,
        comments_repo_root,
        initial_comments,
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

    result
}

/// Starts watching the root diff's repository and wires its events onto
/// `app_tx`, marking the root [`App`] as being in watch mode along the way.
/// Returns a status-bar note if the watcher itself failed to start (e.g.
/// the platform's file-watching backend couldn't be initialized) — a
/// session that asked for `--watch` and silently isn't watching would be a
/// much worse failure mode than one that says so up front.
fn start_watch(stack: &mut ViewStack, app_tx: Sender<AppEvent>) -> Option<String> {
    let View::Diff(app) = stack.root_mut() else {
        // `main.rs` only ever offers `--watch` alongside the plain diff
        // view (not `ktmr open`'s single-file view), so this doesn't
        // happen in practice — but reaching it silently, without a
        // watcher, would be a worse failure than declining loudly.
        return Some("watch: not available for this view".to_owned());
    };
    app.watch_mode = true;
    let (watch_tx, watch_rx) = mpsc::channel::<WatchSignal>();
    match watch::spawn(app.repo_root.clone(), watch_tx) {
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
/// this doesn't gate on `--watch`. Returns the repo root comments are keyed
/// against (`None` for a [`View::File`]/[`View::Timeline`] root, which have
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
            let files: Vec<PathBuf> = app
                .files
                .iter()
                .filter(|f| !f.is_deleted && !f.is_binary)
                .filter_map(|f| f.new_path.as_deref())
                .map(|relative| app.repo_root.join(relative))
                .collect();
            let summary = lsp_manager.warm_up(&files, &app.repo_root);
            summary.capped().then(|| {
                format!(
                    "LSP: opened {} of {} changed files for diagnostics (first {} by diff order)",
                    summary.opened, summary.total_eligible, summary.opened
                )
            })
        }
        View::File(file) => {
            if let Some(path) = file.file_path() {
                lsp_manager.warm_up(std::slice::from_ref(&path.to_path_buf()), file.git_root());
            }
            None
        }
        // Read-only and LSP-free by design (see `TimelineView::hover_query`'s
        // docs) — a timeline session, whether reached via `ktmr timeline` or
        // by toggling from the root diff, has nothing for `LspManager` to
        // warm up.
        View::Timeline(_) => None,
    }
}

/// Reads crossterm events on a dedicated thread and forwards them, since
/// `crossterm::event::read()` blocks and the render loop needs to wait on
/// LSP activity at the same time. Exits quietly once the receiving end goes
/// away (normal shutdown) or a read fails (terminal gone).
fn spawn_input_thread(tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        loop {
            let Ok(ev) = event::read() else { break };
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
    highlighter: &mut LineHighlighter,
    app_rx: &Receiver<AppEvent>,
    lsp_manager: &LspManager,
    startup_status: Option<String>,
    pre_refresh_hook: Option<Box<dyn PreRefreshHook>>,
    initial_watch_status: Option<String>,
    jj_repo: Option<JjRepo>,
    comments_repo_root: Option<PathBuf>,
    initial_comments: Vec<Comment>,
) -> Result<()> {
    let mut hover_state = hover_popup::HoverState::default();
    let mut pending_hover: Option<(u64, Receiver<Result<HoverResult, LspError>>)> = None;
    let mut pending_goto: Option<(JumpEntry, PendingGoto)> = None;
    let mut refs_panel: Option<RefsPanelState> = None;
    let mut jump_stack = JumpStack::new();
    let mut diagnostics = DiagnosticsStore::new();
    let mut lsp_status: Option<String> = startup_status;
    let mut goto_status: Option<String> = None;
    let mut watch_status: Option<WatchStatus> = initial_watch_status.map(WatchStatus::new);
    let mut warned_languages: HashSet<Language> = HashSet::new();
    let mut compose: Option<ComposeState> = None;
    let mut comment_list: Vec<Comment> = initial_comments;
    let mut comment_index: CommentIndex = comments_repo_root
        .as_deref()
        .map(|root| comments::build_index(root, &comment_list))
        .unwrap_or_default();

    loop {
        let size = terminal.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        let content_height = match stack.top() {
            View::Diff(app) => diff_layout(area, app.sidebar_visible).diff.height,
            View::File(_) => file_view::layout(area).content.height,
            View::Timeline(_) => timeline_view::layout(area).diff.height,
        };
        stack.top_mut().set_viewport_height(content_height as usize);

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

        // A once-per-language status hint when the server a hover-eligible
        // position would use is known to be unavailable — see
        // `lsp::adapter::resolve_server`'s error messages, which already
        // read like a status-bar hint ("LSP: typescript ✕ — npm i -g …").
        if let Some(query) = stack.top().hover_query()
            && let Some(language) = Language::detect(&query.file)
            && !warned_languages.contains(&language)
            && let ServerState::Unavailable { reason } =
                lsp_manager.state(&query.file, &query.git_root)
        {
            lsp_status = Some(reason);
            warned_languages.insert(language);
        }

        if watch_status
            .as_ref()
            .is_some_and(|s| s.set_at.elapsed() > WATCH_STATUS_FLASH)
        {
            watch_status = None;
        }
        let status_note = hover_state
            .status_hint()
            .or_else(|| goto_status.clone())
            .or_else(|| lsp_status.clone())
            .or_else(|| watch_status.as_ref().map(|s| s.text.clone()));
        terminal.draw(|frame| {
            draw(
                frame,
                stack.top(),
                highlighter,
                &hover_state,
                &diagnostics,
                refs_panel.as_ref().map(|s| &s.panel),
                status_note.as_deref(),
                &comment_index,
                compose.as_ref(),
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
                    // below.
                    match compose::handle_key(state.buffer_mut(), key) {
                        ComposeOutcome::Continue => {}
                        ComposeOutcome::Cancel => compose = None,
                        ComposeOutcome::Save => {
                            goto_status = Some(finish_compose(
                                state,
                                comments_repo_root.as_deref(),
                                &mut comment_list,
                                &mut comment_index,
                            ));
                            compose = None;
                        }
                    }
                } else {
                    match resolver.feed(KeyChord::from(key)) {
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
                            );
                        }
                        StepResult::Pending => {
                            stack.top_mut().set_pending_keys(resolver.pending_display());
                        }
                        StepResult::Cancelled => stack.top_mut().clear_pending_keys(),
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
                        lsp_status = progress_status_text(&params)
                            .map(|text| format!("{language:?}: {text}"));
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
                watch_status = Some(WatchStatus::new("watch: pending\u{2026}".to_owned()));
            }
            Ok(AppEvent::Watch(WatchSignal::Flushed(batch))) => {
                handle_watch_refresh(
                    batch,
                    stack,
                    pre_refresh_hook.as_deref(),
                    lsp_manager,
                    &mut hover_state,
                    &mut refs_panel,
                    &mut watch_status,
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
fn finish_compose(
    state: &ComposeState,
    repo_root: Option<&Path>,
    comment_list: &mut Vec<Comment>,
    comment_index: &mut CommentIndex,
) -> String {
    let Some(repo_root) = repo_root else {
        return "comment: no repository root to save against".to_owned();
    };
    if state.buffer().is_blank() {
        return "comment: discarded (empty)".to_owned();
    }
    match save_comment(repo_root, state) {
        Ok(comment) => {
            comment_list.push(comment);
            *comment_index = comments::build_index(repo_root, comment_list);
            "comment: saved".to_owned()
        }
        Err(e) => format!("comment: {e}"),
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
) {
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
            View::File(_) | View::Timeline(_) => {
                *goto_status = Some("comment: only available in the diff view".to_owned());
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
        Action::Cancel => {} // nothing open; no effect
        Action::ToggleTimeline => match stack.top_mut() {
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
            // The timeline only relates to the root diff; a `FileView`
            // pushed on top of it (via goto-definition) has nothing for
            // `t` to open.
            View::File(_) => {}
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
fn handle_watch_refresh(
    batch: watch::WatchBatch,
    stack: &mut ViewStack,
    pre_refresh_hook: Option<&dyn PreRefreshHook>,
    lsp_manager: &LspManager,
    hover_state: &mut hover_popup::HoverState,
    refs_panel: &mut Option<RefsPanelState>,
    watch_status: &mut Option<WatchStatus>,
) {
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
    hover_state.bump_generation_for_refresh();

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

    let current_files: Vec<PathBuf> = app
        .files
        .iter()
        .filter(|f| !f.is_deleted && !f.is_binary)
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
    highlighter: &mut LineHighlighter,
    hover_state: &hover_popup::HoverState,
    diagnostics: &DiagnosticsStore,
    refs_panel: Option<&RefsPanel>,
    status_note: Option<&str>,
    comments: &CommentIndex,
    compose: Option<&ComposeState>,
) {
    match view {
        View::Diff(app) => {
            let areas = diff_layout(frame.area(), app.sidebar_visible);
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
            status_bar::render(frame, areas.status, app, effective_layout, status_note);
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
        }
        View::File(file) => {
            let areas = file_view::layout(frame.area());
            file_view::render(frame, areas.content, file, diagnostics);
            file_view::render_status(frame, areas.status, file, status_note);
            if let Some(row) = view.cursor_screen_row() {
                hover_popup::render(frame, areas.content, row, hover_state);
            }
            if let Some(panel) = refs_panel {
                refs_panel::render(frame, areas.content, panel);
            }
        }
        View::Timeline(timeline) => {
            let area = frame.area();
            timeline_view::render(frame, area, timeline, highlighter);
        }
    }
}

struct DiffAreas {
    sidebar: Option<Rect>,
    diff: Rect,
    status: Rect,
}

fn diff_layout(area: Rect, sidebar_visible: bool) -> DiffAreas {
    let rows = RatatuiLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(STATUS_BAR_HEIGHT)])
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

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Ensures a panicking render doesn't leave the user's terminal stuck in
/// raw mode / the alternate screen. Installed once, before the terminal is
/// touched, so it's active for the whole session.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}
