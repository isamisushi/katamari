//! Finds the identifier-like tokens on one line of text, so `Action::Hover`
//! has something more precise to target than "the whole line" and
//! `Action::NextSymbol`/`PrevSymbol` have something to cycle between.
//! "Identifier-like" is deliberately broad — any run of alphanumeric or `_`
//! grapheme clusters, which covers CJK identifiers (Unicode classifies Han,
//! Hiragana, and Katakana characters as alphabetic) the same way it covers
//! `snake_case` — rather than trying to approximate what any particular
//! language's actual identifier grammar allows. A language server is going
//! to reject a nonsensical position gracefully either way; this scanner
//! only needs to be a good enough guess at "where would a person's eye land
//! for a symbol."

use crate::config;
use crate::ui::text::tab_aware_width;
use unicode_segmentation::UnicodeSegmentation;

/// One identifier-like token's span, in the same display-column coordinate
/// space [`crate::diff::ColumnMap`] uses — `[display_start, display_end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Symbol {
    pub display_start: usize,
    pub display_end: usize,
}

/// Scans `line` (raw, unexpanded source text — as [`crate::diff::ColumnMap`]
/// also consumes it) for maximal runs of word graphemes, in left-to-right
/// order, using the configured `[ui] tab_width` (see [`config::tab_width`])
/// to advance the display column across any literal tab the same way
/// `ColumnMap` and `ui::text::expand_tabs_in_spans` do — see
/// [`scan_with_tab_width`] for the explicit-width entry point tests use. A
/// grapheme cluster counts as part of a word if its *first* codepoint is
/// alphanumeric or `_`; combining marks attached to a word character stay
/// part of that word regardless of their own category. A tab is never a
/// word character, so this only affects *where* a symbol after it starts,
/// not word boundaries themselves.
pub fn scan(line: &str) -> Vec<Symbol> {
    scan_with_tab_width(line, config::tab_width())
}

/// As [`scan`], with an explicit `tab_width` rather than reading the
/// installed [`Config`](crate::config::Config)'s — what this module's own
/// tab-stop tests use to stay independent of whatever a given test binary
/// run happens to have installed.
pub fn scan_with_tab_width(line: &str, tab_width: usize) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let mut current_start: Option<usize> = None;
    let mut display_col = 0usize;

    for grapheme in line.graphemes(true) {
        let is_word = grapheme
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if is_word {
            current_start.get_or_insert(display_col);
        } else if let Some(start) = current_start.take() {
            symbols.push(Symbol {
                display_start: start,
                display_end: display_col,
            });
        }
        display_col += tab_aware_width(grapheme, display_col, tab_width);
    }
    if let Some(start) = current_start.take() {
        symbols.push(Symbol {
            display_start: start,
            display_end: display_col,
        });
    }
    symbols
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_ascii_identifiers_on_punctuation_and_whitespace() {
        let symbols = scan("let x = foo(y, z);");
        let starts: Vec<usize> = symbols.iter().map(|s| s.display_start).collect();
        assert_eq!(starts.len(), 5); // let, x, foo, y, z
        assert_eq!(
            symbols[0],
            Symbol {
                display_start: 0,
                display_end: 3
            }
        ); // "let"
        assert_eq!(
            symbols[1],
            Symbol {
                display_start: 4,
                display_end: 5
            }
        ); // "x"
    }

    #[test]
    fn treats_a_run_of_cjk_characters_as_one_symbol() {
        // "名前" is two characters (4 display columns), one identifier.
        let symbols = scan("let 名前 = 1;");
        assert_eq!(symbols.len(), 3); // let, 名前, 1
        assert_eq!(
            symbols[1],
            Symbol {
                display_start: 4,
                display_end: 8
            }
        );
    }

    #[test]
    fn underscore_joins_a_snake_case_identifier_into_one_symbol() {
        let symbols = scan("my_variable_name");
        assert_eq!(
            symbols,
            vec![Symbol {
                display_start: 0,
                display_end: 16
            }]
        );
    }

    #[test]
    fn line_with_no_word_characters_has_no_symbols() {
        assert_eq!(scan(" (), ; == "), Vec::new());
    }

    #[test]
    fn empty_line_has_no_symbols() {
        assert_eq!(scan(""), Vec::new());
    }

    #[test]
    fn a_trailing_symbol_at_end_of_line_is_still_captured() {
        let symbols = scan("return ok");
        assert_eq!(
            symbols.last(),
            Some(&Symbol {
                display_start: 7,
                display_end: 9
            })
        );
    }

    #[test]
    fn a_symbol_after_a_leading_tab_starts_at_the_tab_stop_not_column_one() {
        // Tab width 4: the tab (col 0) expands to width 4, so "x" starts at
        // display column 4 — matching `ColumnMap`'s tab-aware rule, not the
        // pre-M7 "a tab is one column" behavior.
        let symbols = scan_with_tab_width("\tx", 4);
        assert_eq!(
            symbols,
            vec![Symbol {
                display_start: 4,
                display_end: 5
            }]
        );
    }
}
