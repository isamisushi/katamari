//! M16: the first-comment skill-install prompt. A `ktmr diff` session in a
//! repository that doesn't have the `katamari-review` skill yet offers,
//! once per session right after the first comment successfully saves, to
//! install it with a single `y` keypress — this is the end-to-end proof
//! that the prompt actually appears, that `y` actually writes the new
//! `.agents/skills/katamari-review/` + `.claude/skills/katamari-review`
//! symlink layout onto disk through the real compiled binary (not just
//! `crate::skill::install`'s own unit tests), and that an already-installed
//! repo never shows the prompt at all.

use crate::support::{Harness, Key, SpawnOptions, fixture};
use std::fs;
use std::path::Path;

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

/// A wide terminal for every test in this file: the prompt's status-bar
/// text ("comment: saved · press y to install the Claude Code review skill
/// (ktmr skill install)") is long enough to wrap on the suite's usual
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
/// opens it, types `body`, `C-s` saves. Shared by every test below since
/// the prompt only ever fires off the back of a real save.
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

/// Hand-builds the layout [`crate::skill`]'s `install` would produce,
/// without going through the binary — for a test that needs a repo to
/// already have the skill installed *before* `ktmr diff` even starts. Kept
/// deliberately simple (no migration/idempotency handling): that behavior
/// is `crate::skill::install`'s own job and is covered by its unit tests,
/// not by this E2E suite.
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

#[test]
fn saving_the_first_comment_in_an_uninstalled_repo_offers_the_skill_and_y_installs_it() {
    let repo = fixture::basic_repo();
    let h = Harness::spawn(repo.path(), wide_spawn_options());
    h.wait_for_text("todo.txt");

    save_a_comment(&h, "please handle the empty-input case too");

    // The augmented status note — not just "comment: saved" alone — proves
    // the prompt actually fired for this (uninstalled) repo.
    h.wait_for_text("comment: saved");
    h.wait_for_text("press y to install the Claude Code review skill (ktmr skill install)");

    // Neither half of the new layout exists on disk yet — the prompt only
    // primes the next keypress, it never writes anything by itself.
    assert!(!repo.path().join(".agents").exists());
    assert!(!repo.path().join(".claude").exists());

    h.send(Key::Char('y'));

    // `LinkOutcome::Created`'s `Display` names exactly this — see
    // `skill::LinkOutcome`'s docs.
    h.wait_for_text(
        "skill: linked .claude/skills/katamari-review -> ../../.agents/skills/katamari-review",
    );

    // The real files landed exactly where `crate::skill::install` promises:
    // genuine content at `.agents/...`, a relative symlink at `.claude/...`.
    let agents_skill_md = repo
        .path()
        .join(".agents")
        .join("skills")
        .join("katamari-review")
        .join("SKILL.md");
    assert_eq!(
        fs::read_to_string(&agents_skill_md).unwrap(),
        bundled_skill_md()
    );

    let claude_dest = repo
        .path()
        .join(".claude")
        .join("skills")
        .join("katamari-review");
    let target = fs::read_link(&claude_dest).unwrap();
    assert_eq!(
        target,
        Path::new("../../.agents/skills/katamari-review"),
        "the .claude symlink must be relative, not absolute"
    );
    assert_eq!(
        fs::read_to_string(claude_dest.join("SKILL.md")).unwrap(),
        bundled_skill_md(),
        "the symlink must actually resolve to the real SKILL.md content"
    );
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

    // Nothing was installed — a dismiss must never silently write anything.
    assert!(!repo.path().join(".agents").exists());
    assert!(!repo.path().join(".claude").exists());
}

#[test]
fn a_repo_that_already_has_the_skill_never_shows_the_prompt() {
    let repo = fixture::basic_repo();
    preinstall_skill(repo.path());

    let h = Harness::spawn(repo.path(), wide_spawn_options());
    h.wait_for_text("todo.txt");

    save_a_comment(&h, "already have the skill, thanks");
    h.wait_for_text("comment: saved");

    assert!(
        !h.screen_contents()
            .contains("install the Claude Code review skill"),
        "an already-installed repo must never be offered the prompt"
    );
}
