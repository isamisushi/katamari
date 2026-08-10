//! The bottom overlay listing a diff's semantic units — the grouping an
//! agent CLI proposed (see [`crate::groups`]), one line per unit in the
//! proposed reading order. An overlay like [`crate::ui::refs_panel`], not a
//! [`crate::ui::view::ViewStack`] entry: `Esc` closes back to exactly the
//! view state it opened over, and Enter scopes the diff itself to the
//! selected unit (see [`crate::ui::app::UnitFilter`]), with
//! [`render_banner`] keeping that unit's label and rationale pinned above
//! the filtered content.

use crate::diff::DiffFile;
use crate::groups::{Grouping, UnitKind, enumerate_hunks};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::collections::HashMap;

/// One unit, resolved against the *current* parse of the diff.
/// `hunk_count`/`files` reflect only the ids that still resolve, so the
/// panel always describes what `Enter` would actually show.
pub struct UnitEntry {
    pub label: String,
    pub description: String,
    pub kind: UnitKind,
    pub hunk_count: usize,
    /// A short "src/a.rs, src/b.rs +2" summary — the reviewer's scent of
    /// where this unit lives without opening it.
    pub files: String,
    /// The unit's raw hunk ids, resolvable or not — `Enter` builds an
    /// [`crate::ui::app::UnitFilter`] from these, and the filter matches
    /// against live ids on every rederive anyway, so pre-pruning stale
    /// ones here would just duplicate that logic.
    pub hunk_ids: Vec<String>,
}

pub struct UnitsPanel {
    /// Which CLI's judgment this is — part of the title so a reviewer
    /// never mistakes an LLM's proposal for something katamari derived.
    pub agent: String,
    pub entries: Vec<UnitEntry>,
    pub selected: usize,
}

impl UnitsPanel {
    /// Resolves `grouping` against `files`. Ids that no longer resolve
    /// (the diff changed since the grouping was cached — callers gate on
    /// `diff_key`, so this is belt-and-braces, not the expected path) are
    /// simply not counted; a unit with nothing left keeps its line but
    /// `Enter` on it reports "no hunks" instead of scoping to nothing.
    pub fn build(grouping: &Grouping, files: &[DiffFile]) -> Self {
        let by_id: HashMap<String, (usize, usize)> = enumerate_hunks(files)
            .into_iter()
            .map(|h| (h.id, (h.file_idx, h.hunk_idx)))
            .collect();
        let entries = grouping
            .units
            .iter()
            .map(|unit| {
                let resolved: Vec<(usize, usize)> = unit
                    .hunk_ids
                    .iter()
                    .filter_map(|id| by_id.get(id).copied())
                    .collect();
                let mut file_names: Vec<&str> = Vec::new();
                for &(file_idx, _) in &resolved {
                    let name = files[file_idx].display_path();
                    if !file_names.contains(&name) {
                        file_names.push(name);
                    }
                }
                UnitEntry {
                    label: unit.label.clone(),
                    description: unit.description.clone(),
                    kind: unit.kind,
                    hunk_count: resolved.len(),
                    files: summarize_files(&file_names),
                    hunk_ids: unit.hunk_ids.clone(),
                }
            })
            .collect();
        Self {
            agent: grouping.agent.clone(),
            entries,
            selected: 0,
        }
    }

    pub fn select_next(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn selected_entry(&self) -> Option<&UnitEntry> {
        self.entries.get(self.selected)
    }
}

/// The unit banner's height in rows — one for "unit 2/5: label", one for
/// the description. A constant rather than content-derived so the frame
/// preamble's viewport math (`ui::mod`'s `content_height`) and `draw`'s
/// split can never disagree about how much of the pane the banner took.
/// Long lines truncate at the pane edge rather than wrap; the full text is
/// always one `u` away in the panel.
pub const BANNER_HEIGHT: u16 = 2;

/// The always-visible header above a unit-scoped diff: which unit this is
/// and why it exists as a unit. The status bar already carries the short
/// "unit 2/5: label" form, but the description — the agent's stated
/// rationale, the thing a reviewer needs to judge whether the grouping is
/// trustworthy — has no other home while the panel is closed.
pub fn render_banner(frame: &mut Frame, area: Rect, filter: &crate::ui::app::UnitFilter) {
    let title = Line::from(Span::styled(
        format!(" unit {}/{}: {}", filter.index, filter.total, filter.label),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    let description = Line::from(Span::styled(
        format!("   {}", filter.description),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(vec![title, description]), area);
}

/// At most two paths spelled out; the rest becomes "+N". Two, not more,
/// because the panel line also has to fit a label and a hunk count, and a
/// cross-cutting unit touching a dozen files says everything it needs to
/// with "first, second +10".
fn summarize_files(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [a] => (*a).to_owned(),
        [a, b] => format!("{a}, {b}"),
        [a, b, rest @ ..] => format!("{a}, {b} +{}", rest.len()),
    }
}

/// Bottom ~40% of `area`, matching [`crate::ui::refs_panel`]'s footprint so
/// the two bottom overlays feel like one family.
fn panel_rect(area: Rect) -> Rect {
    let height = ((area.height as u32 * 2) / 5).clamp(5, area.height.max(5) as u32) as u16;
    Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(height),
        width: area.width,
        height,
    }
}

pub fn render(frame: &mut Frame, area: Rect, panel: &UnitsPanel) {
    let rect = panel_rect(area);
    frame.render_widget(Clear, rect);

    let block = Block::default().borders(Borders::ALL).title(format!(
        " Units ({}, via {}) ",
        panel.entries.len(),
        panel.agent
    ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    if panel.entries.is_empty() {
        frame.render_widget(Paragraph::new("no units"), inner);
        return;
    }

    // The last inner row is reserved for the selected unit's description —
    // kept out of the per-entry lines so every unit stays exactly one row
    // and the list scroll stays trivial.
    let list_height = (inner.height as usize).saturating_sub(1).max(1);
    let start = panel.selected.saturating_sub(list_height.saturating_sub(1));

    let mut lines: Vec<Line> = panel
        .entries
        .iter()
        .enumerate()
        .skip(start)
        .take(list_height)
        .map(|(idx, entry)| entry_line(idx, entry, idx == panel.selected))
        .collect();

    if inner.height > 1
        && let Some(entry) = panel.selected_entry()
    {
        while lines.len() < list_height {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            entry.description.clone(),
            Style::default().fg(Color::DarkGray),
        )));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn entry_line(idx: usize, entry: &UnitEntry, is_selected: bool) -> Line<'static> {
    let badge = match entry.kind {
        UnitKind::Concern => None,
        UnitKind::Noise => Some(("noise", Color::DarkGray)),
        // Yellow: this bucket existing at all means the agent's proposal
        // missed hunks, which a reviewer should notice, not skim past.
        UnitKind::Misc => Some(("ungrouped", Color::Yellow)),
    };
    let mut spans = vec![
        Span::styled(
            format!("{:>2}. ", idx + 1),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            entry.label.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some((text, color)) = badge {
        spans.push(Span::styled(
            format!(" [{text}]"),
            Style::default().fg(color),
        ));
    }
    spans.push(Span::styled(
        format!(
            "  {} hunk{}  ",
            entry.hunk_count,
            if entry.hunk_count == 1 { "" } else { "s" }
        ),
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled(
        entry.files.clone(),
        Style::default().fg(Color::Cyan),
    ));

    let mut line = Line::from(spans);
    if is_selected {
        line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse_unified_diff;
    use crate::groups::{Unit, diff_key};

    fn fixture() -> (Vec<DiffFile>, Grouping) {
        let files = parse_unified_diff(concat!(
            "diff --git a/src/a.rs b/src/a.rs\n",
            "--- a/src/a.rs\n",
            "+++ b/src/a.rs\n",
            "@@ -1,1 +1,1 @@\n",
            "-old\n",
            "+new\n",
            "diff --git a/src/b.rs b/src/b.rs\n",
            "--- a/src/b.rs\n",
            "+++ b/src/b.rs\n",
            "@@ -1,1 +1,1 @@\n",
            "-x\n",
            "+y\n",
        ));
        let hunks = enumerate_hunks(&files);
        let grouping = Grouping {
            diff_key: diff_key(&hunks),
            agent: "claude".to_owned(),
            created_at: 1,
            units: vec![Unit {
                label: "Rename things".to_owned(),
                description: "Renames across both files.".to_owned(),
                hunk_ids: hunks.iter().map(|h| h.id.clone()).collect(),
                kind: UnitKind::Concern,
            }],
        };
        (files, grouping)
    }

    #[test]
    fn build_resolves_counts_and_summarizes_files() {
        let (files, grouping) = fixture();
        let panel = UnitsPanel::build(&grouping, &files);
        assert_eq!(panel.entries.len(), 1);
        let entry = &panel.entries[0];
        assert_eq!(entry.hunk_count, 2);
        assert_eq!(entry.files, "src/a.rs, src/b.rs");
        assert_eq!(entry.hunk_ids, grouping.units[0].hunk_ids);
    }

    #[test]
    fn stale_ids_zero_the_count_but_keep_the_line() {
        let (files, mut grouping) = fixture();
        grouping.units[0].hunk_ids = vec!["feedbeef00000000".to_owned()];
        let panel = UnitsPanel::build(&grouping, &files);
        assert_eq!(panel.entries.len(), 1);
        assert_eq!(panel.entries[0].hunk_count, 0);
    }

    #[test]
    fn selection_stays_in_bounds() {
        let (files, grouping) = fixture();
        let mut panel = UnitsPanel::build(&grouping, &files);
        panel.select_prev();
        assert_eq!(panel.selected, 0);
        panel.select_next();
        assert_eq!(panel.selected, 0, "one entry: next stays put");
    }

    #[test]
    fn file_summaries_cap_at_two_names() {
        assert_eq!(summarize_files(&[]), "");
        assert_eq!(summarize_files(&["a"]), "a");
        assert_eq!(summarize_files(&["a", "b"]), "a, b");
        assert_eq!(summarize_files(&["a", "b", "c", "d"]), "a, b +2");
    }
}
