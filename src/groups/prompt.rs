//! Builds the grouping prompt sent to the agent CLI, and parses its reply.
//!
//! Two properties drive the prompt's shape, both taken from what worked in
//! the commit-untangling literature:
//!
//! - **The model classifies; it never restates.** Hunks are presented with
//!   short ids and the reply is nothing but ids arranged into labeled
//!   units, so there is no opportunity to hallucinate line numbers or code
//!   — the worst a bad reply can do is claim a nonexistent id, which
//!   [`super::normalize`] silently drops.
//! - **Intent before grouping.** The instructions walk the model through a
//!   what/how/why reading of each hunk before it commits to units; intent
//!   tagging measurably improves grouping over "cluster these" phrasing.
//!
//! Excerpts are capped in tiers rather than truncated mid-stream so the
//! prompt stays deterministic for a given diff — the cache key assumes
//! identical input produces an identical request.

use super::HunkMeta;
use super::Unit;
use crate::diff::{DiffFile, DiffLineKind};
use serde::Deserialize;

/// Changed-line caps tried in order until the assembled prompt fits
/// [`PROMPT_BYTE_BUDGET`]. The budget exists because an agent-authored
/// diff can be arbitrarily large while agent CLIs (and their context
/// windows) are not; past the last tier the prompt ships oversized anyway
/// and the CLI's own limits decide — better a degraded attempt than a
/// hard refusal katamari would have to invent UI for.
const EXCERPT_TIERS: [usize; 3] = [30, 8, 3];
const PROMPT_BYTE_BUDGET: usize = 200_000;

const INSTRUCTIONS: &str = "\
You are grouping a code diff into reviewable units, like a well-factored stack of small PRs.

Below is an inventory of diff hunks. Each has an id, its file, and an excerpt of its changed lines (+ added, - removed).

First, silently work out for each hunk: WHAT it changes, HOW that affects behavior, WHY the author likely did it (bug fix / feature / refactor / tests / docs / config). Then group hunks that serve the same concern into units, and order the units so a reviewer can read them bottom-up: foundations and refactors first, the features built on them next, tests and docs with (or after) the code they cover. Prefer a handful of coherent units over many fragments; do not split one concern across units merely because it spans files.

Reply with ONLY a JSON object, no prose, no code fences, in exactly this shape:
{\"units\":[{\"label\":\"short imperative title\",\"description\":\"one sentence: what this unit does and why it is grouped\",\"hunk_ids\":[\"id\",...]}]}

Rules: use only ids from the inventory; every id belongs to at most one unit; labels under 60 characters.";

/// Assembles the full prompt for `hunks` (the post-noise inventory —
/// callers must not include hunks the reply is never allowed to claim).
pub fn build(files: &[DiffFile], hunks: &[HunkMeta]) -> String {
    for (tier_idx, &cap) in EXCERPT_TIERS.iter().enumerate() {
        let prompt = build_with_cap(files, hunks, cap);
        let last_tier = tier_idx == EXCERPT_TIERS.len() - 1;
        if prompt.len() <= PROMPT_BYTE_BUDGET || last_tier {
            return prompt;
        }
    }
    unreachable!("the loop always returns on the last tier");
}

fn build_with_cap(files: &[DiffFile], hunks: &[HunkMeta], max_lines: usize) -> String {
    use std::fmt::Write;
    let mut out = String::from(INSTRUCTIONS);
    out.push_str("\n\n## Hunk inventory\n");
    for meta in hunks {
        let hunk = &files[meta.file_idx].hunks[meta.hunk_idx];
        let (mut added, mut removed) = (0u32, 0u32);
        for row in &hunk.rows {
            match row.kind {
                DiffLineKind::Add => added += 1,
                DiffLineKind::Del => removed += 1,
                DiffLineKind::Context => {}
            }
        }
        let header = if hunk.header.is_empty() {
            String::new()
        } else {
            format!(" @@ {}", hunk.header.trim())
        };
        let _ = write!(
            out,
            "\n### {} — {} (+{added}/-{removed}){header}\n",
            meta.id, meta.file
        );
        let mut shown = 0usize;
        for row in &hunk.rows {
            let marker = match row.kind {
                DiffLineKind::Add => '+',
                DiffLineKind::Del => '-',
                DiffLineKind::Context => continue,
            };
            if shown == max_lines {
                let _ = writeln!(
                    out,
                    "  … {} more changed lines",
                    (added + removed) as usize - shown
                );
                break;
            }
            let _ = writeln!(out, "{marker}{}", row.text);
            shown += 1;
        }
    }
    out
}

#[derive(Deserialize)]
struct Reply {
    units: Vec<UnitDraft>,
}

#[derive(Deserialize)]
struct UnitDraft {
    label: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    hunk_ids: Vec<String>,
}

/// Extracts the units from an agent reply. Tolerates the decoration real
/// CLIs wrap around the JSON they were told not to wrap — markdown fences,
/// a sentence of preamble — by parsing the outermost `{…}` slice rather
/// than the raw text, but anything unparseable inside that is an error:
/// guessing at a half-valid grouping is worse than reporting the failure
/// and letting the user re-run.
pub fn parse_reply(text: &str) -> Result<Vec<Unit>, String> {
    let start = text
        .find('{')
        .ok_or("agent reply contains no JSON object")?;
    let end = text
        .rfind('}')
        .ok_or("agent reply contains no JSON object")?;
    if end < start {
        return Err("agent reply contains no JSON object".to_owned());
    }
    let reply: Reply = serde_json::from_str(&text[start..=end])
        .map_err(|e| format!("agent reply is not the expected JSON shape: {e}"))?;
    Ok(reply
        .units
        .into_iter()
        .map(|draft| Unit {
            label: draft.label,
            description: draft.description,
            hunk_ids: draft.hunk_ids,
            kind: super::UnitKind::Concern,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse_unified_diff;
    use crate::groups::{enumerate_hunks, split_noise};

    fn fixture() -> (Vec<DiffFile>, Vec<HunkMeta>) {
        let files = parse_unified_diff(concat!(
            "diff --git a/src/a.rs b/src/a.rs\n",
            "--- a/src/a.rs\n",
            "+++ b/src/a.rs\n",
            "@@ -1,3 +1,3 @@ fn a()\n",
            " fn a() {\n",
            "-    old();\n",
            "+    new();\n",
            " }\n",
        ));
        let hunks = enumerate_hunks(&files);
        let (keep, _) = split_noise(&files, hunks);
        (files, keep)
    }

    #[test]
    fn prompt_lists_each_hunk_id_with_its_changed_lines_only() {
        let (files, hunks) = fixture();
        let prompt = build(&files, &hunks);
        assert!(prompt.contains(&hunks[0].id));
        assert!(prompt.contains("-    old();"));
        assert!(prompt.contains("+    new();"));
        assert!(
            !prompt.contains(" fn a() {\n"),
            "context rows must not be shown — only changed lines"
        );
        assert!(
            prompt.contains("@@ fn a()"),
            "git's function header is context worth keeping"
        );
    }

    #[test]
    fn parse_tolerates_fences_and_preamble() {
        let reply = "Sure! Here is the grouping:\n```json\n\
                     {\"units\":[{\"label\":\"L\",\"hunk_ids\":[\"aa\"]}]}\n```";
        let units = parse_reply(reply).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].label, "L");
        assert_eq!(units[0].hunk_ids, vec!["aa"]);
        assert_eq!(units[0].description, "");
    }

    #[test]
    fn parse_rejects_a_reply_with_no_json() {
        assert!(parse_reply("I could not group this diff.").is_err());
    }

    #[test]
    fn parse_rejects_wrong_shaped_json() {
        assert!(parse_reply("{\"notunits\": 3}").is_err());
    }

    #[test]
    fn oversized_diffs_fall_back_to_smaller_excerpts() {
        // One hunk with enough long changed lines that the 30-line tier
        // overflows the budget, forcing a smaller cap — the prompt must
        // then advertise the elision instead of silently ending.
        let long_line = "x".repeat(10_000);
        let mut body = String::from(
            "diff --git a/big.rs b/big.rs\n--- a/big.rs\n+++ b/big.rs\n@@ -1,0 +1,30 @@\n",
        );
        for _ in 0..30 {
            body.push('+');
            body.push_str(&long_line);
            body.push('\n');
        }
        let files = parse_unified_diff(&body);
        let hunks = enumerate_hunks(&files);
        let prompt = build(&files, &hunks);
        assert!(prompt.contains("more changed lines"));
    }
}
