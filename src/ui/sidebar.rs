//! Renders the changed-file list: one line per file with its path and
//! +/- stat counts. Issue #14 gave this pane two independent things to
//! mark, not one: [`App::files_selection`] (the sidebar's own browsed
//! position, reversed/bold) and [`App::diff_file`] (whichever file the
//! background diff cursor sits in, cyan/underlined when it differs from
//! the selection) — see those fields' docs for why they're allowed to
//! diverge at all. Renders through [`PaneChrome`] for the same focused-
//! border/bottom-hint treatment every other focusable pane gets.

use crate::keymap::Keymap;
use crate::ui::app::{App, MainPaneFocus};
use crate::ui::hints;
use crate::ui::pane::PaneChrome;
use crate::ui::text::truncate_to_width;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub fn render(frame: &mut Frame, area: Rect, app: &App, keymap: &Keymap) {
    let focused = app.focus == MainPaneFocus::Files;
    let block = PaneChrome::new(" files ", area.width)
        .focused(focused)
        .hints(&hints::files_pane_hints(keymap))
        .block();
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let diff_file = app.diff_file();
    let path_width = (inner.width as usize).saturating_sub(10);
    let viewport = inner.height as usize;

    let lines: Vec<Line> = app
        .files
        .iter()
        .enumerate()
        .skip(app.files_scroll_offset)
        .take(viewport)
        .map(|(idx, file)| {
            let (added, deleted) = file.stat();
            let path = truncate_to_width(file.display_path(), path_width);
            let mut spans = vec![Span::raw(format!("{path:<path_width$} "))];
            spans.push(Span::styled(
                format!("+{added} "),
                Style::default().fg(Color::Green),
            ));
            spans.push(Span::styled(
                format!("-{deleted}"),
                Style::default().fg(Color::Red),
            ));
            let mut line = Line::from(spans);
            if idx == app.files_selection {
                // Reversed while focused (the classic "this is where your
                // cursor is" treatment); bold-only while `Diff` owns focus,
                // so the selection still reads as "the sidebar's position"
                // without looking like an active cursor it isn't right now.
                let modifier = if focused {
                    Modifier::REVERSED
                } else {
                    Modifier::BOLD
                };
                line = line.patch_style(Style::default().add_modifier(modifier));
            } else if idx == diff_file {
                line = line.patch_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            line
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::DiffFile;
    use crate::keymap::vim_preset;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn app_with_files(paths: &[&str]) -> App {
        let files: Vec<DiffFile> = paths
            .iter()
            .map(|p| DiffFile {
                old_path: Some((*p).to_owned()),
                new_path: Some((*p).to_owned()),
                ..Default::default()
            })
            .collect();
        App::new("repo".to_owned(), PathBuf::from("/repo"), files)
    }

    fn draw(app: &App, keymap: &Keymap, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), app, keymap))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn focused_border_is_cyan_and_bold() {
        let mut app = app_with_files(&["a.rs", "b.rs"]);
        app.focus = MainPaneFocus::Files;
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let buffer = draw(&app, &keymap, 30, 8);
        let corner = buffer.cell((0, 0)).expect("top-left corner");
        assert_eq!(corner.fg, Color::Cyan);
        assert!(corner.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn unfocused_border_is_not_cyan() {
        let app = app_with_files(&["a.rs", "b.rs"]);
        assert_eq!(app.focus, MainPaneFocus::Diff);
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let buffer = draw(&app, &keymap, 30, 8);
        let corner = buffer.cell((0, 0)).expect("top-left corner");
        assert_ne!(corner.fg, Color::Cyan);
    }

    #[test]
    fn selection_and_background_file_get_visibly_distinct_styles_when_they_differ() {
        // Two-file diff: the diff cursor sits in file 0 (`App::diff_file`'s
        // default), but the reviewer has browsed the sidebar down to file 1
        // — the exact divergence req 5 requires stay visibly distinct.
        let mut app = app_with_files(&["a.rs", "b.rs"]);
        app.files_selection = 1;
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let buffer = draw(&app, &keymap, 30, 8);

        // Row 1 (inside the border) is "a.rs" — the background diff file,
        // marked cyan/underlined, not reversed.
        let a_cell = buffer.cell((1, 1)).expect("a.rs row, first column");
        assert_eq!(a_cell.fg, Color::Cyan);
        assert!(a_cell.modifier.contains(Modifier::UNDERLINED));
        assert!(!a_cell.modifier.contains(Modifier::REVERSED));

        // Row 2 is "b.rs" — the files-pane selection, bold (not focused).
        let b_cell = buffer.cell((1, 2)).expect("b.rs row, first column");
        assert!(b_cell.modifier.contains(Modifier::BOLD));
        assert!(!b_cell.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn scroll_offset_windows_the_visible_files() {
        // More files than the pane's inner height (8 rows - 2 borders - 1
        // bottom-hint row = 5 visible) with the selection scrolled well
        // past the top — the pre-#14 renderer drew the *whole* list with no
        // windowing at all (a latent bug this fixes), so file 0 must not
        // appear on screen once scrolled past.
        let paths: Vec<String> = (0..10).map(|i| format!("f{i}.rs")).collect();
        let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let mut app = app_with_files(&path_refs);
        app.files_selection = 9;
        app.files_scroll_offset = 9;
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let buffer = draw(&app, &keymap, 30, 8);
        let contents: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(
            !contents.contains("f0.rs"),
            "scrolled-past file 0 must not render"
        );
        assert!(
            contents.contains("f9.rs"),
            "the scrolled-to file must render"
        );
    }
}
