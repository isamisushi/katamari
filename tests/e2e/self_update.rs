//! `ktmr self-update` — a real-binary, non-PTY smoke test (see `doctor.rs`'s
//! precedent: `std::process::Command::new(env!("CARGO_BIN_EXE_ktmr"))`, no
//! PTY, since this is a plain command that prints and exits) covering only
//! the no-receipt path: with `HOME`/`XDG_CONFIG_HOME` isolated into an empty
//! tempdir, `AxoUpdater::load_receipt` is guaranteed to fail (there's no
//! `katamari-receipt.json` anywhere it could look), so this never reaches
//! `run_sync` and therefore never makes a network call — see
//! `update::has_install_receipt`'s and `axoupdater::receipt`'s docs, both
//! read while implementing this, for why that's a plain fs lookup with
//! nothing async or network-shaped in it. The receipt-*present* path (a real
//! update, or the already-up-to-date report) needs a real cargo-dist
//! install and real GitHub access, so it's covered manually instead — see
//! the milestone task's notes — never as a committed, CI-run test.
//!
//! The `elapsed < ...` assertion isn't padding: it's the actual proof this
//! test's "no network call" claim in the paragraph above holds, not just an
//! assumption. If a future `axoupdater` version started phoning home before
//! `load_receipt` can fail (a version bump changing this crate's behavior
//! out from under us), this would start timing out against
//! [`NETWORK_TIMEOUT_SECS`] instead of silently continuing to pass.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Generous enough that a slow CI machine's process-spawn overhead alone
/// never trips it, but far below any real network round trip (`update.rs`'s
/// own background version check uses a 3s timeout for one HTTP request;
/// this budget is double that for zero HTTP requests).
const NETWORK_TIMEOUT_SECS: u64 = 6;

fn run_self_update(isolated_home: &Path) -> (Output, Duration) {
    let start = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_ktmr"))
        .arg("self-update")
        .env("HOME", isolated_home)
        .env("XDG_CONFIG_HOME", isolated_home.join(".config"))
        .output()
        .expect("failed to spawn ktmr self-update");
    (output, start.elapsed())
}

#[test]
fn a_bare_home_with_no_receipt_reports_guidance_and_exits_nonzero_fast() {
    let home = tempfile::tempdir().expect("tempdir");
    let (output, elapsed) = run_self_update(home.path());

    assert!(
        elapsed < Duration::from_secs(NETWORK_TIMEOUT_SECS),
        "ktmr self-update took {elapsed:?} against an empty HOME — load_receipt should fail \
         from a plain fs check with no network call; something started reaching the network \
         before returning the no-receipt guidance"
    );

    assert!(
        !output.status.success(),
        "self-update must exit nonzero when nothing was updated: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no cargo-dist install receipt"),
        "expected a friendly explanation that this install isn't managed by the shell \
         installer; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("upgrade with:"),
        "expected the fallback upgrade guidance to be included; stderr:\n{stderr}"
    );
    // Isolated HOME/XDG_CONFIG_HOME with no `/Cellar/` or `.cargo/bin` exe
    // path (this is the compiled test binary, not an installed one) means
    // `update::detect_upgrade_command`'s only remaining fallback is the
    // releases page — the same one a fresh `git clone` + source build would
    // get pointed at.
    assert!(
        stderr.contains("https://github.com/isamisushi/katamari/releases"),
        "expected the releases-page fallback with no package manager or receipt match; \
         stderr:\n{stderr}"
    );
}
