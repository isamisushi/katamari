//! `ktmr doctor --no-live` — a real-binary, non-PTY smoke test proving the
//! CLI plumbing (`main.rs`'s `run_doctor`, argv parsing, exit-code
//! translation) actually wires up `doctor::build_report`/`render_text`/
//! `exit_code` end to end, coverage a purely in-process `doctor` unit test
//! can't reach. Follows `skill_install.rs`'s `run_skill_install_cli`
//! precedent (`std::process::Command::new(env!("CARGO_BIN_EXE_ktmr"))`,
//! no PTY — `doctor` is a plain, non-interactive command that prints and
//! exits) rather than the PTY `Harness`, since nothing here needs a real
//! terminal.
//!
//! Every invocation isolates `HOME`/`XDG_CONFIG_HOME`/`XDG_DATA_HOME` into a
//! throwaway tempdir: unlike `ktmr skill install` (which never reads
//! config), `doctor` loads the real merged config and probes katamari's
//! managed LSP prefix — both would otherwise leak whatever's on the machine
//! actually running this suite into these tests' expectations.
//!
//! No live-probe coverage here on purpose (every fixture is `.txt`/`.md`
//! content — see `support::fixture`'s module docs — so there's nothing for
//! the live section to find regardless): the E2E suite is LSP-inert by
//! policy, and `--no-live` is the one invocation cheap and deterministic
//! enough to run through the compiled binary at all.

use crate::support::fixture;
use std::path::Path;
use std::process::{Command, Output};

fn run_doctor(repo_root: &Path, isolated_home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .arg("doctor")
        .args(args)
        .current_dir(repo_root)
        .env("HOME", isolated_home)
        .env("XDG_CONFIG_HOME", isolated_home.join(".config"))
        .env("XDG_DATA_HOME", isolated_home.join(".local/share"))
        .output()
        .expect("failed to spawn ktmr doctor")
}

#[test]
fn no_live_reports_every_static_section_and_exits_zero_in_a_clean_repo() {
    let repo = fixture::basic_repo();
    let home = tempfile::tempdir().expect("tempdir");
    let output = run_doctor(repo.path(), home.path(), &["--no-live"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "doctor exited nonzero:\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(stdout.contains("vcs"), "{stdout}");
    assert!(stdout.contains("config"), "{stdout}");
    assert!(stdout.contains("lsp (resolution)"), "{stdout}");
    assert!(
        !stdout.contains("lsp (live probe)"),
        "--no-live must omit the live section entirely:\n{stdout}"
    );
    assert!(
        stdout.trim_end().ends_with("warnings")
            || stdout.trim_end().ends_with("warning")
            || stdout.trim_end().ends_with("all checks passed"),
        "summary line missing or unexpected:\n{stdout}"
    );
}

#[test]
fn no_live_json_is_valid_and_matches_the_documented_shape() {
    let repo = fixture::basic_repo();
    let home = tempfile::tempdir().expect("tempdir");
    let output = run_doctor(repo.path(), home.path(), &["--no-live", "--json"]);
    assert!(
        output.status.success(),
        "doctor --json exited nonzero:\nstderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("doctor --json must print valid JSON");
    let sections = value["sections"].as_array().expect("sections array");
    assert!(!sections.is_empty(), "{value:#}");
    for section in sections {
        assert!(section["title"].is_string(), "{section:#}");
        let checks = section["checks"].as_array().expect("checks array");
        for check in checks {
            let status = check["status"].as_str().expect("status string");
            assert!(
                ["ok", "warn", "error"].contains(&status),
                "unexpected status {status:?} in {check:#}"
            );
            assert!(check["label"].is_string(), "{check:#}");
            assert!(check["detail"].is_string(), "{check:#}");
        }
    }
}

#[test]
fn outside_a_git_repository_reports_the_vcs_error_and_exits_nonzero() {
    let home = tempfile::tempdir().expect("tempdir");
    let non_repo = tempfile::tempdir().expect("tempdir");
    let output = run_doctor(non_repo.path(), home.path(), &["--no-live"]);
    assert!(
        !output.status.success(),
        "doctor must exit nonzero outside a git repository"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("not inside a git repository"), "{stdout}");
    assert!(stdout.trim_end().ends_with("1 error"), "{stdout}");
}

#[test]
fn a_broken_repo_config_surfaces_as_a_warn_check_not_a_crash() {
    let repo = fixture::basic_repo();
    std::fs::create_dir_all(repo.path().join(".katamari")).unwrap();
    std::fs::write(
        repo.path().join(".katamari").join("config.toml"),
        "this is not [ valid toml",
    )
    .unwrap();
    let home = tempfile::tempdir().expect("tempdir");
    let output = run_doctor(repo.path(), home.path(), &["--no-live"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "a config warning alone must still exit 0:\nstdout: {stdout}"
    );
    assert!(stdout.contains("warn"), "{stdout}");
    assert!(
        stdout.contains(".katamari/config.toml") || stdout.contains("repo config"),
        "{stdout}"
    );
}
