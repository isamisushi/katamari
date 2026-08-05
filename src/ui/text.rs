//! Display-width helpers shared by every pane. All column math in the UI
//! goes through here rather than `str::len`/`chars().count()`, because a
//! Japanese line must not misalign gutters or wrap mid-character: East Asian
//! wide characters occupy two terminal columns, and `unicode-width` is the
//! only thing that knows that.

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
