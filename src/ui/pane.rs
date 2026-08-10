//! Shared pane chrome for views that expose a small, local set of controls.
//!
//! Keeping title, focus, and bottom-hint rendering together prevents each
//! pane from inventing its own focus and hint-width math. Hint prioritization
//! keeps bottom controls inside the border without hiding essential actions.
//! That fit policy applies only to bottom hints; ratatui remains responsible
//! for laying out and truncating pane titles.

use std::borrow::Cow;

use ratatui::layout::Rect;
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

/// The real [`Block::inner`] rect a [`PaneChrome`] of this exact `width`
/// carves out of `area`, regardless of title/hints/focus (none of which
/// change the border geometry itself — only which glyphs draw *in* the
/// border). The one place that geometry lives, so a caller sizing a pane's
/// content *before* the frame it's part of is even drawn (`ui::mod`'s frame
/// preamble, computing next frame's viewport height/width ahead of
/// `draw()`) and the real render call that follows agree on inner size by
/// construction, rather than by two independently hand-counted border
/// widths that can silently drift apart — see
/// `diff_view::unified_content_width`'s docs for exactly the drift issue
/// #14 fixed by routing through this instead of a literal `- 1`/`- 2`.
pub(crate) fn inner_rect(width: u16, area: Rect) -> Rect {
    PaneChrome::new(String::new(), width).block().inner(area)
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

/// Cycles `current` to the next (`forward`) or previous entry in `order`
/// that `visible` accepts, wrapping around at either end — the small
/// mechanic every Tab/BackTab-driven pane-focus view needs (the LSP
/// inspector's Servers/Detail/Journal panes, the timeline's list/diff
/// split, and the main files/diff panes later — issue #14) and none of
/// them should hand-roll separately. Deliberately generic over `T` rather
/// than any one view's own `Focus` enum, and deliberately ignorant of
/// `App`/`View`: this stays a pure focus-ring primitive, matching
/// [`PaneChrome`]'s own "styling primitive, not a layout framework" scope
/// — geometry and rendering remain view-owned.
///
/// - An empty `order` has nothing to cycle through: `current` unchanged.
/// - `current` present in `order`: walks at most `order.len()` steps in
///   the requested direction, skipping any entry `visible` rejects, and
///   wraps past either end. If nothing else is visible (a single-pane
///   view, or every sibling pane currently hidden), this returns `current`
///   unchanged rather than looping forever or landing somewhere invisible
///   — a genuinely single-pane-visible state is a no-op, not an error.
/// - `current` absent from `order` (a pane that disappeared out from under
///   the caller — a state change this module has no way to know about):
///   recovers to the first visible entry in `order`, or `current` itself
///   if nothing is visible at all.
pub(crate) fn cycle_focus<T: Copy + PartialEq>(
    order: &[T],
    current: T,
    forward: bool,
    visible: impl Fn(T) -> bool,
) -> T {
    if order.is_empty() {
        return current;
    }
    let Some(start) = order.iter().position(|&candidate| candidate == current) else {
        return order
            .iter()
            .copied()
            .find(|&candidate| visible(candidate))
            .unwrap_or(current);
    };
    let len = order.len();
    for step in 1..=len {
        let offset = if forward { step } else { len - step };
        let index = (start + offset) % len;
        if visible(order[index]) {
            return order[index];
        }
    }
    current
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

    // ---- cycle_focus (issue #13) ---------------------------------------

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Pane {
        A,
        B,
        C,
    }

    const ORDER: [Pane; 3] = [Pane::A, Pane::B, Pane::C];

    fn all_visible(_pane: Pane) -> bool {
        true
    }

    #[test]
    fn forward_advances_one_step_and_wraps_past_the_end() {
        assert_eq!(cycle_focus(&ORDER, Pane::A, true, all_visible), Pane::B);
        assert_eq!(cycle_focus(&ORDER, Pane::B, true, all_visible), Pane::C);
        assert_eq!(
            cycle_focus(&ORDER, Pane::C, true, all_visible),
            Pane::A,
            "forward from the last entry must wrap to the first"
        );
    }

    #[test]
    fn backward_retreats_one_step_and_wraps_past_the_start() {
        assert_eq!(cycle_focus(&ORDER, Pane::C, false, all_visible), Pane::B);
        assert_eq!(cycle_focus(&ORDER, Pane::B, false, all_visible), Pane::A);
        assert_eq!(
            cycle_focus(&ORDER, Pane::A, false, all_visible),
            Pane::C,
            "backward from the first entry must wrap to the last"
        );
    }

    #[test]
    fn invisible_entries_are_skipped_in_either_direction() {
        let skip_b = |pane: Pane| pane != Pane::B;
        assert_eq!(
            cycle_focus(&ORDER, Pane::A, true, skip_b),
            Pane::C,
            "forward from A must skip hidden B and land on C"
        );
        assert_eq!(
            cycle_focus(&ORDER, Pane::C, false, skip_b),
            Pane::A,
            "backward from C must skip hidden B and land on A"
        );
    }

    #[test]
    fn only_the_current_pane_visible_is_a_no_op() {
        let only_a = |pane: Pane| pane == Pane::A;
        assert_eq!(cycle_focus(&ORDER, Pane::A, true, only_a), Pane::A);
        assert_eq!(cycle_focus(&ORDER, Pane::A, false, only_a), Pane::A);
    }

    #[test]
    fn a_current_pane_absent_from_order_recovers_to_the_first_visible_entry() {
        // `Pane::A` isn't in this order at all — the shape of a pane that
        // disappeared out from under the caller (see `cycle_focus`'s docs).
        let without_a = [Pane::B, Pane::C];
        assert_eq!(cycle_focus(&without_a, Pane::A, true, all_visible), Pane::B);
    }

    #[test]
    fn a_current_pane_absent_from_order_with_nothing_visible_falls_back_to_current() {
        let without_a = [Pane::B, Pane::C];
        assert_eq!(cycle_focus(&without_a, Pane::A, true, |_| false), Pane::A);
    }

    #[test]
    fn an_empty_order_leaves_current_unchanged() {
        let empty: [Pane; 0] = [];
        assert_eq!(cycle_focus(&empty, Pane::A, true, all_visible), Pane::A);
        assert_eq!(cycle_focus(&empty, Pane::A, false, all_visible), Pane::A);
    }
}
