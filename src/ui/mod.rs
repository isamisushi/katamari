//! Owns the terminal session: entering/leaving the alternate screen, the
//! panic hook that guarantees the terminal is restored even on a crash, and
//! the draw/input loop that turns key presses into [`Action`]s via the
//! keymap trie. `main.rs` only ever calls [`run`] — everything about how
//! the screen is laid out into sidebar/diff/status panes lives here, not in
//! the entrypoint.

pub mod app;
pub mod diff_view;
pub mod sidebar;
pub mod status_bar;
pub mod text;

pub use app::App;

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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use std::io::{self, Stdout};

const SIDEBAR_WIDTH: u16 = 30;
const STATUS_BAR_HEIGHT: u16 = 1;

/// Runs the full-screen diff review UI until the user quits. Installs a
/// panic hook and enters the alternate screen on the way in, and restores
/// the terminal on every exit path, including panics.
pub fn run(app: &mut App) -> Result<()> {
    install_panic_hook();
    let mut terminal = init_terminal()?;

    let keymap = Keymap::from_bindings(&vim_preset());
    let mut resolver = keymap.resolver();
    let mut highlighter = LineHighlighter::new();

    let result = event_loop(&mut terminal, app, &mut resolver, &mut highlighter);

    restore_terminal(&mut terminal)?;
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    resolver: &mut Resolver<'_>,
    highlighter: &mut LineHighlighter,
) -> Result<()> {
    loop {
        let size = terminal.size()?;
        let areas = layout(
            Rect::new(0, 0, size.width, size.height),
            app.sidebar_visible,
        );
        app.set_viewport_height(areas.diff.height as usize);

        terminal.draw(|frame| draw(frame, app, highlighter))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match resolver.feed(KeyChord::from(key)) {
                StepResult::Matched(action) => {
                    app.pending_keys.clear();
                    app.update(action);
                }
                StepResult::Pending => app.pending_keys = resolver.pending_display(),
                StepResult::Cancelled => app.pending_keys.clear(),
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn draw(frame: &mut Frame, app: &App, highlighter: &mut LineHighlighter) {
    let areas = layout(frame.area(), app.sidebar_visible);
    if let Some(sidebar_area) = areas.sidebar {
        sidebar::render(frame, sidebar_area, app);
    }
    diff_view::render(frame, areas.diff, app, highlighter);
    status_bar::render(frame, areas.status, app);
}

struct Areas {
    sidebar: Option<Rect>,
    diff: Rect,
    status: Rect,
}

fn layout(area: Rect, sidebar_visible: bool) -> Areas {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(STATUS_BAR_HEIGHT)])
        .split(area);
    let (main, status) = (rows[0], rows[1]);

    if sidebar_visible {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(0)])
            .split(main);
        Areas {
            sidebar: Some(cols[0]),
            diff: cols[1],
            status,
        }
    } else {
        Areas {
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
