//! Text-rendering helpers shared by every pane that draws highlighted source
//! lines (the diff view's unified and side-by-side columns, the file view).
//! All column math goes through here rather than `str::len`/`chars().count()`,
//! because a Japanese line must not misalign gutters or wrap mid-character:
//! East Asian wide characters occupy two terminal columns, and
//! `unicode-width` is the only thing that knows that.

use crate::highlight::{HighlightKind, Span as HlSpan};
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// The terminal column width of `s`, accounting for wide (e.g. CJK)
/// characters.
pub fn display_width(s: &str) -> usize {
    s.width()
}

/// Truncates `s` to at most `max_width` display columns, splitting only on
/// grapheme-cluster boundaries so a multi-byte character is never cut in
/// half. Returns `s` unchanged if it already fits.
pub fn truncate_to_width(s: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for grapheme in s.graphemes(true) {
        let grapheme_width = grapheme.width();
        if width + grapheme_width > max_width {
            break;
        }
        out.push_str(grapheme);
        width += grapheme_width;
    }
    out
}

/// Walks highlighted spans in order, dropping/truncating them once the
/// cumulative display width reaches `max_width`, so a highlighted line never
/// overflows its pane even when the cut falls mid-span.
pub fn truncate_spans_to_width(spans: &[HlSpan], max_width: usize) -> Vec<HlSpan> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        if used >= max_width {
            break;
        }
        let remaining = max_width - used;
        let span_width = display_width(&span.text);
        if span_width <= remaining {
            used += span_width;
            out.push(span.clone());
        } else {
            out.push(HlSpan {
                text: truncate_to_width(&span.text, remaining),
                kind: span.kind,
            });
            break;
        }
    }
    out
}

/// Patches an extra [`Style`] (e.g. underline) onto whichever part of
/// `spans` falls within the display-column window `[start, end)`, splitting
/// spans at the window's boundaries as needed so the rest of each span's
/// styling is untouched. Used to mark the row's active hover symbol without
/// the syntax highlighter needing to know that concept exists — it only
/// ever produces spans for *syntax*; this layers a second, independent
/// decoration on top by display column, the same coordinate space
/// [`crate::ui::symbols::scan`] reports symbol positions in.
///
/// A no-op if `start >= end` (nothing to mark).
pub fn mark_range(
    spans: Vec<Span<'static>>,
    start: usize,
    end: usize,
    extra: Style,
) -> Vec<Span<'static>> {
    if start >= end {
        return spans;
    }
    let mut out = Vec::with_capacity(spans.len());
    let mut col = 0usize;
    for span in spans {
        let text = span.content.into_owned();
        let width = display_width(&text);
        let span_start = col;
        let span_end = col + width;
        col = span_end;

        if end <= span_start || start >= span_end {
            out.push(Span::styled(text, span.style));
            continue;
        }

        let local_start = start.saturating_sub(span_start).min(width);
        let local_end = end.saturating_sub(span_start).min(width);

        // Both are grapheme-safe prefixes of `text`, and `up_to_end` is
        // always at least as long as `before` (`local_start <= local_end`),
        // so slicing between their byte lengths never lands mid-character.
        let before = truncate_to_width(&text, local_start);
        let up_to_end = truncate_to_width(&text, local_end);
        let mid = &text[before.len()..up_to_end.len()];
        let after = &text[up_to_end.len()..];

        if !before.is_empty() {
            out.push(Span::styled(before, span.style));
        }
        if !mid.is_empty() {
            out.push(Span::styled(mid.to_owned(), span.style.patch(extra)));
        }
        if !after.is_empty() {
            out.push(Span::styled(after.to_owned(), span.style));
        }
    }
    out
}

/// Maps a highlighter's coarse semantic category onto a terminal color. The
/// single mapping every pane with syntax highlighting shares, so the diff
/// view and the file view always agree on what a keyword or a string looks
/// like.
pub fn highlight_color(kind: HighlightKind) -> Color {
    match kind {
        HighlightKind::Keyword => Color::Magenta,
        HighlightKind::String => Color::Yellow,
        HighlightKind::Comment => Color::DarkGray,
        HighlightKind::Function => Color::Blue,
        HighlightKind::Type => Color::Cyan,
        HighlightKind::Number => Color::LightMagenta,
        HighlightKind::Operator => Color::Gray,
        HighlightKind::Variable | HighlightKind::Plain => Color::Reset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_width_matches_char_count() {
        assert_eq!(display_width("hello"), 5);
    }

    #[test]
    fn japanese_characters_count_as_double_width() {
        assert_eq!(display_width("こんにちは"), 10);
    }

    #[test]
    fn mixed_ascii_and_japanese_width_sums_correctly() {
        assert_eq!(display_width("fn 日本語() {}"), 3 + 6 + 5);
    }

    #[test]
    fn truncate_stops_before_exceeding_width_ascii() {
        assert_eq!(truncate_to_width("hello world", 5), "hello");
    }

    #[test]
    fn truncate_never_splits_a_wide_character_in_half() {
        // Each character is width 2; a budget of 5 only fits two of them
        // (width 4), not a third that would overflow to width 6.
        assert_eq!(truncate_to_width("日本語", 5), "日本");
        assert_eq!(display_width(&truncate_to_width("日本語", 5)), 4);
    }

    #[test]
    fn truncate_leaves_short_strings_unchanged() {
        assert_eq!(truncate_to_width("hi", 10), "hi");
    }

    use ratatui::style::Modifier;

    fn underline() -> Style {
        Style::default().add_modifier(Modifier::UNDERLINED)
    }

    fn plain_text(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn mark_range_splits_a_single_span_into_three_pieces() {
        let spans = vec![Span::raw("hello world")];
        let marked = mark_range(spans, 2, 7, underline());
        assert_eq!(plain_text(&marked), "hello world");
        assert_eq!(marked.len(), 3);
        assert_eq!(marked[0].content.as_ref(), "he");
        assert_eq!(marked[1].content.as_ref(), "llo w");
        assert_eq!(marked[2].content.as_ref(), "orld");
        assert!(!marked[0].style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(marked[1].style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(!marked[2].style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn mark_range_spans_a_boundary_between_two_input_spans() {
        let spans = vec![Span::raw("foo"), Span::raw("bar")];
        // Range [2, 4) covers the last char of "foo" and the first of "bar".
        let marked = mark_range(spans, 2, 4, underline());
        assert_eq!(plain_text(&marked), "foobar");
        let underlined: String = marked
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(underlined, "ob");
    }

    #[test]
    fn mark_range_never_splits_a_wide_character_in_half() {
        let spans = vec![Span::raw("日本語")]; // each char is 2 display columns
        // Range [1, 3) lands mid-character on both ends. Splitting reuses
        // `truncate_to_width`, which only ever includes a grapheme once its
        // *whole* width fits the budget — so both boundaries snap down to
        // the nearest earlier character edge rather than cutting "日" or
        // "本" in half. The output must still concatenate back to the
        // original text no matter where the marked region ends up.
        let marked = mark_range(spans, 1, 3, underline());
        assert_eq!(plain_text(&marked), "日本語");
        for span in &marked {
            assert!(
                span.content.chars().count() <= 1
                    || !span.style.add_modifier.contains(Modifier::UNDERLINED),
                "no span mixes multiple characters with a partial underline in a way that implies mid-character splitting"
            );
        }
        let underlined: String = marked
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(underlined, "日");
    }

    #[test]
    fn mark_range_is_a_no_op_when_start_is_not_before_end() {
        let spans = vec![Span::raw("hello")];
        let marked = mark_range(spans.clone(), 3, 3, underline());
        assert_eq!(plain_text(&marked), "hello");
        assert_eq!(marked.len(), 1);
        assert!(!marked[0].style.add_modifier.contains(Modifier::UNDERLINED));
    }
}
