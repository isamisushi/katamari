//! GitHub pull-request diffs, fetched through the user's own `gh` CLI.
//!
//! katamari deliberately ships no GitHub client: no HTTP transport, no
//! token handling, no host-trust rules. A logged-in `gh` already owns
//! authentication (private repositories and GitHub Enterprise included)
//! and repository/host resolution, and its error messages — "run `gh
//! auth login`", "run `gh repo set-default`" — are better setup guidance
//! than anything this module could synthesize, which is why failures
//! surface `gh`'s own stderr verbatim. The shape mirrors how semantic
//! units spawn the user's `claude`/`codex` instead of bundling an LLM
//! client: the CLI the user already trusts is the integration surface.
//! (PR #25 proposed a full ureq transport with env-token auth; this is
//! the deliberately smaller cut of that proposal.)

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::num::NonZeroU64;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Hard ceiling on the diff text accepted from `gh` — far above any
/// reviewable PR, low enough that a runaway subprocess can't balloon
/// katamari's memory before the TUI even starts.
const MAX_DIFF_BYTES: usize = 32 * 1024 * 1024;

/// How long `gh` gets before being killed. Generous — it may be behind a
/// slow proxy — but bounded, so a network black hole degrades into an
/// error instead of a silent pre-TUI hang.
const GH_TIMEOUT: Duration = Duration::from_secs(60);

/// The `stderr` tail kept for error messages, same bound the LSP
/// transport uses for the same job.
const STDERR_TAIL_CAP: usize = 16 * 1024;

/// Fetches the aggregate diff of pull request `number` via `gh pr diff`,
/// run from `repo_root` so `gh`'s own remote-based repository resolution
/// applies. Read-only everywhere: no refs are fetched, nothing in the
/// repository changes.
pub fn pull_request_diff(repo_root: &Path, number: NonZeroU64) -> Result<String> {
    let Some(gh) = crate::lsp::adapter::which_on_path("gh") else {
        bail!(
            "--pr needs the GitHub CLI: `gh` was not found on PATH — install it \
             (https://cli.github.com) and authenticate with `gh auth login`"
        );
    };
    let mut command = Command::new(gh);
    command
        .args(["pr", "diff", &number.to_string()])
        .current_dir(repo_root);
    let stdout = run_captured(command, MAX_DIFF_BYTES, GH_TIMEOUT)
        .with_context(|| format!("`gh pr diff {number}` failed"))?;
    String::from_utf8(stdout)
        .map_err(|_| anyhow::anyhow!("`gh pr diff {number}` returned invalid UTF-8"))
}

/// Runs a command to completion with a null stdin (so a CLI that would
/// interactively prompt fails fast instead of waiting on input that can
/// never come), bounded output, and a deadline. Reader threads keep both
/// pipes drained the whole time — a diff bigger than the pipe buffer
/// would otherwise deadlock the child against an un-drained pipe.
fn run_captured(mut command: Command, max_stdout: usize, timeout: Duration) -> Result<Vec<u8>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("failed to spawn gh")?;

    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");

    // `take(max + 1)`: one byte past the cap is enough to prove the
    // output is oversized without buffering an unbounded stream. Past the
    // cap the pipe is still drained (into nothing) rather than dropped —
    // closing it would kill a still-writing `gh` with SIGPIPE, and that
    // broken-pipe death (exit 141, empty stderr) would then masquerade as
    // a `gh` failure before the size check below could ever report the
    // real problem. The drain is bounded by the deadline: a runaway
    // writer gets killed by the timeout path, which EOFs the pipe.
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stdout_pipe)
            .take(max_stdout as u64 + 1)
            .read_to_end(&mut buf);
        if buf.len() > max_stdout {
            let _ = std::io::copy(&mut stdout_pipe, &mut std::io::sink());
        }
        buf
    });
    // A genuine tail: accumulate and drop from the front past the cap, so
    // a long warning preamble can never crowd out the final line — which
    // is where `gh` puts the actionable part.
    let stderr_thread = std::thread::spawn(move || {
        let mut tail: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stderr_pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tail.extend_from_slice(&chunk[..n]);
                    if tail.len() > STDERR_TAIL_CAP {
                        let excess = tail.len() - STDERR_TAIL_CAP;
                        tail.drain(..excess);
                    }
                }
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("failed to wait for gh")? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("gh gave no answer within {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();

    // Size before status: if the output overflowed the cap, that is the
    // story, whatever the exit code says — the reader keeps the pipe
    // drained precisely so this branch stays reachable.
    if stdout.len() > max_stdout {
        bail!(
            "the diff exceeds {} MiB — too large to review interactively",
            max_stdout / (1024 * 1024)
        );
    }
    if !status.success() {
        // gh's own message is the actionable part — "gh auth login",
        // "gh repo set-default", "no pull requests found" — so it is the
        // error, not decoration on one.
        let detail = stderr.trim();
        if detail.is_empty() {
            bail!("gh exited with {status} and no error output");
        }
        bail!("{detail}");
    }
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn a_successful_run_returns_its_stdout() {
        let out = run_captured(
            sh("printf 'diff --git a/x b/x\\n'"),
            1024,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out, b"diff --git a/x b/x\n");
    }

    #[test]
    fn a_failing_run_surfaces_the_commands_own_stderr_as_the_error() {
        let err = run_captured(
            sh("echo 'To get started with GitHub CLI, please run: gh auth login' >&2; exit 1"),
            1024,
            Duration::from_secs(5),
        )
        .unwrap_err();
        // The CLI's own guidance must come through verbatim — it, not a
        // wrapper sentence, is what tells the user what to do next.
        assert!(err.to_string().contains("gh auth login"), "{err}");
    }

    #[test]
    fn oversized_output_is_refused_not_buffered() {
        // The payload must dwarf the kernel pipe buffer (~64 KiB): a
        // small overflow fits in the buffer, the child exits cleanly, and
        // the test would pass without exercising the backpressure path —
        // where a dropped (instead of drained) pipe SIGPIPEs the child
        // and hides the "too large" message behind an exit-141 error.
        let err = run_captured(
            sh("head -c 1000000 /dev/zero"),
            100_000,
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[test]
    fn a_long_stderr_preamble_cannot_crowd_out_the_final_actionable_line() {
        // 20000 filler bytes exceed nothing here (the cap is 16 KiB), so
        // only genuine keep-the-tail semantics preserve the last line.
        let err = run_captured(
            sh("head -c 20000 /dev/zero | tr '\\0' 'x' >&2; \
                echo 'ACTIONABLE: run gh auth login' >&2; exit 1"),
            1024,
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(err.to_string().contains("ACTIONABLE"), "{err}");
    }

    #[test]
    fn a_hung_command_is_killed_at_the_deadline() {
        let start = Instant::now();
        let err = run_captured(sh("sleep 30"), 1024, Duration::from_millis(300)).unwrap_err();
        assert!(err.to_string().contains("no answer within"), "{err}");
        // Well under the sleep: proves the kill, not the child's own end.
        assert!(start.elapsed() < Duration::from_secs(10));
    }
}
