//! Hover popup: the query contract `App`/`FileView` build for
//! `Action::Hover`, the request/response lifecycle that keeps a stale
//! response from ever rendering, and the floating overlay itself.
//!
//! `HoverState` deliberately lives outside `App`/`FileView` — issuing and
//! tracking an LSP request is not a pure state transition, so it belongs in
//! `ui::mod`'s event loop (the one place already responsible for anything
//! that touches the outside world) rather than inside the views those
//! modules keep testable without a terminal or a language server.

use crate::highlight::{Language, LineHighlighter};
use crate::lsp::{HoverResult, LspError};
use crate::ui::text::highlight_color;
use lsp_types::{
    Diagnostic, DiagnosticSeverity, HoverContents, MarkedString, MarkupContent, MarkupKind,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::path::PathBuf;

/// What `Action::Hover` needs to ask a language server about, extracted
/// from whichever view has focus by [`crate::ui::view::View::hover_query`]
/// — the one place that knows how to build this from either `App` or
/// `FileView`'s state, so `ui::mod`'s event loop never matches on the view
/// type just to ask "what's under the cursor".
#[derive(Debug, PartialEq)]
pub struct HoverQuery {
    /// Absolute path to the file to open.
    pub file: PathBuf,
    /// Bounds how far up [`crate::lsp::adapter::workspace_root`] searches
    /// for a `Cargo.toml`.
    pub git_root: PathBuf,
    /// 0-based line number.
    pub line: u32,
    /// The exact text of that line, for converting `display_col` into the
    /// server's negotiated position encoding.
    pub line_text: String,
    /// 0-based display column of the symbol to hover, within `line_text`.
    pub display_col: usize,
}

#[derive(Default)]
enum Status {
    #[default]
    Idle,
    Pending,
    /// A status-bar-only outcome: no eligible target, no server, a hover
    /// error, or a server response with nothing to say. Never a popup —
    /// the milestone calls for "status-bar message, no popup" here.
    Message(String),
    Shown(RenderedHover),
}

struct RenderedHover {
    lines: Vec<Line<'static>>,
    scroll: usize,
}

/// Tracks one in-flight-or-resolved hover request. `ui::mod` owns exactly
/// one of these per view (recreated when the view changes), polling a
/// pending request's `Receiver` each event-loop tick and calling
/// [`Self::apply`] when it resolves.
#[derive(Default)]
pub struct HoverState {
    generation: u64,
    status: Status,
    /// Rendered lines for diagnostics overlapping the row a pending hover
    /// request was issued for, computed synchronously (before the request
    /// is even sent — see [`Self::set_diagnostics_prefix`]) and prepended
    /// once the request resolves, in [`Self::apply`]. Kept separate from
    /// `status` rather than folded into `Status::Pending` because it's set
    /// once per request and *read* by `apply` regardless of how that
    /// request resolves (content, nothing, or an error) — a `match` on
    /// `Status` isn't the right shape for "always available, consumed
    /// exactly once."
    diagnostics_prefix: Vec<Line<'static>>,
}

impl HoverState {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_open(&self) -> bool {
        matches!(self.status, Status::Shown(_))
    }

    pub fn close(&mut self) {
        self.status = Status::Idle;
    }

    /// Bumps the generation counter and drops whatever's currently shown or
    /// pending. Called whenever the cursor's hover target changes (see
    /// `ui::mod`'s `hover_cursor_key` comparison) — a response tagged with
    /// an older generation than this is stale and gets discarded on
    /// arrival by [`Self::apply`], and a popup left open from the old
    /// position would no longer describe what's under the cursor, so it
    /// closes here rather than lingering until the next explicit close.
    pub fn invalidate(&mut self) {
        self.generation += 1;
        self.status = Status::Idle;
        self.diagnostics_prefix.clear();
    }

    pub fn set_pending(&mut self) {
        self.status = Status::Pending;
    }

    /// Bumps the generation counter without touching whatever's currently
    /// shown or pending — a watch-mode refresh's way of discarding any
    /// hover/definition/references request issued before the refresh (its
    /// answer, whenever it arrives, will carry a now-stale generation and
    /// be dropped on arrival, the same check [`Self::apply`] already makes
    /// for a cursor move) without also closing a popup that's already
    /// *showing* content whose anchored row survived the refresh
    /// unchanged. Contrast with [`Self::invalidate`], which additionally
    /// resets to `Idle` — the right choice for a cursor move, since
    /// whatever was shown no longer describes what's under the cursor, but
    /// the wrong one here, since a surviving row's popup content is still
    /// exactly correct and closing it would just be disruptive churn on
    /// every refresh.
    pub fn bump_generation_for_refresh(&mut self) {
        self.generation += 1;
    }

    /// Records the "Diagnostics" section [`Self::apply`] should prepend to
    /// whatever hover content arrives — computed by `ui::mod` from
    /// whichever diagnostics overlap the row `Action::Hover` was issued on,
    /// *before* the (asynchronous) hover request is even sent, since the
    /// diagnostics themselves are already known synchronously. Empty when
    /// nothing overlaps, which is the common case and makes [`Self::apply`]
    /// behave exactly as it did before diagnostics existed.
    pub fn set_diagnostics_prefix(&mut self, diagnostics: &[&Diagnostic]) {
        self.diagnostics_prefix = diagnostics_section(diagnostics);
    }

    pub fn set_message(&mut self, message: impl Into<String>) {
        self.status = Status::Message(message.into());
    }

    /// Applies a hover response tagged with `generation` — silently
    /// ignored if that's not the current generation, i.e. the cursor moved
    /// (or another hover was requested) after this one was sent.
    pub fn apply(
        &mut self,
        generation: u64,
        result: Result<HoverResult, LspError>,
        highlighter: &mut LineHighlighter,
    ) {
        if generation != self.generation {
            return;
        }
        let mut lines = std::mem::take(&mut self.diagnostics_prefix);
        let has_diagnostics = !lines.is_empty();

        self.status = match result {
            Ok(Some(hover)) => {
                if has_diagnostics {
                    lines.push(Line::default());
                }
                lines.extend(render_hover_contents(&hover.contents, highlighter));
                Status::Shown(RenderedHover { lines, scroll: 0 })
            }
            // With nothing to show but diagnostics that overlap this
            // position, the diagnostics themselves are still worth a
            // popup — "no hover information" would otherwise silently
            // swallow them into a status-bar-only message no different
            // from the ordinary "nothing under the cursor" case.
            Ok(None) if has_diagnostics => Status::Shown(RenderedHover { lines, scroll: 0 }),
            Ok(None) => Status::Message("no hover information".to_owned()),
            Err(e) if has_diagnostics => {
                lines.push(Line::from(format!("(hover error: {e})")));
                Status::Shown(RenderedHover { lines, scroll: 0 })
            }
            Err(e) => Status::Message(e.to_string()),
        };
    }

    /// A short status-bar note for a pending or resolved-to-nothing hover.
    /// `None` while idle or while the popup itself is showing — the popup
    /// speaks for itself once it's open.
    pub fn status_hint(&self) -> Option<String> {
        match &self.status {
            Status::Pending => Some("hover: …".to_owned()),
            Status::Message(m) => Some(format!("hover: {m}")),
            Status::Idle | Status::Shown(_) => None,
        }
    }

    pub fn scroll_down(&mut self) {
        if let Status::Shown(rendered) = &mut self.status {
            let max = rendered.lines.len().saturating_sub(1);
            rendered.scroll = (rendered.scroll + 1).min(max);
        }
    }

    pub fn scroll_up(&mut self) {
        if let Status::Shown(rendered) = &mut self.status {
            rendered.scroll = rendered.scroll.saturating_sub(1);
        }
    }
}

/// Renders the popup if one is open, anchored near `cursor_screen_row` (the
/// cursor's row within `area`, 0-based). A no-op when nothing is showing —
/// pending/message states surface through [`HoverState::status_hint`] in
/// the status bar instead.
pub fn render(frame: &mut Frame, area: Rect, cursor_screen_row: u16, state: &HoverState) {
    if let Status::Shown(rendered) = &state.status {
        render_popup(frame, area, cursor_screen_row, rendered);
    }
}

fn render_popup(frame: &mut Frame, area: Rect, cursor_screen_row: u16, rendered: &RenderedHover) {
    let rect = popup_rect(area, cursor_screen_row);
    frame.render_widget(Clear, rect);

    let block = Block::default().borders(Borders::ALL).title(" hover ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let visible: Vec<Line> = rendered
        .lines
        .iter()
        .skip(rendered.scroll)
        .take(inner.height as usize)
        .cloned()
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

/// At most ~60% of `area`'s width and ~40% of its height, positioned just
/// below the cursor's row — or above it, when there isn't enough room
/// below.
fn popup_rect(area: Rect, cursor_screen_row: u16) -> Rect {
    let width = ((area.width as u32 * 3) / 5).clamp(20, area.width.max(20) as u32) as u16;
    let height = ((area.height as u32 * 2) / 5).clamp(3, area.height.max(3) as u32) as u16;
    let x = area.x + area.width.saturating_sub(width) / 2;

    let space_below = area
        .height
        .saturating_sub(cursor_screen_row.saturating_add(1));
    let y = if space_below >= height {
        area.y + cursor_screen_row + 1
    } else if cursor_screen_row >= height {
        area.y + cursor_screen_row - height
    } else {
        area.y
    };
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// A plain-text rendering of hover contents, with none of
/// [`render_hover_contents`]'s markdown-to-styled-spans handling — used by
/// the `ktmr lsp-check` CLI command, which prints to a terminal that isn't
/// running ratatui at all.
pub fn plain_text(contents: &HoverContents) -> String {
    match contents {
        HoverContents::Scalar(marked) => marked_string_plain_text(marked),
        HoverContents::Array(list) => list
            .iter()
            .map(marked_string_plain_text)
            .collect::<Vec<_>>()
            .join("\n---\n"),
        HoverContents::Markup(markup) => markup.value.clone(),
    }
}

/// Renders a "Diagnostics" section: a bold header followed by one line per
/// diagnostic (`[severity] message (source)`), in the order given. Empty
/// input renders as no lines at all, not an empty header — a caller with
/// nothing to show shouldn't have to check that itself.
fn diagnostics_section(diagnostics: &[&Diagnostic]) -> Vec<Line<'static>> {
    if diagnostics.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![Line::from(Span::styled(
        "Diagnostics",
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    for diagnostic in diagnostics {
        let (label, color) = severity_label(diagnostic.severity);
        let source = diagnostic
            .source
            .as_deref()
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!("[{label}] "), Style::default().fg(color)),
            Span::raw(format!("{}{source}", diagnostic.message)),
        ]));
    }
    lines
}

fn severity_label(severity: Option<DiagnosticSeverity>) -> (&'static str, ratatui::style::Color) {
    use ratatui::style::Color;
    match severity {
        Some(DiagnosticSeverity::ERROR) => ("error", Color::Red),
        Some(DiagnosticSeverity::WARNING) => ("warning", Color::Yellow),
        Some(DiagnosticSeverity::HINT) => ("hint", Color::Blue),
        _ => ("info", Color::Blue),
    }
}

fn marked_string_plain_text(marked: &MarkedString) -> String {
    match marked {
        MarkedString::String(s) => s.clone(),
        MarkedString::LanguageString(ls) => format!("```{}\n{}\n```", ls.language, ls.value),
    }
}

fn render_hover_contents(
    contents: &HoverContents,
    highlighter: &mut LineHighlighter,
) -> Vec<Line<'static>> {
    match contents {
        HoverContents::Scalar(marked) => render_marked_string(marked, highlighter),
        HoverContents::Array(list) => list
            .iter()
            .flat_map(|marked| render_marked_string(marked, highlighter))
            .collect(),
        HoverContents::Markup(markup) => render_markup(markup, highlighter),
    }
}

fn render_marked_string(
    marked: &MarkedString,
    highlighter: &mut LineHighlighter,
) -> Vec<Line<'static>> {
    match marked {
        MarkedString::String(text) => render_markdown(text, highlighter),
        MarkedString::LanguageString(ls) => ls
            .value
            .lines()
            .map(|line| highlighted_code_line(&ls.language, line, highlighter))
            .collect(),
    }
}

fn render_markup(markup: &MarkupContent, highlighter: &mut LineHighlighter) -> Vec<Line<'static>> {
    match markup.kind {
        MarkupKind::Markdown => render_markdown(&markup.value, highlighter),
        MarkupKind::PlainText => markup
            .value
            .lines()
            .map(|l| Line::from(l.to_owned()))
            .collect(),
    }
}

/// Minimal markdown rendering: recognizes fenced code blocks (routed
/// through [`LineHighlighter`] using the fence's language tag) and renders
/// headers/whole-line-bold as bold text. Everything else — links, inline
/// code spans, lists — passes through as plain text rather than being
/// stripped or misrendered; a hover popup that shows a stray `**` is a far
/// smaller problem than one that silently drops content.
fn render_markdown(text: &str, highlighter: &mut LineHighlighter) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut fence_language: Option<String> = None;

    for raw_line in text.lines() {
        if let Some(lang) = raw_line.trim_start().strip_prefix("```") {
            fence_language = if fence_language.is_some() {
                None
            } else {
                Some(lang.trim().to_owned())
            };
            continue;
        }
        if let Some(lang) = &fence_language {
            out.push(highlighted_code_line(lang, raw_line, highlighter));
        } else {
            out.push(markdown_plain_line(raw_line));
        }
    }
    out
}

fn highlighted_code_line(
    lang_hint: &str,
    line: &str,
    highlighter: &mut LineHighlighter,
) -> Line<'static> {
    let language = language_from_hint(lang_hint);
    let spans = highlighter.highlight_line(language, line);
    if spans.is_empty() {
        return Line::from(String::new());
    }
    Line::from(
        spans
            .into_iter()
            .map(|s| Span::styled(s.text, Style::default().fg(highlight_color(s.kind))))
            .collect::<Vec<_>>(),
    )
}

fn markdown_plain_line(line: &str) -> Line<'static> {
    let trimmed = line.trim_start();
    let is_header = trimmed.starts_with('#');
    let is_bold_wrapped = trimmed.len() > 4 && trimmed.starts_with("**") && trimmed.ends_with("**");

    let (display, bold) = if is_header {
        (
            trimmed.trim_start_matches('#').trim_start().to_owned(),
            true,
        )
    } else if is_bold_wrapped {
        (trimmed[2..trimmed.len() - 2].to_owned(), true)
    } else {
        (line.to_owned(), false)
    };

    let style = if bold {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Line::from(Span::styled(display, style))
}

/// Maps a markdown fence's language tag (`rust`, `ts`, `py`, ...) onto
/// this codebase's [`Language`] enum. Hover content uses prose names
/// ("rust") where [`Language::detect`] expects file extensions ("rs"), so
/// this is a small separate table rather than routing through `detect` via
/// a fake filename.
fn language_from_hint(hint: &str) -> Language {
    match hint.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Language::Rust,
        "typescript" | "ts" => Language::TypeScript,
        "tsx" => Language::Tsx,
        "python" | "py" => Language::Python,
        "go" | "golang" => Language::Go,
        "kotlin" | "kt" => Language::Kotlin,
        "java" => Language::Java,
        _ => Language::Plain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn renders_a_fenced_code_block_through_the_line_highlighter() {
        let mut hl = LineHighlighter::new();
        let markdown = "before\n```rust\nfn main() {}\n```\nafter";
        let lines = render_markdown(markdown, &mut hl);
        let text = plain_text(&lines);
        assert_eq!(text, vec!["before", "fn main() {}", "after"]);

        // The code line should carry real highlight spans, not one plain
        // span for the whole line.
        assert!(
            lines[1].spans.len() > 1,
            "expected multiple highlighted spans"
        );
    }

    #[test]
    fn header_and_whole_line_bold_markers_are_stripped_and_bolded() {
        let mut hl = LineHighlighter::new();
        let lines = render_markdown("## Title\n**bold line**\nplain", &mut hl);
        let text = plain_text(&lines);
        assert_eq!(text, vec!["Title", "bold line", "plain"]);
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            lines[1].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            !lines[2].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn hover_state_apply_ignores_a_response_for_a_stale_generation() {
        let mut hl = LineHighlighter::new();
        let mut state = HoverState::default();
        state.invalidate(); // generation 1
        let stale_generation = state.generation();
        state.invalidate(); // generation 2 — as if the cursor moved again
        state.set_pending();

        state.apply(stale_generation, Ok(None), &mut hl);
        assert!(
            state.status_hint().is_some_and(|h| h.contains('…')),
            "a stale response must not overwrite the pending state for the current generation"
        );
    }

    #[test]
    fn hover_state_apply_for_the_current_generation_shows_the_result() {
        let mut hl = LineHighlighter::new();
        let mut state = HoverState::default();
        state.invalidate();
        state.set_pending();
        let generation = state.generation();

        let hover = lsp_types::Hover {
            contents: HoverContents::Scalar(MarkedString::String("hello".to_owned())),
            range: None,
        };
        state.apply(generation, Ok(Some(hover)), &mut hl);
        assert!(state.is_open());
    }

    #[test]
    fn diagnostics_prefix_is_shown_even_when_hover_has_nothing() {
        let mut hl = LineHighlighter::new();
        let mut state = HoverState::default();
        state.invalidate();
        let generation = state.generation();
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range::default(),
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "mismatched types".to_owned(),
            ..Default::default()
        };
        state.set_diagnostics_prefix(&[&diagnostic]);
        state.set_pending();
        state.apply(generation, Ok(None), &mut hl);
        assert!(
            state.is_open(),
            "a diagnostic overlapping the hover position must show a popup even with no hover content"
        );
    }

    #[test]
    fn diagnostics_prefix_is_prepended_above_hover_content() {
        let mut hl = LineHighlighter::new();
        let mut state = HoverState::default();
        state.invalidate();
        let generation = state.generation();
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range::default(),
            severity: Some(lsp_types::DiagnosticSeverity::WARNING),
            message: "unused variable".to_owned(),
            ..Default::default()
        };
        state.set_diagnostics_prefix(&[&diagnostic]);
        state.set_pending();
        let hover = lsp_types::Hover {
            contents: HoverContents::Scalar(MarkedString::String("some type info".to_owned())),
            range: None,
        };
        state.apply(generation, Ok(Some(hover)), &mut hl);
        assert!(state.is_open());
    }

    #[test]
    fn bump_generation_for_refresh_discards_a_stale_pending_response_but_keeps_shown_content() {
        let mut hl = LineHighlighter::new();
        let mut state = HoverState::default();
        state.invalidate();
        let generation = state.generation();
        let hover = lsp_types::Hover {
            contents: HoverContents::Scalar(MarkedString::String("hello".to_owned())),
            range: None,
        };
        state.apply(generation, Ok(Some(hover)), &mut hl);
        assert!(state.is_open());

        // A refresh happens while this content is showing; unlike
        // `invalidate`, it must not close the popup.
        state.bump_generation_for_refresh();
        assert!(
            state.is_open(),
            "bump_generation_for_refresh must not close an already-shown popup"
        );

        // But a response for the pre-refresh generation must still be
        // dropped as stale.
        state.apply(generation, Ok(None), &mut hl);
        assert!(
            state.is_open(),
            "a response tagged with the pre-refresh generation must not overwrite the current content"
        );
    }

    #[test]
    fn hover_state_apply_with_no_result_sets_a_status_message_not_a_popup() {
        let mut hl = LineHighlighter::new();
        let mut state = HoverState::default();
        state.invalidate();
        let generation = state.generation();
        state.apply(generation, Ok(None), &mut hl);
        assert!(!state.is_open());
        assert_eq!(
            state.status_hint(),
            Some("hover: no hover information".to_owned())
        );
    }
}
