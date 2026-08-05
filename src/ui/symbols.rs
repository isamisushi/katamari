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

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// One identifier-like token's span, in the same display-column coordinate
/// space [`crate::diff::ColumnMap`] uses — `[display_start, display_end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Symbol {
    pub display_start: usize,
    pub display_end: usize,
}

/// Scans `line` for maximal runs of word graphemes, in left-to-right order.
/// A grapheme cluster counts as part of a word if its *first* codepoint is
/// alphanumeric or `_`; combining marks attached to a word character stay
/// part of that word regardless of their own category.
pub fn scan(line: &str) -> Vec<Symbol> {
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
        display_col += grapheme.width();
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
}
