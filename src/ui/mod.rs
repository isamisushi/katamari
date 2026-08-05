//! Owns the terminal session: entering/leaving the alternate screen, the
//! panic hook that guarantees the terminal is restored even on a crash, and
//! the draw/input loop that turns key presses into [`Action`]s via the
//! keymap trie. `main.rs` only ever calls [`run`] — everything about how a
//! screen is laid out into panes, and which screen is currently active,
//! lives here, not in the entrypoint.
//!
//! Since M3a, this is also the one place that bridges two worlds: terminal
//! input, which arrives synchronously from crossterm, and LSP activity
//! (hover responses, `$/progress` notifications), which arrives
//! asynchronously from background threads owned by [`crate::lsp::LspManager`].
//! Both are funneled onto one channel (see [`AppEvent`]) so the render loop
//! has a single place to wait, with a short timeout, for "anything worth
//! redrawing for."

pub mod app;
pub mod diff_view;
pub mod file_view;
pub mod hover_popup;
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
use crate::lsp::{HoverResult, LspError, LspEvent, LspManager, progress_status_text};
use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout as RatatuiLayout, Rect};
use std::io::{self, Stdout};
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
    Lsp(LspEvent),
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

    let (lsp_tx, lsp_rx) = mpsc::channel::<LspEvent>();
    spawn_lsp_forwarder(lsp_rx, app_tx);
    let lsp_manager = LspManager::new(lsp_tx);

    let result = event_loop(
        &mut terminal,
        stack,
        &mut resolver,
        &mut highlighter,
        &app_rx,
        &lsp_manager,
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

/// Relays `LspManager`'s events (which every spawned server's supervisor
/// thread sends into, unlabeled — see that module's docs on the M3b TODO
/// this implies for multiple concurrent servers) onto the shared
/// [`AppEvent`] channel, wrapping each one. A thin, permanent thread rather
/// than handing `LspManager` the `Sender<AppEvent>` type directly, so `lsp`
/// stays free of any dependency on `ui`'s event-loop plumbing.
fn spawn_lsp_forwarder(rx: Receiver<LspEvent>, tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        for event in rx {
            if tx.send(AppEvent::Lsp(event)).is_err() {
                break;
            }
        }
    });
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    stack: &mut ViewStack,
    resolver: &mut Resolver<'_>,
    highlighter: &mut LineHighlighter,
    app_rx: &Receiver<AppEvent>,
    lsp_manager: &LspManager,
) -> Result<()> {
    let mut hover_state = hover_popup::HoverState::default();
    let mut pending_hover: Option<(u64, Receiver<Result<HoverResult, LspError>>)> = None;
    let mut lsp_status: Option<String> = None;

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

        let status_note = hover_state.status_hint().or_else(|| lsp_status.clone());
        terminal.draw(|frame| {
            draw(
                frame,
                stack.top(),
                highlighter,
                &hover_state,
                status_note.as_deref(),
            )
        })?;

        match app_rx.recv_timeout(EVENT_POLL_INTERVAL) {
            Ok(AppEvent::Terminal(Event::Key(key))) => {
                if key.kind == KeyEventKind::Press {
                    match resolver.feed(KeyChord::from(key)) {
                        StepResult::Matched(action) => {
                            stack.top_mut().clear_pending_keys();
                            handle_action(
                                action,
                                stack,
                                &mut hover_state,
                                &mut pending_hover,
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
            Ok(AppEvent::Lsp(LspEvent::Notification { method, params })) => {
                if method == "$/progress" {
                    lsp_status = progress_status_text(&params);
                }
            }
            Ok(AppEvent::Lsp(LspEvent::Closed { .. })) => lsp_status = None,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }

        if stack.top().should_quit() && !stack.pop() {
            return Ok(());
        }
    }
}

/// Applies one matched [`Action`], handling the two concerns the pure
/// `App`/`FileView::update` methods can't: issuing an LSP request for
/// `Hover`, and letting an open hover popup intercept `j`/`k`/`K`/Esc for
/// scrolling/closing before they'd otherwise move the cursor or reopen it.
/// Every other action goes through the view's own `update`, with the hover
/// popup invalidated afterward if the action actually changed what's under
/// the cursor (compared via [`View::hover_cursor_key`], not by hardcoding
/// which actions move the cursor).
fn handle_action(
    action: Action,
    stack: &mut ViewStack,
    hover_state: &mut hover_popup::HoverState,
    pending_hover: &mut Option<(u64, Receiver<Result<HoverResult, LspError>>)>,
    lsp_manager: &LspManager,
) {
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
        Action::Cancel => {} // nothing open; Esc otherwise has no effect
        other => {
            let before = stack.top().hover_cursor_key();
            stack.top_mut().update(other);
            if stack.top().hover_cursor_key() != before {
                hover_state.invalidate();
            }
        }
    }
}

fn draw(
    frame: &mut Frame,
    view: &View,
    highlighter: &mut LineHighlighter,
    hover_state: &hover_popup::HoverState,
    status_note: Option<&str>,
) {
    match view {
        View::Diff(app) => {
            let areas = diff_layout(frame.area(), app.sidebar_visible);
            if let Some(sidebar_area) = areas.sidebar {
                sidebar::render(frame, sidebar_area, app);
            }
            let effective_layout = diff_view::effective_layout(app.layout, areas.diff.width);
            diff_view::render(frame, areas.diff, app, highlighter, effective_layout);
            status_bar::render(frame, areas.status, app, effective_layout, status_note);
            if let Some(row) = view.cursor_screen_row() {
                hover_popup::render(frame, areas.diff, row, hover_state);
            }
        }
        View::File(file) => {
            let areas = file_view::layout(frame.area());
            file_view::render(frame, areas.content, file);
            file_view::render_status(frame, areas.status, file, status_note);
            if let Some(row) = view.cursor_screen_row() {
                hover_popup::render(frame, areas.content, row, hover_state);
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
