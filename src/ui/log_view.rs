//! `ktmr log` / `L` from a live diff session: a browsable list of a
//! repository's revision history — jj changes (including the working copy,
//! `@`, as a real entry) in a colocated jj repo, or `git log` commits plus a
//! synthetic "local changes" row for the dirty working tree in a git-only
//! one. See [`crate::vcs::LogBackend`] for backend selection and the actual
//! `git`/`jj` invocations.
//!
//! Structurally the opposite of [`crate::ui::timeline_view::TimelineView`]:
//! that view nests a whole second [`App`] as a permanently visible diff
//! pane, because every timeline entry needs *some* diff shown at all times
//! (there's no "browse the op-log without looking at a diff" mode worth
//! having). A log entry's diff is expensive enough (a real `git`/`jj`
//! invocation, not a cheap in-memory recompute) and optional enough (a
//! reviewer might just be scanning history for a change id to hand to `-r`
//! later) that this view stays a plain list — `Enter` computes one revision's
//! diff on demand and *pushes* a full, ordinary [`View::Diff`] on top of the
//! [`crate::ui::ViewStack`] (see `ui::mod::handle_action`'s `Action::Confirm`
//! arm), the same "push a view, `q` pops back" shape goto-definition already
//! uses for jumping into a [`crate::ui::file_view::FileView`]. That also
//! means `Confirm` can't be a pure state transition handled inside
//! [`LogView::update`] the way it is in `TimelineView` — building the
//! resulting `App` needs a real backend call that can fail, so `ui::mod`
//! calls [`LogView::confirm`] directly instead of forwarding the action.

use crate::keymap::Action;
use crate::ui::app::App;
use crate::ui::hints;
use crate::ui::hover_popup::HoverQuery;
use crate::ui::timeline_view::relative_time;
use crate::vcs::{LogBackend, LogEntry, RevisionEntry};
use anyhow::Result;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout as RatLayout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// Default `limit` for [`LogBackend::log`] — bounds the raw history fetched
/// (git commits reachable from `HEAD`, or jj's own default log revset).
/// Shared by `Action::ToggleLogView`'s handling in [`crate::ui::mod`] and
/// `ktmr log`'s CLI entry point in `main.rs`.
pub const DEFAULT_LOG_LIMIT: usize = 200;

/// State for one open log view. Construction fetches the entry list once;
/// nothing here re-fetches it — unlike `TimelineView`, there's no live
/// notification this view needs to react to (a session's own edits don't
/// rewrite committed history), so a stale list simply means "close and
/// reopen" if a reviewer wants to see a new commit made in another
/// terminal.
pub struct LogView {
    backend: LogBackend,
    /// Newest first, exactly as [`LogBackend::log`] returns it (a git-only
    /// repo's synthetic "local changes" row, if present, sorts first within
    /// that).
    entries: Vec<LogEntry>,
    selected: usize,
    /// Set by `v`: the other endpoint of a combined range diff. `None` is
    /// the default "diff this entry against its parent" mode. Excludes the
    /// git-only "local changes" row (see [`Self::toggle_range_select`]'s
    /// docs) — range diffing "some revision..the working tree" would need
    /// git's asymmetric `git diff <rev>` form rather than the `A..B`
    /// two-revision form every other range in this view uses, which isn't
    /// worth the special case for what a reviewer can already get via plain
    /// `ktmr diff <rev>` outside this view.
    range_anchor: Option<usize>,
    /// A transient status-bar note — "range selection doesn't include local
    /// changes," or a backend failure computing a diff — cleared on the
    /// next selection change or successful range toggle.
    status_note: Option<String>,
    pub pending_keys: String,
    pub should_quit: bool,
    viewport_height: usize,
}

impl LogView {
    pub fn new(backend: LogBackend, limit: usize) -> Result<Self> {
        let entries = backend.log(limit)?;
        Ok(Self {
            backend,
            entries,
            selected: 0,
            range_anchor: None,
            status_note: None,
            pending_keys: String::new(),
            should_quit: false,
            viewport_height: 1,
        })
    }

    pub fn set_viewport_height(&mut self, height: usize) {
        self.viewport_height = height.max(1);
    }

    /// Always `None`: a plain list of commit metadata has nothing to hover
    /// — mirrors [`crate::ui::timeline_view::TimelineView::hover_query`].
    pub fn hover_query(&self) -> Option<HoverQuery> {
        None
    }

    /// A cheap, comparable snapshot of "what's selected," mirroring
    /// [`crate::ui::app::App::hover_query`]'s cursor-key sibling on the
    /// other views — see `TimelineView::cursor_key`'s docs for why the
    /// `View` enum's dispatch needs one even here, where it's never used to
    /// invalidate a hover popup.
    pub fn cursor_key(&self) -> (usize, usize) {
        (self.selected, 0)
    }

    pub fn update(&mut self, action: Action) {
        match action {
            Action::CursorDown => self.select(self.selected.saturating_add(1)),
            Action::CursorUp => self.select(self.selected.saturating_sub(1)),
            Action::HalfPageDown => self.select(self.selected.saturating_add(5)),
            Action::HalfPageUp => self.select(self.selected.saturating_sub(5)),
            Action::Top => self.select(0),
            Action::Bottom => self.select(self.entries.len().saturating_sub(1)),
            Action::ToggleRangeSelect => self.toggle_range_select(),
            // `Confirm` is deliberately absent — see this module's docs on
            // why `ui::mod` calls `Self::confirm` directly instead of
            // routing it through here.
            Action::Cancel | Action::Quit => self.should_quit = true,
            _ => {} // hunk/file nav, sidebar, layout — nothing here to act on
        }
    }

    fn select(&mut self, idx: usize) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = idx.min(self.entries.len() - 1);
        self.status_note = None;
    }

    fn toggle_range_select(&mut self) {
        if self.range_anchor.take().is_some() {
            self.status_note = None;
            return;
        }
        if matches!(
            self.entries.get(self.selected),
            Some(LogEntry::LocalChanges { .. })
        ) {
            self.status_note =
                Some("range selection doesn't include the local-changes row".to_owned());
            return;
        }
        self.range_anchor = Some(self.selected);
        self.status_note = None;
    }

    /// `Action::Confirm`'s handling for this view: opens the selected
    /// entry's diff (or, in range mode, the diff between the anchor and the
    /// current selection) as a fresh [`App`] for `ui::mod` to
    /// [`crate::ui::ViewStack::push`] as a [`View::Diff`]. `Ok(None)` when
    /// there's genuinely nothing to open (an empty list, or a range whose
    /// anchor turned out to include the local-changes row — reported via
    /// [`Self::status_note`] instead of an `Err`, since neither is a
    /// backend failure).
    pub fn confirm(&mut self) -> Result<Option<App>> {
        if self.entries.is_empty() {
            return Ok(None);
        }
        match self.range_anchor {
            Some(anchor) if anchor != self.selected => self.confirm_range(anchor),
            _ => self.confirm_single().map(Some),
        }
    }

    fn confirm_single(&self) -> Result<App> {
        match &self.entries[self.selected] {
            LogEntry::LocalChanges { .. } => {
                let text = self.backend.working_tree_diff()?;
                let mut app = self.build_app(&text);
                // The working tree, live — exactly what a plain `ktmr diff`
                // shows, so it gets the same full interactivity.
                app.scope_label = Some("local changes".to_owned());
                Ok(app)
            }
            LogEntry::Revision(entry) => {
                let text = self.backend.revision_diff(entry)?;
                let mut app = self.build_app(&text);
                app.interactive = false;
                app.scope_label = Some(format!("r: {}", entry.short_id));
                Ok(app)
            }
        }
    }

    fn confirm_range(&mut self, anchor: usize) -> Result<Option<App>> {
        // `entries` is newest-first, so the *larger* index is the *older*
        // revision — `LogBackend::range_diff`'s `from`/`to` order, matching
        // `timeline_view::resolve_diff_pair`'s identical convention.
        let (older_idx, newer_idx) = if anchor > self.selected {
            (anchor, self.selected)
        } else {
            (self.selected, anchor)
        };
        let (LogEntry::Revision(from), LogEntry::Revision(to)) =
            (&self.entries[older_idx], &self.entries[newer_idx])
        else {
            self.status_note =
                Some("range selection doesn't include the local-changes row".to_owned());
            return Ok(None);
        };
        let text = self.backend.range_diff(from, to)?;
        let mut app = self.build_app(&text);
        app.interactive = false;
        app.scope_label = Some(format!("{}..{}", from.short_id, to.short_id));
        Ok(Some(app))
    }

    fn build_app(&self, diff_text: &str) -> App {
        let files = crate::diff::parse_unified_diff(diff_text);
        let mut app = App::new(String::new(), self.backend.repo_root().to_owned(), files);
        app.set_viewport_height(self.viewport_height);
        app
    }
}

pub struct Areas {
    pub list: Rect,
    pub status: Rect,
}

/// `status_height` is [`hints::required_height`] applied to
/// [`hints::log_view_items`] and `area`'s width — see
/// `file_view::layout`'s docs for why the caller computes this rather than
/// a fixed constant.
pub fn layout(area: Rect, status_height: u16) -> Areas {
    let rows = RatLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(status_height)])
        .split(area);
    Areas {
        list: rows[0],
        status: rows[1],
    }
}

pub fn render(
    frame: &mut Frame,
    area: Rect,
    view: &LogView,
    keymap: &crate::keymap::Keymap,
    key_display: &crate::ui::key_display::KeyDisplayState,
) {
    let hint_items = hints::log_view_items(keymap);
    let status_height = hints::required_height(&hint_items, area.width);
    let areas = layout(area, status_height);
    render_list(frame, areas.list, view);
    render_status(frame, areas.status, view, &hint_items);
    crate::ui::key_display::render(frame, areas.list, key_display);
}

fn revision_line(entry: &RevisionEntry, selected: bool, in_range: bool) -> Line<'static> {
    let marker = if entry.is_working_copy { "@" } else { " " };
    let refs = if entry.refs.is_empty() {
        String::new()
    } else {
        format!("  [{}]", entry.refs.join(", "))
    };
    let summary = if entry.summary.is_empty() {
        "(no description set)"
    } else {
        entry.summary.as_str()
    };
    let text = format!(
        "{marker} {:>8}  {}  {summary}{refs}",
        relative_time(entry.time_unix),
        entry.short_id,
    );
    let mut style = Style::default();
    if in_range {
        style = style.bg(Color::Rgb(30, 30, 60));
    }
    if entry.is_working_copy {
        style = style.fg(Color::Green);
    }
    if selected {
        style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
    }
    Line::from(Span::styled(text, style))
}

fn local_changes_line(changed_files: usize, selected: bool) -> Line<'static> {
    let text = format!("  local changes  ({changed_files} file(s))");
    let mut style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    Line::from(Span::styled(text, style))
}

fn render_list(frame: &mut Frame, area: Rect, view: &LogView) {
    let block = Block::default().borders(Borders::LEFT).title(" log ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if view.entries.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "(no history yet)",
                Style::default().fg(Color::DarkGray),
            ))),
            inner,
        );
        return;
    }

    let range = view
        .range_anchor
        .map(|anchor| (anchor.min(view.selected), anchor.max(view.selected)));

    let lines: Vec<Line> = view
        .entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let selected = idx == view.selected;
            let in_range = range.is_some_and(|(lo, hi)| idx >= lo && idx <= hi);
            match entry {
                LogEntry::Revision(r) => revision_line(r, selected, in_range),
                LogEntry::LocalChanges { changed_files } => {
                    local_changes_line(*changed_files, selected)
                }
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_status(frame: &mut Frame, area: Rect, view: &LogView, hint_items: &[hints::HintItem]) {
    let mut spans = vec![Span::styled(
        " log ",
        Style::default().add_modifier(Modifier::BOLD),
    )];

    if view.range_anchor.is_some() {
        spans.push(Span::styled(
            "· range mode ",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if !view.pending_keys.is_empty() {
        spans.push(Span::styled(
            format!("· {} ", view.pending_keys),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if let Some(note) = &view.status_note {
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

/// A one-line-per-entry plain-text rendering of `entries` — `ktmr log
/// --dump`'s output, and this module's own tests' way of checking parsed
/// content without a terminal. Deliberately includes `time_unix` (a stable
/// integer) rather than [`relative_time`]'s "3m ago" (which isn't
/// deterministic across a test run) — see the M11 task's warning about
/// relative-time assertions.
pub fn format_dump(entries: &[LogEntry]) -> String {
    if entries.is_empty() {
        return "(no history yet)\n".to_owned();
    }
    let mut out = String::new();
    for entry in entries {
        match entry {
            LogEntry::LocalChanges { changed_files } => {
                out.push_str(&format!("LOCAL  local changes  ({changed_files} files)\n"));
            }
            LogEntry::Revision(r) => {
                let marker = if r.is_working_copy { "@" } else { " " };
                let refs = if r.refs.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", r.refs.join(","))
                };
                out.push_str(&format!(
                    "{marker} {}  {}  {}  {}{refs}\n",
                    r.id, r.time_unix, r.author, r.summary
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revision(id: &str, summary: &str, working_copy: bool) -> LogEntry {
        LogEntry::Revision(RevisionEntry {
            id: id.to_owned(),
            short_id: id.to_owned(),
            summary: summary.to_owned(),
            author: "Test".to_owned(),
            time_unix: 1_780_000_000,
            refs: Vec::new(),
            is_working_copy: working_copy,
        })
    }

    #[test]
    fn format_dump_lists_local_changes_first_then_revisions() {
        let entries = vec![
            LogEntry::LocalChanges { changed_files: 2 },
            revision("aaa", "second", false),
            revision("bbb", "first", false),
        ];
        let dump = format_dump(&entries);
        let lines: Vec<&str> = dump.lines().collect();
        assert_eq!(lines.len(), 3, "dump:\n{dump}");
        assert!(lines[0].starts_with("LOCAL"), "dump:\n{dump}");
        assert!(lines[1].contains("second"), "dump:\n{dump}");
        assert!(lines[2].contains("first"), "dump:\n{dump}");
    }

    #[test]
    fn format_dump_marks_the_working_copy_entry() {
        let entries = vec![revision("aaa", "wip", true)];
        let dump = format_dump(&entries);
        assert!(dump.starts_with('@'), "dump:\n{dump}");
    }

    #[test]
    fn format_dump_on_empty_history_is_a_clear_message_not_a_blank_line() {
        assert_eq!(format_dump(&[]), "(no history yet)\n");
    }
}
