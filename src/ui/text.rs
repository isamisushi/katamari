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
/// characters. Assumes `s` contains no raw tab characters — every span this
/// is called on has already passed through [`expand_tabs_in_spans`], which
/// replaces each tab with the right number of literal spaces for wherever
/// it fell in its line, so by the time width math like this runs there's
/// nothing tab-shaped left to account for specially.
pub fn display_width(s: &str) -> usize {
    s.width()
}

/// One grapheme's on-screen width at display column `col` of its line, with
/// tab stops every `tab_width` columns — the single width rule
/// [`crate::diff::ColumnMap`] (for converting a display column to/from an
/// LSP request's byte/UTF-16 offset) and [`crate::ui::symbols::scan`] (for
/// where the active-symbol underline falls) both apply directly to raw,
/// unexpanded line text, and that [`expand_tabs_in_spans`] applies while
/// rewriting a tab into literal spaces. All three must agree pixel-for-
/// pixel on where a tab lands, or the cursor, an LSP position, and what's
/// actually drawn would each compute a different column for the same
/// character — see this module's and `ColumnMap`'s tests for the tab-stop
/// cases that pin this down. A non-tab grapheme's width is
/// [`unicode_width`]'s ordinary answer, unaffected by `col`.
pub fn tab_aware_width(grapheme: &str, col: usize, tab_width: usize) -> usize {
    if grapheme == "\t" {
        // Distance to the next multiple of `tab_width`, minimum 1 so a tab
        // sitting exactly on a stop still advances a full stop rather than
        // vanishing — the same rule a terminal's own tab stops follow.
        let into_stop = col % tab_width;
        tab_width - into_stop
    } else {
        grapheme.width()
    }
}

/// Rewrites every literal tab character across a line's highlighted
/// `spans`, in place of the tab, into the number of literal space
/// characters [`tab_aware_width`] says that tab occupies at its position in
/// the line — tracking display column continuously *across* every span in
/// order, not restarting at 0 per span, since a tab's width depends on
/// where in the whole line it falls, not where it falls within whichever
/// highlighted token happens to contain it.
///
/// Run this once per line, right after highlighting and before any
/// width/truncation math (`truncate_spans_to_width`, `mark_range`, gutter
/// width arithmetic) — everything downstream of it can then assume, as
/// [`display_width`] does, that no span it sees still contains a raw tab.
/// A terminal has no reliable, pane-relative notion of "tab stop" of its
/// own (it would expand a literal `\t` using its *own* tab settings,
/// measured from the terminal's left edge, not this pane's content column)
/// — so a tab must become real spaces before it ever reaches ratatui, or
/// on-screen alignment would silently stop matching what [`ColumnMap`] and
/// [`symbols::scan`] computed for the LSP-facing coordinate space.
///
/// [`ColumnMap`]: crate::diff::ColumnMap
/// [`symbols::scan`]: crate::ui::symbols::scan
pub fn expand_tabs_in_spans(spans: Vec<HlSpan>, tab_width: usize) -> Vec<HlSpan> {
    let mut col = 0usize;
    spans
        .into_iter()
        .map(|span| {
            let mut text = String::with_capacity(span.text.len());
            for grapheme in span.text.graphemes(true) {
                let width = tab_aware_width(grapheme, col, tab_width);
                if grapheme == "\t" {
                    text.extend(std::iter::repeat_n(' ', width));
                } else {
                    text.push_str(grapheme);
                }
                col += width;
            }
            HlSpan {
                text,
                kind: span.kind,
            }
        })
        .collect()
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

/// Word-wraps `text` to at most `width` display columns per line, splitting
/// only on whitespace boundaries (never mid-word) — used to fit a comment
/// body's free-form prose into the diff pane's inline comment block, where
/// unlike a syntax-highlighted source line, there are no existing span
/// boundaries to truncate at. A single word wider than `width` on its own
/// is left to overflow rather than broken mid-character, since character-
/// splitting a URL or an identifier the reviewer wrote in a comment reads
/// worse than an occasionally-overflowing line.
///
/// Each input line (split on `\n`) wraps independently, so a comment's own
/// paragraph breaks are preserved; a blank input line stays blank. `width ==
/// 0` degrades to one line per input line, unwrapped, rather than looping
/// forever trying to fit words into no space at all.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return text.lines().map(str::to_owned).collect();
    }
    let mut out = Vec::new();
    for raw_line in text.lines() {
        if raw_line.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0;
        for word in raw_line.split_whitespace() {
            let word_width = display_width(word);
            let sep_width = if current.is_empty() { 0 } else { 1 };
            if !current.is_empty() && current_width + sep_width + word_width > width {
                out.push(std::mem::take(&mut current));
                current_width = 0;
            }
            if !current.is_empty() {
                current.push(' ');
                current_width += 1;
            }
            current.push_str(word);
            current_width += word_width;
        }
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
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
    fn wrap_text_breaks_only_on_whitespace_boundaries() {
        let wrapped = wrap_text("the quick brown fox jumps", 10);
        assert_eq!(wrapped, vec!["the quick", "brown fox", "jumps"]);
    }

    #[test]
    fn wrap_text_preserves_blank_lines_and_paragraph_breaks() {
        let wrapped = wrap_text("first\n\nsecond", 20);
        assert_eq!(wrapped, vec!["first", "", "second"]);
    }

    #[test]
    fn wrap_text_leaves_an_overlong_single_word_unsplit() {
        let wrapped = wrap_text("https://example.com/very/long/path", 10);
        assert_eq!(wrapped, vec!["https://example.com/very/long/path"]);
    }

    #[test]
    fn wrap_text_with_zero_width_returns_lines_unwrapped() {
        assert_eq!(wrap_text("a b c", 0), vec!["a b c"]);
    }

    #[test]
    fn mark_range_is_a_no_op_when_start_is_not_before_end() {
        let spans = vec![Span::raw("hello")];
        let marked = mark_range(spans.clone(), 3, 3, underline());
        assert_eq!(plain_text(&marked), "hello");
        assert_eq!(marked.len(), 1);
        assert!(!marked[0].style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn tab_aware_width_advances_to_the_next_stop() {
        // Tab width 4: a tab at column 0 reaches column 4 (width 4); a tab
        // at column 1 also reaches column 4 (width 3) — a tab always lands
        // *on* a stop, however far away that stop currently is.
        assert_eq!(tab_aware_width("\t", 0, 4), 4);
        assert_eq!(tab_aware_width("\t", 1, 4), 3);
        assert_eq!(tab_aware_width("\t", 3, 4), 1);
    }

    #[test]
    fn tab_aware_width_sitting_exactly_on_a_stop_advances_a_full_stop() {
        // A tab at column 4 (already on a stop) advances to column 8, not
        // column 4 again — matches a real terminal's own tab-stop behavior.
        assert_eq!(tab_aware_width("\t", 4, 4), 4);
    }

    #[test]
    fn tab_aware_width_is_ordinary_grapheme_width_for_non_tab_input() {
        assert_eq!(tab_aware_width("a", 7, 4), 1);
        assert_eq!(tab_aware_width("日", 1, 4), 2);
    }

    fn plain_hl(text: &str) -> HlSpan {
        HlSpan {
            text: text.to_owned(),
            kind: HighlightKind::Plain,
        }
    }

    #[test]
    fn expand_tabs_in_spans_replaces_a_leading_tab_with_spaces_to_the_first_stop() {
        let spans = vec![plain_hl("\tx")];
        let expanded = expand_tabs_in_spans(spans, 4);
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].text, "    x");
    }

    #[test]
    fn expand_tabs_in_spans_tracks_display_column_across_span_boundaries() {
        // "ab" (cols 0-1) then a tab in a *second* span starting at col 2:
        // with tab width 4 that tab must reach col 4 (width 2), not restart
        // counting from 0 as if the second span were its own line.
        let spans = vec![plain_hl("ab"), plain_hl("\tc")];
        let expanded = expand_tabs_in_spans(spans, 4);
        assert_eq!(expanded[0].text, "ab");
        assert_eq!(expanded[1].text, "  c");
    }

    #[test]
    fn expand_tabs_in_spans_leaves_tab_free_text_unchanged() {
        let spans = vec![plain_hl("no tabs here")];
        let expanded = expand_tabs_in_spans(spans.clone(), 4);
        assert_eq!(expanded, spans);
    }

    #[test]
    fn expand_tabs_in_spans_matches_the_display_width_a_tab_aware_line_would_report() {
        // Expanding "a\tb" (tab width 4) then measuring with plain
        // `display_width` must agree with what `ColumnMap`'s tab-aware rule
        // computes directly on the raw text — the equivalence the whole
        // tab-stop design rests on (see this function's doc comment).
        let expanded = expand_tabs_in_spans(vec![plain_hl("a\tb")], 4);
        let rendered: String = expanded.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(display_width(&rendered), 5); // 'a' (1) + tab-to-stop (3) + 'b' (1)
    }
}
