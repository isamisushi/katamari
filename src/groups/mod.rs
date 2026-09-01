//! Semantic grouping of a diff's hunks into reviewable units — the data
//! model behind the "units" view, which renders one big agent-authored diff
//! as an ordered list of concerns (a stacked-PR-like reading order) instead
//! of a flat file list.
//!
//! The split of responsibilities follows what the commit-untangling
//! literature found actually matters (see the design notes in the repo's
//! issue tracker): everything that can be decided deterministically — hunk
//! identity, noise bucketing, coverage of the final grouping — is decided
//! *here*, in plain code; only the genuinely semantic judgment ("which
//! hunks serve the same concern, and in what order should a reviewer read
//! them") is delegated to an external agent CLI ([`agent`]), whose output
//! is treated as an untrusted proposal and normalized by [`normalize`]
//! before anything renders it. The LLM never restates diff content and
//! never gets to violate the coverage invariant (every hunk in exactly one
//! unit); at worst a bad response degrades into everything landing in the
//! "ungrouped" bucket.

pub mod agent;
pub mod prompt;
pub mod store;

use crate::diff::{DiffFile, DiffLineKind, is_lockfile_ish};
use serde::{Deserialize, Serialize};

/// One hunk's identity within a grouping, resolvable back to the live diff
/// via `(file_idx, hunk_idx)`. The indices are positions in the parsed
/// `Vec<DiffFile>` this inventory was enumerated from — valid only against
/// that same parse, which is why [`Grouping`] persists hunk *ids* and the
/// UI re-resolves them through a fresh [`enumerate_hunks`] on load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkMeta {
    pub id: String,
    pub file: String,
    pub file_idx: usize,
    pub hunk_idx: usize,
}

/// How a unit came to exist — the UI renders these differently (a noise
/// bucket collapses by default; the misc bucket is a visible warning that
/// the agent's proposal didn't cover everything).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    /// Proposed by the agent as a coherent concern.
    #[default]
    Concern,
    /// Bucketed deterministically before the agent ever saw the diff
    /// (lockfiles, generated files, binaries) — see [`split_noise`].
    Noise,
    /// Hunks the agent's response failed to claim, swept in by
    /// [`normalize`] so the coverage invariant holds regardless of what
    /// the LLM returned.
    Misc,
}

/// One reviewable unit: an ordered slice of the diff with a label. Units
/// themselves are ordered (the `Vec<Unit>` in [`Grouping`]) — that order is
/// the agent's proposed reading order, foundation first, which is the
/// stacked-PR property this whole feature exists to recover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unit {
    pub label: String,
    #[serde(default)]
    pub description: String,
    pub hunk_ids: Vec<String>,
    #[serde(default)]
    pub kind: UnitKind,
}

/// A persisted grouping of one particular diff, keyed by [`diff_key`] so a
/// cache hit is exact: any change to any hunk's content produces a new key
/// and the stale grouping simply stops matching (it is never mutated to
/// fit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grouping {
    pub diff_key: String,
    /// Which CLI produced it (`"claude"`/`"codex"`) — shown in the UI so a
    /// reviewer knows whose judgment they're reading, and useful when
    /// comparing the two.
    pub agent: String,
    pub created_at: u64,
    pub units: Vec<Unit>,
}

/// FNV-1a, hand-rolled. `DefaultHasher`'s algorithm is explicitly not
/// guaranteed stable across Rust releases, and these hashes are persisted
/// to `.katamari/groups.jsonl` — a toolchain upgrade must not silently
/// invalidate every cached grouping, so the hash must be one we own.
fn fnv1a<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for chunk in chunks {
        for &byte in chunk {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// Enumerates every hunk of every non-binary file with a stable id.
///
/// The id hashes the file path plus the hunk's *changed rows only* — their
/// kind and text, not their line numbers and not the surrounding context
/// rows. That choice is what keeps ids stable under the edits that happen
/// *around* a hunk: unrelated changes above it shift its line numbers, and
/// a context-width setting or a neighboring hunk merging changes its
/// context rows, but neither touches what the hunk actually changes, so
/// neither should change its identity (a grouping cached before such a
/// shift keeps matching after it).
///
/// Two byte-identical changes in the same file are disambiguated by a rank
/// folded into the hash — duplicate ids would make "every hunk in exactly
/// one unit" ambiguous, and (more importantly, see below) would let
/// [`crate::reviewed`] silently transfer a reviewed mark onto the wrong
/// occurrence. That rank is **not** simply "the *n*-th time this content is
/// seen while walking `file.hunks` in order" — an earlier version of this
/// function did exactly that, and it quietly broke the one property this
/// whole scheme exists for: a hunk's id is supposed to depend only on that
/// hunk, never on what else happens to be in the file. Iteration order is
/// position in the *current* parse, so inserting or deleting an unrelated
/// same-content hunk anywhere earlier in the file shifted every later
/// occurrence's rank by one — a reviewed mark keyed on rank 1 would silently
/// point at whatever new content now happened to be the second occurrence,
/// not the hunk a human actually reviewed (see the katamari-review-state
/// design notes, and `App::mark_current_hunk_reviewed`'s callers, for why
/// that's worse than merely losing the mark: it actively mislabels new,
/// never-reviewed content as already-reviewed).
///
/// Instead, only a hunk whose *changed* rows actually collide with
/// another's in the same file pays for anything beyond that plain content
/// hash at all — an ordinary, non-duplicated hunk (the overwhelming
/// majority in any real diff) gets exactly `fnv1a(changed_hash, 0u32)`,
/// bit-for-bit the same id this function has always produced for it, so a
/// toolchain upgrade to this fix doesn't invalidate a single existing mark
/// or cached grouping outside the narrow duplicate case it's actually
/// fixing. For a hunk that *does* collide, its own **surrounding context**
/// — every row of the hunk, changed and unchanged alike, hashed separately
/// — is folded directly into the id as a value, not used to *count*
/// anything. That distinction matters: **any** scheme that assigns a
/// disambiguator as "how many colliding occurrences sort before me" is
/// vulnerable to the exact same bug regardless of what the sort key is —
/// inserting a new colliding occurrence can always land "before" an
/// existing one in *some* total order (by file position, by `old_start`,
/// even by a context hash's raw numeric value), bumping the existing one's
/// count and so its id; an earlier draft of this fix tried ranking by
/// context instead of embedding it and still had exactly this problem.
/// Embedding the context hash's actual *value* sidesteps counting entirely:
/// two hunks only ever collide when their changed rows *and* their context
/// rows are both byte-identical, and whether that's true of any two
/// *specific* hunks never depends on whether a third hunk exists. Context
/// text is practically always enough to tell two occurrences of the same
/// changed line apart (`+dup` sitting between different neighboring lines
/// in two places), which is why this closes the bug for the case that
/// actually matters. Only the fully degenerate residual — changed rows
/// *and* every context row byte-identical, i.e. the same code, context
/// included, truly duplicated verbatim elsewhere in the file — still falls
/// back to a position-based rank (old-side anchor, then array position) to
/// keep ids distinct at all; *that* narrow case can still shift under an
/// insertion, same as the old scheme always did for every duplicate, but
/// there is no remaining hash left to fold in that would tell such hunks
/// apart.
pub fn enumerate_hunks(files: &[DiffFile]) -> Vec<HunkMeta> {
    let mut out = Vec::new();
    for (file_idx, file) in files.iter().enumerate() {
        let path = file.display_path();

        // The id-defining hash: path plus changed rows only, exactly as
        // before — every stability guarantee in this function's docs is
        // about *this* hash, so it must stay untouched by anything below.
        let changed_hashes: Vec<u64> = file
            .hunks
            .iter()
            .map(|hunk| {
                fnv1a(
                    std::iter::once(path.as_bytes()).chain(hunk.rows.iter().filter_map(|row| {
                        (row.kind != DiffLineKind::Context).then_some(row.text.as_bytes())
                    })),
                )
            })
            .collect();

        let mut group_size: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
        for &h in &changed_hashes {
            *group_size.entry(h).or_insert(0) += 1;
        }

        // The disambiguation hash — every row, context included — computed
        // only for a hunk whose `changed_hash` actually collides with
        // another's (`None` otherwise), so an ordinary hunk's id below
        // never depends on context at all. Folded directly into the id as
        // a value below, never used to rank/count — see this function's
        // docs on why that distinction is the actual fix.
        let context_hashes: Vec<Option<u64>> = file
            .hunks
            .iter()
            .enumerate()
            .map(|(i, hunk)| {
                (group_size[&changed_hashes[i]] > 1).then(|| {
                    fnv1a(
                        std::iter::once(path.as_bytes())
                            .chain(hunk.rows.iter().map(|row| row.text.as_bytes())),
                    )
                })
            })
            .collect();

        // The only remaining source of a collision: two hunks whose
        // `(changed_hash, context_hash)` pair is *itself* identical (see
        // the doc comment above). Rank those specific collisions by
        // old-side position as a last resort — `order` walks the hunks in
        // that fallback order; `rank` then records, for each hunk, how many
        // earlier-in-`order` hunks shared its full pair. Every hunk with a
        // unique pair (every non-colliding hunk, and any colliding one
        // whose context alone already distinguishes it) gets rank 0.
        let mut order: Vec<usize> = (0..file.hunks.len()).collect();
        order.sort_by_key(|&i| {
            (
                changed_hashes[i],
                context_hashes[i],
                file.hunks[i].old_start,
                file.hunks[i].new_start,
                i,
            )
        });
        let mut rank = vec![0u32; file.hunks.len()];
        let mut seen_in_file: std::collections::HashMap<(u64, Option<u64>), u32> =
            std::collections::HashMap::new();
        for &i in &order {
            let occurrence = seen_in_file
                .entry((changed_hashes[i], context_hashes[i]))
                .or_insert(0);
            rank[i] = *occurrence;
            *occurrence += 1;
        }

        for (hunk_idx, _) in file.hunks.iter().enumerate() {
            let content_hash = match context_hashes[hunk_idx] {
                // Untouched formula for the ordinary, non-colliding case —
                // see this function's docs on why bit-for-bit stability
                // here matters.
                None => fnv1a([
                    &changed_hashes[hunk_idx].to_le_bytes()[..],
                    &0u32.to_le_bytes()[..],
                ]),
                Some(context_hash) => fnv1a([
                    &changed_hashes[hunk_idx].to_le_bytes()[..],
                    &context_hash.to_le_bytes()[..],
                    &rank[hunk_idx].to_le_bytes()[..],
                ]),
            };
            out.push(HunkMeta {
                id: format!("{content_hash:016x}"),
                file: path.to_owned(),
                file_idx,
                hunk_idx,
            });
        }
    }
    out
}

/// The cache key for one diff: a hash over its hunk ids, in order. Content
/// changes flow in via the per-hunk hashes; file order matters too (a
/// rename that reorders files produces a different reading experience, so
/// a fresh grouping is the safe answer).
pub fn diff_key(hunks: &[HunkMeta]) -> String {
    let key = fnv1a(hunks.iter().map(|h| h.id.as_bytes()));
    format!("{key:016x}")
}

/// Paths whose changes are mechanical byproducts a reviewer never needs an
/// LLM's help to categorize: lockfiles/minified bundles (the existing
/// [`is_lockfile_ish`] rule), checksum databases, snapshot fixtures, and
/// anything under a `generated` directory. Bucketed *before* the agent
/// sees the diff — the strongest single lever the untangling literature
/// found was removing noise from the LLM's input, and these are free to
/// remove deterministically.
fn is_generated_ish(display_path: &str) -> bool {
    if is_lockfile_ish(display_path) {
        return true;
    }
    let name = display_path.rsplit('/').next().unwrap_or(display_path);
    name == "go.sum"
        || name.ends_with(".snap")
        || name.contains(".generated.")
        || name.ends_with(".pb.go")
        || display_path
            .split('/')
            .any(|seg| seg == "generated" || seg == "__generated__")
}

/// Splits the inventory into the hunks worth classifying and (when any
/// exist) a pre-made noise unit. Binary files carry no hunks at all (see
/// [`DiffFile::is_binary`]), so they never appear in either half — nothing
/// to group, nothing to cover.
pub fn split_noise(files: &[DiffFile], hunks: Vec<HunkMeta>) -> (Vec<HunkMeta>, Option<Unit>) {
    let (noise, keep): (Vec<HunkMeta>, Vec<HunkMeta>) = hunks
        .into_iter()
        .partition(|h| is_generated_ish(files[h.file_idx].display_path()));
    let noise_unit = (!noise.is_empty()).then(|| Unit {
        label: "Lockfiles & generated".to_owned(),
        description: "Mechanical changes bucketed without the agent: lockfiles, checksums, \
                      generated files."
            .to_owned(),
        hunk_ids: noise.into_iter().map(|h| h.id).collect(),
        kind: UnitKind::Noise,
    });
    (keep, noise_unit)
}

/// Forces an agent-proposed grouping into one that satisfies the coverage
/// invariant against `hunks` (the classifiable inventory, i.e. *after*
/// [`split_noise`]): every hunk id appears in exactly one unit.
///
/// - An id the inventory doesn't contain is dropped (hallucinated, or
///   stale from a diff that has since changed).
/// - An id claimed twice keeps its first placement — first-wins rather
///   than last-wins so the agent's proposed reading order decides ties.
/// - Ids the proposal never claimed are swept into a trailing `Misc` unit.
/// - Units emptied by the above are dropped entirely.
///
/// This is the boundary where the LLM's output stops being a proposal and
/// becomes state the UI may trust; nothing downstream re-checks coverage.
pub fn normalize(proposed: Vec<Unit>, hunks: &[HunkMeta]) -> Vec<Unit> {
    let known: std::collections::HashSet<&str> = hunks.iter().map(|h| h.id.as_str()).collect();
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut units: Vec<Unit> = Vec::new();
    for mut unit in proposed {
        unit.hunk_ids
            .retain(|id| known.contains(id.as_str()) && claimed.insert(id.clone()));
        if !unit.hunk_ids.is_empty() {
            unit.kind = UnitKind::Concern;
            units.push(unit);
        }
    }
    let unclaimed: Vec<String> = hunks
        .iter()
        .filter(|h| !claimed.contains(&h.id))
        .map(|h| h.id.clone())
        .collect();
    if !unclaimed.is_empty() {
        units.push(Unit {
            label: "Ungrouped".to_owned(),
            description: "Hunks the agent's proposal did not place.".to_owned(),
            hunk_ids: unclaimed,
            kind: UnitKind::Misc,
        });
    }
    units
}

/// How long [`generate`] lets the agent CLI think. Generous because a
/// large diff legitimately takes a while to read; bounded because the CLI
/// hanging on a hidden login prompt must not hold a background thread (and
/// the user's patience) forever.
const AGENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// The cached grouping for this exact diff, if one exists — the fast path
/// the UI tries before ever considering a spawn. `None` on any store
/// trouble too: a cache that can't be read is a cache miss, not an error.
pub fn cached(repo_root: &std::path::Path, files: &[DiffFile]) -> Option<Grouping> {
    let key = diff_key(&enumerate_hunks(files));
    store::GroupStore::new(repo_root)
        .load_for(&key)
        .ok()
        .flatten()
}

/// The slow path: one full grouping round against the user's agent CLI,
/// persisted on success. Blocking (the agent call dominates at seconds to
/// minutes) — callers run it on a background thread and deliver the result
/// over a channel. Errors are `String` because their one consumer is the
/// status bar; there is no programmatic recovery beyond "tell the user and
/// let them re-run". `units` is the merged `[units]` config: CLI
/// preference plus per-CLI model/effort tuning.
pub fn generate(
    repo_root: &std::path::Path,
    files: &[DiffFile],
    units: &crate::config::UnitsConfig,
) -> Result<Grouping, String> {
    let cli = agent::detect_preferring(units.agent.as_deref()).ok_or(
        "no agent CLI found — grouping needs `claude` or `codex` on PATH (see ktmr doctor)",
    )?;
    let hunks = enumerate_hunks(files);
    if hunks.is_empty() {
        return Err("nothing to group — the diff has no hunks".to_owned());
    }
    let key = diff_key(&hunks);
    let (keep, noise_unit) = split_noise(files, hunks);

    // A diff that is *pure* noise never needs the agent at all.
    let mut grouped = if keep.is_empty() {
        Vec::new()
    } else {
        let request = prompt::build(files, &keep);
        let reply = agent::run(&cli, units, &request, AGENT_TIMEOUT).map_err(|e| e.to_string())?;
        let proposed = prompt::parse_reply(&reply)?;
        normalize(proposed, &keep)
    };
    // Noise trails everything, including the Misc sweep: it's the part of
    // the diff a reviewer most wants to skip, so it reads last.
    grouped.extend(noise_unit);

    let grouping = Grouping {
        diff_key: key,
        agent: agent_description(&cli, units),
        created_at: crate::comments::now_unix(),
        units: grouped,
    };
    // A grouping that can't be persisted is still a perfectly good
    // grouping for this session — the cache write is best-effort.
    let _ = store::GroupStore::new(repo_root).append(&grouping);
    Ok(grouping)
}

/// What [`Grouping::agent`] records — the binary name plus whatever
/// model/effort tuning was in force, e.g. `"claude (opus/high)"`. Shown in
/// the panel title, so a reviewer comparing two regenerated groupings can
/// tell whether they came from the same configuration.
fn agent_description(cli: &agent::AgentCli, units: &crate::config::UnitsConfig) -> String {
    let (model, effort) = match cli.kind {
        agent::AgentKind::Claude => (&units.claude_model, &units.claude_effort),
        agent::AgentKind::Codex => (&units.codex_model, &units.codex_effort),
    };
    let tuning: Vec<&str> = [model.as_deref(), effort.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    if tuning.is_empty() {
        cli.kind.binary().to_owned()
    } else {
        format!("{} ({})", cli.kind.binary(), tuning.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse_unified_diff;

    fn sample_diff() -> Vec<DiffFile> {
        parse_unified_diff(concat!(
            "diff --git a/src/a.rs b/src/a.rs\n",
            "--- a/src/a.rs\n",
            "+++ b/src/a.rs\n",
            "@@ -1,3 +1,3 @@\n",
            " fn a() {\n",
            "-    old();\n",
            "+    new();\n",
            " }\n",
            "@@ -10,2 +10,3 @@\n",
            " fn b() {\n",
            "+    added();\n",
            " }\n",
            "diff --git a/Cargo.lock b/Cargo.lock\n",
            "--- a/Cargo.lock\n",
            "+++ b/Cargo.lock\n",
            "@@ -1,1 +1,2 @@\n",
            " [package]\n",
            "+version = \"2\"\n",
        ))
    }

    #[test]
    fn hunk_ids_are_stable_across_line_number_shifts() {
        let files = sample_diff();
        let ids_before: Vec<String> = enumerate_hunks(&files).into_iter().map(|h| h.id).collect();

        // The same changes, shifted 100 lines down: ids must not move.
        let shifted = parse_unified_diff(concat!(
            "diff --git a/src/a.rs b/src/a.rs\n",
            "--- a/src/a.rs\n",
            "+++ b/src/a.rs\n",
            "@@ -101,3 +101,3 @@\n",
            " fn a() {\n",
            "-    old();\n",
            "+    new();\n",
            " }\n",
            "@@ -110,2 +110,3 @@\n",
            " fn b() {\n",
            "+    added();\n",
            " }\n",
            "diff --git a/Cargo.lock b/Cargo.lock\n",
            "--- a/Cargo.lock\n",
            "+++ b/Cargo.lock\n",
            "@@ -1,1 +1,2 @@\n",
            " [package]\n",
            "+version = \"2\"\n",
        ));
        let ids_after: Vec<String> = enumerate_hunks(&shifted)
            .into_iter()
            .map(|h| h.id)
            .collect();
        assert_eq!(ids_before, ids_after);
    }

    /// The reviewed-hunk feature (`crate::reviewed`) leans on this id being
    /// stable not just across line-number shifts (the test above) but
    /// across reordering too — a rebase or an unrelated commit reordering
    /// files/hunks must not silently drop a reviewer's marks. The
    /// *content* hash of any one hunk never depends on its neighbors'
    /// positions, only on its own path and changed rows.
    #[test]
    fn hunk_ids_are_a_set_unaffected_by_reordering_files_or_hunks_within_one() {
        let files = sample_diff(); // [src/a.rs (2 hunks), Cargo.lock (1 hunk)]
        let ids_before: std::collections::HashSet<String> =
            enumerate_hunks(&files).into_iter().map(|h| h.id).collect();

        let mut reordered = files;
        reordered.reverse(); // Cargo.lock first, src/a.rs second
        reordered[1].hunks.reverse(); // src/a.rs's own two hunks swapped

        let ids_after: std::collections::HashSet<String> = enumerate_hunks(&reordered)
            .into_iter()
            .map(|h| h.id)
            .collect();
        assert_eq!(
            ids_before, ids_after,
            "the set of hunk ids must be identical regardless of file/hunk order"
        );
    }

    #[test]
    fn identical_changes_in_one_file_get_distinct_ids() {
        let files = parse_unified_diff(concat!(
            "diff --git a/x.rs b/x.rs\n",
            "--- a/x.rs\n",
            "+++ b/x.rs\n",
            "@@ -1,1 +1,1 @@\n",
            "-dup\n",
            "+dup2\n",
            "@@ -10,1 +10,1 @@\n",
            "-dup\n",
            "+dup2\n",
        ));
        let hunks = enumerate_hunks(&files);
        assert_eq!(hunks.len(), 2);
        assert_ne!(hunks[0].id, hunks[1].id);
    }

    /// The bug a reviewer caught live in the reviewed-hunk feature: the old
    /// occurrence-counter disambiguation ranked duplicate-content hunks by
    /// their position in `file.hunks` at derivation time, so inserting a
    /// brand-new same-content hunk *earlier* in the file silently reassigned
    /// an already-reviewed hunk's id onto the new, never-seen occurrence —
    /// mislabeling unrelated content as reviewed rather than merely losing
    /// the mark. Each occurrence here carries distinct surrounding context
    /// (realistic — git includes it by default), so embedding that context
    /// hash's own value into the id (see [`enumerate_hunks`]'s docs) must
    /// keep each pre-existing occurrence's id fixed regardless of what gets
    /// inserted or removed around it, purely because each one's own
    /// neighborhood never changes.
    #[test]
    fn duplicate_content_ids_survive_a_same_content_hunk_inserted_earlier_in_the_file() {
        let before = parse_unified_diff(concat!(
            "diff --git a/x.rs b/x.rs\n",
            "--- a/x.rs\n",
            "+++ b/x.rs\n",
            "@@ -19,2 +19,3 @@\n",
            " before twenty\n",
            "+dup\n",
            " after twenty\n",
            "@@ -49,2 +50,3 @@\n",
            " before fifty\n",
            "+dup\n",
            " after fifty\n",
        ));
        let ids_before: Vec<String> = enumerate_hunks(&before).into_iter().map(|h| h.id).collect();
        assert_eq!(ids_before.len(), 2);
        assert_ne!(ids_before[0], ids_before[1]);

        // An unrelated later edit: a third, brand-new occurrence of the
        // identical changed line — in its own distinct neighborhood — lands
        // even earlier in the file (old line 2).
        let after = parse_unified_diff(concat!(
            "diff --git a/x.rs b/x.rs\n",
            "--- a/x.rs\n",
            "+++ b/x.rs\n",
            "@@ -1,2 +1,3 @@\n",
            " before two\n",
            "+dup\n",
            " after two\n",
            "@@ -19,2 +20,3 @@\n",
            " before twenty\n",
            "+dup\n",
            " after twenty\n",
            "@@ -49,2 +51,3 @@\n",
            " before fifty\n",
            "+dup\n",
            " after fifty\n",
        ));
        let hunks_after = enumerate_hunks(&after);
        let ids_after: Vec<String> = hunks_after.iter().map(|h| h.id.clone()).collect();
        assert_eq!(ids_after.len(), 3);

        // The two pre-existing occurrences (now hunks 1 and 2 by file
        // position) must keep exactly the ids they had before — not shift
        // onto the new occurrence's identity just because it now sorts
        // ahead of them by old-side position.
        assert_eq!(
            ids_after[1..],
            ids_before,
            "the original two occurrences' ids must be unaffected by the new one inserted ahead of them"
        );
        assert!(
            !ids_before.contains(&ids_after[0]),
            "the brand-new occurrence must not collide with either original id"
        );
    }

    #[test]
    fn diff_key_changes_when_any_hunk_content_changes() {
        let files = sample_diff();
        let key_before = diff_key(&enumerate_hunks(&files));

        let mut edited = sample_diff();
        edited[0].hunks[0].rows[2].text = "    different();".to_owned();
        let key_after = diff_key(&enumerate_hunks(&edited));
        assert_ne!(key_before, key_after);
    }

    #[test]
    fn split_noise_buckets_lockfiles_and_leaves_the_rest() {
        let files = sample_diff();
        let hunks = enumerate_hunks(&files);
        let (keep, noise) = split_noise(&files, hunks);
        assert_eq!(keep.len(), 2);
        let noise = noise.expect("Cargo.lock must land in the noise unit");
        assert_eq!(noise.kind, UnitKind::Noise);
        assert_eq!(noise.hunk_ids.len(), 1);
    }

    #[test]
    fn normalize_enforces_exactly_once_coverage() {
        let files = sample_diff();
        let (keep, _) = split_noise(&files, enumerate_hunks(&files));
        let (id_a, id_b) = (keep[0].id.clone(), keep[1].id.clone());

        let proposed = vec![
            Unit {
                label: "real".into(),
                description: String::new(),
                // One real id, the same id again (duplicate), and a
                // hallucinated one — only the first survives.
                hunk_ids: vec![id_a.clone(), id_a.clone(), "feedbeef".into()],
                kind: UnitKind::Concern,
            },
            Unit {
                label: "emptied".into(),
                description: String::new(),
                hunk_ids: vec!["deadc0de".into()],
                kind: UnitKind::Concern,
            },
        ];
        let units = normalize(proposed, &keep);
        assert_eq!(
            units.len(),
            2,
            "real unit + swept Misc; emptied unit dropped"
        );
        assert_eq!(units[0].hunk_ids, vec![id_a]);
        assert_eq!(units[1].kind, UnitKind::Misc);
        assert_eq!(units[1].hunk_ids, vec![id_b]);
    }

    #[test]
    fn normalize_of_an_empty_proposal_sweeps_everything_into_misc() {
        let files = sample_diff();
        let (keep, _) = split_noise(&files, enumerate_hunks(&files));
        let units = normalize(Vec::new(), &keep);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].kind, UnitKind::Misc);
        assert_eq!(units[0].hunk_ids.len(), keep.len());
    }
}
