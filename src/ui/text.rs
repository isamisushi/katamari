//! Text-rendering helpers shared by every pane that draws highlighted source
//! lines (the diff view's unified and side-by-side columns, the file view).
//! All column math goes through here rather than `str::len`/`chars().count()`,
//! because a Japanese line must not misalign gutters or wrap mid-character:
//! East Asian wide characters occupy two terminal columns, and
//! `unicode-width` is the only thing that knows that.

use crate::highlight::{HighlightKind, Span as HlSpan};
use ratatui::style::Color;
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
}
