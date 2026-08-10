//! Review comments: a reviewer's file/line-anchored notes on a diff, handed
//! off to an AI coding agent and addressed asynchronously — no daemon, no
//! socket, just an append-only JSONL file under `.katamari/` that both the
//! TUI (via [`store::CommentStore`] plus a live watch — see
//! [`crate::watch::spawn_comments_watcher`]) and the `ktmr comments` CLI
//! subcommands read and write. [`store`] owns the file format and its
//! append/fold semantics; [`index`] owns turning a loaded comment list into
//! something a diff render pass can look up by `(file, line)`; this module
//! owns the shared data model plus the one genuinely tricky piece of logic
//! both depend on: [`relocate_range`], which re-finds a comment's anchor(s)
//! after the file it was left on has changed underneath it.
//!
//! # Anchoring and drift
//!
//! A comment is anchored to a specific line of a specific file at the moment
//! it's written — [`Anchor::new_line`], 1-based, matching the diff's own
//! working-tree line numbers. That anchor inevitably drifts: the agent edits
//! the file, lines shift, the exact line the comment was about might move,
//! change, or disappear entirely. [`Anchor::content_hash`] (a hash of the
//! anchored line's own text) and [`Anchor::context_hash`] (a hash of the
//! ~5-line neighborhood around it) exist so relocation can tell those cases
//! apart without storing the full line text redundantly in every comment
//! record.
//!
//! Both hashes use [`std::collections::hash_map::DefaultHasher`] — the same
//! non-cryptographic, session-local hasher `ui::refresh` already uses for
//! anchor preservation across a watch refresh (see that module's
//! `hash_text`). It is *not* a security boundary and its exact output is not
//! guaranteed stable across Rust compiler versions (see
//! [`std::hash::Hasher`]'s docs) — acceptable here because a hash mismatch
//! only ever costs a comment its automatic relocation (falling back to
//! "detached"), never data loss or a wrong answer trusted as authoritative.

pub mod index;
pub mod store;

pub use index::{CommentAnnotation, CommentIndex, build_index};
pub use store::CommentStore;

use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Where a comment is anchored within its file, and enough of a fingerprint
/// of that position for [`relocate_range`] to re-find it after an edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// 1-based line number in the working-tree file, at the moment the
    /// comment was written.
    pub new_line: u32,
    /// Hash of the anchored line's own text.
    pub content_hash: u64,
    /// Hash of the up-to-five-line neighborhood (two lines of context on
    /// each side, clamped at file boundaries) around the anchored line,
    /// joined with `\n`. Used only to disambiguate when more than one
    /// remaining line matches `content_hash` — see [`relocate_range`].
    pub context_hash: u64,
}

/// A comment's resolution state. Serializes as the lowercase strings the
/// milestone spec and the CLI's `--status` filter both use verbatim
/// (`"open"`/`"resolved"`), so a hand-written amendment record or a script
/// piping `ktmr comments list --json` through `jq` never has to know this is
/// a Rust enum underneath.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Open,
    Resolved,
}

/// One review comment, as written by [`store::CommentStore::append_comment`]
/// (the JSONL record's full shape — see that method's docs on why status
/// *changes* are a different, smaller record shape instead of a rewrite of
/// this one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub id: String,
    /// Unix seconds.
    pub created_at: u64,
    /// Repo-relative path, exactly as it appears in the diff (e.g.
    /// `src/lib.rs`) — not absolute, since a comment file is meant to be
    /// portable across whoever's working tree reads it (the reviewer's and
    /// the agent's are the same checkout in practice, but nothing about the
    /// format should assume that).
    pub file: String,
    pub anchor: Anchor,
    /// `None` for a single-line comment (every pre-#18 record, and the
    /// default for a fresh one); `Some` makes `anchor` the inclusive
    /// *start* of a same-file range and this its inclusive end. This type
    /// doesn't itself enforce `end.new_line >= anchor.new_line` — `ktmr
    /// comments add`'s `--end-line` validation is what rejects that before
    /// a `Comment` is ever constructed, matching `Anchor`'s existing
    /// "trust the caller, verify by relocating" philosophy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_anchor: Option<Anchor>,
    pub body: String,
    pub status: Status,
    /// Unix seconds; set exactly when `status` is [`Status::Resolved`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<u64>,
}

/// Hashes one piece of text with the module's shared (non-cryptographic,
/// version-unstable — see the module docs) hasher. The single hashing
/// entry point every anchor/relocation computation goes through, so
/// `Anchor::content_hash`/`context_hash` and relocation's own re-hashing of
/// the current file can never silently drift apart onto two different
/// algorithms.
pub fn hash_line(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// The `context_hash` neighborhood for the line at `idx0` (0-based) in
/// `lines`: up to two lines before and after, clamped at the file's edges,
/// joined with `\n`. Shared by [`anchor_for`] (computing a fresh anchor) and
/// [`relocate_anchor`] (recomputing the same window against the current
/// file, to compare) — both must build this identically or `context_hash`
/// could never match even for a candidate line that's genuinely the right
/// one.
fn context_window(lines: &[&str], idx0: usize) -> String {
    let start = idx0.saturating_sub(2);
    let end = (idx0 + 2).min(lines.len().saturating_sub(1));
    lines[start..=end].join("\n")
}

/// Builds the [`Anchor`] for `line1` (1-based) against `lines` — the file
/// content at comment-creation time. `None` when `line1` is `0` or past the
/// end of `lines`, the two ways a caller (the TUI compose overlay, or the
/// `ktmr comments add` CLI command) could hand this a line that doesn't
/// exist to anchor to.
pub fn anchor_for(lines: &[&str], line1: u32) -> Option<Anchor> {
    let idx0 = (line1.checked_sub(1)?) as usize;
    let text = *lines.get(idx0)?;
    Some(Anchor {
        new_line: line1,
        content_hash: hash_line(text),
        context_hash: hash_line(&context_window(lines, idx0)),
    })
}

/// Re-finds one [`Anchor`] against `current_lines` (the file's present
/// working-tree content), tolerating drift in the order the milestone spec
/// calls for:
///
/// 1. If the line still at `anchor.new_line` still hash-matches, nothing
///    moved — keep it.
/// 2. Otherwise, scan the whole file for lines whose content hash matches
///    and pick the one nearest the original line number, breaking ties
///    (multiple equally-near candidates — e.g. a duplicated line like a
///    lone `}` or a repeated `return None;`) by preferring whichever
///    candidate's *context* also matches, since content alone can't tell
///    identical-looking lines apart.
/// 3. If nothing matches at all, the anchored line is gone — `None`. The
///    caller renders this as "detached" at the original line number with a
///    dimmed marker, per the milestone spec, rather than losing the comment.
///
/// The single-anchor core [`relocate_range`] runs once per endpoint (twice
/// for a range, once for a single-line comment treated as a one-line range)
/// — a range's two endpoints can drift by different amounts or in different
/// directions, so there is no shortcut that relocates both at once. Also the
/// core of the `#[cfg(test)]`-only [`relocate`], kept for the pre-#18
/// single-anchor tests.
fn relocate_anchor(anchor: &Anchor, current_lines: &[&str]) -> Option<u32> {
    let original = anchor.new_line;
    if original >= 1
        && let Some(&text) = current_lines.get((original - 1) as usize)
        && hash_line(text) == anchor.content_hash
    {
        return Some(original);
    }

    let mut candidates: Vec<(u32, bool, i64)> = current_lines
        .iter()
        .enumerate()
        .filter(|&(_, &text)| hash_line(text) == anchor.content_hash)
        .map(|(idx0, _)| {
            let line1 = (idx0 + 1) as u32;
            let context_matches =
                hash_line(&context_window(current_lines, idx0)) == anchor.context_hash;
            let distance = (i64::from(line1) - i64::from(original)).abs();
            (line1, context_matches, distance)
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // Nearest first; among equally-near candidates, a context match sorts
    // before a non-match (`!context_matches`: `false` < `true`); any
    // remaining tie breaks toward the earlier line for determinism.
    candidates
        .sort_by_key(|&(line1, context_matches, distance)| (distance, !context_matches, line1));
    Some(candidates[0].0)
}

/// Re-finds `comment`'s anchor against `current_lines` — the single-line
/// case, delegating to [`relocate_anchor`]. Every production call site now
/// goes through [`relocate_range`] instead (it needs the `detached` flag and
/// handles a single-line comment as a one-line range already), which would
/// make this dead code in a non-test build — `#[cfg(test)]`'d rather than
/// deleted so the pre-#18 single-anchor relocation tests (here and in
/// `cjk_regression`) keep exercising exactly the function they always did,
/// rather than being rewritten to unwrap a `RelocatedRange` for no
/// behavioral reason.
#[cfg(test)]
pub fn relocate(comment: &Comment, current_lines: &[&str]) -> Option<u32> {
    relocate_anchor(&comment.anchor, current_lines)
}

/// The result of relocating a comment's full extent — one line for a
/// single-line comment, both endpoints for a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelocatedRange {
    pub start: u32,
    pub end: u32,
    /// `true` when the range (or single line) couldn't be placed on the
    /// current file — either endpoint's anchored line is gone, or the two
    /// endpoints relocated but landed inverted (`start` after `end`). When
    /// `true`, `start`/`end` are the comment's *original* stored anchor
    /// lines, unchanged — never reordered or shrunk to make them look
    /// plausible, since a misleading placement is worse than an honest
    /// "couldn't find it."
    pub detached: bool,
}

/// Relocates `comment`'s full extent against `current_lines`: both endpoints
/// independently through [`relocate_anchor`] for a range, or the one anchor
/// treated as a one-line range for a single-line comment (`end_anchor ==
/// None`). Attaches only when *both* endpoints relocate and the relocated
/// start doesn't land after the relocated end — a comment whose start moved
/// below its end (e.g. the start line was duplicated elsewhere and matched a
/// candidate past the original end) is exactly the "misleading position"
/// case [`RelocatedRange::detached`] exists to refuse rather than paper over
/// by silently swapping or clamping the pair.
pub fn relocate_range(comment: &Comment, current_lines: &[&str]) -> RelocatedRange {
    let original_start = comment.anchor.new_line;
    let original_end = comment.end_anchor.map_or(original_start, |a| a.new_line);

    let start = relocate_anchor(&comment.anchor, current_lines);
    let end = match comment.end_anchor {
        None => start,
        Some(end_anchor) => relocate_anchor(&end_anchor, current_lines),
    };

    match (start, end) {
        (Some(s), Some(e)) if s <= e => RelocatedRange {
            start: s,
            end: e,
            detached: false,
        },
        _ => RelocatedRange {
            start: original_start,
            end: original_end,
            detached: true,
        },
    }
}

/// A short random-looking hex id for a new comment: not cryptographically
/// random (no new dependency is worth pulling in for an id whose only job is
/// "don't collide with the other comments in this repo"), but seeded from a
/// per-process counter, the OS pid, and the current time in nanoseconds —
/// three sources that only coincide across two calls if the same process
/// generated two ids at the exact same nanosecond, which the counter alone
/// already rules out.
pub fn generate_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = u64::from(std::process::id());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (counter, pid, nanos).hash(&mut hasher);
    format!("{:016x}", hasher.finish())[..8].to_owned()
}

/// Unix seconds for `created_at`/`resolved_at` fields — the one place that
/// clock read happens, so every caller agrees on both the source and the
/// unit.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment_at(line: u32, lines: &[&str]) -> Comment {
        Comment {
            id: "abc123".to_owned(),
            created_at: 0,
            file: "src/lib.rs".to_owned(),
            anchor: anchor_for(lines, line).expect("line exists"),
            end_anchor: None,
            body: "fix this".to_owned(),
            status: Status::Open,
            resolved_at: None,
        }
    }

    /// As [`comment_at`], but a range from `start` to `end` (inclusive),
    /// both anchored against `lines` at comment-creation time.
    fn comment_range(start: u32, end: u32, lines: &[&str]) -> Comment {
        Comment {
            end_anchor: Some(anchor_for(lines, end).expect("end line exists")),
            ..comment_at(start, lines)
        }
    }

    #[test]
    fn relocate_keeps_an_unchanged_line_in_place() {
        let lines = ["one", "two", "three"];
        let comment = comment_at(2, &lines);
        assert_eq!(relocate(&comment, &lines), Some(2));
    }

    #[test]
    fn relocate_follows_a_line_moved_down_by_an_insertion_above_it() {
        let before = ["one", "two", "three"];
        let comment = comment_at(2, &before); // anchored to "two"

        let after = ["zero", "one", "two", "three"];
        assert_eq!(
            relocate(&comment, &after),
            Some(3),
            "\"two\" is still unique content, now three lines down"
        );
    }

    #[test]
    fn relocate_returns_none_when_the_anchored_line_is_gone() {
        let before = ["one", "two", "three"];
        let comment = comment_at(2, &before);

        let after = ["one", "three"]; // "two" deleted outright
        assert_eq!(relocate(&comment, &after), None);
    }

    /// Two occurrences of the anchored line's exact text end up equally far
    /// (three lines) from the original anchor after an edit — a
    /// nearest-distance-only search can't break that tie. Only one
    /// occurrence's surrounding neighborhood still matches the captured
    /// context, and `relocate` must prefer it.
    #[test]
    fn relocate_prefers_the_context_matching_candidate_among_equidistant_matches() {
        let before = [
            "b0", "b1", "CTXA", "CTXB", "TARGET", "CTXC", "CTXD", "b7", "b8",
        ];
        // Anchored to "TARGET" at line 5 (idx0=4); its unclamped context
        // window (idx0-2..=idx0+2) is lines 3..=7: "CTXA CTXB TARGET CTXC
        // CTXD".
        let comment = comment_at(5, &before);

        let current = [
            "x0", "TARGET", // line 2 (idx0=1): decoy, distance |2-5| = 3
            "x2", "x3", "x4", "CTXA", "CTXB", "TARGET", // line 8 (idx0=7): real match
            "CTXC", "CTXD",
        ];
        // The decoy's own neighborhood ("x0 TARGET x2 x3") doesn't
        // reproduce the original context; line 8's does exactly (its
        // idx0=7 window is lines[5..=9] = "CTXA CTXB TARGET CTXC CTXD").
        // Both are distance 3 from the original anchor (line 5) — a
        // genuine tie broken only by context.
        assert_eq!(
            relocate(&comment, &current),
            Some(8),
            "the context-matching occurrence must win an equidistant tie over the decoy"
        );
    }

    /// A pre-#18 creation record — no `end_anchor` key at all — must still
    /// deserialize, and reserializing it must not introduce the key: the
    /// `skip_serializing_if = "Option::is_none"` contract this whole
    /// backward-compatibility story depends on.
    #[test]
    fn comment_without_end_anchor_deserializes_and_omits_it_on_reserialize() {
        let json = r#"{"id":"abc123","created_at":0,"file":"src/lib.rs",
            "anchor":{"new_line":2,"content_hash":1,"context_hash":2},
            "body":"fix this","status":"open"}"#;
        let comment: Comment = serde_json::from_str(json).expect("legacy record must parse");
        assert_eq!(comment.end_anchor, None);

        let reserialized = serde_json::to_string(&comment).unwrap();
        assert!(
            !reserialized.contains("end_anchor"),
            "a single-line comment must not gain an end_anchor key on reserialize: {reserialized}"
        );
    }

    #[test]
    fn comment_with_end_anchor_round_trips_through_serde() {
        let lines = ["one", "two", "three"];
        let comment = comment_range(1, 3, &lines);

        let json = serde_json::to_string(&comment).unwrap();
        assert!(json.contains("end_anchor"));
        let round_tripped: Comment = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, comment);
    }

    #[test]
    fn relocate_range_keeps_both_endpoints_when_neither_moved() {
        let lines = ["one", "two", "three", "four"];
        let comment = comment_range(2, 3, &lines);

        let relocated = relocate_range(&comment, &lines);
        assert_eq!(relocated.start, 2);
        assert_eq!(relocated.end, 3);
        assert!(!relocated.detached);
    }

    #[test]
    fn relocate_range_follows_both_endpoints_after_an_insertion_above_the_range() {
        let before = ["one", "two", "three", "four"];
        let comment = comment_range(2, 3, &before); // "two"..="three"

        let after = ["zero", "one", "two", "three", "four"];
        let relocated = relocate_range(&comment, &after);
        assert_eq!(relocated.start, 3);
        assert_eq!(relocated.end, 4);
        assert!(!relocated.detached);
    }

    #[test]
    fn relocate_range_detaches_when_the_start_anchor_is_deleted() {
        let before = ["one", "two", "three", "four"];
        let comment = comment_range(2, 3, &before); // "two"..="three"

        let after = ["one", "three", "four"]; // "two" deleted
        let relocated = relocate_range(&comment, &after);
        assert!(relocated.detached);
        assert_eq!(relocated.start, 2, "original start, not reordered");
        assert_eq!(relocated.end, 3, "original end, not reordered");
    }

    #[test]
    fn relocate_range_detaches_when_the_end_anchor_is_deleted() {
        let before = ["one", "two", "three", "four"];
        let comment = comment_range(2, 3, &before); // "two"..="three"

        let after = ["one", "two", "four"]; // "three" deleted
        let relocated = relocate_range(&comment, &after);
        assert!(relocated.detached);
        assert_eq!(relocated.start, 2);
        assert_eq!(relocated.end, 3);
    }

    /// Both endpoints still find *a* home somewhere in the current file, but
    /// content shuffling landed the relocated start after the relocated end
    /// — an inverted, misleading pair that must detach rather than attach
    /// with `start > end`.
    #[test]
    fn relocate_range_detaches_when_relocated_endpoints_invert() {
        let before = ["a", "START", "mid", "END", "b"];
        let comment = comment_range(2, 4, &before); // "START"..="END"

        // "END" now precedes "START" in the current file.
        let after = ["a", "END", "mid", "START", "b"];
        let relocated = relocate_range(&comment, &after);
        assert!(relocated.detached);
        assert_eq!(relocated.start, 2, "original start preserved");
        assert_eq!(relocated.end, 4, "original end preserved");
    }

    /// The single-anchor context tie-break
    /// (`relocate_prefers_the_context_matching_candidate_among_equidistant_matches`)
    /// must stay reachable through the range path too — a one-line range is
    /// the shape every pre-#18 comment takes through `relocate_range`.
    #[test]
    fn relocate_range_applies_the_context_tiebreak_through_the_range_path() {
        let before = [
            "b0", "b1", "CTXA", "CTXB", "TARGET", "CTXC", "CTXD", "b7", "b8",
        ];
        let comment = comment_range(5, 5, &before); // one-line range on "TARGET"

        let current = [
            "x0", "TARGET", // decoy, distance 3
            "x2", "x3", "x4", "CTXA", "CTXB", "TARGET", // real match, distance 3
            "CTXC", "CTXD",
        ];
        let relocated = relocate_range(&comment, &current);
        assert_eq!(relocated.start, 8);
        assert_eq!(relocated.end, 8);
        assert!(!relocated.detached);
    }

    /// The two endpoints drift by *different* amounts (+1 vs +3), so a
    /// shortcut that relocates only the start and rederives the end from
    /// the stored offset (`end = relocated_start + (original_end -
    /// original_start)`, here 3 + 2 = 5, the "z" line) fails loudly —
    /// each anchor must be re-found from its own content/context hashes.
    #[test]
    fn relocate_range_relocates_each_endpoint_by_its_own_drift() {
        let before = ["a", "START", "mid", "END", "b"];
        let comment = comment_range(2, 4, &before); // "START"..="END"

        let current = ["a", "x", "START", "mid", "y", "z", "END", "b"];
        let relocated = relocate_range(&comment, &current);
        assert_eq!(relocated.start, 3, "START moved down by one line");
        assert_eq!(relocated.end, 7, "END moved down by three lines");
        assert!(!relocated.detached);
    }

    #[test]
    fn relocate_range_treats_a_single_line_comment_as_a_one_line_range() {
        let lines = ["one", "two", "three"];
        let comment = comment_at(2, &lines); // end_anchor: None

        let relocated = relocate_range(&comment, &lines);
        assert_eq!(relocated.start, 2);
        assert_eq!(relocated.end, 2);
        assert!(!relocated.detached);
    }

    #[test]
    fn generated_ids_are_unique_across_many_calls() {
        let mut ids = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(ids.insert(generate_id()), "id collision");
        }
    }

    #[test]
    fn anchor_for_is_none_past_the_end_of_the_file() {
        let lines = ["one"];
        assert_eq!(anchor_for(&lines, 0), None);
        assert_eq!(anchor_for(&lines, 2), None);
        assert!(anchor_for(&lines, 1).is_some());
    }
}
