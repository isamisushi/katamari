//! Renders the single-line mode bar: repo name, position within the diff,
//! any in-progress key sequence, and a hint of the available bindings.

use crate::ui::app::{App, Layout};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

const HINTS: &str = "j/k move  C-d/C-u half-page  gg/G top/bottom  ]c/[c hunk  ]f/[f file  ]d/[d diag  b sidebar  s layout  K hover  gd def  gr refs  C-o/C-t jump  Tab symbol  c comment  C toggle  q quit";

/// `effective_layout` is `diff_view::effective_layout(app.layout, pane_width)`
/// — the layout actually being drawn this frame, which may differ from
/// `app.layout` (what the user asked for) when the terminal is too narrow
/// for side-by-side. The two are compared here, not passed as a single
/// value, because only that comparison tells us whether a note is needed.
///
/// `status_note` is a transient one-line message from outside `App`'s own
/// state — currently a pending/failed hover request or LSP server status
/// (see [`crate::ui::hover_popup::HoverState::status_hint`]) — shown the
/// same way the pending-key indicator is. It lives outside `App` because
/// issuing and tracking it isn't a pure state transition; see
/// `hover_popup`'s module docs for why.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    effective_layout: Layout,
    status_note: Option<&str>,
) {
    let position = format!("{}/{}", app.cursor + 1, app.rows.len().max(1));
    let mut spans = vec![
        Span::styled(
            format!(" {} ", app.repo_name),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("· {position} ")),
    ];

    if !app.pending_keys.is_empty() {
        spans.push(Span::styled(
            format!("· {} ", app.pending_keys),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if app.watch_mode {
        spans.push(Span::styled(
            "· \u{29BF} watch ", // ⦿ — BULLSEYE
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if app.layout == Layout::SideBySide && effective_layout == Layout::Unified {
        spans.push(Span::styled(
            "· side-by-side needs a wider terminal, showing unified ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if let Some(note) = status_note {
        spans.push(Span::styled(
            format!("· {note} "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }

    spans.push(Span::styled(
        format!("· {HINTS}"),
        Style::default().fg(Color::DarkGray),
    ));

    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), area);
}
