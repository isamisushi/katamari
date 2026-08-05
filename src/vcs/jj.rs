//! A `jj`-backed source of *snapshot history*, sitting alongside
//! [`super::git::GitSource`] rather than implementing [`super::DiffSource`]
//! itself: `DiffSource` answers "what changed against a baseline," which
//! `git` already answers perfectly well for a colocated jj repo's working
//! tree (jj keeps the git index/refs in sync). What only jj can answer is
//! "what did the working copy look like after each of an AI agent's edit
//! bursts" — the operation log — which is what this module exists for.
//!
//! Every jj CLI detail this milestone depends on — which subcommand
//! triggers a snapshot on this jj version, the op-log template format, how
//! `jj op diff --git` wraps its diff body — is isolated here so a future jj
//! release that moves any of it needs exactly one file touched. See
//! [`JjRepo::snapshot`] and [`strip_op_diff_wrapper`] for the two spots
//! where jj's actual (as opposed to documented-and-hoped-for) 0.43 behavior
//! mattered enough to write down.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolves a `jj` executable from `PATH`, the way a shell would (first
/// `PATH` entry containing a file named `jj`). Kept separate from
/// [`JjRepo::detect`] so PATH resolution — inherently dependent on process
/// environment — and "does this directory look like a jj repo" — a pure
/// function of the filesystem — can each be tested independently; a caller
/// normally chains them as `resolve_jj_bin().and_then(|bin| JjRepo::detect(root, bin))`.
///
/// Doesn't check the executable bit: a non-executable `jj` on PATH is rare
/// enough, and the failure mode specific enough (the first real invocation
/// fails with a clear OS error), that checking for it here would just be
/// another way for this function to be wrong about what "resolved" means.
pub fn resolve_jj_bin() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join("jj");
        candidate.is_file().then_some(candidate)
    })
}

/// One entry in the operation log, filtered down to working-copy snapshots
/// — see [`JjRepo::snapshot_ops`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotOp {
    /// The operation's full (never abbreviated) id. jj's own `.short()`
    /// template method truncates to the shortest prefix unambiguous *right
    /// now*, which can grow as more operations are logged — unsuitable for
    /// an id this module holds onto across calls (snapshot-detection's
    /// before/after comparison, a timeline selection outliving a live
    /// snapshot). The full id has no such expiry.
    pub op_id: String,
    /// Unix seconds the operation completed — deliberately not a formatted
    /// string: jj is free to change its own display format, and rendering
    /// "3m ago" is a UI concern ([`crate::ui::timeline_view`]'s), not this
    /// module's.
    pub time_unix: i64,
    pub description: String,
}

/// A `jj`-backed repository, resolved once and reused for every snapshot
/// and timeline query in a session. Holds nothing but the two things every
/// invocation needs (the binary, the repo root), the same lightweight shape
/// as [`super::git::GitSource`] — cheap enough to [`Clone`] freely, which
/// [`crate::ui::mod`]'s watch-mode hook and every pushed
/// [`crate::ui::timeline_view::TimelineView`] each need their own owned
/// copy of.
#[derive(Clone)]
pub struct JjRepo {
    jj_bin: PathBuf,
    repo_root: PathBuf,
}

impl JjRepo {
    /// `jj_bin` must already be resolved from `PATH` (see
    /// [`resolve_jj_bin`]) — this function's only remaining job is checking
    /// whether `repo_root` looks like a jj repo. Per the product's expected
    /// setup, that means colocated with git: a `.jj` directory sitting
    /// right alongside `.git` in the same root [`crate::vcs::git::GitSource`]
    /// resolved. A non-colocated jj repo (`.jj` elsewhere, or containing a
    /// hidden `.git` jj manages itself) isn't detected — out of scope for
    /// M5, since every other part of this program talks to `git` directly
    /// and would see nothing there.
    pub fn detect(repo_root: &Path, jj_bin: PathBuf) -> Option<Self> {
        if !repo_root.join(".jj").is_dir() {
            return None;
        }
        Some(Self {
            jj_bin,
            repo_root: repo_root.to_owned(),
        })
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// A bare `jj -R <repo_root>` invocation, color and pager disabled so
    /// captured output is never contaminated by ANSI escapes or paged
    /// output shaped for a terminal. Every other command builder in this
    /// file starts from this one.
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.jj_bin);
        cmd.arg("-R")
            .arg(&self.repo_root)
            .args(["--color", "never", "--no-pager"]);
        cmd
    }

    /// As [`Self::command`], plus `--ignore-working-copy` — every read-only
    /// query in this module (op log, op diff) goes through this instead of
    /// [`Self::command`] directly, so the flag is never forgotten on a new
    /// call site. [`Self::snapshot`] is the one caller that deliberately
    /// does *not* use this: it needs the working copy actually snapshotted.
    fn readonly_command(&self) -> Command {
        let mut cmd = self.command();
        cmd.arg("--ignore-working-copy");
        cmd
    }

    /// The current operation's full id — used by [`Self::snapshot`] to
    /// detect whether triggering a snapshot actually created a new
    /// operation (jj is a no-op when nothing in the working copy changed).
    fn current_op_id(&self) -> Result<String> {
        let output = self
            .readonly_command()
            .args(["op", "log", "-n1", "--no-graph", "-T", "id"])
            .output()
            .context("failed to run `jj op log`")?;
        if !output.status.success() {
            bail!(
                "jj op log failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        String::from_utf8(output.stdout).context("jj op log produced non-UTF-8 output")
    }

    /// Triggers a working-copy snapshot and reports whether it actually
    /// created a new operation (`false` when the working copy hadn't
    /// changed since the last one — jj is a no-op then, which is not a
    /// failure). This is katamari's own snapshot trigger, called from
    /// [`crate::ui::refresh::PreRefreshHook::before_refresh`] before every
    /// watch-mode re-diff — see this module's docs for why katamari can't
    /// just rely on jj's own watchman-triggered auto-snapshot.
    pub fn snapshot(&self) -> Result<bool> {
        let before = self.current_op_id()?;
        self.run_snapshot_command()?;
        let after = self.current_op_id()?;
        Ok(before != after)
    }

    /// Runs [`SNAPSHOT_COMMAND_CHAIN`] against a real `jj` process — the
    /// only caller of [`select_snapshot_command`] that isn't a unit test.
    fn run_snapshot_command(&self) -> Result<()> {
        let jj_bin = &self.jj_bin;
        let repo_root = &self.repo_root;
        select_snapshot_command(move |args| {
            let output = Command::new(jj_bin)
                .arg("-R")
                .arg(repo_root)
                .args(["--color", "never", "--no-pager"])
                .args(args)
                .output()
                .with_context(|| format!("failed to run `jj {}`", args.join(" ")))?;
            Ok(AttemptOutcome {
                success: output.status.success(),
                exit_code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        })
    }

    /// Field separator for [`Self::snapshot_ops`]'s op-log template — the
    /// ASCII Unit Separator, chosen because it can't appear in a jj
    /// operation description typed (or generated) as ordinary text, unlike
    /// a comma, tab, or pipe.
    const OP_LOG_FIELD_SEP: &'static str = "\u{1f}";

    /// The operation log, filtered to working-copy snapshots (what
    /// [`Self::snapshot`] produces), newest first — the raw material
    /// [`crate::ui::timeline_view`] renders as a browsable list. `limit`
    /// bounds how many *raw* operations are fetched (jj interleaves
    /// non-snapshot operations — `import git head` and the like — into the
    /// same log), so the returned `Vec` can be shorter than `limit` even
    /// when more snapshot history exists further back; a caller wanting
    /// more re-queries with a larger limit rather than this module trying
    /// to guess how far back to look for `limit` matches.
    pub fn snapshot_ops(&self, limit: usize) -> Result<Vec<SnapshotOp>> {
        let template = format!(
            "id ++ \"{sep}\" ++ time.end().format(\"%s\") ++ \"{sep}\" ++ description ++ \"\\n\"",
            sep = Self::OP_LOG_FIELD_SEP
        );
        let output = self
            .readonly_command()
            .args(["op", "log", "-n", &limit.to_string(), "--no-graph", "-T"])
            .arg(&template)
            .output()
            .context("failed to run `jj op log`")?;
        if !output.status.success() {
            bail!(
                "jj op log failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let text =
            String::from_utf8(output.stdout).context("jj op log produced non-UTF-8 output")?;
        Ok(parse_op_log(&text))
    }

    /// The unified diff between two operations' working-copy states —
    /// `jj op diff --from <from> --to <to> --git`, parseable by the
    /// existing [`crate::diff::parse_unified_diff`] once
    /// [`strip_op_diff_wrapper`] removes the commentary `jj op diff` wraps
    /// the actual diff body in (see that function's docs). `--git` and
    /// `--no-graph` are both confirmed present on jj 0.43 (this project's
    /// pinned dev version); no fallback path was needed for either.
    pub fn op_diff(&self, from: &str, to: &str) -> Result<String> {
        let output = self
            .readonly_command()
            .args([
                "op",
                "diff",
                "--from",
                from,
                "--to",
                to,
                "--git",
                "--no-graph",
            ])
            .output()
            .context("failed to run `jj op diff`")?;
        if !output.status.success() {
            bail!(
                "jj op diff failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let text =
            String::from_utf8(output.stdout).context("jj op diff produced non-UTF-8 output")?;
        Ok(strip_op_diff_wrapper(&text).to_owned())
    }
}

/// Candidate subcommands to trigger a working-copy snapshot, most to least
/// preferred, tried in order by [`select_snapshot_command`] until one
/// succeeds.
///
/// What actually ships on jj 0.43 (verified via `jj util --help` / `jj
/// debug --help` before writing this): `jj util snapshot` exists and is the
/// documented, non-deprecated way to do this. `jj debug snapshot` also
/// exists but is marked `[DEPRECATED] ... use jj util snapshot instead` —
/// kept as the second candidate anyway, since a jj version either side of
/// 0.43 in real deployments could easily have only the deprecated name, or
/// only the new one, and there's no version check worth writing when trying
/// the next candidate is this cheap. `jj status` is the last-resort
/// fallback the milestone plan calls for: literally any jj command
/// snapshots the working copy as a side effect by design, so even a jj
/// version with neither dedicated subcommand still gets a real snapshot out
/// of this, just without a command whose *name* says so.
const SNAPSHOT_COMMAND_CHAIN: &[&[&str]] =
    &[&["util", "snapshot"], &["debug", "snapshot"], &["status"]];

/// One attempt's outcome, abstracted away from an actual [`std::process::Command`]
/// so [`select_snapshot_command`] is unit-testable against canned outcomes
/// instead of a real `jj` binary.
struct AttemptOutcome {
    success: bool,
    exit_code: Option<i32>,
    stderr: String,
}

/// clap (which jj's CLI is built on) reports an unknown subcommand as exit
/// code 2 with `error: unrecognized subcommand '...'` on stderr — the
/// signature [`select_snapshot_command`] uses to tell "this jj version
/// doesn't have this subcommand, try the next candidate" apart from a real
/// failure (a conflicted working copy, a corrupt repo) that trying another
/// candidate wouldn't fix and would be wrong to mask.
fn is_unrecognized_subcommand(outcome: &AttemptOutcome) -> bool {
    outcome.exit_code == Some(2) && outcome.stderr.contains("unrecognized subcommand")
}

/// Walks [`SNAPSHOT_COMMAND_CHAIN`], calling `attempt` for each candidate
/// until one succeeds, treating "unrecognized subcommand" as "try the next
/// one" and any other failure as final. Generic over `attempt` (rather than
/// shelling out to `jj` itself) so this selection logic has unit tests that
/// don't depend on which subcommands the `jj` on the test machine happens to
/// support — see this module's tests.
fn select_snapshot_command(
    mut attempt: impl FnMut(&[&str]) -> Result<AttemptOutcome>,
) -> Result<()> {
    let mut last_error = String::new();
    for candidate in SNAPSHOT_COMMAND_CHAIN {
        let outcome = attempt(candidate)?;
        if outcome.success {
            return Ok(());
        }
        if !is_unrecognized_subcommand(&outcome) {
            bail!(
                "jj {} failed: {}",
                candidate.join(" "),
                outcome.stderr.trim()
            );
        }
        last_error = outcome.stderr;
    }
    bail!(
        "no working-copy snapshot command is supported by this jj version (tried: {}); last error: {}",
        SNAPSHOT_COMMAND_CHAIN
            .iter()
            .map(|c| format!("`jj {}`", c.join(" ")))
            .collect::<Vec<_>>()
            .join(", "),
        last_error.trim()
    );
}

/// The exact description [`JjRepo::snapshot`] produces, whichever command in
/// [`SNAPSHOT_COMMAND_CHAIN`] actually ran — confirmed against real jj 0.43
/// output for both `util snapshot` and `debug snapshot`; `jj status` (the
/// last-resort fallback) does *not* produce this description, so a session
/// that ever had to fall all the way to it would undercount its own
/// snapshots in the timeline. Documented as a known M5 gap rather than
/// worked around, since every jj version this module has actually seen
/// supports `util snapshot` and the fallback is there for robustness, not
/// because it's expected to fire.
const SNAPSHOT_DESCRIPTION: &str = "snapshot working copy";

/// Parses [`JjRepo::snapshot_ops`]'s raw, `\x1f`-field-separated `jj op log`
/// output into [`SnapshotOp`]s, keeping only entries whose description is
/// exactly [`SNAPSHOT_DESCRIPTION`] — the op log interleaves other
/// operations (`import git head`, a user's own `jj describe`, ...) that
/// aren't part of the save-by-save timeline this exists to show.
fn parse_op_log(text: &str) -> Vec<SnapshotOp> {
    text.lines()
        .filter_map(parse_op_log_line)
        .filter(|op| op.description == SNAPSHOT_DESCRIPTION)
        .collect()
}

fn parse_op_log_line(line: &str) -> Option<SnapshotOp> {
    let mut fields = line.splitn(3, JjRepo::OP_LOG_FIELD_SEP);
    let op_id = fields.next()?.to_owned();
    let time_unix = fields.next()?.parse().ok()?;
    let description = fields.next().unwrap_or_default().to_owned();
    Some(SnapshotOp {
        op_id,
        time_unix,
        description,
    })
}

/// `jj op diff --git --no-graph` wraps the git-format diff body in
/// commentary: a `From operation: ... \n  To operation: ...` header and a
/// `Changed commits:` label before it (harmless to
/// [`crate::diff::parse_unified_diff`] — it already ignores any line before
/// the first `diff --git ` it recognizes), but also a trailing `Changed
/// working copy <workspace>:` section whose `+ <change-id> ...` / `-
/// <change-id> ...` summary lines are *not* harmless: parsed as diff body
/// text, their leading `+`/`-` would be read as add/del lines appended to
/// the last hunk of the last file, corrupting it. This truncates the text
/// at that marker, keeping only what's actually a unified diff.
///
/// Confirmed against real jj 0.43 output — see this module's tests for the
/// exact shape.
fn strip_op_diff_wrapper(raw: &str) -> &str {
    match raw.find("\nChanged working copy ") {
        Some(idx) => &raw[..idx],
        None => raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- select_snapshot_command --------------------------------------

    fn outcome_ok() -> Result<AttemptOutcome> {
        Ok(AttemptOutcome {
            success: true,
            exit_code: Some(0),
            stderr: String::new(),
        })
    }

    fn outcome_unrecognized() -> Result<AttemptOutcome> {
        Ok(AttemptOutcome {
            success: false,
            exit_code: Some(2),
            stderr: "error: unrecognized subcommand 'snapshot'".to_owned(),
        })
    }

    fn outcome_real_failure() -> Result<AttemptOutcome> {
        Ok(AttemptOutcome {
            success: false,
            exit_code: Some(1),
            stderr: "Internal error: repository is corrupt".to_owned(),
        })
    }

    /// Records each attempt's args as owned strings — `attempt`'s `&[&str]`
    /// parameter is only valid for the duration of one call (an HRTB), so a
    /// test that wants to inspect which candidates were tried afterward has
    /// to copy out of it rather than collect the borrows themselves.
    fn record(attempts: &mut Vec<Vec<String>>, args: &[&str]) {
        attempts.push(args.iter().map(|s| s.to_string()).collect());
    }

    #[test]
    fn first_candidate_succeeding_stops_the_chain() {
        let mut attempts = Vec::new();
        let result = select_snapshot_command(|args| {
            record(&mut attempts, args);
            outcome_ok()
        });
        assert!(result.is_ok());
        assert_eq!(attempts, vec![vec!["util", "snapshot"]]);
    }

    #[test]
    fn unrecognized_subcommand_falls_through_to_the_next_candidate() {
        let mut attempts = Vec::new();
        let result = select_snapshot_command(|args| {
            record(&mut attempts, args);
            if args == ["util", "snapshot"] {
                outcome_unrecognized()
            } else {
                outcome_ok()
            }
        });
        assert!(result.is_ok());
        assert_eq!(
            attempts,
            vec![vec!["util", "snapshot"], vec!["debug", "snapshot"]]
        );
    }

    #[test]
    fn falls_all_the_way_to_status_when_neither_dedicated_command_exists() {
        let mut attempts = Vec::new();
        let result = select_snapshot_command(|args| {
            record(&mut attempts, args);
            if args == ["status"] {
                outcome_ok()
            } else {
                outcome_unrecognized()
            }
        });
        assert!(result.is_ok());
        assert_eq!(
            attempts,
            vec![
                vec!["util", "snapshot"],
                vec!["debug", "snapshot"],
                vec!["status"]
            ]
        );
    }

    #[test]
    fn a_real_failure_stops_the_chain_instead_of_masking_it_with_the_next_candidate() {
        let mut attempts = Vec::new();
        let result = select_snapshot_command(|args| {
            record(&mut attempts, args);
            outcome_real_failure()
        });
        let err = result.unwrap_err();
        assert!(err.to_string().contains("repository is corrupt"));
        assert_eq!(
            attempts,
            vec![vec!["util", "snapshot"]],
            "a real failure must not fall through to later candidates"
        );
    }

    #[test]
    fn every_candidate_unrecognized_reports_a_clear_final_error() {
        let result = select_snapshot_command(|_| outcome_unrecognized());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("no working-copy snapshot command"));
    }

    // ---- parse_op_log ---------------------------------------------------

    #[test]
    fn parses_snapshot_ops_and_drops_interleaved_non_snapshot_operations() {
        let sep = JjRepo::OP_LOG_FIELD_SEP;
        let text = format!(
            "b05cf263a89c{sep}1780000004{sep}snapshot working copy\n\
             0f31486b1b80{sep}1780000003{sep}import git head\n\
             a1a1a1a1a1a1{sep}1780000002{sep}snapshot working copy\n\
             dc87abe56558{sep}1780000001{sep}add workspace 'default'\n"
        );
        let ops = parse_op_log(&text);
        assert_eq!(
            ops,
            vec![
                SnapshotOp {
                    op_id: "b05cf263a89c".to_owned(),
                    time_unix: 1780000004,
                    description: "snapshot working copy".to_owned(),
                },
                SnapshotOp {
                    op_id: "a1a1a1a1a1a1".to_owned(),
                    time_unix: 1780000002,
                    description: "snapshot working copy".to_owned(),
                },
            ]
        );
    }

    /// A user's own `jj describe -m '...'` (or any other operation with a
    /// non-English, non-ASCII description) must neither be misclassified as
    /// a snapshot nor corrupt parsing of the lines around it — the `\x1f`
    /// separator is ASCII, so splitting on it is safe regardless of what
    /// multi-byte UTF-8 text sits in the description field.
    #[test]
    fn japanese_description_on_a_non_snapshot_op_is_filtered_out_without_corrupting_neighbors() {
        let sep = JjRepo::OP_LOG_FIELD_SEP;
        let text = format!(
            "cccccccccccc{sep}1780000003{sep}snapshot working copy\n\
             bbbbbbbbbbbb{sep}1780000002{sep}テスト機能を追加\n\
             aaaaaaaaaaaa{sep}1780000001{sep}snapshot working copy\n"
        );
        let ops = parse_op_log(&text);
        assert_eq!(
            ops.iter().map(|o| o.op_id.as_str()).collect::<Vec<_>>(),
            vec!["cccccccccccc", "aaaaaaaaaaaa"]
        );
    }

    #[test]
    fn empty_op_log_parses_to_no_snapshots() {
        assert_eq!(parse_op_log(""), Vec::new());
    }

    #[test]
    fn a_line_missing_fields_is_skipped_rather_than_panicking() {
        let sep = JjRepo::OP_LOG_FIELD_SEP;
        let text = format!("onlyonefield\nvalid{sep}123{sep}snapshot working copy\n");
        let ops = parse_op_log(&text);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op_id, "valid");
    }

    // ---- strip_op_diff_wrapper -------------------------------------------

    /// Canned output shaped exactly like real `jj op diff --from X --to Y
    /// --git --no-graph --ignore-working-copy` on jj 0.43 (captured against
    /// a real colocated test repo while building this module).
    const CANNED_OP_DIFF: &str = "From operation: 0f31486b1b80 (2026-08-05 19:29:57) import git head\n  To operation: b05cf263a89c (2026-08-05 19:30:04) snapshot working copy\n\nChanged commits:\n+ skxxxurs b6fccfd6 (no description set)\n- skxxxurs/1 46f782c9 (hidden) (empty) (no description set)\ndiff --git a/a.txt b/a.txt\nindex ce01362503..77b7a359f3 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1,1 +1,2 @@\n hello\n+edit1\n\nChanged working copy default@:\n+ skxxxurs b6fccfd6 (no description set)\n- skxxxurs/1 46f782c9 (hidden) (empty) (no description set)\n";

    #[test]
    fn strips_the_trailing_changed_working_copy_section() {
        let stripped = strip_op_diff_wrapper(CANNED_OP_DIFF);
        assert!(!stripped.contains("Changed working copy"));
        assert!(
            stripped.trim_end().ends_with("+edit1"),
            "stripped:\n{stripped}"
        );
    }

    /// The stripped text is what actually matters: it must parse cleanly
    /// with the existing unified-diff parser, with none of the wrapper's
    /// `+`/`-` summary lines leaking into the last hunk as spurious rows.
    #[test]
    fn stripped_output_parses_correctly_with_the_existing_diff_parser() {
        let stripped = strip_op_diff_wrapper(CANNED_OP_DIFF);
        let files = crate::diff::parse_unified_diff(stripped);
        assert_eq!(files.len(), 1);
        let hunk = &files[0].hunks[0];
        assert_eq!(hunk.rows.len(), 2, "rows: {:?}", hunk.rows);
        assert_eq!(hunk.rows[1].text, "edit1");
    }

    #[test]
    fn leaves_output_with_no_trailing_section_unchanged() {
        let raw = "diff --git a/a.txt b/a.txt\n@@ -1,1 +1,1 @@\n-x\n+y\n";
        assert_eq!(strip_op_diff_wrapper(raw), raw);
    }

    // ---- detect -----------------------------------------------------------

    #[test]
    fn detect_requires_a_dot_jj_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(JjRepo::detect(dir.path(), PathBuf::from("/usr/bin/jj")).is_none());

        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        let repo = JjRepo::detect(dir.path(), PathBuf::from("/usr/bin/jj"));
        assert!(repo.is_some());
        assert_eq!(repo.unwrap().repo_root(), dir.path());
    }
}
