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
pub mod diff_view;
pub mod file_view;
pub mod hover_popup;
pub mod navigation;
pub mod refs_panel;
pub mod scroll;
pub mod sidebar;
pub mod status_bar;
pub mod symbols;
pub mod text;
pub mod view;

pub use app::App;
pub use file_view::FileView;
pub use view::{View, ViewStack};

use crate::highlight::LineHighlighter;
use crate::keymap::{Action, KeyChord, Keymap, Resolver, StepResult, vim_preset};
use crate::lsp::adapter::Language;
use crate::lsp::client::uri_to_path;
use crate::lsp::manager::ServerState;
use crate::lsp::{
    DefinitionResult, DiagnosticsStore, HoverResult, LspError, LspManager, ReferencesResult,
    ServerEvent, parse_publish_diagnostics, progress_status_text,
};
use crate::ui::navigation::{JumpEntry, JumpStack, navigate_to};
use crate::ui::refs_panel::RefsPanel;
use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use lsp_types::PositionEncodingKind;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout as RatatuiLayout, Rect};
use std::collections::HashSet;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

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
}

/// Runs the full-screen UI until the view stack empties (every view has been
/// quit or popped back past the root). Installs a panic hook and enters the
/// alternate screen on the way in, and restores the terminal on every exit
/// path, including panics.
pub fn run(stack: &mut ViewStack) -> Result<()> {
    install_panic_hook();
    let mut terminal = init_terminal()?;

    let keymap = Keymap::from_bindings(&vim_preset());
    let mut resolver = keymap.resolver();
    let mut highlighter = LineHighlighter::new();

    let (app_tx, app_rx) = mpsc::channel::<AppEvent>();
    spawn_input_thread(app_tx.clone());

    let (lsp_tx, lsp_rx) = mpsc::channel::<ServerEvent>();
    spawn_lsp_forwarder(lsp_rx, app_tx);
    let lsp_manager = LspManager::new(lsp_tx);

    // Proactively `didOpen`s the files this session starts out looking at,
    // so diagnostics gutters have something to show without the user
    // hovering first — see `LspManager::warm_up`'s docs for why hovering
    // alone isn't enough to make that happen promptly.
    let startup_status = warm_up_root(stack.top(), &lsp_manager);

    let result = event_loop(
        &mut terminal,
        stack,
        &mut resolver,
        &mut highlighter,
        &app_rx,
        &lsp_manager,
        startup_status,
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

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    stack: &mut ViewStack,
    resolver: &mut Resolver<'_>,
    highlighter: &mut LineHighlighter,
    app_rx: &Receiver<AppEvent>,
    lsp_manager: &LspManager,
    startup_status: Option<String>,
) -> Result<()> {
    let mut hover_state = hover_popup::HoverState::default();
    let mut pending_hover: Option<(u64, Receiver<Result<HoverResult, LspError>>)> = None;
    let mut pending_goto: Option<(JumpEntry, PendingGoto)> = None;
    let mut refs_panel: Option<RefsPanelState> = None;
    let mut jump_stack = JumpStack::new();
    let mut diagnostics = DiagnosticsStore::new();
    let mut lsp_status: Option<String> = startup_status;
    let mut goto_status: Option<String> = None;
    let mut warned_languages: HashSet<Language> = HashSet::new();

    loop {
        let size = terminal.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        let content_height = match stack.top() {
            View::Diff(app) => diff_layout(area, app.sidebar_visible).diff.height,
            View::File(_) => file_view::layout(area).content.height,
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

        let status_note = hover_state
            .status_hint()
            .or_else(|| goto_status.clone())
            .or_else(|| lsp_status.clone());
        terminal.draw(|frame| {
            draw(
                frame,
                stack.top(),
                highlighter,
                &hover_state,
                &diagnostics,
                refs_panel.as_ref().map(|s| &s.panel),
                status_note.as_deref(),
            )
        })?;

        match app_rx.recv_timeout(EVENT_POLL_INTERVAL) {
            Ok(AppEvent::Terminal(Event::Key(key))) => {
                if key.kind == KeyEventKind::Press {
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
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }

        if stack.top().should_quit() && !stack.pop() {
            return Ok(());
        }
    }
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
        Action::Cancel | Action::Confirm => {} // nothing open; no effect
        other => {
            let before = stack.top().hover_cursor_key();
            stack.top_mut().update(other);
            if stack.top().hover_cursor_key() != before {
                hover_state.invalidate();
            }
        }
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

fn draw(
    frame: &mut Frame,
    view: &View,
    highlighter: &mut LineHighlighter,
    hover_state: &hover_popup::HoverState,
    diagnostics: &DiagnosticsStore,
    refs_panel: Option<&RefsPanel>,
    status_note: Option<&str>,
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
            );
            status_bar::render(frame, areas.status, app, effective_layout, status_note);
            if let Some(row) = view.cursor_screen_row() {
                hover_popup::render(frame, areas.diff, row, hover_state);
            }
            if let Some(panel) = refs_panel {
                refs_panel::render(frame, areas.diff, panel);
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
