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

use std::collections::{HashMap, VecDeque};
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
    Kotlin,
    Java,
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
            "kt" | "kts" => Language::Kotlin,
            "java" => Language::Java,
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
        Language::Kotlin => (
            tree_sitter_kotlin_sg::LANGUAGE.into(),
            tree_sitter_kotlin_sg::HIGHLIGHTS_QUERY,
            "",
        ),
        Language::Java => (
            tree_sitter_java::LANGUAGE.into(),
            tree_sitter_java::HIGHLIGHTS_QUERY,
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
        Language::Kotlin => "kotlin",
        Language::Java => "java",
        Language::Plain => "plain",
    }
}

/// The most (language, line-text) entries [`LineHighlighter`]'s cache keeps
/// before evicting the oldest — generous relative to a terminal's visible
/// rows plus the small overscan `ui::diff_view`/`ui::file_view` highlight
/// per frame, so an ordinary review session's redraws at a fixed scroll
/// position never evict anything, while a very long session that's scrolled
/// through many distinct lines still has a bound on memory rather than
/// growing forever.
const CACHE_CAPACITY: usize = 4096;

/// Highlights individual lines, caching one compiled [`HighlightConfiguration`]
/// per language so repeated calls (once per visible diff line, every frame)
/// don't recompile grammar queries, *and* caching each line's own resulting
/// spans by `(language, line text)` so redrawing the same visible window
/// across frames — the overwhelming common case, since nothing about a
/// terminal UI's own idle redraws changes line content — never re-runs
/// tree-sitter at all for a line it's already highlighted. Construction is
/// cheap; the expensive grammar setup and per-line highlighting both happen
/// lazily, only for content actually encountered.
pub struct LineHighlighter {
    highlighter: Highlighter,
    configs: HashMap<Language, Option<HighlightConfiguration>>,
    cache: HashMap<(Language, String), Vec<Span>>,
    /// Insertion order for [`CACHE_CAPACITY`]-bounded eviction — a plain
    /// FIFO rather than true LRU, since a diff review's access pattern
    /// (mostly: redraw whatever's currently visible) doesn't reward the
    /// extra bookkeeping true LRU would need to tell "reused often" from
    /// "inserted long ago" apart.
    cache_order: VecDeque<(Language, String)>,
}

impl LineHighlighter {
    pub fn new() -> Self {
        Self {
            highlighter: Highlighter::new(),
            configs: HashMap::new(),
            cache: HashMap::new(),
            cache_order: VecDeque::new(),
        }
    }

    /// Highlights `line` as if it were a standalone file in `language`. On
    /// any parse or query failure, returns the whole line as a single
    /// [`HighlightKind::Plain`] span rather than propagating an error —
    /// highlighting is a rendering nicety, never a reason to fail the diff
    /// view. Content-addressed: a cache hit for the exact same `(language,
    /// line)` pair skips tree-sitter entirely, regardless of which row or
    /// file that text happens to be rendering for this time.
    pub fn highlight_line(&mut self, language: Language, line: &str) -> Vec<Span> {
        if line.is_empty() {
            return Vec::new();
        }

        let key = (language, line.to_owned());
        if let Some(cached) = self.cache.get(&key) {
            return cached.clone();
        }

        let spans =
            Self::highlight_uncached(&mut self.highlighter, &mut self.configs, language, line);
        self.insert_cache(key, spans.clone());
        spans
    }

    fn insert_cache(&mut self, key: (Language, String), spans: Vec<Span>) {
        if self.cache.len() >= CACHE_CAPACITY
            && let Some(oldest) = self.cache_order.pop_front()
        {
            self.cache.remove(&oldest);
        }
        self.cache_order.push_back(key.clone());
        self.cache.insert(key, spans);
    }

    /// Drops every cached line — called once a diff has been re-run (watch
    /// mode's refresh, or a fresh `App`/`FileView` swapped in) so stale
    /// entries from content the reviewer has moved past don't sit in memory
    /// for the rest of a long session. Not required for *correctness*: the
    /// cache is keyed by exact line text, so a stale entry for text that
    /// still exists somewhere would still be the right answer if reused —
    /// this is purely a memory-bound housekeeping call.
    pub fn clear_cache(&mut self) {
        self.cache.clear();
        self.cache_order.clear();
    }

    fn highlight_uncached(
        highlighter: &mut Highlighter,
        configs: &mut HashMap<Language, Option<HighlightConfiguration>>,
        language: Language,
        line: &str,
    ) -> Vec<Span> {
        let plain = || {
            vec![Span {
                text: line.to_owned(),
                kind: HighlightKind::Plain,
            }]
        };

        let config = match configs
            .entry(language)
            .or_insert_with(|| build_config(language))
        {
            Some(config) => &*config,
            None => return plain(),
        };

        let events = match highlighter.highlight(config, line.as_bytes(), None, |_| None) {
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
        assert_eq!(Language::detect("src/Main.kt"), Language::Kotlin);
        assert_eq!(Language::detect("build.gradle.kts"), Language::Kotlin);
        assert_eq!(Language::detect("src/Main.java"), Language::Java);
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
    fn a_repeated_call_with_the_same_language_and_text_returns_the_same_spans_from_cache() {
        let mut hl = LineHighlighter::new();
        let first = hl.highlight_line(Language::Rust, "fn main() {}");
        let second = hl.highlight_line(Language::Rust, "fn main() {}");
        assert_eq!(first, second);
        assert_eq!(
            hl.cache.len(),
            1,
            "one cache entry for the one distinct line"
        );
    }

    #[test]
    fn clear_cache_empties_the_cache_but_highlighting_still_works_afterward() {
        let mut hl = LineHighlighter::new();
        hl.highlight_line(Language::Rust, "fn main() {}");
        assert_eq!(hl.cache.len(), 1);
        hl.clear_cache();
        assert_eq!(hl.cache.len(), 0);
        let spans = hl.highlight_line(Language::Rust, "fn main() {}");
        assert!(
            spans
                .iter()
                .any(|s| s.kind == HighlightKind::Keyword && s.text == "fn")
        );
    }

    #[test]
    fn cache_evicts_the_oldest_entry_once_past_capacity() {
        let mut hl = LineHighlighter::new();
        for n in 0..CACHE_CAPACITY + 1 {
            hl.highlight_line(Language::Plain, &format!("line {n}"));
        }
        assert_eq!(
            hl.cache.len(),
            CACHE_CAPACITY,
            "capacity is enforced rather than growing without bound"
        );
        assert!(
            !hl.cache
                .contains_key(&(Language::Plain, "line 0".to_owned())),
            "the oldest entry was evicted to make room for the newest"
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
    fn kotlin_keyword_is_highlighted() {
        let mut hl = LineHighlighter::new();
        let spans = hl.highlight_line(Language::Kotlin, "val x = 1");
        assert!(
            spans
                .iter()
                .any(|s| s.kind == HighlightKind::Keyword && s.text == "val")
        );
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "val x = 1");
    }

    #[test]
    fn java_keyword_is_highlighted() {
        let mut hl = LineHighlighter::new();
        let spans = hl.highlight_line(Language::Java, "public class Main {}");
        assert!(
            spans
                .iter()
                .any(|s| s.kind == HighlightKind::Keyword && s.text == "class")
        );
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "public class Main {}");
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
