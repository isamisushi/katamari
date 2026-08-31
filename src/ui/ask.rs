//! `Action::AskAgent` — a one-shot question about the cursor's row or a
//! visual selection, sent to the resident ACP agent (see
//! [`crate::acp::session`]). Two things live here, the same split
//! [`crate::ui::compose`] draws between its own pure buffer/derivation and
//! `ui::mod`'s overlay lifecycle:
//!
//! - [`build_ask_context`]: pure derivation from an already-resolved
//!   selection to the prompt's context block, or why it can't build one.
//! - [`AskComposeState`]/[`render`]: the overlay's own state and rendering,
//!   sharing [`crate::ui::compose::ComposeBuffer`]/[`crate::ui::compose::handle_key`]/
//!   [`crate::ui::compose::render_editor`] wholesale rather than
//!   reimplementing a second text box — see [`AskComposeState`]'s own docs
//!   for why this is a sibling type, not a generalized `ComposeState`.
//!
//! `AskAgent`'s eligibility is deliberately looser than `Action::AddComment`'s
//! own [`crate::ui::app::App::comment_target`]: a saved comment has to
//! survive being re-anchored into live file content later (see that
//! method's own docs on why it refuses a historical diff, a deletion, a
//! multi-file or discontinuous range), but a one-shot question has none of
//! that concern — "explain this deleted line" or "what changed in this PR
//! hunk" are exactly the questions a reviewer would want to ask on content
//! `comment_target` refuses. So this mirrors
//! [`Action::YankSelection`](crate::keymap::Action::YankSelection)'s looser
//! rule instead: any selection (or bare cursor row) that resolves to at
//! least one [`crate::diff::RenderRow::Line`], on any diff including a
//! read-only one — [`crate::ui::app::App::toggle_visual`]'s own docs
//! already establish that stance for selection itself.

use crate::diff::{DiffFile, RenderRow};
use crate::ui::clipboard::{self, YankError};
use crate::ui::compose::{ComposeBuffer, ComposeKeymap, render_editor};
use ratatui::Frame;
use ratatui::layout::Rect;

/// Cap on the assembled context block's size. Deliberately *not*
/// [`clipboard::OSC52_MAX_BYTES`]: that bound exists because an OSC 52
/// payload travels inside a terminal escape sequence, a constraint that has
/// nothing to do with what's reasonable to hand an agent as prompt context.
/// Set a few times more generous instead (256 KiB) — the exact number is a
/// judgment call worth revisiting once real usage patterns exist, not
/// treated as settled here.
pub const ASK_CONTEXT_MAX_BYTES: usize = 256 * 1024;

/// What [`build_ask_context`] hands back: a short title for the overlay's
/// border (and, doubled, for the prompt's own framing text) plus the
/// already-formatted diff block — see [`crate::ui::clipboard::format_diff_selection`]'s
/// documented `path\nold:new | line\n<marker><text>\n...` shape, which
/// already names the file, so nothing here repeats the path/line-range
/// redundantly above it.
#[derive(Debug)]
pub struct AskContext {
    pub title: String,
    pub diff_block: String,
}

/// Why [`build_ask_context`] couldn't build one — mirrors
/// [`clipboard::YankError`] 1:1 (this function is a thin wrapper over
/// [`clipboard::resolve_selection`]/[`clipboard::format_diff_selection`]),
/// kept as its own type rather than reusing `YankError` directly so a
/// caller reading `ui::ask`'s API isn't left wondering whether an
/// OSC-52-flavored error variant applies here too.
#[derive(Debug)]
pub enum AskContextError {
    Empty,
    TooLarge { byte_count: usize },
}

/// Resolves `selected` (or, for a bare cursor row, a one-element slice) back
/// to real diff content and formats it into a question's context block.
/// `title` is derived from the first resolved line's path plus the min/max
/// of whichever of `old_line`/`new_line` each resolved line carries —
/// [`crate::comments::location_label`]'s own formatting, for the same
/// `path:line`/`path:start-end` look the compose overlay's own title has,
/// without actually depending on [`crate::ui::app::CommentTarget`] (this
/// selection may include deletions/discontinuities a `CommentTarget` could
/// never represent).
pub fn build_ask_context(
    rows: &[RenderRow],
    files: &[DiffFile],
    selected: &[usize],
) -> Result<AskContext, AskContextError> {
    let lines = clipboard::resolve_selection(rows, files, selected);
    let Some(first) = lines.first() else {
        return Err(AskContextError::Empty);
    };
    let path = first.path.to_owned();
    let numbers: Vec<u32> = lines
        .iter()
        .filter_map(|line| line.new_line.or(line.old_line))
        .collect();
    let title = match (numbers.iter().min(), numbers.iter().max()) {
        (Some(&start), Some(&end)) => crate::comments::location_label(&path, start, end),
        _ => path,
    };

    match clipboard::format_diff_selection(lines, ASK_CONTEXT_MAX_BYTES) {
        Ok(formatted) => Ok(AskContext {
            title,
            diff_block: formatted.text,
        }),
        Err(YankError::Empty) => Err(AskContextError::Empty),
        Err(YankError::TooLarge { byte_count }) => Err(AskContextError::TooLarge { byte_count }),
    }
}

/// The exact prompt sent to the agent — assembled by `ui::mod`'s
/// `Save` handler once the reviewer confirms their question, from
/// [`AskContext::diff_block`] (already self-describing — see this module's
/// own docs) and the compose buffer's text. One blank line separates each
/// part; no separate path/line-range line is added since the diff block
/// already carries one.
pub fn build_prompt(context: &AskContext, question: &str) -> String {
    format!(
        "Reviewing this diff selection:\n\n{}\n\n{question}",
        context.diff_block
    )
}

/// State for one open "ask the agent" overlay — a sibling of
/// [`crate::ui::compose::ComposeState`], not a generalization of it:
/// `ComposeState` carries a [`crate::ui::app::CommentTarget`] this overlay
/// has no use for (see the module docs on why eligibility is looser here),
/// and unlike compose, saving this one doesn't touch
/// [`crate::comments::CommentStore`] at all — it sends a prompt instead.
/// Reuses [`ComposeBuffer`] and (via [`render`]) [`render_editor`]
/// wholesale, so the actual typing/wrap/scroll-follow behavior is
/// byte-for-byte identical to the comment overlay's.
pub struct AskComposeState {
    context: AskContext,
    buffer: ComposeBuffer,
    scroll_offset: usize,
}

impl AskComposeState {
    pub fn new(context: AskContext) -> Self {
        Self {
            context,
            buffer: ComposeBuffer::new(),
            scroll_offset: 0,
        }
    }

    pub fn buffer_mut(&mut self) -> &mut ComposeBuffer {
        &mut self.buffer
    }

    pub fn buffer(&self) -> &ComposeBuffer {
        &self.buffer
    }

    pub fn context(&self) -> &AskContext {
        &self.context
    }
}

/// Draws the overlay via the shared [`render_editor`] body, with a
/// `" ask: <location> "` title in place of the comment overlay's own
/// `" comment: <location> "` — see [`crate::ui::compose::render`]'s docs
/// for why the title is the one thing that differs between the two.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    cursor_screen_row: u16,
    state: &mut AskComposeState,
    keys: &ComposeKeymap,
) {
    let title = format!(" ask: {} ", state.context.title);
    render_editor(
        frame,
        area,
        cursor_screen_row,
        &mut state.buffer,
        &mut state.scroll_offset,
        keys,
        &title,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffFile, DiffHunk, DiffLineKind, DiffRow, flatten};

    fn diff_row(kind: DiffLineKind, text: &str, old: Option<u32>, new: Option<u32>) -> DiffRow {
        DiffRow {
            kind,
            text: text.to_owned(),
            old_line: old,
            new_line: new,
        }
    }

    fn file(path: &str, rows: Vec<DiffRow>) -> DiffFile {
        DiffFile {
            new_path: Some(path.to_owned()),
            hunks: vec![DiffHunk {
                rows,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn build_ask_context_on_an_empty_selection_reports_empty() {
        let files = vec![file(
            "a.rs",
            vec![diff_row(DiffLineKind::Context, "x", Some(1), Some(1))],
        )];
        let rows = flatten(&files);
        let result = build_ask_context(&rows, &files, &[]);
        assert!(matches!(result, Err(AskContextError::Empty)));
    }

    #[test]
    fn build_ask_context_succeeds_across_a_deleted_line_unlike_comment_target() {
        // The whole point of issue-#19-style eligibility being too strict
        // for `AskAgent`: a selection spanning a `Del` row must still build
        // a context, where `App::comment_target` would refuse it outright.
        let files = vec![file(
            "a.rs",
            vec![
                diff_row(DiffLineKind::Context, "before", Some(1), Some(1)),
                diff_row(DiffLineKind::Del, "removed", Some(2), None),
                diff_row(DiffLineKind::Add, "added", None, Some(2)),
            ],
        )];
        let rows = flatten(&files);
        let selected: Vec<usize> = (0..rows.len())
            .filter(|&i| matches!(rows[i], RenderRow::Line { .. }))
            .collect();
        let ctx = build_ask_context(&rows, &files, &selected)
            .unwrap_or_else(|_| panic!("a selection spanning a deletion must still build"));
        assert!(ctx.diff_block.contains("-removed"));
        assert!(ctx.diff_block.contains("+added"));
    }

    #[test]
    fn build_ask_context_title_spans_the_selections_line_range() {
        let files = vec![file(
            "src/lib.rs",
            vec![
                diff_row(DiffLineKind::Context, "one", Some(10), Some(10)),
                diff_row(DiffLineKind::Context, "two", Some(11), Some(11)),
            ],
        )];
        let rows = flatten(&files);
        let selected: Vec<usize> = (0..rows.len())
            .filter(|&i| matches!(rows[i], RenderRow::Line { .. }))
            .collect();
        let ctx = build_ask_context(&rows, &files, &selected).unwrap();
        assert_eq!(
            ctx.title,
            crate::comments::location_label("src/lib.rs", 10, 11)
        );
    }

    #[test]
    fn build_ask_context_rejects_an_oversized_selection() {
        let long_text = "x".repeat(ASK_CONTEXT_MAX_BYTES + 10);
        let files = vec![file(
            "big.rs",
            vec![diff_row(DiffLineKind::Add, &long_text, None, Some(1))],
        )];
        let rows = flatten(&files);
        let selected: Vec<usize> = (0..rows.len())
            .filter(|&i| matches!(rows[i], RenderRow::Line { .. }))
            .collect();
        match build_ask_context(&rows, &files, &selected) {
            Err(AskContextError::TooLarge { byte_count }) => {
                assert!(byte_count > ASK_CONTEXT_MAX_BYTES);
            }
            other => panic!(
                "expected TooLarge, got a {}",
                if other.is_ok() {
                    "context"
                } else {
                    "different error"
                }
            ),
        }
    }

    #[test]
    fn build_prompt_separates_the_diff_block_and_question_with_blank_lines() {
        let ctx = AskContext {
            title: "a.rs:1".to_owned(),
            diff_block: "a.rs\nold:new | line\n1:1 |  hi".to_owned(),
        };
        let prompt = build_prompt(&ctx, "what does this do?");
        assert_eq!(
            prompt,
            "Reviewing this diff selection:\n\na.rs\nold:new | line\n1:1 |  hi\n\nwhat does this do?"
        );
    }
}
