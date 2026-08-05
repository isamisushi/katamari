//! Owns the terminal session: entering/leaving the alternate screen, the
//! panic hook that guarantees the terminal is restored even on a crash, and
//! the draw/input loop that turns key presses into [`Action`]s via the
//! keymap trie. `main.rs` only ever calls [`run`] — everything about how a
//! screen is laid out into panes, and which screen is currently active,
//! lives here, not in the entrypoint.

pub mod app;
pub mod diff_view;
pub mod file_view;
pub mod scroll;
pub mod sidebar;
pub mod status_bar;
pub mod text;
pub mod view;

pub use app::App;
pub use file_view::FileView;
pub use view::{View, ViewStack};

use crate::highlight::LineHighlighter;
use crate::keymap::{KeyChord, Keymap, Resolver, StepResult, vim_preset};
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

const SIDEBAR_WIDTH: u16 = 30;
const STATUS_BAR_HEIGHT: u16 = 1;

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

    let result = event_loop(&mut terminal, stack, &mut resolver, &mut highlighter);

    restore_terminal(&mut terminal)?;
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    stack: &mut ViewStack,
    resolver: &mut Resolver<'_>,
    highlighter: &mut LineHighlighter,
) -> Result<()> {
    loop {
        let size = terminal.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        let content_height = match stack.top() {
            View::Diff(app) => diff_layout(area, app.sidebar_visible).diff.height,
            View::File(_) => file_view::layout(area).content.height,
        };
        stack.top_mut().set_viewport_height(content_height as usize);

        terminal.draw(|frame| draw(frame, stack.top(), highlighter))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match resolver.feed(KeyChord::from(key)) {
                StepResult::Matched(action) => {
                    stack.top_mut().clear_pending_keys();
                    stack.top_mut().update(action);
                }
                StepResult::Pending => stack.top_mut().set_pending_keys(resolver.pending_display()),
                StepResult::Cancelled => stack.top_mut().clear_pending_keys(),
            }
        }

        if stack.top().should_quit() && !stack.pop() {
            return Ok(());
        }
    }
}

fn draw(frame: &mut Frame, view: &View, highlighter: &mut LineHighlighter) {
    match view {
        View::Diff(app) => {
            let areas = diff_layout(frame.area(), app.sidebar_visible);
            if let Some(sidebar_area) = areas.sidebar {
                sidebar::render(frame, sidebar_area, app);
            }
            let effective_layout = diff_view::effective_layout(app.layout, areas.diff.width);
            diff_view::render(frame, areas.diff, app, highlighter, effective_layout);
            status_bar::render(frame, areas.status, app, effective_layout);
        }
        View::File(file) => {
            let areas = file_view::layout(frame.area());
            file_view::render(frame, areas.content, file);
            file_view::render_status(frame, areas.status, file);
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
