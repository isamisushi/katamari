//! Shared pane chrome for views that expose a small, local set of controls.
//!
//! Keeping title, focus, and bottom-hint rendering together prevents each
//! pane from inventing its own focus and hint-width math. Hint prioritization
//! keeps bottom controls inside the border without hiding essential actions.
//! That fit policy applies only to bottom hints; ratatui remains responsible
//! for laying out and truncating pane titles.

use std::borrow::Cow;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hint<'a> {
    pub(crate) key: Cow<'a, str>,
    pub(crate) description: Cow<'a, str>,
    pub(crate) essential: bool,
}

impl<'a> Hint<'a> {
    pub(crate) fn new(
        key: impl Into<Cow<'a, str>>,
        description: impl Into<Cow<'a, str>>,
        essential: bool,
    ) -> Self {
        Self {
            key: key.into(),
            description: description.into(),
            essential,
        }
    }
}

/// Builds the consistent border treatment used by a pane without coupling it
/// to a particular view. The caller supplies only the controls that belong to
/// that pane; this component owns their styling and fit-to-border behavior.
pub(crate) struct PaneChrome<'a> {
    title: String,
    focused: bool,
    width: u16,
    hints: Vec<Hint<'a>>,
}

impl<'a> PaneChrome<'a> {
    pub(crate) fn new(title: impl Into<String>, width: u16) -> Self {
        Self {
            title: title.into(),
            focused: false,
            width,
            hints: Vec::new(),
        }
    }

    pub(crate) fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub(crate) fn hints(mut self, hints: &[Hint<'a>]) -> Self {
        self.hints = hints.to_vec();
        self
    }

    pub(crate) fn block(self) -> Block<'static> {
        let mut block = Block::default().title(self.title).borders(Borders::ALL);
        if let Some(hint_line) = hint_line(&self.hints, self.width) {
            block = block.title_bottom(hint_line);
        }
        if self.focused {
            block = block.border_style(focus_style()).title_style(focus_style());
        }
        block
    }
}

fn focus_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn hint_line(hints: &[Hint<'_>], width: u16) -> Option<Line<'static>> {
    if hints.is_empty() {
        return None;
    }
    let available = width.saturating_sub(2) as usize;
    let full = hints
        .iter()
        .cloned()
        .map(|hint| (hint, false))
        .collect::<Vec<_>>();
    if choices_width(&full) <= available {
        return Some(build_hint_line(&full));
    }

    let essential_width = |compact| {
        choices_width(
            &hints
                .iter()
                .filter(|hint| hint.essential)
                .cloned()
                .map(|hint| (hint, compact))
                .collect::<Vec<_>>(),
        )
    };
    let compact_essentials = essential_width(false) > available;
    let mut modes = vec![None; hints.len()];
    for (index, hint) in hints.iter().enumerate() {
        if hint.essential {
            modes[index] = Some(compact_essentials);
        }
    }
    let choices = |modes: &[Option<bool>]| {
        hints
            .iter()
            .cloned()
            .zip(modes.iter().copied())
            .filter_map(|(hint, mode)| mode.map(|compact| (hint, compact)))
            .collect::<Vec<_>>()
    };
    if choices_width(&choices(&modes)) > available {
        let compact_keys = hints
            .iter()
            .filter(|hint| hint.essential)
            .map(|hint| hint.key.as_ref())
            .collect::<Vec<_>>()
            .join("/");
        if compact_keys.width() + 2 <= available {
            return Some(Line::from(vec![
                Span::raw(" ".to_owned()),
                Span::styled(compact_keys, hint_key_style()),
                Span::raw(" ".to_owned()),
            ]));
        }
        return None;
    }

    for (index, hint) in hints.iter().enumerate() {
        if hint.essential {
            continue;
        }
        modes[index] = Some(false);
        if choices_width(&choices(&modes)) > available {
            modes[index] = Some(true);
            if choices_width(&choices(&modes)) > available {
                modes[index] = None;
            }
        }
    }
    Some(build_hint_line(&choices(&modes)))
}

fn choices_width(hints: &[(Hint<'_>, bool)]) -> usize {
    if hints.is_empty() {
        return 0;
    }
    2 + hints
        .iter()
        .map(|(hint, compact)| {
            hint.key.width()
                + if *compact {
                    0
                } else {
                    1 + hint.description.width()
                }
        })
        .sum::<usize>()
        + 3 * hints.len().saturating_sub(1)
}

fn build_hint_line(hints: &[(Hint<'_>, bool)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ".to_owned())];
    for (index, (hint, compact)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ".to_owned(), hint_description_style()));
        }
        spans.push(Span::styled(hint.key.to_string(), hint_key_style()));
        if !compact {
            spans.push(Span::styled(
                format!(" {}", hint.description),
                hint_description_style(),
            ));
        }
    }
    spans.push(Span::raw(" ".to_owned()));
    Line::from(spans)
}

fn hint_key_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn hint_description_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::text::Line;
    use ratatui::widgets::Paragraph;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn prioritization_keeps_essential_keys_in_a_compact_border() {
        let hints = [
            Hint::new("j/k", "move", false),
            Hint::new("C-u/C-d", "page", false),
            Hint::new("gg/G", "top/bottom", false),
            Hint::new("V", "select", true),
            Hint::new("y", "yank", true),
        ];
        let line = hint_line(&hints, 12).expect("compact hint should fit");
        let text = line_text(&line);
        assert!(text.contains('V'));
        assert!(text.contains('y'));
        assert!(line.width() <= 10);
    }

    #[test]
    fn wide_hints_keep_descriptions_and_style_keys_separately() {
        let hints = [Hint::new("V", "select", true), Hint::new("y", "yank", true)];
        let line = hint_line(&hints, 80).expect("wide hint should fit");
        assert_eq!(line_text(&line), " V select · y yank ");
        let key_span = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "V")
            .expect("key span");
        assert_eq!(key_span.style.fg, Some(Color::Cyan));
        assert!(key_span.style.add_modifier.contains(Modifier::BOLD));
        let description_span = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == " select")
            .expect("description span");
        assert_eq!(description_span.style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn focused_chrome_styles_border_and_preserves_corners() {
        let hints = [Hint::new("V", "select", true), Hint::new("y", "yank", true)];
        let backend = TestBackend::new(30, 5);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let block = PaneChrome::new(" Journal ", 30)
                    .focused(true)
                    .hints(&hints)
                    .block();
                frame.render_widget(Paragraph::new("body").block(block), frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let corner = buffer.cell((0, 4)).expect("bottom-left corner");
        assert_eq!(corner.symbol(), "└");
        assert_eq!(corner.fg, Color::Cyan);
        assert!(corner.modifier.contains(Modifier::BOLD));
        let bottom = buffer
            .content
            .chunks(30)
            .nth(4)
            .expect("bottom border")
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(bottom.contains("V select · y yank"));
    }

    #[test]
    fn runtime_owned_and_scoped_labels_build_an_owned_chrome() {
        let key = format!("C-{}", "x");
        let description = String::from("runtime action");
        let scope = String::from("scoped action");
        let scoped: &str = &scope;
        let hints = [
            Hint::new(key, description, true),
            Hint::new(scoped, "optional", false),
        ];

        let line = hint_line(&hints, 80).expect("runtime hint should fit");
        let text = line_text(&line);
        assert!(text.contains("C-x runtime action"));
        assert!(text.contains("scoped action"));

        let _owned_block: Block<'static> = PaneChrome::new("Runtime", 80).hints(&hints).block();
    }
}
