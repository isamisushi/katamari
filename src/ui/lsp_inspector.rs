//! Full-screen, read-only view of the current session's language servers and
//! their bounded observability journal.

use crate::keymap::Action;
use crate::lsp::{EventLevel, ObservationHandle, ServerIdentity, ServerPhase, ServerSnapshot};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use std::io::{self, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::pane::{Hint as PaneHint, PaneChrome};

const SERVER_LIST_WIDTH: u16 = 34;
// The detail pane has twelve data fields. Values are pre-wrapped into the
// value column, so continuation rows retain their alignment; detail_scroll
// handles vertical overflow in small panes.
const DETAIL_ROWS: u16 = 14;
const DETAIL_LABEL_WIDTH: usize = 12;
/// OSC 52 is sent through a terminal escape sequence, so refusing oversized
/// selections before encoding them keeps a keypress from flooding a
/// terminal/multiplexer and makes the copy operation's privacy boundary
/// explicit. Journal records themselves are already bounded by the observer.
const OSC52_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Servers,
    Detail,
    Journal,
}

#[derive(Debug, Clone)]
struct JournalRow {
    text: String,
    level: EventLevel,
    /// The displayed row came from this complete event. Keeping this mapping
    /// lets visual selection copy each wrapped record once, even if the
    /// cursor starts on the middle of a wrapped record.
    event_index: usize,
}

/// A copy request produced by the inspector after the selection has been
/// cleared. The terminal write lives in `ui::mod`'s event loop, where a
/// failed stdout write can be turned into an in-inspector status message.
#[derive(Debug, PartialEq, Eq)]
pub struct CopyPayload {
    pub text: String,
    pub record_count: usize,
    pub byte_count: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InspectorKeyOutcome {
    /// The key belongs to the ordinary keymap and must continue through the
    /// resolver.
    Unhandled,
    /// The inspector consumed a literal `V`/`y` key without a copy write.
    Handled,
    /// The inspector consumed `y` and asks the event loop to emit OSC 52.
    Copy(CopyPayload),
}

pub struct LspInspectorView {
    observer: ObservationHandle,
    snapshots: Vec<ServerSnapshot>,
    events: Vec<crate::lsp::JournalEvent>,
    log_rows: Vec<JournalRow>,
    selected: usize,
    focus: Focus,
    log_scroll: usize,
    journal_cursor: usize,
    journal_selection_anchor: Option<usize>,
    follow: bool,
    last_event_revision: u64,
    last_snapshot_revision: u64,
    last_snapshot_refresh: Instant,
    pub pending_keys: String,
    pub should_quit: bool,
    /// Height of the journal's actual content area (outside its border).
    /// The full inspector is often much taller than this pane, especially in
    /// the wide layout, so using the screen height here would hide the tail.
    journal_viewport_height: usize,
    journal_viewport_width: usize,
    /// `ViewStack` reports the outer inspector height before each render, but
    /// the journal occupies only the lower pane. Once render has measured the
    /// pane, keep that measurement for key handling; replacing it with the
    /// outer height makes every upward move get re-anchored at the pane tail.
    journal_geometry_measured: bool,
    /// Detail rows are pre-wrapped at the measured pane width. Keep their
    /// viewport separate from Journal so long values do not starve its tail.
    detail_scroll: usize,
    detail_viewport_height: usize,
    detail_total_rows: usize,
    detail_identity: Option<ServerIdentity>,
    status_message: Option<String>,
}

impl LspInspectorView {
    pub fn new(observer: ObservationHandle, preferred: Option<ServerIdentity>) -> Self {
        let mut view = Self {
            observer,
            snapshots: Vec::new(),
            events: Vec::new(),
            log_rows: Vec::new(),
            selected: 0,
            focus: Focus::Servers,
            log_scroll: 0,
            journal_cursor: 0,
            journal_selection_anchor: None,
            follow: true,
            last_event_revision: 0,
            last_snapshot_revision: u64::MAX,
            last_snapshot_refresh: Instant::now(),
            pending_keys: String::new(),
            should_quit: false,
            journal_viewport_height: 1,
            journal_viewport_width: 80,
            journal_geometry_measured: false,
            detail_scroll: 0,
            detail_viewport_height: 1,
            detail_total_rows: 0,
            detail_identity: None,
            status_message: None,
        };
        view.refresh();
        let preferred_index = preferred.and_then(|preferred| {
            view.snapshots
                .iter()
                .position(|snapshot| snapshot.identity == preferred)
        });
        if let Some(index) = preferred_index {
            view.selected = index;
        } else if let Some(index) = view.snapshots.iter().position(|snapshot| {
            matches!(
                snapshot.phase,
                ServerPhase::Unavailable | ServerPhase::Crashed
            )
        }) {
            view.selected = index;
        } else if let Some((index, _)) = view
            .snapshots
            .iter()
            .enumerate()
            .max_by_key(|(_, snapshot)| snapshot.last_activity_ms.unwrap_or(0))
        {
            view.selected = index;
        }
        view
    }

    fn refresh(&mut self) {
        let snapshot_revision = self.observer.revision();
        if snapshot_revision != self.last_snapshot_revision
            || self.last_snapshot_refresh.elapsed() >= Duration::from_secs(1)
        {
            let selected_identity = self
                .snapshots
                .get(self.selected)
                .map(|snapshot| snapshot.identity.clone());
            // State changes are reflected immediately through the revision;
            // the one-second tick exists only to advance the derived age.
            // Rebuilding this snapshot every terminal frame made millisecond
            // counters visually noisy without conveying useful information.
            self.snapshots = self.observer.snapshots();
            self.snapshots
                .sort_by_key(|snapshot| snapshot.identity.label());
            self.last_snapshot_revision = snapshot_revision;
            self.last_snapshot_refresh = Instant::now();
            if let Some(identity) = selected_identity
                && let Some(index) = self
                    .snapshots
                    .iter()
                    .position(|snapshot| snapshot.identity == identity)
            {
                self.selected = index;
            }
        }
        let event_revision = self.observer.event_revision();
        // A visual selection is an intentional snapshot of the rows the user
        // is looking at. Leave the observer revision pending while it is
        // active; rebuilding rows from a live journal would invalidate the
        // anchor as soon as a server emits another log entry. Once the
        // selection is copied or cancelled, the next refresh catches up.
        if event_revision != self.last_event_revision && self.journal_selection_anchor.is_none() {
            self.events = self.observer.events();
            self.last_event_revision = event_revision;
            self.rebuild_log_rows();
        }
        if self.snapshots.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.snapshots.len() - 1);
        }
        self.sync_detail_selection();
        if self.follow {
            self.journal_cursor = self.log_rows.len().saturating_sub(1);
            self.log_scroll = self
                .log_rows
                .len()
                .saturating_sub(self.journal_viewport_height.max(1));
        } else {
            self.journal_cursor = self
                .journal_cursor
                .min(self.log_rows.len().saturating_sub(1));
            self.log_scroll = self.log_scroll.min(
                self.log_rows
                    .len()
                    .saturating_sub(self.journal_viewport_height.max(1)),
            );
            self.ensure_journal_cursor_visible();
        }
    }

    fn sync_detail_selection(&mut self) {
        let selected_identity = self
            .snapshots
            .get(self.selected)
            .map(|snapshot| snapshot.identity.clone());
        if self.detail_identity != selected_identity {
            self.detail_identity = selected_identity;
            self.detail_scroll = 0;
            self.detail_total_rows = 0;
        }
    }

    fn rebuild_log_rows(&mut self) {
        self.log_rows = self
            .events
            .iter()
            .enumerate()
            .flat_map(|(event_index, event)| {
                let level = event.level;
                wrap_journal_line(
                    event.format_line(0).trim_end(),
                    self.journal_viewport_width.max(1),
                )
                .into_iter()
                .map(move |text| JournalRow {
                    text,
                    level,
                    event_index,
                })
            })
            .collect();
        // Width changes are handled by `set_journal_geometry`, which cancels
        // visual mode before calling this rebuild. Event-driven rebuilds are
        // deferred while a selection is active, so this function itself can
        // preserve an anchor safely.
        self.journal_cursor = self
            .journal_cursor
            .min(self.log_rows.len().saturating_sub(1));
    }

    fn set_journal_geometry(&mut self, area: Rect) {
        self.journal_geometry_measured = true;
        self.journal_viewport_height = area.height.saturating_sub(2).max(1) as usize;
        let width = area.width.saturating_sub(2).max(1) as usize;
        if width != self.journal_viewport_width {
            self.journal_viewport_width = width;
            if self.journal_selection_anchor.take().is_some() {
                self.status_message = Some("journal selection cancelled after resize".to_owned());
            }
            self.rebuild_log_rows();
        }
        if self.follow {
            self.journal_cursor = self.log_rows.len().saturating_sub(1);
            self.log_scroll = self
                .log_rows
                .len()
                .saturating_sub(self.journal_viewport_height);
        } else {
            self.ensure_journal_cursor_visible();
        }
    }

    pub fn set_viewport_height(&mut self, height: usize) {
        // Before the first render there is no layout rectangle to measure.
        // render() replaces this fallback with the journal pane's real
        // content height. After that, the generic ViewStack value is the
        // inspector's outer height, not the journal pane's height; accepting
        // it would make a keypress calculate visibility against the wrong
        // viewport and snap a paused cursor back to the pane's bottom.
        if !self.journal_geometry_measured {
            self.journal_viewport_height = height.max(1);
        }
        self.refresh();
    }

    pub fn set_content_width(&mut self, _width: usize) {}

    pub fn cursor_key(&self) -> (usize, usize) {
        (self.selected, self.journal_cursor)
    }

    pub fn hover_query(&self) -> Option<crate::ui::hover_popup::HoverQuery> {
        None
    }

    pub fn update(&mut self, action: Action) {
        self.refresh();
        match action {
            // `q` never reaches here as `Action::Quit`: it's intercepted as
            // a global quit at the keymap resolver, before a matched action
            // is ever dispatched to a view (see `ui::mod::event_loop`'s
            // `StepResult::Matched(Action::Quit)` arm).
            Action::ToggleLspInspector => {
                self.should_quit = true;
            }
            Action::Cancel => {
                if self.journal_selection_anchor.take().is_some() {
                    self.status_message = Some("journal selection cancelled".to_owned());
                } else {
                    self.should_quit = true;
                }
            }
            Action::NextSymbol => {
                self.focus = match self.focus {
                    Focus::Servers => Focus::Detail,
                    Focus::Detail => Focus::Journal,
                    Focus::Journal => Focus::Servers,
                }
            }
            Action::PrevSymbol => {
                self.focus = match self.focus {
                    Focus::Servers => Focus::Journal,
                    Focus::Detail => Focus::Servers,
                    Focus::Journal => Focus::Detail,
                }
            }
            Action::CursorDown => match self.focus {
                Focus::Servers => {
                    self.selected = self
                        .selected
                        .saturating_add(1)
                        .min(self.snapshots.len().saturating_sub(1))
                }
                Focus::Detail => self.move_detail_scroll(1),
                Focus::Journal => self.move_journal_cursor(1),
            },
            Action::CursorUp => match self.focus {
                Focus::Servers => self.selected = self.selected.saturating_sub(1),
                Focus::Detail => self.move_detail_scroll(-1),
                Focus::Journal => self.move_journal_cursor(-1),
            },
            Action::HalfPageDown => match self.focus {
                Focus::Servers => {
                    self.selected = self
                        .selected
                        .saturating_add((self.journal_viewport_height / 2).max(1))
                        .min(self.snapshots.len().saturating_sub(1))
                }
                Focus::Detail => self
                    .move_detail_scroll(((self.detail_viewport_height.max(1) / 2).max(1)) as isize),
                Focus::Journal => self.move_journal_cursor(
                    ((self.journal_viewport_height.max(1) / 2).max(1)) as isize,
                ),
            },
            Action::HalfPageUp => match self.focus {
                Focus::Servers => {
                    self.selected = self
                        .selected
                        .saturating_sub((self.journal_viewport_height / 2).max(1))
                }
                Focus::Detail => self.move_detail_scroll(
                    -((self.detail_viewport_height.max(1) / 2).max(1) as isize),
                ),
                Focus::Journal => self.move_journal_cursor(
                    -((self.journal_viewport_height.max(1) / 2).max(1) as isize),
                ),
            },
            Action::Top => match self.focus {
                Focus::Servers => self.selected = 0,
                Focus::Detail => self.move_detail_scroll_to(0),
                Focus::Journal => self.move_journal_cursor_to(0),
            },
            Action::Bottom => match self.focus {
                Focus::Servers => self.selected = self.snapshots.len().saturating_sub(1),
                Focus::Detail => self.move_detail_scroll_to(usize::MAX),
                Focus::Journal => {
                    self.move_journal_cursor_to(self.log_rows.len().saturating_sub(1))
                }
            },
            _ => {}
        }
    }

    /// Handles the two literal keys that are intentionally not global
    /// actions: `V` starts/toggles visual-line mode and `y` copies it. Keeping
    /// this small bypass in the event loop avoids assigning a yank action that
    /// would unexpectedly change the keymap outside the inspector.
    pub fn handle_literal_key(&mut self, key: KeyEvent) -> InspectorKeyOutcome {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return InspectorKeyOutcome::Unhandled;
        }
        match key.code {
            KeyCode::Char('V') => {
                if self.focus == Focus::Journal {
                    self.toggle_journal_selection();
                } else {
                    self.status_message = Some("V/y are available in Journal focus".to_owned());
                }
                InspectorKeyOutcome::Handled
            }
            KeyCode::Char('y') => {
                if self.focus != Focus::Journal {
                    self.status_message = Some("V/y are available in Journal focus".to_owned());
                    return InspectorKeyOutcome::Handled;
                }
                match self.copy_selection() {
                    Some(payload) => InspectorKeyOutcome::Copy(payload),
                    None => InspectorKeyOutcome::Handled,
                }
            }
            _ => InspectorKeyOutcome::Unhandled,
        }
    }

    pub fn set_copy_status(&mut self, status: impl Into<String>) {
        self.status_message = Some(status.into());
    }

    fn move_journal_cursor(&mut self, amount: isize) {
        let max = self.log_rows.len().saturating_sub(1);
        self.journal_cursor = if amount.is_negative() {
            self.journal_cursor.saturating_sub(amount.unsigned_abs())
        } else {
            self.journal_cursor.saturating_add(amount as usize).min(max)
        };
        self.follow = self.journal_cursor == max;
        self.ensure_journal_cursor_visible();
    }

    fn move_journal_cursor_to(&mut self, row: usize) {
        self.journal_cursor = row.min(self.log_rows.len().saturating_sub(1));
        self.follow = self.journal_cursor == self.log_rows.len().saturating_sub(1);
        self.ensure_journal_cursor_visible();
    }

    fn ensure_journal_cursor_visible(&mut self) {
        let viewport = self.journal_viewport_height.max(1);
        if self.log_rows.is_empty() {
            self.journal_cursor = 0;
            self.log_scroll = 0;
            return;
        }
        if self.journal_cursor < self.log_scroll {
            self.log_scroll = self.journal_cursor;
        } else if self.journal_cursor >= self.log_scroll.saturating_add(viewport) {
            self.log_scroll = self.journal_cursor + 1 - viewport;
        }
        self.log_scroll = self
            .log_scroll
            .min(self.log_rows.len().saturating_sub(viewport));
    }

    fn move_detail_scroll(&mut self, amount: isize) {
        let max = self
            .detail_total_rows
            .saturating_sub(self.detail_viewport_height.max(1));
        self.detail_scroll = if amount.is_negative() {
            self.detail_scroll.saturating_sub(amount.unsigned_abs())
        } else {
            self.detail_scroll.saturating_add(amount as usize).min(max)
        };
    }

    fn move_detail_scroll_to(&mut self, row: usize) {
        let max = self
            .detail_total_rows
            .saturating_sub(self.detail_viewport_height.max(1));
        self.detail_scroll = row.min(max);
    }

    fn toggle_journal_selection(&mut self) {
        if self.log_rows.is_empty() {
            self.status_message = Some("journal: no entries to select".to_owned());
            return;
        }
        if self.journal_selection_anchor.take().is_some() {
            self.status_message = Some("journal selection cancelled".to_owned());
        } else {
            self.journal_selection_anchor = Some(self.journal_cursor);
            self.follow = false;
            self.status_message = Some(
                "visual lines: j/k extend · y copies complete records · Esc cancels".to_owned(),
            );
        }
    }

    fn selection_bounds(&self) -> Option<(usize, usize)> {
        let anchor = self.journal_selection_anchor?;
        Some((
            anchor.min(self.journal_cursor),
            anchor.max(self.journal_cursor),
        ))
    }

    fn copy_selection(&mut self) -> Option<CopyPayload> {
        if self.log_rows.is_empty() {
            self.status_message = Some("journal: no entries to copy".to_owned());
            return None;
        }
        let Some((start, end)) = self.selection_bounds() else {
            self.status_message = Some("journal: press V to select lines first".to_owned());
            return None;
        };
        let Some(rows) = self.log_rows.get(start..=end) else {
            // This is defensive against a stale cursor/anchor after an
            // external resize or a future row-rebuild path. Copy must never
            // turn an empty journal into a panic, nor index outside the
            // currently rendered rows.
            self.journal_selection_anchor = None;
            self.status_message = Some("journal: selection is no longer available".to_owned());
            return None;
        };
        let mut event_indices = Vec::new();
        for row in rows {
            if event_indices.last().copied() != Some(row.event_index) {
                event_indices.push(row.event_index);
            }
        }
        let text = event_indices
            .iter()
            .filter_map(|&index| self.events.get(index))
            .map(|event| event.format_line(0).trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n");
        let byte_count = text.len();
        if byte_count > OSC52_MAX_BYTES {
            self.status_message = Some(format!(
                "journal selection is {byte_count} bytes; copy limit is {OSC52_MAX_BYTES}"
            ));
            return None;
        }
        self.journal_selection_anchor = None;
        Some(CopyPayload {
            text,
            record_count: event_indices.len(),
            byte_count,
        })
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.refresh();
        let outer_title = match self.status_message.as_deref() {
            Some(status) => format!(" LSP Inspector · {status} "),
            None => " LSP Inspector ".to_owned(),
        };
        let outer_hints = [
            PaneHint::new("Tab/BackTab", "focus", true),
            PaneHint::new("I/Esc", "close", true),
            // Split from the `I/Esc` hint above rather than folded into
            // one combined "close" label the way it used to read: `q` now
            // quits the whole katamari session (issue #12), not just this
            // pane — a materially different outcome from `I`/`Esc` popping
            // back to whatever's underneath, so it earns its own label.
            PaneHint::new("q", "quit", true),
        ];
        let outer = PaneChrome::new(outer_title, area.width)
            .focused(false)
            .hints(&outer_hints)
            .block();
        let inner = outer.inner(area);
        frame.render_widget(outer, area);
        if self.snapshots.is_empty() {
            let mut message = vec![
                Line::from(Span::styled(
                    "No language server has been attempted yet.",
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(
                    "Server startup is lazy: hover, definition, references, or diagnostics warm-up will create an instance.",
                ),
                Line::from("Press I or Esc to close, q to quit katamari."),
            ];
            if let Some(error) = self.observer.setup_error() {
                message.push(Line::from(Span::styled(
                    format!("Journal setup warning: {error}"),
                    Style::default().fg(Color::Red),
                )));
            }
            message.push(Line::from(format!("Journal: {}", self.journal_label())));
            frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: true }), inner);
            return;
        }

        let narrow = inner.width < 100;
        if narrow {
            let list_height = (self.snapshots.len().min(5) as u16 * 2 + 2).max(4);
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(list_height),
                    Constraint::Length(DETAIL_ROWS),
                    Constraint::Min(4),
                ])
                .split(inner);
            self.set_journal_geometry(rows[2]);
            self.refresh();
            self.render_servers(frame, rows[0]);
            self.render_detail(frame, rows[1]);
            self.render_log(frame, rows[2]);
        } else {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(SERVER_LIST_WIDTH), Constraint::Min(20)])
                .split(inner);
            self.render_servers(frame, cols[0]);
            let right = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(DETAIL_ROWS), Constraint::Min(4)])
                .split(cols[1]);
            self.set_journal_geometry(right[1]);
            self.refresh();
            self.render_detail(frame, right[0]);
            self.render_log(frame, right[1]);
        }
    }

    fn render_servers(&self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Servers;
        let items = self.snapshots.iter().map(|snapshot| {
            let state =
                if snapshot.phase == ServerPhase::Running && !snapshot.active_progress.is_empty() {
                    format!(
                        "running + active progress ({})",
                        snapshot.active_progress.len()
                    )
                } else {
                    snapshot.phase.to_string()
                };
            let text = format!("{}\n  {}", snapshot.identity, state);
            let style = match snapshot.phase {
                ServerPhase::Running => Style::default().fg(Color::Green),
                ServerPhase::Crashed | ServerPhase::Unavailable => Style::default().fg(Color::Red),
                _ => Style::default().fg(Color::Yellow),
            };
            ListItem::new(text).style(style)
        });
        let mut state = ListState::default();
        state.select(Some(self.selected));
        let hints = [
            PaneHint::new("j/k", "select", true),
            PaneHint::new("C-u/C-d", "page", false),
            PaneHint::new("gg/G", "top/bottom", false),
        ];
        let block = PaneChrome::new(" Servers ", area.width)
            .focused(focused)
            .hints(&hints)
            .block();
        let highlight = if focused {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        frame.render_stateful_widget(
            List::new(items)
                .block(block)
                .highlight_symbol(if focused { "▶ " } else { "  " })
                .highlight_style(highlight),
            area,
            &mut state,
        );
    }

    fn render_detail(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Detail;
        self.sync_detail_selection();
        let Some(snapshot) = self.snapshots.get(self.selected).cloned() else {
            return;
        };
        let value_width = area
            .width
            .saturating_sub(2)
            .saturating_sub((DETAIL_LABEL_WIDTH + 3) as u16)
            .max(1) as usize;
        let lines = detail_lines(&snapshot, &self.journal_label(), value_width);
        self.detail_viewport_height = area.height.saturating_sub(2).max(1) as usize;
        self.detail_total_rows = lines.len();
        let detail_max_scroll = self
            .detail_total_rows
            .saturating_sub(self.detail_viewport_height);
        self.detail_scroll = self.detail_scroll.min(detail_max_scroll);
        let detail_overflow = detail_max_scroll > 0;
        let detail_hints = [
            PaneHint::new("j/k", "scroll", true),
            PaneHint::new("C-u/C-d", "page", false),
            PaneHint::new("gg/G", "top/bottom", false),
        ];
        let block = PaneChrome::new(" Server detail [read-only] ", area.width)
            .focused(focused)
            .hints(if detail_overflow { &detail_hints } else { &[] })
            .block();
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .scroll((self.detail_scroll.min(u16::MAX as usize) as u16, 0)),
            area,
        );
    }

    fn render_log(&self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Journal;
        let height = area.height.saturating_sub(2) as usize;
        let start = self.log_scroll.min(self.log_rows.len());
        let selection = self.selection_bounds();
        let lines = self
            .log_rows
            .iter()
            .skip(start)
            .take(height)
            .enumerate()
            .map(|(offset, row)| {
                let row_index = start + offset;
                let level = match row.level {
                    EventLevel::Error => Color::Red,
                    EventLevel::Warn => Color::Yellow,
                    EventLevel::Debug => Color::DarkGray,
                    EventLevel::Info => Color::Reset,
                };
                let selected =
                    selection.is_some_and(|(from, to)| row_index >= from && row_index <= to);
                let cursor = focused && row_index == self.journal_cursor;
                let mut style = Style::default().fg(level);
                if selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                if cursor {
                    style = style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
                }
                Line::from(Span::styled(row.text.clone(), style))
            })
            .collect::<Vec<_>>();
        let title = if self.follow {
            " Journal (following) "
        } else {
            " Journal (paused; G to follow) "
        };
        let hints = [
            PaneHint::new("j/k", "move", false),
            PaneHint::new("C-u/C-d", "page", false),
            PaneHint::new("gg/G", "top/bottom", false),
            PaneHint::new("V", "select", true),
            PaneHint::new("y", "yank", true),
        ];
        let block = PaneChrome::new(title, area.width)
            .focused(focused)
            .hints(&hints)
            .block();
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn journal_label(&self) -> String {
        if let Some(error) = self.observer.setup_error() {
            if let Some(path) = self.observer.journal_dir() {
                format!("unavailable ({error}); attempted {}", path.display())
            } else {
                format!("unavailable ({error})")
            }
        } else if let Some(path) = self.observer.journal_dir() {
            path.display().to_string()
        } else {
            "disabled (in-memory only)".to_owned()
        }
    }
}

fn detail_lines(
    snapshot: &ServerSnapshot,
    journal_label: &str,
    value_width: usize,
) -> Vec<Line<'static>> {
    let command = snapshot.program.as_deref().map_or_else(
        || "(not resolved)".to_owned(),
        |program| {
            if snapshot.args.is_empty() {
                program.to_owned()
            } else {
                format!("{program} {}", snapshot.args.join(" "))
            }
        },
    );
    let progress = if snapshot.active_progress.is_empty() {
        "none".to_owned()
    } else {
        snapshot
            .active_progress
            .iter()
            .map(|progress| {
                let mut text = progress.token.clone();
                if let Some(title) = &progress.title {
                    text.push_str(&format!(": {title}"));
                }
                if let Some(message) = &progress.message {
                    text.push_str(&format!(" — {message}"));
                }
                if let Some(percent) = progress.percentage {
                    text.push_str(&format!(" ({percent}%)"));
                }
                text
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let muted = Style::default().fg(Color::DarkGray);
    let mut lines = Vec::new();
    lines.extend(detail_field_lines(
        "identity",
        &snapshot.identity.to_string(),
        Style::default(),
        value_width,
    ));
    lines.extend(detail_field_lines(
        "state",
        &format!("generation #{} · {}", snapshot.generation, snapshot.phase),
        phase_style(snapshot.phase),
        value_width,
    ));
    lines.extend(detail_field_lines(
        "command",
        &command,
        Style::default(),
        value_width,
    ));
    lines.extend(detail_field_lines(
        "process",
        &format!(
            "pid={} · age={} · exit={}",
            snapshot
                .pid
                .map_or_else(|| "-".to_owned(), |pid| pid.to_string()),
            format_age(snapshot.state_age_ms),
            snapshot.exit_status.as_deref().unwrap_or("-")
        ),
        Style::default(),
        value_width,
    ));
    lines.extend(detail_field_lines(
        "activity",
        &snapshot.last_activity_ms.map_or_else(
            || "-".to_owned(),
            |at| format!("{} ago", format_age(now_ms().saturating_sub(at))),
        ),
        Style::default(),
        value_width,
    ));
    lines.extend(detail_field_lines(
        "server",
        &format!(
            "{} {}",
            snapshot.server_name.as_deref().unwrap_or("-"),
            snapshot.server_version.as_deref().unwrap_or("")
        ),
        Style::default(),
        value_width,
    ));
    lines.extend(detail_field_lines(
        "capabilities",
        &format!(
            "hover={} definition={} references={} diagnostics={}",
            snapshot.capabilities.hover,
            snapshot.capabilities.definition,
            snapshot.capabilities.references,
            snapshot.capabilities.diagnostics
        ),
        capability_style(snapshot),
        value_width,
    ));
    lines.extend(detail_field_lines(
        "encoding",
        snapshot.position_encoding.as_deref().unwrap_or("-"),
        if snapshot.position_encoding.is_some() {
            Style::default()
        } else {
            muted
        },
        value_width,
    ));
    lines.extend(detail_field_lines(
        "documents",
        &format!(
            "open={} · queued={} · in flight={}",
            snapshot.open_documents, snapshot.queued_requests, snapshot.in_flight_requests
        ),
        Style::default(),
        value_width,
    ));
    lines.extend(detail_field_lines(
        "progress",
        &progress,
        if snapshot.active_progress.is_empty() {
            muted
        } else {
            Style::default().fg(Color::Yellow)
        },
        value_width,
    ));
    lines.extend(detail_field_lines(
        "last error",
        snapshot.last_error.as_deref().unwrap_or("-"),
        if snapshot.last_error.is_some() {
            Style::default().fg(Color::Red)
        } else {
            muted
        },
        value_width,
    ));
    lines.extend(detail_field_lines(
        "journal",
        journal_label,
        Style::default(),
        value_width,
    ));
    lines
}

fn detail_field_lines(
    label: &str,
    value: &str,
    value_style: Style,
    value_width: usize,
) -> Vec<Line<'static>> {
    debug_assert!(label.width() <= DETAIL_LABEL_WIDTH);
    let label_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let separator_style = Style::default().fg(Color::DarkGray);
    let continuation_prefix = " ".repeat(DETAIL_LABEL_WIDTH + 3);
    wrap_journal_line(value, value_width)
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            if index == 0 {
                Line::from(vec![
                    Span::styled(
                        format!("{label:<width$}", width = DETAIL_LABEL_WIDTH),
                        label_style,
                    ),
                    Span::styled(" │ ", separator_style),
                    Span::styled(value, value_style),
                ])
            } else {
                Line::from(vec![
                    Span::raw(continuation_prefix.clone()),
                    Span::styled(value, value_style),
                ])
            }
        })
        .collect()
}

fn phase_style(phase: ServerPhase) -> Style {
    match phase {
        ServerPhase::Running => Style::default().fg(Color::Green),
        ServerPhase::Crashed | ServerPhase::Unavailable => Style::default().fg(Color::Red),
        ServerPhase::Resolving | ServerPhase::Installing | ServerPhase::Initializing => {
            Style::default().fg(Color::Yellow)
        }
        ServerPhase::NotStarted | ServerPhase::Stopped => Style::default().fg(Color::DarkGray),
    }
}

fn capability_style(snapshot: &ServerSnapshot) -> Style {
    let capabilities = [
        snapshot.capabilities.hover,
        snapshot.capabilities.definition,
        snapshot.capabilities.references,
        snapshot.capabilities.diagnostics,
    ];
    if capabilities.iter().all(|enabled| !enabled) {
        Style::default().fg(Color::DarkGray)
    } else if capabilities.iter().any(|enabled| !enabled) {
        Style::default().fg(Color::Gray)
    } else {
        Style::default()
    }
}

/// Emits a terminal-native clipboard update. OSC 52 avoids invoking a
/// platform-specific clipboard process and can work over SSH or a configured
/// multiplexer; terminal support and policy determine whether the sequence is
/// actually accepted. The caller has already applied the inspector's byte
/// bound and selected-record mapping.
///
/// Hand-rolled on purpose: crossterm 0.29 ships a byte-identical
/// `clipboard::CopyToClipboard`, but only behind its non-default `osc52`
/// feature, which drags in the `base64` crate — a new dependency to replace
/// forty tested lines isn't a trade this repo makes.
pub fn write_osc52(text: &str) -> io::Result<()> {
    if text.len() > OSC52_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "OSC 52 payload exceeds the inspector copy limit",
        ));
    }
    let encoded = base64_encode(text.as_bytes());
    let mut stdout = io::stdout().lock();
    write!(stdout, "\x1b]52;c;{encoded}\x1b\\")?;
    stdout.flush()
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        let second = chunk.get(1).copied();
        encoded.push(
            ALPHABET[((first & 0x03) << 4 | second.map_or(0, |byte| byte >> 4)) as usize] as char,
        );
        if let Some(second) = second {
            encoded.push(
                ALPHABET[((second & 0x0f) << 2 | chunk.get(2).copied().map_or(0, |byte| byte >> 6))
                    as usize] as char,
            );
        } else {
            encoded.push('=');
        }
        if let Some(third) = chunk.get(2).copied() {
            encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

/// Hard-wrap a journal record instead of relying on word boundaries. Server
/// messages commonly contain paths, identifiers, and JSON with no spaces;
/// soft word wrapping would still clip exactly the diagnostic material this
/// view exists to expose. Grapheme-aware width keeps CJK and emoji intact.
fn wrap_journal_line(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = grapheme.width();
        if used > 0 && used + grapheme_width > width {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push_str(grapheme);
        used += grapheme_width;
    }
    rows.push(row);
    rows
}

fn format_age(milliseconds: u128) -> String {
    let seconds = milliseconds / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 60 * 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!(
            "{}h {:02}m {:02}s",
            seconds / (60 * 60),
            (seconds / 60) % 60,
            seconds % 60
        )
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::adapter::LangKey;
    use crate::lsp::observe::{EventSource, JournalEvent};

    fn identity(root: &str) -> ServerIdentity {
        ServerIdentity::new(LangKey::Custom("rust".to_owned()), root)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn rendered_journal_cursor_offset(view: &mut LspInspectorView, area: Rect) -> usize {
        let backend = ratatui::backend::TestBackend::new(area.width, area.height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view.render_log(frame, frame.area()))
            .unwrap();
        let width = terminal.backend().buffer().area.width as usize;
        terminal
            .backend()
            .buffer()
            .content
            .chunks(width)
            .enumerate()
            .find_map(|(screen_row, row)| {
                row.iter()
                    .any(|cell| cell.modifier.contains(Modifier::UNDERLINED))
                    .then_some(screen_row.saturating_sub(1))
            })
            .expect("journal cursor should be rendered in the content area")
    }

    #[test]
    fn empty_state_exposes_lazy_startup_without_a_selection() {
        let view = LspInspectorView::new(crate::lsp::ObservationStore::in_memory(), None);
        assert!(view.snapshots.is_empty());
        assert!(view.events.is_empty());
        assert_eq!(view.selected, 0);
    }

    #[test]
    fn empty_journal_visual_keys_report_status_without_creating_an_invalid_selection() {
        let store = crate::lsp::ObservationStore::in_memory();
        let mut view = LspInspectorView::new(store, None);
        view.update(Action::NextSymbol);
        view.update(Action::NextSymbol);

        assert_eq!(
            view.handle_literal_key(key(KeyCode::Char('V'))),
            InspectorKeyOutcome::Handled
        );
        assert!(view.journal_selection_anchor.is_none());
        assert_eq!(
            view.handle_literal_key(key(KeyCode::Char('y'))),
            InspectorKeyOutcome::Handled
        );
        assert!(view.journal_selection_anchor.is_none());
        assert_eq!(
            view.status_message.as_deref(),
            Some("journal: no entries to copy")
        );
    }

    #[test]
    fn preferred_server_wins_over_failure_and_recent_activity_ordering() {
        let store = crate::lsp::ObservationStore::in_memory();
        let failed = identity("/failed");
        let preferred = identity("/preferred");
        let failed_generation = store.begin_generation(failed.clone(), None, vec![]);
        store.transition(&failed, failed_generation, ServerPhase::Crashed, "exited");
        store.begin_generation(preferred.clone(), None, vec![]);
        let view = LspInspectorView::new(store, Some(preferred.clone()));
        assert_eq!(view.snapshots[view.selected].identity, preferred);
    }

    #[test]
    fn tab_and_backtab_cycle_servers_detail_and_journal_with_multiple_servers() {
        let store = crate::lsp::ObservationStore::in_memory();
        store.begin_generation(identity("/one"), None, vec![]);
        store.begin_generation(identity("/two"), None, vec![]);
        let mut view = LspInspectorView::new(store, None);

        assert_eq!(view.focus, Focus::Servers);
        view.update(Action::CursorDown);
        assert_eq!(view.selected, 1);
        view.update(Action::NextSymbol);
        assert_eq!(view.focus, Focus::Detail);
        let selected = view.selected;
        view.update(Action::CursorDown);
        assert_eq!(view.selected, selected, "detail is explicitly read-only");
        view.update(Action::NextSymbol);
        assert_eq!(view.focus, Focus::Journal);
        view.update(Action::NextSymbol);
        assert_eq!(view.focus, Focus::Servers);
        view.update(Action::PrevSymbol);
        assert_eq!(view.focus, Focus::Journal);
        view.update(Action::PrevSymbol);
        assert_eq!(view.focus, Focus::Detail);
        view.update(Action::PrevSymbol);
        assert_eq!(view.focus, Focus::Servers);
    }

    #[test]
    fn focused_pane_borders_make_each_target_obvious_without_title_suffixes() {
        let store = crate::lsp::ObservationStore::in_memory();
        let server = identity("/repo");
        let generation = store.begin_generation(server.clone(), None, vec![]);
        store.transition(&server, generation, ServerPhase::Running, "ready");
        store.record(JournalEvent::simple(
            EventSource::Ui,
            EventLevel::Info,
            None,
            "event",
        ));
        let mut view = LspInspectorView::new(store, None);
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        for (expected_title, border_x, border_y, action) in [
            ("Servers", 1, 1, Some(Action::NextSymbol)),
            ("Server detail [read-only]", 35, 1, Some(Action::NextSymbol)),
            ("Journal (following", 35, 15, None),
        ] {
            terminal
                .draw(|frame| view.render(frame, frame.area()))
                .unwrap();
            let width = terminal.backend().buffer().area.width as usize;
            let lines = terminal
                .backend()
                .buffer()
                .content
                .chunks(width)
                .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                .collect::<Vec<_>>();
            assert!(
                lines.iter().any(|line| line.contains(expected_title)),
                "focused title {expected_title:?} should be visible"
            );
            assert!(
                lines.iter().all(|line| !line.contains("[focused]")),
                "focus should be conveyed by styling, not a title suffix"
            );
            let focused_border = terminal
                .backend()
                .buffer()
                .cell((border_x, border_y))
                .expect("focused pane border cell");
            assert_eq!(focused_border.fg, Color::Cyan);
            assert!(focused_border.modifier.contains(Modifier::BOLD));
            if expected_title.starts_with("Journal") {
                assert!(lines.iter().any(|line| line.contains("V select · y yank")));
            }
            if let Some(action) = action {
                view.update(action);
            }
        }
    }

    #[test]
    fn moving_up_pauses_follow_and_bottom_resumes_it() {
        let store = crate::lsp::ObservationStore::in_memory();
        for index in 0..8 {
            store.record(JournalEvent::simple(
                EventSource::Ui,
                EventLevel::Info,
                None,
                format!("event {index}"),
            ));
        }
        let mut view = LspInspectorView::new(store, None);
        view.set_viewport_height(2);
        view.update(Action::NextSymbol);
        view.update(Action::NextSymbol);
        view.update(Action::CursorUp);
        assert!(!view.follow);
        view.update(Action::Bottom);
        assert!(view.follow);
    }

    #[test]
    fn paused_journal_cursor_scrolls_up_and_ignores_new_tail_events() {
        let store = crate::lsp::ObservationStore::in_memory();
        for index in 0..12 {
            store.record(JournalEvent::simple(
                EventSource::Ui,
                EventLevel::Info,
                None,
                format!("event {index}"),
            ));
        }
        let mut view = LspInspectorView::new(store.clone(), None);
        let journal_area = Rect::new(0, 0, 120, 8);
        view.set_journal_geometry(journal_area);
        view.update(Action::NextSymbol);
        view.update(Action::NextSymbol);

        let viewport = view.journal_viewport_height;
        let initial_cursor = view.journal_cursor;
        let initial_scroll = view.log_scroll;
        assert_eq!(viewport, 6);
        assert_eq!(initial_cursor, view.log_rows.len() - 1);
        assert_eq!(initial_scroll, view.log_rows.len() - viewport);
        assert_eq!(
            rendered_journal_cursor_offset(&mut view, journal_area),
            viewport - 1
        );

        // The run loop reports the full inspector height before handling a
        // key. Once render has measured the journal pane, that value must not
        // replace the pane height used by cursor visibility calculations.
        view.set_viewport_height(38);
        view.update(Action::CursorUp);
        assert!(!view.follow);
        assert_eq!(view.journal_cursor, initial_cursor - 1);
        assert_eq!(view.log_scroll, initial_scroll);
        assert_eq!(
            rendered_journal_cursor_offset(&mut view, journal_area),
            viewport - 2
        );

        for _ in 0..(viewport - 2) {
            view.set_viewport_height(38);
            view.update(Action::CursorUp);
        }
        assert_eq!(view.journal_cursor, initial_scroll);
        assert_eq!(view.log_scroll, initial_scroll);
        assert_eq!(rendered_journal_cursor_offset(&mut view, journal_area), 0);

        let older_scroll = view.log_scroll;
        view.set_viewport_height(38);
        view.update(Action::CursorUp);
        assert_eq!(view.journal_cursor, older_scroll - 1);
        assert_eq!(view.log_scroll, older_scroll - 1);
        assert_eq!(rendered_journal_cursor_offset(&mut view, journal_area), 0);

        let paused_cursor = view.journal_cursor;
        let paused_scroll = view.log_scroll;
        store.record(JournalEvent::simple(
            EventSource::Ui,
            EventLevel::Info,
            None,
            "arrived while paused",
        ));
        view.set_viewport_height(38);
        assert!(!view.follow);
        assert_eq!(view.journal_cursor, paused_cursor);
        assert_eq!(view.log_scroll, paused_scroll);
        assert_eq!(rendered_journal_cursor_offset(&mut view, journal_area), 0);

        while !view.follow {
            view.set_viewport_height(38);
            view.update(Action::CursorDown);
        }
        assert_eq!(view.journal_cursor, view.log_rows.len() - 1);
        assert_eq!(
            view.log_scroll,
            view.log_rows.len().saturating_sub(viewport)
        );
    }

    #[test]
    fn ages_are_coarsened_to_human_scale_units() {
        assert_eq!(format_age(999), "0s");
        assert_eq!(format_age(1_999), "1s");
        assert_eq!(format_age(65_999), "1m 05s");
        assert_eq!(format_age(3_661_999), "1h 01m 01s");
    }

    #[test]
    fn an_unbroken_journal_message_wraps_without_losing_content() {
        let store = crate::lsp::ObservationStore::in_memory();
        store.record(JournalEvent::simple(
            EventSource::Stderr,
            EventLevel::Error,
            None,
            "abcdefghijklmnopqrstuvwxyz0123456789",
        ));
        let mut view = LspInspectorView::new(store, None);
        let original = view.events[0].format_line(0).trim_end().to_owned();

        // A 22-column pane has 20 columns inside its border. The message has
        // no whitespace, so this specifically guards against ordinary word
        // wrapping leaving it clipped.
        view.set_journal_geometry(Rect::new(0, 0, 22, 8));
        assert!(view.log_rows.len() > 1);
        assert_eq!(
            view.log_rows
                .iter()
                .map(|row| row.text.as_str())
                .collect::<String>(),
            original
        );
        assert!(
            view.log_rows
                .iter()
                .all(|row| row.text.as_str().width() <= 20)
        );
    }

    #[test]
    fn visual_selection_copies_complete_wrapped_records_once() {
        let store = crate::lsp::ObservationStore::in_memory();
        store.record(JournalEvent::simple(
            EventSource::Stderr,
            EventLevel::Error,
            None,
            "first-record-with-a-long-unbroken-diagnostic",
        ));
        store.record(JournalEvent::simple(
            EventSource::Ui,
            EventLevel::Info,
            None,
            "second-record-with-another-long-diagnostic",
        ));
        let mut view = LspInspectorView::new(store, None);
        view.set_journal_geometry(Rect::new(0, 0, 24, 8));
        view.update(Action::NextSymbol);
        view.update(Action::NextSymbol);
        view.update(Action::Top);

        assert_eq!(
            view.handle_literal_key(key(KeyCode::Char('V'))),
            InspectorKeyOutcome::Handled
        );
        view.update(Action::Bottom);
        let InspectorKeyOutcome::Copy(payload) = view.handle_literal_key(key(KeyCode::Char('y')))
        else {
            panic!("visual selection should yield an OSC 52 copy payload");
        };
        assert_eq!(payload.record_count, 2);
        assert_eq!(payload.byte_count, payload.text.len());
        assert_eq!(
            payload
                .text
                .matches("first-record-with-a-long-unbroken-diagnostic")
                .count(),
            1
        );
        assert_eq!(
            payload
                .text
                .matches("second-record-with-another-long-diagnostic")
                .count(),
            1
        );
        assert!(view.journal_selection_anchor.is_none());
    }

    #[test]
    fn journal_events_are_deferred_during_selection_and_caught_up_after_copy() {
        let store = crate::lsp::ObservationStore::in_memory();
        store.record(JournalEvent::simple(
            EventSource::Ui,
            EventLevel::Info,
            None,
            "selected-event",
        ));
        let mut view = LspInspectorView::new(store.clone(), None);
        view.set_journal_geometry(Rect::new(0, 0, 32, 8));
        view.update(Action::NextSymbol);
        view.update(Action::NextSymbol);
        view.update(Action::Top);
        assert_eq!(
            view.handle_literal_key(key(KeyCode::Char('V'))),
            InspectorKeyOutcome::Handled
        );
        let rows_before = view.log_rows.len();
        assert!(view.journal_selection_anchor.is_some());

        store.record(JournalEvent::simple(
            EventSource::Ui,
            EventLevel::Info,
            None,
            "arrived-during-selection",
        ));
        view.refresh();
        assert_eq!(view.events.len(), 1);
        assert_eq!(view.log_rows.len(), rows_before);

        let InspectorKeyOutcome::Copy(payload) = view.handle_literal_key(key(KeyCode::Char('y')))
        else {
            panic!("selection should still be copyable after a deferred event");
        };
        assert_eq!(payload.record_count, 1);
        assert!(payload.text.contains("selected-event"));
        assert!(!payload.text.contains("arrived-during-selection"));

        view.refresh();
        assert_eq!(view.events.len(), 2);
        assert!(view.log_rows.iter().any(|row| row.event_index == 1));
    }

    #[test]
    fn escape_cancels_visual_selection_before_closing_inspector() {
        let store = crate::lsp::ObservationStore::in_memory();
        store.record(JournalEvent::simple(
            EventSource::Ui,
            EventLevel::Info,
            None,
            "event",
        ));
        let mut view = LspInspectorView::new(store, None);
        view.update(Action::NextSymbol);
        view.update(Action::NextSymbol);
        assert_eq!(
            view.handle_literal_key(key(KeyCode::Char('V'))),
            InspectorKeyOutcome::Handled
        );
        view.update(Action::Cancel);
        assert!(!view.should_quit);
        view.update(Action::Cancel);
        assert!(view.should_quit);
    }

    #[test]
    fn base64_encoding_is_terminal_safe() {
        assert_eq!(base64_encode(b"Katamari"), "S2F0YW1hcmk=");
        assert_eq!(base64_encode(b"\x00\xff"), "AP8=");
        let too_large = "x".repeat(OSC52_MAX_BYTES + 1);
        assert_eq!(
            write_osc52(&too_large).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn scoped_inspector_hints_fit_wide_and_boundary_layouts() {
        let store = crate::lsp::ObservationStore::in_memory();
        let server = identity("/repo");
        let generation = store.begin_generation(server.clone(), None, vec![]);
        store.transition(&server, generation, ServerPhase::Running, "ready");
        for width in [120, 100] {
            let mut view = LspInspectorView::new(store.clone(), None);
            let backend = ratatui::backend::TestBackend::new(width, 40);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| view.render(frame, frame.area()))
                .unwrap();
            let rendered_lines = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| {
                let terminal_width = terminal.backend().buffer().area.width as usize;
                terminal
                    .backend()
                    .buffer()
                    .content
                    .chunks(terminal_width)
                    .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                    .collect::<Vec<_>>()
            };
            let lines = rendered_lines(&terminal);
            assert!(lines.iter().any(|line| line.contains("Tab/BackTab")));
            assert!(lines.iter().any(|line| line.contains("I/Esc")));
            assert!(lines.iter().any(|line| line.contains("q quit")));
            assert!(lines.iter().any(|line| line.contains("j/k select")));
            assert!(lines.iter().all(|line| !line.contains("controls")));
            assert!(lines.iter().all(|line| !line.contains("journal keys")));

            view.snapshots[0].program = Some("long-command-part-".repeat(16));
            view.update(Action::NextSymbol);
            terminal
                .draw(|frame| view.render(frame, frame.area()))
                .unwrap();
            let lines = rendered_lines(&terminal);
            assert!(lines.iter().any(|line| line.contains("j/k scroll")));
            assert!(lines.iter().any(|line| line.contains("gg/G top/bottom")));
            assert!(lines.iter().all(|line| !line.contains("controls")));

            view.update(Action::NextSymbol);
            terminal
                .draw(|frame| view.render(frame, frame.area()))
                .unwrap();
            let lines = rendered_lines(&terminal);
            assert!(lines.iter().any(|line| line.contains("V select")));
            assert!(lines.iter().any(|line| line.contains("y yank")));
        }
    }

    #[test]
    fn detail_fields_align_and_color_state_at_wide_and_boundary_layouts() {
        for (phase, expected_phase_color) in [
            (ServerPhase::Running, Color::Green),
            (ServerPhase::Unavailable, Color::Red),
        ] {
            for width in [120, 100] {
                let store = crate::lsp::ObservationStore::in_memory();
                let server = identity("/repo");
                let generation = store.begin_generation(server.clone(), None, vec![]);
                store.transition(&server, generation, phase, "state");
                let mut view = LspInspectorView::new(store, None);
                let backend = ratatui::backend::TestBackend::new(width, 40);
                let mut terminal = ratatui::Terminal::new(backend).unwrap();
                terminal
                    .draw(|frame| view.render(frame, frame.area()))
                    .unwrap();

                let terminal_width = terminal.backend().buffer().area.width as usize;
                let buffer_rows = terminal
                    .backend()
                    .buffer()
                    .content
                    .chunks(terminal_width)
                    .collect::<Vec<_>>();
                let field = |label: &str| {
                    let prefix = format!("{label:<width$} │", width = DETAIL_LABEL_WIDTH);
                    let expected = prefix.chars().map(|character| character.to_string());
                    let expected = expected.collect::<Vec<_>>();
                    buffer_rows
                        .iter()
                        .enumerate()
                        .find_map(|(row, cells)| {
                            cells
                                .windows(expected.len())
                                .position(|window| {
                                    window
                                        .iter()
                                        .zip(expected.iter())
                                        .all(|(cell, expected)| cell.symbol() == expected)
                                })
                                .map(|label_x| {
                                    let separator_x = label_x + DETAIL_LABEL_WIDTH + 1;
                                    (row, label_x, separator_x)
                                })
                        })
                        .unwrap_or_else(|| panic!("missing aligned detail field {label:?}"))
                };

                let fields = ["identity", "state", "command", "process", "activity"];
                let separator_x = fields
                    .iter()
                    .map(|label| field(label).2)
                    .collect::<Vec<_>>();
                assert!(
                    separator_x.iter().all(|x| *x == separator_x[0]),
                    "phase={phase:?} width={width}: separators={separator_x:?}"
                );

                let (state_row, state_label_x, _) = field("state");
                let label_cell = terminal
                    .backend()
                    .buffer()
                    .cell((state_label_x as u16, state_row as u16))
                    .expect("state label cell");
                assert_eq!(label_cell.fg, Color::DarkGray);
                assert!(label_cell.modifier.contains(Modifier::BOLD));

                let phase_text = phase.to_string();
                let phase_chars = phase_text.chars().map(|character| character.to_string());
                let phase_chars = phase_chars.collect::<Vec<_>>();
                let phase_x = buffer_rows[state_row]
                    .windows(phase_chars.len())
                    .position(|window| {
                        window
                            .iter()
                            .zip(phase_chars.iter())
                            .all(|(cell, expected)| cell.symbol() == expected)
                    })
                    .expect("phase value should be visible");
                let phase_cell = terminal
                    .backend()
                    .buffer()
                    .cell((phase_x as u16, state_row as u16))
                    .expect("phase value cell");
                assert_eq!(phase_cell.fg, expected_phase_color);
            }
        }
    }

    #[test]
    fn detail_overflow_scrolls_to_final_fields_without_starving_journal() {
        let store = crate::lsp::ObservationStore::in_memory();
        let identity = ServerIdentity::new(
            LangKey::Custom("rust".to_owned()),
            format!("/workspace/{}", "long-root-segment-".repeat(12)),
        );
        let generation = store.begin_generation(
            identity.clone(),
            Some(format!("language-server-{}", "command-part-".repeat(12))),
            vec![format!("--workspace={}", "argument-".repeat(12))],
        );
        store.transition(&identity, generation, ServerPhase::Running, "ready");
        let mut view = LspInspectorView::new(store, None);
        let mut snapshot = view.snapshots[0].clone();
        snapshot
            .active_progress
            .push(crate::lsp::observe::ProgressSnapshot {
                token: "progress-token-".repeat(12),
                title: Some("indexing a large workspace".to_owned()),
                message: Some("progress-message-".repeat(12)),
                percentage: Some(50),
                started_at_ms: 0,
            });
        snapshot.last_error = Some("error-detail-".repeat(12));
        view.snapshots[0] = snapshot.clone();

        let long_journal = "journal-location-".repeat(16);
        let long_lines = detail_lines(&snapshot, &long_journal, 28);
        let long_text = long_lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(
            long_lines.len() > 14,
            "long values should produce overflow rows"
        );
        assert!(long_text.contains("journal-location-"));

        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        assert!(view.detail_total_rows > view.detail_viewport_height);
        assert!(view.journal_viewport_height >= 4);

        view.update(Action::NextSymbol);
        view.update(Action::Bottom);
        assert_eq!(
            view.detail_scroll,
            view.detail_total_rows - view.detail_viewport_height
        );
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let width = terminal.backend().buffer().area.width as usize;
        let lines = terminal
            .backend()
            .buffer()
            .content
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        assert!(lines.iter().any(|line| line.contains("journal")));
        assert!(lines.iter().all(|line| !line.contains("controls")));
        assert!(lines.iter().all(|line| !line.contains("journal keys")));
        assert!(lines.iter().any(|line| line.contains("j/k scroll")));

        view.update(Action::Top);
        assert_eq!(view.detail_scroll, 0);
    }

    #[test]
    fn follow_uses_the_journal_pane_height_after_layout() {
        let store = crate::lsp::ObservationStore::in_memory();
        let server = identity("/repo");
        let generation = store.begin_generation(server, None, vec![]);
        store.transition(
            &identity("/repo"),
            generation,
            ServerPhase::Running,
            "ready",
        );
        for index in 0..40 {
            store.record(JournalEvent::simple(
                EventSource::Ui,
                EventLevel::Info,
                None,
                format!("event {index}"),
            ));
        }
        let mut view = LspInspectorView::new(store, None);
        view.update(Action::NextSymbol);
        view.update(Action::NextSymbol);
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();

        // Wide layout: 38 inner rows minus the 14-row detail pane leaves a
        // 22-row journal content area. The tail must be based on 22, not the
        // whole 38-row inspector content area.
        assert_eq!(view.journal_viewport_height, 22);
        assert_eq!(
            view.log_scroll,
            view.events.len() - view.journal_viewport_height
        );
        let width = terminal.backend().buffer().area.width as usize;
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .chunks(width)
            .any(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .contains("event 39")
            });
        assert!(rendered, "the followed journal tail should be visible");
        let lines = terminal
            .backend()
            .buffer()
            .content
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("disabled (in-memory only)"))
        );
        assert!(lines.iter().any(|line| line.contains("Tab/BackTab")));
        assert!(lines.iter().any(|line| line.contains("V select · y yank")));
    }
}
