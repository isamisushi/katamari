//! Best-effort syntax highlighting for individual diff lines via
//! tree-sitter. M1 tokenizes each line as a standalone snippet rather than
//! parsing whole files — good enough for coloring keywords/strings/comments
//! in a diff pane, and cheap enough to redo per visible line. A future
//! milestone can upgrade this to parse full file sides without callers
//! changing: the public API (`Language`, `Span`, `LineHighlighter`) doesn't
//! expose how highlighting is computed, only its result.
//!
//! This module knows nothing about ratatui or terminal colors — it reports
//! [`HighlightKind`], and `ui::diff_view` decides what color that means.
//! Keeping the mapping in the UI layer is what lets the UI's color choices
//! change without this module caring.

use std::collections::HashMap;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// A source language this module can highlight. Detected from file
/// extension; anything else falls back to [`Language::Plain`], which never
/// attempts a parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    TypeScript,
    Tsx,
    Python,
    Go,
    Plain,
}

impl Language {
    pub fn detect(path: &str) -> Self {
        match path.rsplit('.').next().unwrap_or("") {
            "rs" => Language::Rust,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "py" | "pyi" => Language::Python,
            "go" => Language::Go,
            _ => Language::Plain,
        }
    }
}

/// The semantic category of one highlighted span. Deliberately coarse: it's
/// a palette-sized set the UI layer can map onto colors, not a mirror of
/// tree-sitter's fine-grained capture names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighlightKind {
    Keyword,
    String,
    Comment,
    Function,
    Type,
    Number,
    Operator,
    Variable,
    Plain,
}

/// One styled slice of a highlighted line. Concatenating `text` across a
/// line's spans reproduces the original line exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub kind: HighlightKind,
}

/// The capture names queried from each grammar's `highlights.scm`. Order
/// fixes the index tree-sitter-highlight reports in `HighlightEvent::Source`
/// / `HighlightStart`, so `kind_for_index` must stay in sync with this list.
const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "escape",
    "function",
    "function.builtin",
    "function.macro",
    "function.method",
    "keyword",
    "label",
    "number",
    "operator",
    "property",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "string",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

fn kind_for_index(index: usize) -> HighlightKind {
    match HIGHLIGHT_NAMES.get(index) {
        Some(name) if name.starts_with("comment") => HighlightKind::Comment,
        Some(name) if name.starts_with("string") || name.starts_with("escape") => {
            HighlightKind::String
        }
        Some(name) if name.starts_with("keyword") => HighlightKind::Keyword,
        Some(name) if name.starts_with("function") => HighlightKind::Function,
        Some(name) if name.starts_with("type") || name.starts_with("constructor") => {
            HighlightKind::Type
        }
        Some(name) if name.starts_with("number") || name.starts_with("constant") => {
            HighlightKind::Number
        }
        Some(name) if name.starts_with("variable") => HighlightKind::Variable,
        Some(name) if name.starts_with("operator") || name.starts_with("punctuation") => {
            HighlightKind::Operator
        }
        _ => HighlightKind::Plain,
    }
}

fn build_config(language: Language) -> Option<HighlightConfiguration> {
    let (ts_language, highlights_query, injections_query) = match language {
        Language::Rust => (
            tree_sitter_rust::LANGUAGE.into(),
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            tree_sitter_rust::INJECTIONS_QUERY,
        ),
        Language::TypeScript => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            "",
        ),
        Language::Tsx => (
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            "",
        ),
        Language::Python => (
            tree_sitter_python::LANGUAGE.into(),
            tree_sitter_python::HIGHLIGHTS_QUERY,
            "",
        ),
        Language::Go => (
            tree_sitter_go::LANGUAGE.into(),
            tree_sitter_go::HIGHLIGHTS_QUERY,
            "",
        ),
        Language::Plain => return None,
    };

    let mut config = HighlightConfiguration::new(
        ts_language,
        language_name(language),
        highlights_query,
        injections_query,
        "",
    )
    .ok()?;
    config.configure(HIGHLIGHT_NAMES);
    Some(config)
}

fn language_name(language: Language) -> &'static str {
    match language {
        Language::Rust => "rust",
        Language::TypeScript => "typescript",
        Language::Tsx => "tsx",
        Language::Python => "python",
        Language::Go => "go",
        Language::Plain => "plain",
    }
}

/// Highlights individual lines, caching one compiled [`HighlightConfiguration`]
/// per language so repeated calls (once per visible diff line, every frame)
/// don't recompile grammar queries. Construction is cheap; the expensive
/// grammar setup happens lazily, only for languages actually encountered.
pub struct LineHighlighter {
    highlighter: Highlighter,
    configs: HashMap<Language, Option<HighlightConfiguration>>,
}

impl LineHighlighter {
    pub fn new() -> Self {
        Self {
            highlighter: Highlighter::new(),
            configs: HashMap::new(),
        }
    }

    /// Highlights `line` as if it were a standalone file in `language`. On
    /// any parse or query failure, returns the whole line as a single
    /// [`HighlightKind::Plain`] span rather than propagating an error —
    /// highlighting is a rendering nicety, never a reason to fail the diff
    /// view.
    pub fn highlight_line(&mut self, language: Language, line: &str) -> Vec<Span> {
        let plain = || {
            vec![Span {
                text: line.to_owned(),
                kind: HighlightKind::Plain,
            }]
        };

        if line.is_empty() {
            return Vec::new();
        }

        let config = match self
            .configs
            .entry(language)
            .or_insert_with(|| build_config(language))
        {
            Some(config) => &*config,
            None => return plain(),
        };

        let events = match self
            .highlighter
            .highlight(config, line.as_bytes(), None, |_| None)
        {
            Ok(events) => events,
            Err(_) => return plain(),
        };

        let mut spans = Vec::new();
        let mut kind_stack: Vec<HighlightKind> = Vec::new();
        for event in events {
            let Ok(event) = event else { return plain() };
            match event {
                HighlightEvent::HighlightStart(h) => kind_stack.push(kind_for_index(h.0)),
                HighlightEvent::HighlightEnd => {
                    kind_stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    let Some(text) = line.get(start..end) else {
                        continue;
                    };
                    if text.is_empty() {
                        continue;
                    }
                    let kind = kind_stack.last().copied().unwrap_or(HighlightKind::Plain);
                    spans.push(Span {
                        text: text.to_owned(),
                        kind,
                    });
                }
            }
        }

        if spans.is_empty() { plain() } else { spans }
    }
}

impl Default for LineHighlighter {
    fn default() -> Self {
        Self::new()
    }
}

/// Highlights an entire file in a single parse, then serves spans back one
/// line at a time. Unlike [`LineHighlighter`] (which treats each line as an
/// independent snippet — cheap to redo per visible row, but blind to
/// constructs that span lines, like block comments), `FileHighlighter` sees
/// the file's real syntax tree. [`ui::file_view::FileView`] builds one per
/// opened file; it isn't meant to be reused across files or re-highlighted
/// per frame the way `LineHighlighter` is.
///
/// [`ui::file_view::FileView`]: crate::ui::file_view::FileView
pub struct FileHighlighter {
    lines: Vec<Vec<Span>>,
}

impl FileHighlighter {
    /// Parses `source` once and splits the resulting spans across line
    /// boundaries. On any parse or query failure, or for [`Language::Plain`],
    /// falls back to one unhighlighted [`HighlightKind::Plain`] span per
    /// line — highlighting is a rendering nicety, never a reason to fail
    /// opening a file.
    pub fn new(language: Language, source: &str) -> Self {
        let plain = || FileHighlighter {
            lines: source
                .lines()
                .map(|line| {
                    vec![Span {
                        text: line.to_owned(),
                        kind: HighlightKind::Plain,
                    }]
                })
                .collect(),
        };

        let Some(config) = build_config(language) else {
            return plain();
        };

        let mut highlighter = Highlighter::new();
        let events = match highlighter.highlight(&config, source.as_bytes(), None, |_| None) {
            Ok(events) => events,
            Err(_) => return plain(),
        };

        let mut lines: Vec<Vec<Span>> = Vec::new();
        let mut current_line: Vec<Span> = Vec::new();
        let mut kind_stack: Vec<HighlightKind> = Vec::new();

        for event in events {
            let Ok(event) = event else { return plain() };
            match event {
                HighlightEvent::HighlightStart(h) => kind_stack.push(kind_for_index(h.0)),
                HighlightEvent::HighlightEnd => {
                    kind_stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    let Some(text) = source.get(start..end) else {
                        continue;
                    };
                    let kind = kind_stack.last().copied().unwrap_or(HighlightKind::Plain);
                    push_source_chunk(&mut lines, &mut current_line, text, kind);
                }
            }
        }
        if !current_line.is_empty() {
            lines.push(current_line);
        }

        FileHighlighter { lines }
    }

    /// The spans making up line `idx` (0-indexed). Empty for a blank line or
    /// an out-of-range index — [`ui::file_view`] treats both the same way,
    /// as a line with nothing to draw.
    ///
    /// [`ui::file_view`]: crate::ui::file_view
    pub fn line(&self, idx: usize) -> &[Span] {
        self.lines.get(idx).map_or(&[], Vec::as_slice)
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
}

/// Splits one highlighted chunk of source text (which may itself span
/// several lines, e.g. a block comment) across `lines`/`current_line`,
/// matching `str::lines()`'s notion of line boundaries: a trailing `\n` ends
/// a line without starting a new, empty one after it.
fn push_source_chunk(
    lines: &mut Vec<Vec<Span>>,
    current_line: &mut Vec<Span>,
    text: &str,
    kind: HighlightKind,
) {
    let mut parts = text.split('\n');
    if let Some(first) = parts.next()
        && !first.is_empty()
    {
        current_line.push(Span {
            text: first.to_owned(),
            kind,
        });
    }
    for part in parts {
        lines.push(std::mem::take(current_line));
        if !part.is_empty() {
            current_line.push(Span {
                text: part.to_owned(),
                kind,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_language_by_extension() {
        assert_eq!(Language::detect("src/main.rs"), Language::Rust);
        assert_eq!(Language::detect("src/app.tsx"), Language::Tsx);
        assert_eq!(Language::detect("src/app.ts"), Language::TypeScript);
        assert_eq!(Language::detect("script.py"), Language::Python);
        assert_eq!(Language::detect("main.go"), Language::Go);
        assert_eq!(Language::detect("README.md"), Language::Plain);
        assert_eq!(Language::detect("no_extension"), Language::Plain);
    }

    #[test]
    fn plain_language_returns_whole_line_unhighlighted() {
        let mut hl = LineHighlighter::new();
        let spans = hl.highlight_line(Language::Plain, "some text");
        assert_eq!(
            spans,
            vec![Span {
                text: "some text".to_owned(),
                kind: HighlightKind::Plain
            }]
        );
    }

    #[test]
    fn rust_keyword_is_highlighted() {
        let mut hl = LineHighlighter::new();
        let spans = hl.highlight_line(Language::Rust, "fn main() {}");
        assert!(
            spans
                .iter()
                .any(|s| s.kind == HighlightKind::Keyword && s.text == "fn")
        );
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "fn main() {}");
    }

    #[test]
    fn empty_line_yields_no_spans() {
        let mut hl = LineHighlighter::new();
        assert_eq!(hl.highlight_line(Language::Rust, ""), Vec::new());
    }

    #[test]
    fn file_highlighter_splits_spans_across_line_boundaries() {
        let source = "fn main() {\n    let x = 1;\n}\n";
        let hl = FileHighlighter::new(Language::Rust, source);
        assert_eq!(hl.line_count(), 3);
        assert!(
            hl.line(0)
                .iter()
                .any(|s| s.kind == HighlightKind::Keyword && s.text == "fn")
        );
        assert!(
            hl.line(1)
                .iter()
                .any(|s| s.kind == HighlightKind::Keyword && s.text == "let")
        );
        let joined: String = hl
            .line(1)
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(joined, "    let x = 1;");
    }

    #[test]
    fn file_highlighter_line_count_matches_str_lines_with_and_without_trailing_newline() {
        assert_eq!(
            FileHighlighter::new(Language::Plain, "a\nb\nc\n").line_count(),
            "a\nb\nc\n".lines().count()
        );
        assert_eq!(
            FileHighlighter::new(Language::Plain, "a\nb\nc").line_count(),
            "a\nb\nc".lines().count()
        );
        assert_eq!(FileHighlighter::new(Language::Plain, "").line_count(), 0);
    }

    #[test]
    fn file_highlighter_out_of_range_line_is_empty() {
        let hl = FileHighlighter::new(Language::Plain, "only line\n");
        assert_eq!(hl.line(5), &[] as &[Span]);
    }

    #[test]
    fn file_highlighter_multiline_comment_carries_its_kind_onto_every_line_it_spans() {
        let source = "/* line one\nline two */\nfn f() {}\n";
        let hl = FileHighlighter::new(Language::Rust, source);
        assert_eq!(hl.line_count(), 3);
        assert!(hl.line(0).iter().all(|s| s.kind == HighlightKind::Comment));
        assert!(hl.line(1).iter().all(|s| s.kind == HighlightKind::Comment));
    }
}
