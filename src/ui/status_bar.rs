//! Renders the mode bar: repo name, position within the diff, any
//! in-progress key sequence, and a hint of the available bindings — wrapped
//! onto as many rows as [`hints::required_height`] says this frame needs
//! (see that function's docs), rather than a single row that used to
//! silently truncate on a narrow terminal.

use crate::ui::app::{App, Layout};
use crate::ui::compose;
use crate::ui::hints::{self, HintItem};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

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
///
/// `search_prompt` is `Some((text, char_cursor_index))` exactly while
/// Issue #5's `/` overlay is open, and takes **absolute priority** over
/// `status_note` on this line — deliberately not folded into `status_note`
/// itself (a plain `&str`) the way every other transient note is: a
/// hover-status note computed the same frame would otherwise silently mask
/// the echo mid-typing (see `ui::mod`'s event loop, where `hover_state`'s
/// `status_hint()` outranks `goto_status`/`lsp_status`/`watch_status` in
/// the chain that produces `status_note`), and a bare `&str` has no way to
/// carry a cursor position for [`compose::cursor_marked_line`] to mark.
/// Threaded straight from `ui::mod`'s live `search_prompt: Option<SearchPromptState>`
/// every frame while the prompt is open, unlike `status_note`'s note-typed
/// siblings, which are one-shot until something else replaces them.
///
/// `hint_items` is [`hints::diff_view_items`] read off the session's active
/// [`crate::keymap::Keymap`] — built once per frame by the caller (which
/// also uses it to size `area` via [`hints::required_height`] before the
/// frame was even split into panes) rather than rebuilt here, so the exact
/// same list that sized the area is the one wrapped into it.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    effective_layout: Layout,
    status_note: Option<&str>,
    search_prompt: Option<(&str, usize)>,
    hint_items: &[HintItem],
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

    if let Some(label) = &app.scope_label {
        spans.push(Span::styled(
            format!("· {label} "),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Same slot family as `scope_label` (what subset of the repo's changes
    // is on screen), rendered separately because the two compose: a
    // revision diff scoped to one of its units shows both.
    if let Some(filter) = app.unit_filter() {
        spans.push(Span::styled(
            format!(
                "· unit {}/{}: {} ",
                filter.index, filter.total, filter.label
            ),
            Style::default()
                .fg(Color::Cyan)
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

    if let Some((text, cursor)) = search_prompt {
        // Absolute priority over `status_note` — see this function's docs.
        // `compose::cursor_marked_line` already returns styled spans (the
        // char under the cursor reverse-video'd), so these are appended
        // directly rather than wrapped in one more `Span::styled` the way
        // `status_note`'s plain text is.
        spans.push(Span::raw("\u{b7} /"));
        spans.extend(compose::cursor_marked_line(text, cursor).spans);
        spans.push(Span::raw(" "));
    } else if let Some(note) = status_note {
        spans.push(Span::styled(
            format!("· {note} "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let wrapped = hints::wrap_for_area(hint_items, area.width);
    let mut lines = vec![Line::from(spans)];
    lines.extend(hints::render_lines(&wrapped));
    frame.render_widget(Paragraph::new(lines), area);
}
