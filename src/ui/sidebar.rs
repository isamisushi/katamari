//! Renders the changed-file list: one line per file with its path and
//! +/- stat counts, highlighting whichever file the diff pane's cursor is
//! currently within.

use crate::ui::app::App;
use crate::ui::text::truncate_to_width;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::RIGHT).title(" files ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let selected = app.selected_file();
    let path_width = (inner.width as usize).saturating_sub(10);

    let lines: Vec<Line> = app
        .files
        .iter()
        .enumerate()
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
            if idx == selected {
                line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
            }
            line
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}
