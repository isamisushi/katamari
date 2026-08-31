//! M16 gave `ktmr diff` a first-comment prompt that installs the
//! `katamari-review` skill. M17 extends both the harness [`install`]
//! writes and this prompt's gating to cover the rest of it —
//! `<repo_root>/AGENTS.md` (a marked katamari section, coexisting with
//! whatever else is already there) and `<repo_root>/CLAUDE.md` (a relative
//! symlink to it) — so an agent working in a katamari-reviewed repo finds
//! the same instructions whether it reads `AGENTS.md` directly or through
//! Claude Code's own `CLAUDE.md` convention.
//!
//! This file is the end-to-end proof, through the real compiled binary,
//! that: the prompt now waits for the *full* harness, not just the skill,
//! before deciding a repo is done; `y` writes all four pieces with the
//! exact shapes `crate::skill::install` promises (not just presence —
//! symlink targets and file/symlink kind, via `std::fs`
//! metadata/`read_link`, the same way the M16 suite already checked the
//! skill half); a pre-existing `AGENTS.md`/`CLAUDE.md` are respected the
//! way `crate::skill`'s own unit tests describe; and the CLI command is
//! idempotent end-to-end, not just at the `crate::skill::install` unit
//! level.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// The real bundled `SKILL.md`, read from the same file `include_str!`
/// embeds into the binary — used to confirm the installed copy is the
/// genuine current content, not just "some file exists here."
fn bundled_skill_md() -> String {
    fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/skills/katamari-review/SKILL.md"
    ))
    .expect("skills/katamari-review/SKILL.md must exist in the workspace")
}

/// A wide terminal for every prompt-driving test in this file: the
/// prompt's status-bar text is long enough — now naming all three
/// link/write outcomes, not just one — to wrap on the suite's usual
/// 100-column default, and a wrapped line would insert a newline into the
/// middle of whatever substring a test waits for.
fn wide_spawn_options() -> SpawnOptions {
    SpawnOptions {
        cols: 220,
        rows: 40,
        args: vec!["diff"],
        ..Default::default()
    }
}

/// Drives the compose overlay to save one throwaway comment, `c`
/// opens it, types `body`, `C-s` saves. Shared by every prompt test below
/// since the prompt only ever fires off the back of a real save.
///
/// A session's cursor starts on row 0, which in every fixture this file
/// uses is a file-header row — not a `Context`/`Add` line, so
/// `App::comment_target` would refuse it (see that method's docs). Three
/// `j` presses walks onto `README.md`'s second hunk-body line (a blank
/// `Context` row, still eligible), the same three-row offset in every
/// fixture here since they all start with `basic_repo`'s unchanged
/// `README.md` hunk.
fn save_a_comment(h: &Harness, body: &str) {
    for _ in 0..3 {
        h.send(Key::Char('j'));
    }
    h.send(Key::Char('c'));
    h.wait_for_text("C-s save"); // the compose overlay's own hint line
    for c in body.chars() {
        h.send(Key::Char(c));
    }
    h.send(Key::CtrlS);
}

/// Hand-builds the skill half of the layout `crate::skill::install` would
/// produce, without going through the binary — for a test that needs a
/// repo to already have the skill (but deliberately nothing else)
/// installed before `ktmr diff` even starts. Kept deliberately simple (no
/// migration/idempotency handling): that behavior is `crate::skill`'s own
/// job and is covered by its unit tests, not by this E2E suite.
fn preinstall_skill(repo_root: &Path) {
    let agents_dir = repo_root
        .join(".agents")
        .join("skills")
        .join("katamari-review");
    fs::create_dir_all(&agents_dir).unwrap();
    fs::write(agents_dir.join("SKILL.md"), bundled_skill_md()).unwrap();

    let claude_skills_dir = repo_root.join(".claude").join("skills");
    fs::create_dir_all(&claude_skills_dir).unwrap();
    std::os::unix::fs::symlink(
        "../../.agents/skills/katamari-review",
        claude_skills_dir.join("katamari-review"),
    )
    .unwrap();
}

/// As [`preinstall_skill`], but the *complete* M17 harness: skill,
/// `AGENTS.md`, and `CLAUDE.md` — for a test that needs the prompt gate to
/// see nothing missing at all.
fn preinstall_full_harness(repo_root: &Path) {
    preinstall_skill(repo_root);
    fs::write(
        repo_root.join("AGENTS.md"),
        "<!-- katamari:begin -->\nplaceholder katamari section\n<!-- katamari:end -->\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("AGENTS.md", repo_root.join("CLAUDE.md")).unwrap();
}

/// Runs the real `ktmr skill install` CLI subcommand directly (no PTY —
/// it's a plain, non-interactive command that prints and exits) in
/// `repo_root`.
fn run_skill_install_cli(repo_root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .args(["skill", "install"])
        .current_dir(repo_root)
        .output()
        .expect("failed to spawn ktmr skill install")
}

/// As [`preinstall_skill`], but named for the `--user` tests below, whose
/// argument stands in for `$HOME` rather than a repo root — the on-disk
/// layout `install_skill_files` produces is identical either way (see its
/// docs), so this is deliberately just a differently-named alias, kept
/// separate for readability at each call site.
fn preinstall_user_skill(home: &Path) {
    preinstall_skill(home);
}

/// Runs the real `ktmr skill install --user` CLI subcommand directly, with
/// `home` as `$HOME` and `cwd` as the working directory — `cwd` is
/// deliberately free to be a non-git directory in every caller below,
/// since working from outside any repo is the point of `--user`.
fn run_skill_install_user_cli(cwd: &Path, home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .args(["skill", "install", "--user"])
        .current_dir(cwd)
        .env("HOME", home)
        .output()
        .expect("failed to spawn ktmr skill install --user")
}

#[test]
fn saving_the_first_comment_in_an_uninstalled_repo_offers_the_full_harness_and_y_installs_it() {
    let repo = fixture::basic_repo();
    let h = Harness::spawn(repo.path(), wide_spawn_options());
    h.wait_for_text("todo.txt");

    save_a_comment(&h, "please handle the empty-input case too");

    // The augmented status note — not just "comment: saved" alone — proves
    // the prompt actually fired for this (uninstalled) repo.
    h.wait_for_text("comment: saved");
    h.wait_for_text("press y to install the Claude Code review skill (ktmr skill install)");

    // None of the four pieces exists on disk yet — the prompt only primes
    // the next keypress, it never writes anything by itself.
    assert!(!repo.path().join(".agents").exists());
    assert!(!repo.path().join(".claude").exists());
    assert!(!repo.path().join("AGENTS.md").exists());
    assert!(!repo.path().join("CLAUDE.md").exists());

    h.send(Key::Char('y'));

    // Each of the three link/write outcomes' `Display` names exactly this
    // — see `skill::LinkOutcome`/`AgentsMdOutcome`/`ClaudeMdOutcome`'s docs.
    h.wait_for_text(
        "skill: linked .claude/skills/katamari-review -> ../../.agents/skills/katamari-review",
    );
    h.wait_for_text("wrote AGENTS.md");
    h.wait_for_text("linked CLAUDE.md -> AGENTS.md");

    // Piece 1: the real skill content, a genuine file (not a symlink).
    let agents_skill_md = repo
        .path()
        .join(".agents")
        .join("skills")
        .join("katamari-review")
        .join("SKILL.md");
    let skill_md_meta = fs::symlink_metadata(&agents_skill_md).unwrap();
    assert!(skill_md_meta.is_file());
    assert!(!skill_md_meta.is_symlink());
    assert_eq!(
        fs::read_to_string(&agents_skill_md).unwrap(),
        bundled_skill_md()
    );

    // Piece 2: `.claude/skills/katamari-review` is a symlink with the
    // exact relative target `crate::skill::install` promises.
    let claude_dest = repo
        .path()
        .join(".claude")
        .join("skills")
        .join("katamari-review");
    let claude_dest_meta = fs::symlink_metadata(&claude_dest).unwrap();
    assert!(claude_dest_meta.is_symlink());
    assert_eq!(
        fs::read_link(&claude_dest).unwrap(),
        Path::new("../../.agents/skills/katamari-review"),
    );
    assert_eq!(
        fs::read_to_string(claude_dest.join("SKILL.md")).unwrap(),
        bundled_skill_md(),
        "the symlink must actually resolve to the real SKILL.md content"
    );

    // Piece 3: `AGENTS.md` contains the marked katamari section.
    let agents_md_path = repo.path().join("AGENTS.md");
    let agents_md_meta = fs::symlink_metadata(&agents_md_path).unwrap();
    assert!(agents_md_meta.is_file());
    assert!(!agents_md_meta.is_symlink());
    let agents_md = fs::read_to_string(&agents_md_path).unwrap();
    assert!(agents_md.contains("<!-- katamari:begin -->"));
    assert!(agents_md.contains("<!-- katamari:end -->"));
    assert!(agents_md.contains("ktmr comments list --json"));
    assert!(agents_md.contains("ktmr comments resolve"));
    assert!(agents_md.contains(".agents/skills/katamari-review/SKILL.md"));

    // Piece 4: `CLAUDE.md` is a relative symlink to `AGENTS.md`, resolving
    // to that exact content.
    let claude_md_path = repo.path().join("CLAUDE.md");
    let claude_md_meta = fs::symlink_metadata(&claude_md_path).unwrap();
    assert!(claude_md_meta.is_symlink());
    assert_eq!(
        fs::read_link(&claude_md_path).unwrap(),
        Path::new("AGENTS.md"),
        "CLAUDE.md must be a relative symlink, not absolute or a copy"
    );
    assert_eq!(fs::read_to_string(&claude_md_path).unwrap(), agents_md);
}

#[test]
fn dismissing_the_prompt_with_another_key_both_clears_it_and_still_acts_on_that_key() {
    let repo = fixture::basic_repo();
    let h = Harness::spawn(repo.path(), wide_spawn_options());
    h.wait_for_text("todo.txt");

    save_a_comment(&h, "leaving this one open");
    h.wait_for_text("press y to install the Claude Code review skill");

    // `j` (cursor down) is not `y`: it must dismiss the prompt *and* still
    // move the cursor, per the "dismiss-and-process" design — a reviewer
    // shouldn't need a wasted keypress just to clear the prompt before
    // their next real action takes effect.
    h.send(Key::Char('j'));
    h.wait_until(std::time::Duration::from_secs(3), |screen| {
        !screen.contents().contains("press y to install")
    });

    // Nothing was installed — a dismiss must never silently write anything,
    // for any of the four pieces.
    assert!(!repo.path().join(".agents").exists());
    assert!(!repo.path().join(".claude").exists());
    assert!(!repo.path().join("AGENTS.md").exists());
    assert!(!repo.path().join("CLAUDE.md").exists());
}

#[test]
fn a_repo_with_only_the_skill_preinstalled_still_offers_the_prompt_for_the_rest() {
    // M17 extended the prompt's gate from "is the skill installed" to "is
    // the *whole* harness installed" (see `skill::harness_installed`'s
    // docs) — a repo that only ever ran an older katamari's `ktmr skill
    // install`, before AGENTS.md/CLAUDE.md existed, must still be offered
    // the rest exactly once. This supersedes M16's
    // `a_repo_that_already_has_the_skill_never_shows_the_prompt`, whose
    // premise (skill alone means "fully installed") M17 deliberately
    // changed.
    let repo = fixture::basic_repo();
    preinstall_skill(repo.path());

    let h = Harness::spawn(repo.path(), wide_spawn_options());
    h.wait_for_text("todo.txt");

    save_a_comment(&h, "skill already here, but not the rest");
    h.wait_for_text("comment: saved");
    h.wait_for_text("press y to install the Claude Code review skill");

    h.send(Key::Char('y'));
    // The skill link is already correct and the AGENTS.md/CLAUDE.md pieces
    // are freshly written — a mix of "already" and "wrote"/"linked",
    // proving install ran against the real partial state rather than
    // blindly redoing everything.
    h.wait_for_text(".claude/skills/katamari-review already linked correctly");
    h.wait_for_text("wrote AGENTS.md");
    h.wait_for_text("linked CLAUDE.md -> AGENTS.md");

    assert!(repo.path().join("AGENTS.md").is_file());
    assert_eq!(
        fs::read_link(repo.path().join("CLAUDE.md")).unwrap(),
        Path::new("AGENTS.md")
    );
}

#[test]
fn a_repo_with_the_full_harness_already_installed_never_shows_the_prompt() {
    let repo = fixture::basic_repo();
    preinstall_full_harness(repo.path());

    let h = Harness::spawn(repo.path(), wide_spawn_options());
    h.wait_for_text("todo.txt");

    save_a_comment(&h, "already have everything, thanks");
    h.wait_for_text("comment: saved");

    assert!(
        !h.screen_contents()
            .contains("install the Claude Code review skill"),
        "a repo with the complete harness must never be offered the prompt"
    );
}

#[test]
fn cli_install_preserves_a_custom_agents_md_and_leaves_a_real_claude_md_untouched() {
    let repo = fixture::basic_repo();
    let custom_agents_md =
        "# Contributor notes\n\nBuild with `cargo build`.\nRun tests with `cargo test`.\n";
    fs::write(repo.path().join("AGENTS.md"), custom_agents_md).unwrap();
    let custom_claude_md =
        "# Claude-specific notes\n\nSomething this project already wrote here.\n";
    fs::write(repo.path().join("CLAUDE.md"), custom_claude_md).unwrap();

    let output = run_skill_install_cli(repo.path());
    assert!(
        output.status.success(),
        "ktmr skill install failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("AGENTS.md exists; appended katamari section"));
    assert!(stdout.contains("warning: CLAUDE.md already exists and points elsewhere"));

    // The custom content is preserved exactly, with the katamari section
    // only appended after it — never replacing or reordering what was
    // already there.
    let updated_agents_md = fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
    assert!(
        updated_agents_md.starts_with(custom_agents_md),
        "custom AGENTS.md content must come first, byte-for-byte: {updated_agents_md:?}"
    );
    assert!(updated_agents_md.contains("<!-- katamari:begin -->"));
    assert!(updated_agents_md.contains("<!-- katamari:end -->"));

    // A real pre-existing CLAUDE.md is foreign territory — left completely
    // untouched, byte-identical to what the test wrote.
    assert_eq!(
        fs::read_to_string(repo.path().join("CLAUDE.md")).unwrap(),
        custom_claude_md,
        "a real pre-existing CLAUDE.md must never be modified"
    );
    assert!(
        !repo
            .path()
            .join("CLAUDE.md")
            .symlink_metadata()
            .unwrap()
            .is_symlink(),
        "it must still be the real file, not replaced by a symlink"
    );
}

#[test]
fn cli_install_run_twice_is_idempotent() {
    let repo = fixture::basic_repo();

    let first = run_skill_install_cli(repo.path());
    assert!(first.status.success());
    let first_stdout = String::from_utf8_lossy(&first.stdout).into_owned();
    assert!(first_stdout.contains("wrote"));
    assert!(first_stdout.contains("linked .claude/skills/katamari-review"));
    assert!(first_stdout.contains("wrote AGENTS.md"));
    assert!(first_stdout.contains("linked CLAUDE.md -> AGENTS.md"));

    let agents_skill_md_path = repo
        .path()
        .join(".agents")
        .join("skills")
        .join("katamari-review")
        .join("SKILL.md");
    let agents_md_path = repo.path().join("AGENTS.md");
    let skill_md_after_first = fs::read_to_string(&agents_skill_md_path).unwrap();
    let agents_md_after_first = fs::read_to_string(&agents_md_path).unwrap();
    let claude_link_after_first = fs::read_link(repo.path().join("CLAUDE.md")).unwrap();

    let second = run_skill_install_cli(repo.path());
    assert!(second.status.success());
    let second_stdout = String::from_utf8_lossy(&second.stdout).into_owned();
    assert!(
        second_stdout.contains("already up to date"),
        "second run's SKILL.md line should report no change: {second_stdout}"
    );
    assert!(second_stdout.contains(".claude/skills/katamari-review already linked correctly"));
    assert!(second_stdout.contains("AGENTS.md already up to date"));
    assert!(second_stdout.contains("CLAUDE.md already linked correctly"));

    // Every file is byte-identical to after the first run — a second,
    // already-current install must never rewrite anything.
    assert_eq!(
        fs::read_to_string(&agents_skill_md_path).unwrap(),
        skill_md_after_first
    );
    assert_eq!(
        fs::read_to_string(&agents_md_path).unwrap(),
        agents_md_after_first
    );
    assert_eq!(
        fs::read_link(repo.path().join("CLAUDE.md")).unwrap(),
        claude_link_after_first
    );
}

// --- --user -----------------------------------------------------------

#[test]
fn cli_user_install_writes_into_home_not_cwd_and_skips_agents_claude_md() {
    let home = tempfile::tempdir().unwrap();
    // Deliberately not a git repo — `--user` must work from anywhere.
    let cwd = tempfile::tempdir().unwrap();

    let output = run_skill_install_user_cli(cwd.path(), home.path());
    assert!(
        output.status.success(),
        "ktmr skill install --user failed outside a git repo:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("wrote"));
    assert!(
        stdout.contains(
            "linked .claude/skills/katamari-review -> ../../.agents/skills/katamari-review"
        )
    );

    // The skill landed under $HOME, in the exact shape `crate::skill`
    // promises.
    let agents_skill_md = home
        .path()
        .join(".agents")
        .join("skills")
        .join("katamari-review")
        .join("SKILL.md");
    assert_eq!(
        fs::read_to_string(&agents_skill_md).unwrap(),
        bundled_skill_md()
    );
    let claude_dest = home
        .path()
        .join(".claude")
        .join("skills")
        .join("katamari-review");
    assert!(fs::symlink_metadata(&claude_dest).unwrap().is_symlink());
    assert_eq!(
        fs::read_link(&claude_dest).unwrap(),
        Path::new("../../.agents/skills/katamari-review"),
    );

    // Nothing landed under cwd, and no AGENTS.md/CLAUDE.md anywhere — the
    // whole point of `--user`.
    assert!(!cwd.path().join(".agents").exists());
    assert!(!cwd.path().join(".claude").exists());
    assert!(!home.path().join("AGENTS.md").exists());
    assert!(!home.path().join("CLAUDE.md").exists());
    assert!(!cwd.path().join("AGENTS.md").exists());
    assert!(!cwd.path().join("CLAUDE.md").exists());
}

#[test]
fn cli_user_install_run_twice_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();

    let first = run_skill_install_user_cli(cwd.path(), home.path());
    assert!(first.status.success());
    let first_stdout = String::from_utf8_lossy(&first.stdout).into_owned();
    assert!(first_stdout.contains("wrote"));

    let second = run_skill_install_user_cli(cwd.path(), home.path());
    assert!(second.status.success());
    let second_stdout = String::from_utf8_lossy(&second.stdout).into_owned();
    assert!(
        second_stdout.contains("already up to date"),
        "second run's SKILL.md line should report no change: {second_stdout}"
    );
    assert!(second_stdout.contains(".claude/skills/katamari-review already linked correctly"));
}

#[test]
fn cli_user_install_without_home_fails_with_a_clear_message() {
    let cwd = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .args(["skill", "install", "--user"])
        .current_dir(cwd.path())
        .env_remove("HOME")
        .output()
        .expect("failed to spawn ktmr skill install --user");

    assert!(
        !output.status.success(),
        "must fail without $HOME:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("$HOME is not set"),
        "expected the $HOME error message: {stderr}"
    );

    // A failed lookup must never write anything.
    assert!(!cwd.path().join(".agents").exists());
    assert!(!cwd.path().join(".claude").exists());
}

#[test]
fn saving_the_first_comment_never_offers_the_prompt_when_home_already_has_the_skill() {
    // A repo with nothing installed of its own, but a `$HOME` that already
    // has the skill from an earlier `ktmr skill install --user` — the
    // prompt gate (`ui::mod`'s event loop) must treat that as "already
    // done" and never offer a redundant per-repo copy.
    let repo = fixture::basic_repo();
    let user_home = tempfile::tempdir().unwrap();
    preinstall_user_skill(user_home.path());

    let mut opts = wide_spawn_options();
    // Harness::spawn seeds its own per-test $HOME first; this later entry
    // in extra_env overrides it, so the spawned session sees `user_home`
    // as $HOME instead.
    opts.extra_env
        .push(("HOME".into(), user_home.path().as_os_str().to_owned()));
    let h = Harness::spawn(repo.path(), opts);
    h.wait_for_text("todo.txt");

    save_a_comment(&h, "already inherited the skill from $HOME");
    h.wait_for_text("comment: saved");

    assert!(
        !h.screen_contents()
            .contains("install the Claude Code review skill"),
        "a repo whose $HOME already has the skill must never be offered the per-repo prompt"
    );

    // The repo itself stays untouched — the skill lives only under
    // $HOME, never copied into the repo just because a comment was saved.
    assert!(!repo.path().join(".agents").exists());
    assert!(!repo.path().join(".claude").exists());
    assert!(!repo.path().join("AGENTS.md").exists());
    assert!(!repo.path().join("CLAUDE.md").exists());
}
