//! `Action::ToggleAgentPanel` — a full-screen, read-only view of the
//! resident ACP agent's transcript, shaped like
//! [`crate::ui::lsp_inspector::LspInspectorView`] but much simpler: one
//! scrolling pane, no multi-pane focus, no journal-selection/copy concept.
//! Like the inspector, this view only *borrows* an [`AgentHandle`] — the
//! session it displays lives on `ui::mod`'s own manager thread (see
//! [`crate::acp::session`]) and keeps running whether or not this view is
//! currently pushed onto the [`crate::ui::view::ViewStack`]; closing the
//! panel (`Esc`/`A` again) never kills it, and reopening it shows the same
//! transcript picked up where it left off.

use crate::acp::session::{AgentHandle, TranscriptLine, TurnState};
use crate::keymap::Action;
use crate::ui::mouse::FrameGeometry;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

pub struct AgentPanelView {
    handle: AgentHandle,
    last_revision: u64,
    lines: Vec<TranscriptLine>,
    state: TurnState,
    adapter_description: Option<String>,
    /// Auto-scroll-to-bottom while the transcript is streaming — the same
    /// `follow` idiom [`crate::ui::lsp_inspector::LspInspectorView`] uses
    /// for its own Journal pane. Cleared the moment the reviewer scrolls
    /// manually; `Bottom` (`G`) is the only way back to following.
    follow: bool,
    scroll_offset: usize,
    viewport_height: usize,
    pub pending_keys: String,
    pub should_quit: bool,
}

impl AgentPanelView {
    pub fn new(handle: AgentHandle) -> Self {
        let mut view = Self {
            handle,
            last_revision: 0,
            lines: Vec::new(),
            state: TurnState::Idle,
            adapter_description: None,
            follow: true,
            scroll_offset: 0,
            viewport_height: 1,
            pending_keys: String::new(),
            should_quit: false,
        };
        view.refresh();
        view
    }

    /// Poll-by-revision refresh, exactly
    /// [`crate::ui::lsp_inspector::LspInspectorView`]'s own
    /// `last_event_revision` idiom: a no-op unless the store actually
    /// changed since the last call, called at the top of both
    /// [`Self::update`] and [`Self::render`] so neither path can act on a
    /// stale transcript.
    fn refresh(&mut self) {
        let revision = self.handle.revision();
        if revision == self.last_revision {
            return;
        }
        self.last_revision = revision;
        self.lines = self.handle.transcript();
        self.state = self.handle.state();
        self.adapter_description = self.handle.adapter_description();
        if self.follow {
            self.scroll_to_bottom();
        } else {
            self.clamp_scroll();
        }
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.viewport_height)
    }

    fn clamp_scroll(&mut self) {
        self.scroll_offset = self.scroll_offset.min(self.max_scroll());
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll_offset = self.max_scroll();
    }

    pub fn set_viewport_height(&mut self, height: usize) {
        self.viewport_height = height.max(1);
        if self.follow {
            self.scroll_to_bottom();
        } else {
            self.clamp_scroll();
        }
    }

    /// No soft-wrap — a transcript line is shown as one row, truncated
    /// rather than wrapped, matching `View::Timeline`/`View::Log`'s own
    /// no-op here (see their docs).
    pub fn set_content_width(&mut self, _width: usize) {}

    /// A cheap, comparable key for `ui::mod`'s hover-staleness check — this
    /// view has no hover concept at all (see [`Self::hover_query`]), but
    /// still needs *some* value that changes as the reviewer scrolls, the
    /// same reason every other view reports one.
    pub fn cursor_key(&self) -> (usize, usize) {
        (self.scroll_offset, 0)
    }

    pub fn hover_query(&self) -> Option<crate::ui::hover_popup::HoverQuery> {
        None
    }

    pub fn update(&mut self, action: Action) {
        self.refresh();
        let half_page = (self.viewport_height / 2).max(1);
        match action {
            Action::ToggleAgentPanel | Action::Cancel => self.should_quit = true,
            Action::CursorDown => {
                self.follow = false;
                self.scroll_offset = (self.scroll_offset + 1).min(self.max_scroll());
            }
            Action::CursorUp => {
                self.follow = false;
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            Action::HalfPageDown => {
                self.follow = false;
                self.scroll_offset = (self.scroll_offset + half_page).min(self.max_scroll());
            }
            Action::HalfPageUp => {
                self.follow = false;
                self.scroll_offset = self.scroll_offset.saturating_sub(half_page);
            }
            Action::Top => {
                self.follow = false;
                self.scroll_offset = 0;
            }
            Action::Bottom => {
                self.follow = true;
                self.scroll_to_bottom();
            }
            _ => {}
        }
    }

    /// Self-contained, like [`crate::ui::lsp_inspector::LspInspectorView::render`]
    /// — no `hints`/`keymap` parameters needed, since this view's own hint
    /// text is static rather than keymap-derived (unlike the main diff
    /// view's status bar).
    pub fn render(&mut self, frame: &mut Frame, area: Rect, _geometry: &mut FrameGeometry) {
        self.refresh();

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        let (content_area, footer_area) = (layout[0], layout[1]);

        let title = match &self.adapter_description {
            Some(desc) => format!(" agent — {desc} "),
            None => " agent ".to_owned(),
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(content_area);
        frame.render_widget(block, content_area);
        self.set_viewport_height(inner.height as usize);

        let rows: Vec<Line> = self
            .lines
            .iter()
            .skip(self.scroll_offset)
            .take(inner.height as usize)
            .map(|line| {
                let style = if line.is_system() {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(line.text().to_owned(), style))
            })
            .collect();
        frame.render_widget(Paragraph::new(rows), inner);

        // Static, not keymap-derived (see the struct docs) — mirrors
        // `AGENT_BUSY_MSG`'s own precedent of a hardcoded key mention. The
        // hint alternates on `self.state.is_active()`: cancel is only ever
        // useful while a turn's actually running, and a follow-up ask would
        // only get rejected while one is (see `Action::AskAgent`'s
        // `View::Agent` arm), so showing both regardless of state would be
        // half-misleading either way.
        let footer = format!(
            "{} · j/k scroll · gg/G top/bottom · {} · Esc/A close",
            self.state.status_text(),
            if self.state.is_active() {
                "C-g cancel"
            } else {
                "a follow-up"
            }
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                footer,
                Style::default().fg(Color::DarkGray),
            ))),
            footer_area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::session;

    fn handle() -> AgentHandle {
        session::for_test()
    }

    #[test]
    fn new_starts_idle_with_an_empty_transcript() {
        let view = AgentPanelView::new(handle());
        assert!(view.lines.is_empty());
        assert_eq!(view.state, TurnState::Idle);
    }

    #[test]
    fn toggle_agent_panel_and_cancel_both_request_close() {
        let mut view = AgentPanelView::new(handle());
        view.update(Action::ToggleAgentPanel);
        assert!(view.should_quit);

        let mut view = AgentPanelView::new(handle());
        view.update(Action::Cancel);
        assert!(view.should_quit);
    }

    #[test]
    fn cursor_down_stops_following_and_advances_the_offset() {
        let mut view = AgentPanelView::new(handle());
        view.lines = vec![
            TranscriptLine::System("one".to_owned()),
            TranscriptLine::System("two".to_owned()),
            TranscriptLine::System("three".to_owned()),
        ];
        view.viewport_height = 1;
        view.follow = true;
        view.scroll_offset = 0;
        view.update(Action::CursorDown);
        assert!(!view.follow);
        assert_eq!(view.scroll_offset, 1);
    }

    #[test]
    fn bottom_resumes_following_and_snaps_to_the_end() {
        let mut view = AgentPanelView::new(handle());
        view.lines = vec![
            TranscriptLine::System("one".to_owned()),
            TranscriptLine::System("two".to_owned()),
            TranscriptLine::System("three".to_owned()),
        ];
        view.viewport_height = 1;
        view.follow = false;
        view.scroll_offset = 0;
        view.update(Action::Bottom);
        assert!(view.follow);
        assert_eq!(view.scroll_offset, 2);
    }
}
