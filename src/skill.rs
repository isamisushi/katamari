//! Installs the bundled `katamari-review` Claude Code skill into a
//! repository — the shared mechanism behind both `ktmr skill install` (see
//! `main.rs::run_skill_install`) and the TUI's one-time first-comment prompt
//! (see `ui::mod`'s `event_loop`).
//!
//! The real files live at `<repo_root>/.agents/skills/katamari-review/` —
//! deliberately not under `.claude/`, so any other agent tool that adopts
//! the same `.agents/` convention picks the skill up for free without a
//! second install path. `.claude/skills/katamari-review` is a *relative*
//! symlink into that directory (`../../.agents/skills/katamari-review`) —
//! relative so a cloned or relocated checkout never breaks it, and a plain
//! symlink (not a copy) so there is exactly one place this content is ever
//! written, which [`install`] refreshing keeps in sync automatically.
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

/// Installs (or refreshes) the `katamari-review` skill into `repo_root` —
/// see the module docs for the layout this produces. Idempotent: re-running
/// against an already-current install only re-checks, never rewrites
/// anything unnecessarily (see [`InstallReport::skill_md_changed`] and
/// [`LinkOutcome::AlreadyLinked`]).
///
/// The `.agents/skills/katamari-review/SKILL.md` write always happens,
/// regardless of what state `.claude/skills/katamari-review` is in — even
/// when a foreign entry there means the `.claude` symlink is left alone (see
/// [`LinkOutcome::ForeignEntryLeftAlone`]), the single source of truth stays
/// current. Only the `.claude`-side link is ever skipped, never the content
/// it would point at.
pub fn install(repo_root: &Path) -> Result<InstallReport, InstallError> {
    let agents_dir = repo_root.join(AGENTS_REL);
    fs::create_dir_all(&agents_dir).map_err(|e| io_err(&agents_dir, e))?;
    let skill_md_path = agents_dir.join("SKILL.md");
    let skill_md_changed = write_if_changed(&skill_md_path, SKILL_MD)?;

    let claude_skills_dir = repo_root.join(".claude").join("skills");
    fs::create_dir_all(&claude_skills_dir).map_err(|e| io_err(&claude_skills_dir, e))?;
    let claude_dest = repo_root.join(CLAUDE_REL);
    let link = ensure_claude_link(&claude_dest)?;

    Ok(InstallReport {
        skill_md_path,
        skill_md_changed,
        link,
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
            create_symlink(claude_dest)?;
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
        create_symlink(claude_dest)?;
        return Ok(LinkOutcome::MigratedLegacyDir { backup });
    }

    // An ordinary file sitting at this path is as foreign as a symlink
    // pointing elsewhere — never seen in practice, but the same "don't
    // destroy what's there" rule applies.
    Ok(LinkOutcome::ForeignEntryLeftAlone {
        target: claude_dest.to_path_buf(),
    })
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

#[cfg(unix)]
fn create_symlink(dest: &Path) -> Result<(), InstallError> {
    std::os::unix::fs::symlink(SYMLINK_TARGET, dest).map_err(|e| io_err(dest, e))
}

#[cfg(not(unix))]
fn create_symlink(dest: &Path) -> Result<(), InstallError> {
    Err(io_err(
        dest,
        io::Error::new(
            io::ErrorKind::Unsupported,
            "skill install needs a symlink, which this module only creates on \
             macOS/Linux; on other platforms, copy .agents/skills/katamari-review \
             to .claude/skills/katamari-review by hand",
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
}
