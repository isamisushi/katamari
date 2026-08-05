//! Per-file diagnostics, as published by `textDocument/publishDiagnostics`
//! notifications — the gutter markers in `ui::diff_view`/`ui::file_view`
//! and the hover popup's "Diagnostics" section both read from one
//! [`DiagnosticsStore`] rather than each parsing notifications themselves.
//!
//! Diagnostics only ever arrive for a document the server has been told
//! about via `textDocument/didOpen` (see [`crate::lsp::manager::LspManager::warm_up`]
//! for how `katamari` makes that happen proactively rather than waiting for
//! a hover to trigger it lazily); a file this store has no entry for isn't
//! necessarily clean, it may just not have been opened yet.

use lsp_types::{Diagnostic, DiagnosticSeverity, PublishDiagnosticsParams};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct DiagnosticsStore {
    by_file: HashMap<PathBuf, Vec<Diagnostic>>,
}

impl DiagnosticsStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces `file`'s diagnostics wholesale — `publishDiagnostics` is
    /// always a full snapshot for that URI, never a delta, so there's
    /// nothing to merge. An empty list removes the entry rather than
    /// keeping a `Vec::new()` around, so [`Self::for_file`] and every
    /// gutter/hover lookup built on it see "no entry" and "explicitly
    /// cleared" as the same thing, which they are.
    pub fn set(&mut self, file: PathBuf, diagnostics: Vec<Diagnostic>) {
        if diagnostics.is_empty() {
            self.by_file.remove(&file);
        } else {
            self.by_file.insert(file, diagnostics);
        }
    }

    pub fn for_file(&self, file: &Path) -> &[Diagnostic] {
        self.by_file.get(file).map_or(&[], Vec::as_slice)
    }

    /// Every diagnostic in `file` whose range touches 0-based `line` —
    /// including a diagnostic that spans past this line on either end, so a
    /// multi-line error's whole extent lights up the gutter, not just the
    /// line it starts on.
    pub fn diagnostics_on_line(&self, file: &Path, line: u32) -> Vec<&Diagnostic> {
        self.for_file(file)
            .iter()
            .filter(|d| d.range.start.line <= line && line <= d.range.end.line)
            .collect()
    }

    /// The most severe diagnostic touching `line`, for a single gutter
    /// glyph — `None` when nothing does. A diagnostic with no `severity` at
    /// all (the LSP spec allows omitting it) is treated as `INFORMATION`
    /// rather than excluded, since "the server flagged this line but didn't
    /// say how badly" is still worth a glyph. `DiagnosticSeverity`'s
    /// derived `Ord` already runs from `ERROR` (`1`, least) to `HINT` (`4`,
    /// greatest), so "most severe" is simply `min()`.
    pub fn severity_at(&self, file: &Path, line: u32) -> Option<DiagnosticSeverity> {
        self.diagnostics_on_line(file, line)
            .into_iter()
            .map(|d| d.severity.unwrap_or(DiagnosticSeverity::INFORMATION))
            .min()
    }

    /// Every 0-based line number in `file` that carries at least one
    /// diagnostic, ascending and deduplicated — what
    /// [`crate::ui::app::App::jump_to_diagnostic`] and
    /// [`crate::ui::file_view::FileView::jump_to_diagnostic`] search for the
    /// next/previous hit relative to the cursor.
    pub fn lines_with_diagnostics(&self, file: &Path) -> Vec<u32> {
        let mut lines: Vec<u32> = self
            .for_file(file)
            .iter()
            .map(|d| d.range.start.line)
            .collect();
        lines.sort_unstable();
        lines.dedup();
        lines
    }
}

/// Parses a `textDocument/publishDiagnostics` notification's raw JSON-RPC
/// params into its typed form. `None` on anything that doesn't parse as
/// this notification's documented shape — a malformed notification from a
/// server isn't this client's error to surface, so it's dropped rather than
/// panicking or propagating.
pub fn parse_publish_diagnostics(params: &serde_json::Value) -> Option<PublishDiagnosticsParams> {
    serde_json::from_value(params.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn diagnostic(start_line: u32, end_line: u32, severity: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line: start_line,
                    character: 0,
                },
                end: Position {
                    line: end_line,
                    character: 5,
                },
            },
            severity: Some(severity),
            message: "boom".to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn severity_at_picks_the_most_severe_of_several_on_one_line() {
        let mut store = DiagnosticsStore::new();
        let file = PathBuf::from("/repo/src/lib.rs");
        store.set(
            file.clone(),
            vec![
                diagnostic(3, 3, DiagnosticSeverity::WARNING),
                diagnostic(3, 3, DiagnosticSeverity::ERROR),
                diagnostic(3, 3, DiagnosticSeverity::HINT),
            ],
        );
        assert_eq!(store.severity_at(&file, 3), Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn severity_at_is_none_off_the_diagnostics_lines() {
        let mut store = DiagnosticsStore::new();
        let file = PathBuf::from("/repo/src/lib.rs");
        store.set(
            file.clone(),
            vec![diagnostic(3, 3, DiagnosticSeverity::ERROR)],
        );
        assert_eq!(store.severity_at(&file, 4), None);
    }

    #[test]
    fn a_multiline_diagnostic_covers_every_line_it_spans() {
        let mut store = DiagnosticsStore::new();
        let file = PathBuf::from("/repo/src/lib.rs");
        store.set(
            file.clone(),
            vec![diagnostic(2, 5, DiagnosticSeverity::ERROR)],
        );
        for line in 2..=5 {
            assert_eq!(
                store.severity_at(&file, line),
                Some(DiagnosticSeverity::ERROR)
            );
        }
        assert_eq!(store.severity_at(&file, 6), None);
    }

    #[test]
    fn setting_an_empty_list_clears_the_file_entirely() {
        let mut store = DiagnosticsStore::new();
        let file = PathBuf::from("/repo/src/lib.rs");
        store.set(
            file.clone(),
            vec![diagnostic(1, 1, DiagnosticSeverity::ERROR)],
        );
        assert!(!store.for_file(&file).is_empty());
        store.set(file.clone(), Vec::new());
        assert!(store.for_file(&file).is_empty());
    }

    #[test]
    fn lines_with_diagnostics_is_sorted_and_deduplicated() {
        let mut store = DiagnosticsStore::new();
        let file = PathBuf::from("/repo/src/lib.rs");
        store.set(
            file.clone(),
            vec![
                diagnostic(5, 5, DiagnosticSeverity::WARNING),
                diagnostic(1, 1, DiagnosticSeverity::ERROR),
                diagnostic(5, 5, DiagnosticSeverity::ERROR),
            ],
        );
        assert_eq!(store.lines_with_diagnostics(&file), vec![1, 5]);
    }

    #[test]
    fn parse_publish_diagnostics_reads_uri_and_diagnostics() {
        let params = serde_json::json!({
            "uri": "file:///repo/src/lib.rs",
            "diagnostics": [
                {
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
                    "severity": 1,
                    "message": "mismatched types"
                }
            ]
        });
        let parsed = parse_publish_diagnostics(&params).expect("valid publishDiagnostics params");
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(parsed.diagnostics[0].message, "mismatched types");
    }

    #[test]
    fn parse_publish_diagnostics_rejects_malformed_params() {
        let params = serde_json::json!({"not": "valid"});
        assert!(parse_publish_diagnostics(&params).is_none());
    }
}
