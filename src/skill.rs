//! Installs katamari's tool-agnostic review harness into a repository — the
//! shared mechanism behind both `ktmr skill install` (see
//! `main.rs::run_skill_install`) and the TUI's one-time first-comment prompt
//! (see `ui::mod`'s `event_loop`). Three pieces, all managed by one
//! [`install`] call:
//!
//! - The `katamari-review` skill itself: real files at
//!   `<repo_root>/.agents/skills/katamari-review/` — deliberately not under
//!   `.claude/`, so any other agent tool that adopts the same `.agents/`
//!   convention picks the skill up for free without a second install path —
//!   with `.claude/skills/katamari-review` kept as a *relative* symlink into
//!   that directory (`../../.agents/skills/katamari-review`). Relative so a
//!   cloned or relocated checkout never breaks it; a symlink rather than a
//!   copy so there is exactly one place this content is ever written, which
//!   [`install`] refreshing keeps in sync automatically.
//! - `<repo_root>/AGENTS.md`: a marked katamari section (see
//!   [`ensure_agents_md`]) added to — never replacing — whatever else a repo
//!   keeps there, since `AGENTS.md` is shared real estate other tools and
//!   humans write to as well.
//! - `<repo_root>/CLAUDE.md`: a relative symlink to `AGENTS.md` (see
//!   [`ensure_claude_md_link`]), so Claude Code picks up the same
//!   instructions every other `AGENTS.md`-reading tool does, from the one
//!   file that's actually maintained.
//!
//! Symlinks are a POSIX filesystem feature; this module only works on
//! Unix-like platforms (macOS/Linux — every target this project ships for).
//! On anything else, [`install`] reports a clear error rather than silently
//! doing nothing or panicking.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The bundled skill content, embedded at compile time so `ktmr skill
/// install` (and the first-comment prompt) work from the installed binary
/// alone — no separate asset to ship or locate on disk.
pub const SKILL_MD: &str = include_str!("../skills/katamari-review/SKILL.md");

/// Repo-relative path to the real skill files — the single source of truth
/// every agent tool's own integration should point at.
const AGENTS_REL: &str = ".agents/skills/katamari-review";

/// Repo-relative path to the Claude-Code-specific entry point: either the
/// symlink this module manages, or (pre-migration) a real directory from
/// the layout that predates this module.
const CLAUDE_REL: &str = ".claude/skills/katamari-review";

/// The exact link target [`install`] writes for `CLAUDE_REL`: `../../` walks
/// back out of `.claude/skills/` to the repo root, then down into
/// [`AGENTS_REL`]. Compared byte-for-byte against `readlink` on every
/// install to decide whether an existing symlink is already correct — see
/// [`install`]'s docs on why that comparison, not just "is this a symlink at
/// all," is what "already installed" means here.
const SYMLINK_TARGET: &str = "../../.agents/skills/katamari-review";

/// The exact link target [`ensure_claude_md_link`] writes for `CLAUDE.md`:
/// both it and `AGENTS.md` live directly in `repo_root`, so — unlike
/// [`SYMLINK_TARGET`], which walks back out of two nested directories —
/// this is just the bare filename.
const CLAUDE_MD_TARGET: &str = "AGENTS.md";

/// The marker pair [`ensure_agents_md`] uses to find (and only ever touch)
/// its own section of a target repo's `AGENTS.md`, however much other
/// content — from other tools, or hand-written — surrounds it.
const AGENTS_MD_BEGIN: &str = "<!-- katamari:begin -->";
const AGENTS_MD_END: &str = "<!-- katamari:end -->";

/// The katamari section [`ensure_agents_md`] writes into a target repo's
/// `AGENTS.md`, markers included — kept as one literal (not
/// `include_str!`ed the way [`SKILL_MD`] is) since, unlike the full skill
/// body, it's short enough that a separate asset file would be pure
/// indirection. Points at the real skill for the full workflow rather than
/// duplicating it, since `AGENTS.md` is meant to be a quick pointer, not a
/// second copy of the same instructions to keep in sync.
const AGENTS_MD_SECTION: &str = "<!-- katamari:begin -->
## Reviewing with katamari

This repo is reviewed with [katamari](https://github.com/) (`ktmr`), a
terminal diff-review tool. A human reviewer leaves comments anchored to
file/line positions, stored in `.katamari/comments.jsonl`. When asked to
address review feedback:

1. `ktmr comments list --json` — list open comments (one JSON object per
   line).
2. Make the requested change for each one.
3. `ktmr comments resolve <id>` — mark it resolved; a live `ktmr diff`
   session picks this up immediately, no restart needed.

You can leave your own comments the same way, e.g. to flag something you
noticed but aren't fixing now: `ktmr comments add <file> <line> <body>`.

Full workflow, JSON shape, and other commands:
`.agents/skills/katamari-review/SKILL.md`.
<!-- katamari:end -->";

/// What [`install`] did with `.claude/skills/katamari-review` — the part of
/// the layout with more than one possible starting state (unlike
/// `.agents/skills/katamari-review`, which is always just "write the current
/// content").
#[derive(Debug)]
pub enum LinkOutcome {
    /// Nothing was there before; a fresh relative symlink was created.
    Created,
    /// A symlink was already there and already pointed at [`SYMLINK_TARGET`]
    /// — left untouched.
    AlreadyLinked,
    /// `.claude/skills/katamari-review` was a real directory (the layout
    /// this module replaces) rather than a symlink. Its contents were moved
    /// aside to `backup` — preserving whatever was there, including any
    /// hand-edited `SKILL.md`, rather than silently discarding it — and a
    /// fresh symlink to the current embedded skill was created in its
    /// place.
    MigratedLegacyDir { backup: PathBuf },
    /// Something was already at `.claude/skills/katamari-review` that isn't
    /// the symlink this module would have created — a symlink pointing
    /// somewhere else, or an ordinary file. Left completely alone: it might
    /// be a reviewer's own customization, and silently replacing it would
    /// destroy that. `target` is a best-effort description (the symlink's
    /// target, or the path itself) for the warning callers print.
    ForeignEntryLeftAlone { target: PathBuf },
}

impl fmt::Display for LinkOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkOutcome::Created => {
                write!(f, "linked {CLAUDE_REL} -> {SYMLINK_TARGET}")
            }
            LinkOutcome::AlreadyLinked => {
                write!(f, "{CLAUDE_REL} already linked correctly")
            }
            LinkOutcome::MigratedLegacyDir { backup } => {
                write!(
                    f,
                    "migrated legacy {CLAUDE_REL} directory to the new layout \
                     (old contents moved to {})",
                    backup.display()
                )
            }
            LinkOutcome::ForeignEntryLeftAlone { target } => {
                write!(
                    f,
                    "warning: {CLAUDE_REL} already exists and points elsewhere \
                     ({}) — left untouched",
                    target.display()
                )
            }
        }
    }
}

/// What [`ensure_agents_md`] did to a target repo's `AGENTS.md` — the
/// `AGENTS.md` analogue of [`LinkOutcome`], except there's no "foreign
/// entry" case: unlike `.claude/skills/katamari-review`, `AGENTS.md` is
/// shared real estate other tools and humans already write to, so the only
/// thing that could ever block katamari's section is the marker pair
/// itself, which [`ensure_agents_md`] always finds and updates in place
/// rather than refusing to touch a file it doesn't fully own.
#[derive(Debug)]
pub enum AgentsMdOutcome {
    /// `AGENTS.md` didn't exist (or existed but was empty/whitespace-only)
    /// — its entire content is now just the katamari section.
    Written,
    /// `AGENTS.md` existed with other content and no katamari markers — the
    /// section was appended after a blank-line separator, everything
    /// already there left exactly as it was.
    Appended,
    /// The markers were already present but wrapped stale content (an older
    /// katamari version's wording) — replaced in place with the current
    /// section; everything outside the markers untouched.
    Refreshed,
    /// The markers were already present and already current — no write.
    UpToDate,
}

impl fmt::Display for AgentsMdOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentsMdOutcome::Written => write!(f, "wrote AGENTS.md"),
            AgentsMdOutcome::Appended => {
                write!(f, "AGENTS.md exists; appended katamari section")
            }
            AgentsMdOutcome::Refreshed => {
                write!(f, "AGENTS.md: katamari section refreshed")
            }
            AgentsMdOutcome::UpToDate => write!(f, "AGENTS.md already up to date"),
        }
    }
}

/// What [`ensure_claude_md_link`] did with `<repo_root>/CLAUDE.md` — the
/// same three-way split as [`LinkOutcome`] minus the legacy-directory
/// migration case, which has no `CLAUDE.md` equivalent (there is no
/// pre-M17 layout to migrate away from).
#[derive(Debug)]
pub enum ClaudeMdOutcome {
    /// Nothing was there before; a fresh relative symlink to `AGENTS.md`
    /// was created.
    Created,
    /// A symlink was already there and already pointed at
    /// [`CLAUDE_MD_TARGET`] — left untouched.
    AlreadyLinked,
    /// `CLAUDE.md` already exists as a real file, or as a symlink pointing
    /// somewhere else — left completely alone: it might be a project's own
    /// content (or a symlink to some other instructions file entirely), and
    /// silently replacing it would destroy that. `target` is a best-effort
    /// description (the symlink's target, or the path itself) for the
    /// warning callers print.
    ForeignEntryLeftAlone { target: PathBuf },
}

impl fmt::Display for ClaudeMdOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClaudeMdOutcome::Created => write!(f, "linked CLAUDE.md -> {CLAUDE_MD_TARGET}"),
            ClaudeMdOutcome::AlreadyLinked => write!(f, "CLAUDE.md already linked correctly"),
            ClaudeMdOutcome::ForeignEntryLeftAlone { target } => write!(
                f,
                "warning: CLAUDE.md already exists and points elsewhere \
                 ({}) — left untouched",
                target.display()
            ),
        }
    }
}

/// What one [`install`] call did, in full — everything `ktmr skill install`
/// needs to print exactly what it wrote/linked, and everything the
/// first-comment prompt needs to report success.
#[derive(Debug)]
pub struct InstallReport {
    /// Where the current `SKILL.md` content was written —
    /// `<repo_root>/.agents/skills/katamari-review/SKILL.md`.
    pub skill_md_path: PathBuf,
    /// Whether that file's content actually changed (a stale copy from an
    /// older katamari version, or none at all) — `false` on a re-run against
    /// an already-current install, so a caller can say "nothing to do" only
    /// when it's actually true.
    pub skill_md_changed: bool,
    pub link: LinkOutcome,
    /// What happened to `<repo_root>/AGENTS.md` — see [`AgentsMdOutcome`].
    pub agents_md: AgentsMdOutcome,
    /// What happened to `<repo_root>/CLAUDE.md` — see [`ClaudeMdOutcome`].
    pub claude_md: ClaudeMdOutcome,
}

/// Any failure writing the skill's files — a filesystem error at a specific
/// path, or (non-Unix only) symlinks simply not being available. Reported to
/// the CLI as a plain error and to the TUI as a status-bar note.
#[derive(Debug)]
pub struct InstallError {
    path: PathBuf,
    source: io::Error,
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.source)
    }
}

impl std::error::Error for InstallError {}

fn io_err(path: &Path, source: io::Error) -> InstallError {
    InstallError {
        path: path.to_path_buf(),
        source,
    }
}

/// Installs (or refreshes) katamari's full review harness into `repo_root`
/// — the skill, `AGENTS.md`, and `CLAUDE.md`; see the module docs for the
/// layout this produces. Idempotent: re-running against an already-current
/// install only re-checks, never rewrites anything unnecessarily (see
/// [`InstallReport::skill_md_changed`], [`LinkOutcome::AlreadyLinked`],
/// [`AgentsMdOutcome::UpToDate`], [`ClaudeMdOutcome::AlreadyLinked`]).
///
/// The `.agents/skills/katamari-review/SKILL.md` write and the `AGENTS.md`
/// section both always happen, regardless of what state
/// `.claude/skills/katamari-review` or `CLAUDE.md` are in — even when a
/// foreign entry means one of those links is left alone (see
/// [`LinkOutcome::ForeignEntryLeftAlone`] /
/// [`ClaudeMdOutcome::ForeignEntryLeftAlone`]), the content each would point
/// at stays current. Only the links themselves are ever skipped.
pub fn install(repo_root: &Path) -> Result<InstallReport, InstallError> {
    let agents_dir = repo_root.join(AGENTS_REL);
    fs::create_dir_all(&agents_dir).map_err(|e| io_err(&agents_dir, e))?;
    let skill_md_path = agents_dir.join("SKILL.md");
    let skill_md_changed = write_if_changed(&skill_md_path, SKILL_MD)?;

    let claude_skills_dir = repo_root.join(".claude").join("skills");
    fs::create_dir_all(&claude_skills_dir).map_err(|e| io_err(&claude_skills_dir, e))?;
    let claude_dest = repo_root.join(CLAUDE_REL);
    let link = ensure_claude_link(&claude_dest)?;

    // AGENTS.md always runs before the CLAUDE.md link below: if CLAUDE.md
    // doesn't exist yet, its symlink is created pointing at AGENTS.md
    // regardless of whether AGENTS.md itself needed writing — but doing
    // this second means a fresh install never has the symlink briefly
    // dangling ahead of its target existing.
    let agents_md = ensure_agents_md(repo_root)?;
    let claude_md = ensure_claude_md_link(repo_root)?;

    Ok(InstallReport {
        skill_md_path,
        skill_md_changed,
        link,
        agents_md,
        claude_md,
    })
}

/// Writes `content` to `path` unless it's already exactly that — a plain
/// read-compare-write rather than tracking a hash or mtime separately, since
/// `SKILL.md` is small enough (a few KB) that reading it back is free.
fn write_if_changed(path: &Path, content: &str) -> Result<bool, InstallError> {
    if fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(false);
    }
    fs::write(path, content).map_err(|e| io_err(path, e))?;
    Ok(true)
}

/// Resolves `claude_dest` (`.claude/skills/katamari-review`) into exactly
/// one of [`LinkOutcome`]'s four states, per the module docs — the one
/// function that has to know every prior layout this module might find on
/// disk.
fn ensure_claude_link(claude_dest: &Path) -> Result<LinkOutcome, InstallError> {
    // `symlink_metadata` (not `metadata`): a symlink must be inspected as
    // itself, not followed — following it here would make a perfectly
    // correct symlink indistinguishable from a real directory once resolved
    // (both look like "there's a directory with SKILL.md in it").
    let meta = match fs::symlink_metadata(claude_dest) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            create_symlink(claude_dest, SYMLINK_TARGET)?;
            return Ok(LinkOutcome::Created);
        }
        Err(e) => return Err(io_err(claude_dest, e)),
    };

    if meta.is_symlink() {
        let target = fs::read_link(claude_dest).map_err(|e| io_err(claude_dest, e))?;
        return Ok(if target == Path::new(SYMLINK_TARGET) {
            LinkOutcome::AlreadyLinked
        } else {
            LinkOutcome::ForeignEntryLeftAlone { target }
        });
    }

    if meta.is_dir() {
        let backup = unique_backup_path(claude_dest);
        fs::rename(claude_dest, &backup).map_err(|e| io_err(claude_dest, e))?;
        create_symlink(claude_dest, SYMLINK_TARGET)?;
        return Ok(LinkOutcome::MigratedLegacyDir { backup });
    }

    // An ordinary file sitting at this path is as foreign as a symlink
    // pointing elsewhere — never seen in practice, but the same "don't
    // destroy what's there" rule applies.
    Ok(LinkOutcome::ForeignEntryLeftAlone {
        target: claude_dest.to_path_buf(),
    })
}

/// Resolves `<repo_root>/AGENTS.md` into exactly one of
/// [`AgentsMdOutcome`]'s four states. Unlike [`ensure_claude_link`], there's
/// no "foreign entry" branch here — an existing `AGENTS.md` is always a
/// file (never a directory or symlink in practice), and this function's
/// whole job is to coexist with whatever content is already in it, not to
/// refuse to touch it.
fn ensure_agents_md(repo_root: &Path) -> Result<AgentsMdOutcome, InstallError> {
    let path = repo_root.join("AGENTS.md");
    let existing = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io_err(&path, e)),
    };

    if let Some((start, end)) = find_marked_section(&existing) {
        if &existing[start..end] == AGENTS_MD_SECTION {
            return Ok(AgentsMdOutcome::UpToDate);
        }
        // Splices only the marked byte range, so anything before the begin
        // marker or after the end marker — including a trailing newline
        // that came after it originally — is carried through untouched.
        let mut updated = existing.clone();
        updated.replace_range(start..end, AGENTS_MD_SECTION);
        fs::write(&path, updated).map_err(|e| io_err(&path, e))?;
        return Ok(AgentsMdOutcome::Refreshed);
    }

    // `.trim().is_empty()` (not `.is_empty()`) so a repo with an
    // already-created-but-blank `AGENTS.md` (e.g. `touch AGENTS.md`) is
    // treated as "nothing here yet" rather than getting a spurious leading
    // blank line ahead of the section.
    if existing.trim().is_empty() {
        fs::write(&path, format!("{AGENTS_MD_SECTION}\n")).map_err(|e| io_err(&path, e))?;
        return Ok(AgentsMdOutcome::Written);
    }

    let mut updated = existing;
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push('\n'); // blank-line separator ahead of the appended section
    updated.push_str(AGENTS_MD_SECTION);
    updated.push('\n');
    fs::write(&path, updated).map_err(|e| io_err(&path, e))?;
    Ok(AgentsMdOutcome::Appended)
}

/// Locates the byte range `[AGENTS_MD_BEGIN, AGENTS_MD_END]` spans in
/// `text` — inclusive of both markers, so [`ensure_agents_md`] can replace
/// exactly that slice to refresh stale content without disturbing anything
/// outside it. `None` if no katamari section is present yet.
fn find_marked_section(text: &str) -> Option<(usize, usize)> {
    let start = text.find(AGENTS_MD_BEGIN)?;
    let end_marker_start = text[start..].find(AGENTS_MD_END)? + start;
    Some((start, end_marker_start + AGENTS_MD_END.len()))
}

/// Resolves `<repo_root>/CLAUDE.md` into exactly one of
/// [`ClaudeMdOutcome`]'s three states — the `CLAUDE.md` analogue of
/// [`ensure_claude_link`], minus the legacy-directory case that has no
/// equivalent here.
fn ensure_claude_md_link(repo_root: &Path) -> Result<ClaudeMdOutcome, InstallError> {
    let claude_md = repo_root.join("CLAUDE.md");
    // `symlink_metadata`, not `metadata`, for the same reason
    // `ensure_claude_link` uses it — a symlink must be inspected as itself,
    // not followed.
    let meta = match fs::symlink_metadata(&claude_md) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            create_symlink(&claude_md, CLAUDE_MD_TARGET)?;
            return Ok(ClaudeMdOutcome::Created);
        }
        Err(e) => return Err(io_err(&claude_md, e)),
    };

    if meta.is_symlink() {
        let target = fs::read_link(&claude_md).map_err(|e| io_err(&claude_md, e))?;
        return Ok(if target == Path::new(CLAUDE_MD_TARGET) {
            ClaudeMdOutcome::AlreadyLinked
        } else {
            ClaudeMdOutcome::ForeignEntryLeftAlone { target }
        });
    }

    // A real file (or anything else) at CLAUDE.md is as foreign as a
    // symlink pointing elsewhere — left alone, same as `ensure_claude_link`'s
    // identical case.
    Ok(ClaudeMdOutcome::ForeignEntryLeftAlone { target: claude_md })
}

/// `<claude_dest>.pre-migration-backup`, or `-2`/`-3`/... if that's already
/// taken — covers the unlikely case of migrating the same repo more than
/// once without ever cleaning up the previous backup.
fn unique_backup_path(claude_dest: &Path) -> PathBuf {
    let parent = claude_dest
        .parent()
        .expect("claude_dest always has a parent (.claude/skills/)");
    let name = claude_dest
        .file_name()
        .expect("claude_dest always has a file name")
        .to_string_lossy();
    let mut candidate = parent.join(format!("{name}.pre-migration-backup"));
    let mut n = 2;
    while candidate.symlink_metadata().is_ok() {
        candidate = parent.join(format!("{name}.pre-migration-backup-{n}"));
        n += 1;
    }
    candidate
}

/// Creates a relative symlink at `dest` pointing at `target` — shared by
/// every symlink [`install`] creates ([`SYMLINK_TARGET`] for
/// `.claude/skills/katamari-review`, [`CLAUDE_MD_TARGET`] for `CLAUDE.md`),
/// since the platform-gated implementation is identical either way.
#[cfg(unix)]
fn create_symlink(dest: &Path, target: &str) -> Result<(), InstallError> {
    std::os::unix::fs::symlink(target, dest).map_err(|e| io_err(dest, e))
}

#[cfg(not(unix))]
fn create_symlink(dest: &Path, _target: &str) -> Result<(), InstallError> {
    Err(io_err(
        dest,
        io::Error::new(
            io::ErrorKind::Unsupported,
            "katamari's review harness needs a symlink, which this module only \
             creates on macOS/Linux; on other platforms, create the missing \
             symlink by hand (see the module docs for the exact layout)",
        ),
    ))
}

/// Whether `repo_root` already has a working `katamari-review` skill install
/// — either this module's own symlink layout or the legacy real-directory
/// one, resolved the same way for both by simply checking that
/// `.claude/skills/katamari-review/SKILL.md` reads as a real file:
/// [`Path::is_file`] follows symlinks, so a correctly-pointing symlink and a
/// legacy real directory both satisfy this identically, with no special
/// case needed for either. Used to gate the TUI's first-comment prompt (see
/// `ui::mod`'s event loop) — an already-installed repo should never be
/// offered the prompt.
pub fn skill_installed(repo_root: &Path) -> bool {
    repo_root.join(CLAUDE_REL).join("SKILL.md").is_file()
}

/// Whether `repo_root` has an `AGENTS.md` with a katamari section already
/// in it — regardless of whether that section's *content* happens to match
/// the current [`AGENTS_MD_SECTION`] verbatim. Deliberately as shallow a
/// check as [`skill_installed`]: a wording tweak in a newer katamari
/// version refreshing the section on the next explicit `ktmr skill
/// install` is enough; it shouldn't also make [`harness_installed`] start
/// reporting "not installed" and re-offering the TUI prompt.
fn agents_md_has_section(repo_root: &Path) -> bool {
    fs::read_to_string(repo_root.join("AGENTS.md"))
        .is_ok_and(|content| find_marked_section(&content).is_some())
}

/// Whether `repo_root`'s `CLAUDE.md` is a symlink pointing at
/// [`CLAUDE_MD_TARGET`] — the `CLAUDE.md` analogue of [`skill_installed`].
fn claude_md_linked(repo_root: &Path) -> bool {
    fs::read_link(repo_root.join("CLAUDE.md"))
        .is_ok_and(|target| target == Path::new(CLAUDE_MD_TARGET))
}

/// Whether `repo_root` already has the *complete* harness [`install`]
/// produces: the skill, an `AGENTS.md` katamari section, and `CLAUDE.md`
/// linked to it. This — not [`skill_installed`] alone — is what gates the
/// TUI's first-comment prompt (see `ui::mod`'s event loop), so a repo that
/// only ran an older katamari's `ktmr skill install` (before `AGENTS.md`/
/// `CLAUDE.md` were part of the harness) still gets offered the rest of it
/// once, rather than being stuck without them forever just because the
/// skill half already exists. [`install`] is fully idempotent, so
/// re-running it against a repo that's already complete is always a no-op
/// — the only cost of offering again is one keypress.
///
/// One known gap: if `.claude/skills/katamari-review` or `CLAUDE.md` is a
/// foreign entry (see [`LinkOutcome::ForeignEntryLeftAlone`] /
/// [`ClaudeMdOutcome::ForeignEntryLeftAlone`]), this can never become
/// `true` no matter how many times `install` runs, since `install`
/// deliberately never overwrites a foreign entry — so the TUI prompt would
/// offer again every session. Accepted rather than solved here: it's the
/// same tradeoff [`skill_installed`] already made for the skill-only case
/// pre-M17, and a repo with a foreign `CLAUDE.md` is by definition already
/// using that file for something else, so a once-a-session status-bar
/// mention costs little.
pub fn harness_installed(repo_root: &Path) -> bool {
    skill_installed(repo_root) && agents_md_has_section(repo_root) && claude_md_linked(repo_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn fresh_install_writes_skill_md_and_a_relative_symlink() {
        let repo = repo();
        let report = install(repo.path()).unwrap();

        assert!(report.skill_md_changed);
        assert_eq!(fs::read_to_string(&report.skill_md_path).unwrap(), SKILL_MD);
        assert!(matches!(report.link, LinkOutcome::Created));

        let claude_dest = repo.path().join(CLAUDE_REL);
        let target = fs::read_link(&claude_dest).unwrap();
        assert_eq!(
            target,
            Path::new(SYMLINK_TARGET),
            "the symlink target must be relative, not absolute, so a moved/cloned \
             repo doesn't break it"
        );

        // The symlink actually resolves to the real SKILL.md content.
        assert_eq!(
            fs::read_to_string(claude_dest.join("SKILL.md")).unwrap(),
            SKILL_MD
        );
        assert!(skill_installed(repo.path()));
    }

    #[test]
    fn a_second_install_is_idempotent() {
        let repo = repo();
        install(repo.path()).unwrap();
        let second = install(repo.path()).unwrap();

        assert!(
            !second.skill_md_changed,
            "re-running against an already-current install shouldn't rewrite SKILL.md"
        );
        assert!(matches!(second.link, LinkOutcome::AlreadyLinked));
    }

    #[test]
    fn a_stale_skill_md_is_refreshed_even_when_the_link_is_already_correct() {
        let repo = repo();
        install(repo.path()).unwrap();
        let agents_skill_md = repo.path().join(AGENTS_REL).join("SKILL.md");
        fs::write(&agents_skill_md, "stale content from an older version").unwrap();

        let report = install(repo.path()).unwrap();
        assert!(report.skill_md_changed);
        assert_eq!(fs::read_to_string(&agents_skill_md).unwrap(), SKILL_MD);
        assert!(matches!(report.link, LinkOutcome::AlreadyLinked));
    }

    #[test]
    fn a_legacy_real_directory_is_migrated_and_its_contents_preserved() {
        let repo = repo();
        let legacy_dir = repo.path().join(CLAUDE_REL);
        fs::create_dir_all(&legacy_dir).unwrap();
        fs::write(legacy_dir.join("SKILL.md"), "a reviewer's customized skill").unwrap();
        fs::write(
            legacy_dir.join("NOTES.md"),
            "extra notes only the old layout has",
        )
        .unwrap();

        let report = install(repo.path()).unwrap();
        let LinkOutcome::MigratedLegacyDir { backup } = &report.link else {
            panic!("expected MigratedLegacyDir, got {:?}", report.link);
        };

        // The old content is preserved, not discarded.
        assert_eq!(
            fs::read_to_string(backup.join("SKILL.md")).unwrap(),
            "a reviewer's customized skill"
        );
        assert_eq!(
            fs::read_to_string(backup.join("NOTES.md")).unwrap(),
            "extra notes only the old layout has"
        );

        // The new layout is in place and points at the fresh embedded skill,
        // not the old customized one.
        assert!(skill_installed(repo.path()));
        assert_eq!(
            fs::read_to_string(repo.path().join(CLAUDE_REL).join("SKILL.md")).unwrap(),
            SKILL_MD
        );
    }

    #[test]
    fn migrating_twice_never_clobbers_the_first_backup() {
        let repo = repo();
        let legacy_dir = repo.path().join(CLAUDE_REL);
        fs::create_dir_all(&legacy_dir).unwrap();
        fs::write(legacy_dir.join("SKILL.md"), "first legacy install").unwrap();
        install(repo.path()).unwrap();

        // Simulate a second legacy directory somehow reappearing (e.g. a
        // reviewer manually restored an old backup) and migrate again.
        fs::remove_file(repo.path().join(CLAUDE_REL)).unwrap(); // remove the symlink
        let legacy_dir_again = repo.path().join(CLAUDE_REL);
        fs::create_dir_all(&legacy_dir_again).unwrap();
        fs::write(legacy_dir_again.join("SKILL.md"), "second legacy install").unwrap();
        let report = install(repo.path()).unwrap();

        let LinkOutcome::MigratedLegacyDir { backup } = &report.link else {
            panic!("expected MigratedLegacyDir, got {:?}", report.link);
        };
        assert!(
            backup.to_string_lossy().contains("pre-migration-backup-2"),
            "the first backup must not be overwritten: got {}",
            backup.display()
        );
        assert_eq!(
            fs::read_to_string(backup.join("SKILL.md")).unwrap(),
            "second legacy install"
        );
    }

    #[test]
    fn a_foreign_symlink_is_left_untouched_and_reported() {
        let repo = repo();
        let claude_skills_dir = repo.path().join(".claude").join("skills");
        fs::create_dir_all(&claude_skills_dir).unwrap();
        let elsewhere = repo.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, claude_skills_dir.join("katamari-review")).unwrap();

        let report = install(repo.path()).unwrap();
        let LinkOutcome::ForeignEntryLeftAlone { target } = &report.link else {
            panic!("expected ForeignEntryLeftAlone, got {:?}", report.link);
        };
        assert_eq!(target, &elsewhere);

        // Untouched: still points at `elsewhere`, not at the new layout.
        let readback = fs::read_link(claude_skills_dir.join("katamari-review")).unwrap();
        assert_eq!(readback, elsewhere);

        // But the single source of truth still got the current skill
        // content — only the .claude-side link was left alone.
        assert_eq!(
            fs::read_to_string(repo.path().join(AGENTS_REL).join("SKILL.md")).unwrap(),
            SKILL_MD
        );

        assert!(
            !skill_installed(repo.path()),
            "a foreign symlink that doesn't resolve to a SKILL.md must not read as installed"
        );
    }

    #[test]
    fn skill_installed_is_false_before_any_install() {
        let repo = repo();
        assert!(!skill_installed(repo.path()));
    }

    #[test]
    fn skill_installed_is_true_for_a_legacy_real_dir_without_migrating_it() {
        let repo = repo();
        let legacy_dir = repo.path().join(CLAUDE_REL);
        fs::create_dir_all(&legacy_dir).unwrap();
        fs::write(legacy_dir.join("SKILL.md"), "anything").unwrap();

        assert!(skill_installed(repo.path()));
        // Merely checking must never migrate anything.
        assert!(legacy_dir.symlink_metadata().unwrap().is_dir());
    }

    // --- M17: AGENTS.md / CLAUDE.md -------------------------------------

    #[test]
    fn fresh_install_writes_agents_md_and_links_claude_md_to_it() {
        let repo = repo();
        let report = install(repo.path()).unwrap();

        assert!(matches!(report.agents_md, AgentsMdOutcome::Written));
        let agents_md = fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
        assert!(agents_md.contains(AGENTS_MD_BEGIN));
        assert!(agents_md.contains(AGENTS_MD_END));
        assert!(agents_md.contains("ktmr comments list --json"));

        assert!(matches!(report.claude_md, ClaudeMdOutcome::Created));
        let claude_md = repo.path().join("CLAUDE.md");
        let target = fs::read_link(&claude_md).unwrap();
        assert_eq!(target, Path::new("AGENTS.md"));
        assert_eq!(fs::read_to_string(&claude_md).unwrap(), agents_md);

        assert!(harness_installed(repo.path()));
    }

    #[test]
    fn install_appends_to_an_existing_agents_md_without_a_marker() {
        let repo = repo();
        let custom = "# My Project\n\nSome hand-written contributor notes.\n";
        fs::write(repo.path().join("AGENTS.md"), custom).unwrap();

        let report = install(repo.path()).unwrap();
        assert!(matches!(report.agents_md, AgentsMdOutcome::Appended));

        let updated = fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
        assert!(
            updated.starts_with(custom),
            "the pre-existing content must be preserved exactly, with the \
             katamari section only appended after it"
        );
        assert!(updated.contains(AGENTS_MD_SECTION));
    }

    #[test]
    fn install_writes_agents_md_when_the_file_exists_but_is_blank() {
        let repo = repo();
        fs::write(repo.path().join("AGENTS.md"), "   \n\n").unwrap();

        let report = install(repo.path()).unwrap();
        assert!(matches!(report.agents_md, AgentsMdOutcome::Written));
        let updated = fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
        assert!(
            !updated.starts_with('\n'),
            "a blank pre-existing file must not leave a spurious leading blank \
             line ahead of the section: {updated:?}"
        );
    }

    #[test]
    fn a_second_install_leaves_an_already_current_agents_md_untouched() {
        let repo = repo();
        install(repo.path()).unwrap();
        let before = fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();

        let second = install(repo.path()).unwrap();
        assert!(matches!(second.agents_md, AgentsMdOutcome::UpToDate));
        assert!(matches!(second.claude_md, ClaudeMdOutcome::AlreadyLinked));
        assert_eq!(
            fs::read_to_string(repo.path().join("AGENTS.md")).unwrap(),
            before
        );
    }

    #[test]
    fn install_refreshes_a_stale_agents_md_section_leaving_the_rest_alone() {
        let repo = repo();
        let content = "# My Project\n\n<!-- katamari:begin -->\nstale content from an \
             older katamari\n<!-- katamari:end -->\n\nMore notes below.\n";
        fs::write(repo.path().join("AGENTS.md"), content).unwrap();

        let report = install(repo.path()).unwrap();
        assert!(matches!(report.agents_md, AgentsMdOutcome::Refreshed));

        let updated = fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
        assert!(updated.starts_with("# My Project\n\n"));
        assert!(updated.ends_with("More notes below.\n"));
        assert!(updated.contains(AGENTS_MD_SECTION));
        assert!(!updated.contains("stale content from an older katamari"));
    }

    #[test]
    fn claude_md_left_alone_when_it_is_a_real_file() {
        let repo = repo();
        fs::write(repo.path().join("CLAUDE.md"), "hand-written instructions").unwrap();

        let report = install(repo.path()).unwrap();
        let ClaudeMdOutcome::ForeignEntryLeftAlone { target } = &report.claude_md else {
            panic!("expected ForeignEntryLeftAlone, got {:?}", report.claude_md);
        };
        assert_eq!(target, &repo.path().join("CLAUDE.md"));
        assert_eq!(
            fs::read_to_string(repo.path().join("CLAUDE.md")).unwrap(),
            "hand-written instructions",
            "a real pre-existing CLAUDE.md must never be overwritten"
        );
        assert!(!harness_installed(repo.path()));
    }

    #[test]
    fn claude_md_left_alone_when_it_symlinks_elsewhere() {
        let repo = repo();
        std::os::unix::fs::symlink("docs/OTHER.md", repo.path().join("CLAUDE.md")).unwrap();

        let report = install(repo.path()).unwrap();
        let ClaudeMdOutcome::ForeignEntryLeftAlone { target } = &report.claude_md else {
            panic!("expected ForeignEntryLeftAlone, got {:?}", report.claude_md);
        };
        assert_eq!(target, Path::new("docs/OTHER.md"));
        assert_eq!(
            fs::read_link(repo.path().join("CLAUDE.md")).unwrap(),
            Path::new("docs/OTHER.md")
        );
    }

    #[test]
    fn harness_installed_requires_all_three_pieces() {
        let repo = repo();
        assert!(!harness_installed(repo.path()));

        // Skill only (pre-M17 install): still incomplete.
        let agents_dir = repo.path().join(AGENTS_REL);
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("SKILL.md"), SKILL_MD).unwrap();
        let claude_skills_dir = repo.path().join(".claude").join("skills");
        fs::create_dir_all(&claude_skills_dir).unwrap();
        std::os::unix::fs::symlink(
            "../../.agents/skills/katamari-review",
            claude_skills_dir.join("katamari-review"),
        )
        .unwrap();
        assert!(skill_installed(repo.path()));
        assert!(!harness_installed(repo.path()));

        // A full `install` completes it.
        install(repo.path()).unwrap();
        assert!(harness_installed(repo.path()));
    }
}
