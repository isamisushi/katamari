//! The bottom overlay listing go-to-definition results (when there's more
//! than one candidate) or find-references results: one line per match,
//! grouped visually by file, with the matched range underlined. An overlay
//! on top of whatever view is active — not a [`crate::ui::view::ViewStack`]
//! entry — the same way [`crate::ui::hover_popup`]'s popup is: `Esc` closes
//! it back to exactly the view/cursor state it was opened over, and Enter
//! navigates through [`crate::ui::navigation::navigate_to`], the same
//! machinery `gd` uses when there's only one result to jump to directly.

use crate::diff::ColumnMap;
use crate::lsp::client::uri_to_path;
use crate::ui::text::display_width;
use lsp_types::{Location, PositionEncodingKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::path::{Path, PathBuf};

/// The most results shown at once — a find-references on a widely used
/// symbol in a large codebase can return results numbering in the
/// thousands; past this many, [`RefsPanel`] shows a "+N more" note instead
/// of an unusably long list.
pub const MAX_RESULTS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    pub file: PathBuf,
    pub display_path: String,
    /// 0-based line number.
    pub line: u32,
    /// The match's display-column range within `snippet`, `[start, end)` —
    /// already adjusted for `snippet` having had its leading whitespace
    /// trimmed, so it can be used directly against `snippet`'s own
    /// characters without the caller re-deriving an offset.
    pub match_range: (usize, usize),
    /// The matched line's text, trimmed of leading/trailing whitespace so
    /// deeply indented code doesn't waste the panel's width.
    pub snippet: String,
}

/// The overlay's state: which entries to show, which one the cursor is on,
/// and how many results were cut off by [`MAX_RESULTS`].
pub struct RefsPanel {
    pub title: String,
    pub entries: Vec<RefEntry>,
    pub truncated: usize,
    pub selected: usize,
}

impl RefsPanel {
    pub fn new(title: impl Into<String>, entries: Vec<RefEntry>, truncated: usize) -> Self {
        Self {
            title: title.into(),
            entries,
            truncated,
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

    pub fn selected_entry(&self) -> Option<&RefEntry> {
        self.entries.get(self.selected)
    }
}

/// Converts a `textDocument/definition`/`references` response's locations
/// into display-ready [`RefEntry`]s: reads each match's line off disk for a
/// snippet, and converts the LSP range (in `encoding`, the responding
/// server's negotiated position encoding — see
/// [`crate::lsp::LspManager::position_encoding`]) into a display-column
/// range within that (trimmed) snippet. Locations whose URI isn't a
/// resolvable local file path are skipped; capped at [`MAX_RESULTS`], with
/// the second return value being how many were cut off by that cap.
pub fn build_entries(
    locations: &[Location],
    repo_root: &Path,
    encoding: &PositionEncodingKind,
) -> (Vec<RefEntry>, usize) {
    let is_utf8 = encoding.as_str() == "utf-8";
    let entries = locations
        .iter()
        .take(MAX_RESULTS)
        .filter_map(|loc| build_entry(loc, repo_root, is_utf8))
        .collect();
    let truncated = locations
        .len()
        .saturating_sub(MAX_RESULTS.min(locations.len()));
    (entries, truncated)
}

fn build_entry(loc: &Location, repo_root: &Path, is_utf8: bool) -> Option<RefEntry> {
    let file = uri_to_path(&loc.uri)?;
    let content = std::fs::read_to_string(&file).unwrap_or_default();
    let raw_line = content
        .lines()
        .nth(loc.range.start.line as usize)
        .unwrap_or("");
    let columns = ColumnMap::new(raw_line);

    let to_display = |character: u32| -> usize {
        if is_utf8 {
            columns.utf8_to_display(character as usize)
        } else {
            columns.utf16_to_display(character as usize)
        }
    };
    let start_col = to_display(loc.range.start.character);
    let end_col = if loc.range.end.line == loc.range.start.line {
        to_display(loc.range.end.character).max(start_col)
    } else {
        // A multi-line match's end lands on a different line than its
        // start; only the start line is shown as a snippet, so the
        // highlighted range simply runs to the end of it.
        display_width(raw_line)
    };

    let trimmed = raw_line.trim_start();
    let leading = display_width(&raw_line[..raw_line.len() - trimmed.len()]);
    let snippet = trimmed.trim_end().to_owned();
    let snippet_len = display_width(&snippet);
    let adjust = |col: usize| col.saturating_sub(leading).min(snippet_len);

    let display_path = file
        .strip_prefix(repo_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| file.display().to_string());

    Some(RefEntry {
        file,
        display_path,
        line: loc.range.start.line,
        match_range: (adjust(start_col), adjust(end_col).max(adjust(start_col))),
        snippet,
    })
}

/// Bottom ~40% of `area`, full width.
fn panel_rect(area: Rect) -> Rect {
    let height = ((area.height as u32 * 2) / 5).clamp(5, area.height.max(5) as u32) as u16;
    Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(height),
        width: area.width,
        height,
    }
}

pub fn render(frame: &mut Frame, area: Rect, panel: &RefsPanel) {
    let rect = panel_rect(area);
    frame.render_widget(Clear, rect);

    let count_note = if panel.truncated > 0 {
        format!(" ({}, +{} more) ", panel.entries.len(), panel.truncated)
    } else {
        format!(" ({}) ", panel.entries.len())
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {}{count_note}", panel.title));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    if panel.entries.is_empty() {
        frame.render_widget(Paragraph::new("no results"), inner);
        return;
    }

    let visible_height = (inner.height as usize).max(1);
    let start = panel
        .selected
        .saturating_sub(visible_height.saturating_sub(1));

    let mut last_file: Option<&Path> = None;
    let lines: Vec<Line> = panel
        .entries
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_height)
        .map(|(idx, entry)| {
            let show_path = last_file != Some(entry.file.as_path());
            last_file = Some(entry.file.as_path());
            entry_line(entry, show_path, idx == panel.selected)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn entry_line(entry: &RefEntry, show_path: bool, is_selected: bool) -> Line<'static> {
    let path_label = if show_path {
        entry.display_path.clone()
    } else {
        String::new()
    };
    let mut spans = vec![
        Span::styled(
            format!("{path_label:<24}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>5}  ", entry.line + 1),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    spans.extend(highlighted_snippet(&entry.snippet, entry.match_range));

    let mut line = Line::from(spans);
    if is_selected {
        line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
    }
    line
}

fn highlighted_snippet(snippet: &str, (start, end): (usize, usize)) -> Vec<Span<'static>> {
    // `match_range` is in display columns; since these are line-of-source
    // snippets (no wide CJK-vs-byte subtleties this rendering needs to get
    // right beyond what's already visible), splitting the plain string by
    // character count is close enough for a highlight boundary — an
    // over/under-highlight by one combining character is a cosmetic detail
    // in a list of many matches, not a correctness issue the way a
    // hover/jump target's own column would be.
    let chars: Vec<char> = snippet.chars().collect();
    let start = start.min(chars.len());
    let end = end.clamp(start, chars.len());

    let before: String = chars[..start].iter().collect();
    let matched: String = chars[start..end].iter().collect();
    let after: String = chars[end..].iter().collect();

    let mut spans = Vec::new();
    if !before.is_empty() {
        spans.push(Span::raw(before));
    }
    if !matched.is_empty() {
        spans.push(Span::styled(
            matched,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ));
    }
    if !after.is_empty() {
        spans.push(Span::raw(after));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_next_and_prev_stay_in_bounds() {
        let entries = vec![
            RefEntry {
                file: PathBuf::from("/repo/a.rs"),
                display_path: "a.rs".to_owned(),
                line: 0,
                match_range: (0, 1),
                snippet: "a".to_owned(),
            },
            RefEntry {
                file: PathBuf::from("/repo/b.rs"),
                display_path: "b.rs".to_owned(),
                line: 0,
                match_range: (0, 1),
                snippet: "b".to_owned(),
            },
        ];
        let mut panel = RefsPanel::new("References", entries, 0);
        assert_eq!(panel.selected, 0);
        panel.select_prev();
        assert_eq!(panel.selected, 0, "cannot go above the first entry");
        panel.select_next();
        assert_eq!(panel.selected, 1);
        panel.select_next();
        assert_eq!(panel.selected, 1, "cannot go past the last entry");
        assert_eq!(panel.selected_entry().unwrap().display_path, "b.rs");
    }

    #[test]
    fn select_on_an_empty_panel_is_a_no_op() {
        let mut panel = RefsPanel::new("References", Vec::new(), 0);
        panel.select_next();
        panel.select_prev();
        assert_eq!(panel.selected, 0);
        assert_eq!(panel.selected_entry(), None);
    }

    #[test]
    fn build_entries_reads_snippet_and_converts_utf16_columns() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "    let 名前 = 1;\n").unwrap();
        let uri: lsp_types::Uri = format!("file://{}", file.display()).parse().unwrap();

        // "名前" starts at UTF-16 offset 8 (4 ASCII "let " chars + 4 leading
        // spaces = 8 UTF-16 units), 2 UTF-16 units wide.
        let loc = Location {
            uri,
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 8,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 10,
                },
            },
        };
        let (entries, truncated) = build_entries(&[loc], dir.path(), &PositionEncodingKind::UTF16);
        assert_eq!(truncated, 0);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.snippet, "let 名前 = 1;");
        // 4 leading spaces trimmed off; "名前" now starts at display column 4.
        assert_eq!(entry.match_range, (4, 8));
    }

    #[test]
    fn build_entries_caps_at_max_results_and_reports_the_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "line one\n").unwrap();
        let uri: lsp_types::Uri = format!("file://{}", file.display()).parse().unwrap();
        let loc = Location {
            uri,
            range: lsp_types::Range::default(),
        };
        let locations = vec![loc; MAX_RESULTS + 7];
        let (entries, truncated) =
            build_entries(&locations, dir.path(), &PositionEncodingKind::UTF8);
        assert_eq!(entries.len(), MAX_RESULTS);
        assert_eq!(truncated, 7);
    }
}
