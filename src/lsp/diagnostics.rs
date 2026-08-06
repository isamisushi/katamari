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

use lsp_types::{
    Diagnostic, DiagnosticSeverity, DocumentDiagnosticReport, DocumentDiagnosticReportKind,
    DocumentDiagnosticReportResult, PublishDiagnosticsParams, Uri,
};
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

/// One document's outcome from a `textDocument/diagnostic` pull, folded from
/// the wire's full/unchanged distinction into what
/// [`crate::lsp::manager::LspManager`] actually needs to act on. Either way
/// it carries the `resultId` (if the server sent one) to store and replay as
/// `previousResultId` on that document's next pull — the mechanism that lets
/// a server answer "still accurate" instead of resending an identical
/// diagnostic set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PulledDocument {
    /// The diagnostics changed (or this is the document's first pull) —
    /// `items` is the full, current set and should replace whatever
    /// [`DiagnosticsStore`] held for this document, exactly as a
    /// `publishDiagnostics` notification would.
    Full {
        items: Vec<Diagnostic>,
        result_id: Option<String>,
    },
    /// The server confirmed nothing has changed since `result_id` was last
    /// sent as `previousResultId` — [`DiagnosticsStore`] is left untouched.
    Unchanged { result_id: String },
}

/// Folds one `textDocument/diagnostic` response into a flat list of
/// `(document, outcome)` pairs: the document that was pulled, plus — when
/// the server folded in `relatedDocuments` (a cross-file diagnostic; see
/// [`crate::lsp::client::client_capabilities`]'s `related_document_support`)
/// — every other document the response also carries an outcome for. A
/// caller applies each pair to [`DiagnosticsStore`] uniformly; nothing
/// distinguishes "the document I asked about" from "a related document" once
/// this returns, because [`DiagnosticsStore`] doesn't either.
///
/// Empty for [`DocumentDiagnosticReportResult::Partial`]: that shape only
/// arises from a streamed `$/progress`-tagged pull this client never issues
/// (no `partial_result_params` token is sent — see
/// [`crate::lsp::client::Client::pull_diagnostics`]), so a conforming server
/// should never send it back; folding it to nothing rather than panicking
/// keeps a server's protocol slip from becoming a crash.
pub fn fold_pull_result(
    primary: Uri,
    result: DocumentDiagnosticReportResult,
) -> Vec<(Uri, PulledDocument)> {
    let report = match result {
        DocumentDiagnosticReportResult::Report(report) => report,
        DocumentDiagnosticReportResult::Partial(_) => return Vec::new(),
    };
    match report {
        DocumentDiagnosticReport::Full(full) => {
            let mut out = vec![(
                primary,
                fold_report_kind(DocumentDiagnosticReportKind::Full(
                    full.full_document_diagnostic_report,
                )),
            )];
            out.extend(related_documents(full.related_documents));
            out
        }
        DocumentDiagnosticReport::Unchanged(unchanged) => {
            let mut out = vec![(
                primary,
                fold_report_kind(DocumentDiagnosticReportKind::Unchanged(
                    unchanged.unchanged_document_diagnostic_report,
                )),
            )];
            out.extend(related_documents(unchanged.related_documents));
            out
        }
    }
}

fn related_documents(
    related: Option<HashMap<Uri, DocumentDiagnosticReportKind>>,
) -> Vec<(Uri, PulledDocument)> {
    related
        .into_iter()
        .flatten()
        .map(|(uri, kind)| (uri, fold_report_kind(kind)))
        .collect()
}

fn fold_report_kind(kind: DocumentDiagnosticReportKind) -> PulledDocument {
    match kind {
        DocumentDiagnosticReportKind::Full(full) => PulledDocument::Full {
            items: full.items,
            result_id: full.result_id,
        },
        DocumentDiagnosticReportKind::Unchanged(unchanged) => PulledDocument::Unchanged {
            result_id: unchanged.result_id,
        },
    }
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

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    #[test]
    fn fold_pull_result_extracts_items_and_result_id_from_a_full_report() {
        let params = serde_json::json!({
            "kind": "full",
            "resultId": "r1",
            "items": [diagnostic(0, 0, DiagnosticSeverity::ERROR)],
        });
        let report: DocumentDiagnosticReportResult = serde_json::from_value(params).unwrap();
        let primary = uri("file:///repo/src/lib.rs");
        let folded = fold_pull_result(primary.clone(), report);

        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].0, primary);
        match &folded[0].1 {
            PulledDocument::Full { items, result_id } => {
                assert_eq!(items.len(), 1);
                assert_eq!(result_id.as_deref(), Some("r1"));
            }
            other => panic!("expected Full, got {other:?}"),
        }
    }

    #[test]
    fn fold_pull_result_carries_the_result_id_forward_on_an_unchanged_report() {
        let params = serde_json::json!({
            "kind": "unchanged",
            "resultId": "r1",
        });
        let report: DocumentDiagnosticReportResult = serde_json::from_value(params).unwrap();
        let primary = uri("file:///repo/src/lib.rs");
        let folded = fold_pull_result(primary.clone(), report);

        assert_eq!(
            folded,
            vec![(
                primary,
                PulledDocument::Unchanged {
                    result_id: "r1".to_owned()
                }
            )]
        );
    }

    #[test]
    fn fold_pull_result_folds_related_documents_in_alongside_the_primary_one() {
        // A macro in the primary document produced a diagnostic in a header
        // it depends on — exactly the cross-file case
        // `related_document_support` exists for.
        let params = serde_json::json!({
            "kind": "full",
            "resultId": "primary-r1",
            "items": [diagnostic(0, 0, DiagnosticSeverity::ERROR)],
            "relatedDocuments": {
                "file:///repo/src/header.h": {
                    "kind": "full",
                    "resultId": "related-r1",
                    "items": [diagnostic(2, 2, DiagnosticSeverity::WARNING)],
                }
            }
        });
        let report: DocumentDiagnosticReportResult = serde_json::from_value(params).unwrap();
        let primary = uri("file:///repo/src/lib.rs");
        let related = uri("file:///repo/src/header.h");
        let mut folded = fold_pull_result(primary.clone(), report);
        folded.sort_by_key(|(uri, _)| uri.as_str().to_owned());

        assert_eq!(folded.len(), 2);
        let (found_primary, primary_outcome) = folded
            .iter()
            .find(|(u, _)| *u == primary)
            .expect("primary document present");
        assert_eq!(found_primary, &primary);
        assert!(matches!(
            primary_outcome,
            PulledDocument::Full { result_id: Some(id), .. } if id == "primary-r1"
        ));
        let (_, related_outcome) = folded
            .iter()
            .find(|(u, _)| *u == related)
            .expect("related document present");
        assert!(matches!(
            related_outcome,
            PulledDocument::Full { result_id: Some(id), .. } if id == "related-r1"
        ));
    }

    #[test]
    fn fold_pull_result_is_empty_for_a_partial_result() {
        // This client never issues a partial-result token (see
        // `Client::pull_diagnostics`), so a well-behaved server should never
        // send this shape back — but folding it to nothing rather than
        // panicking means a server's protocol slip can't crash the caller.
        let params = serde_json::json!({});
        let report: DocumentDiagnosticReportResult = serde_json::from_value(params).unwrap();
        assert!(matches!(report, DocumentDiagnosticReportResult::Partial(_)));
        let folded = fold_pull_result(uri("file:///repo/src/lib.rs"), report);
        assert!(folded.is_empty());
    }
}
