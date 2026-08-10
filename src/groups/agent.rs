//! Resolving and driving the user's own agent CLI (`claude` or `codex`) as
//! a headless grouping backend.
//!
//! katamari deliberately has no LLM client and no API key of its own: the
//! people drowning in agent-authored diffs already have an authenticated
//! agent CLI on PATH, so the cheapest reliable inference backend is to
//! spawn that CLI in its non-interactive mode and read its answer. The
//! shape mirrors [`crate::lsp::adapter`]'s server resolution — probe PATH,
//! remember what was found, degrade gracefully to "feature unavailable"
//! rather than erroring the whole program.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Codex,
}

impl AgentKind {
    pub fn binary(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentCli {
    pub kind: AgentKind,
    pub path: PathBuf,
}

/// Every agent CLI present on PATH, claude first — the probe order is the
/// preference order [`detect`] uses, and doctor prints the full list so a
/// user with both installed can see which one grouping will pick.
pub fn detect_all() -> Vec<AgentCli> {
    [AgentKind::Claude, AgentKind::Codex]
        .into_iter()
        .filter_map(|kind| {
            crate::lsp::adapter::which_on_path(kind.binary()).map(|path| AgentCli { kind, path })
        })
        .collect()
}

/// [`detect_all`]'s first hit, with the user's `[units] agent` preference
/// applied: a preferred CLI that's actually installed wins; a preference
/// for one that isn't falls back to detection order rather than failing —
/// config written on a machine with both CLIs shouldn't brick grouping on
/// a machine with one.
pub fn detect_preferring(preferred: Option<&str>) -> Option<AgentCli> {
    let all = detect_all();
    if let Some(name) = preferred
        && let Some(found) = all.iter().find(|cli| cli.kind.binary() == name)
    {
        return Some(found.clone());
    }
    all.into_iter().next()
}

#[derive(Debug)]
pub enum AgentError {
    Spawn(std::io::Error),
    /// Killed after [`run`]'s deadline passed. The whole grouping request
    /// is best-effort UI sugar; a CLI that hangs (waiting on a login
    /// prompt it can't show, a network stall) must not wedge the review
    /// session that spawned it.
    Timeout(Duration),
    Failed {
        status: Option<i32>,
        stderr_tail: String,
    },
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentError::Spawn(e) => write!(f, "failed to start agent CLI: {e}"),
            AgentError::Timeout(d) => write!(f, "agent CLI gave no answer within {}s", d.as_secs()),
            AgentError::Failed {
                status,
                stderr_tail,
            } => {
                let code = status.map_or("killed by signal".to_owned(), |c| format!("exit {c}"));
                write!(f, "agent CLI failed ({code}): {}", stderr_tail.trim())
            }
        }
    }
}

impl std::error::Error for AgentError {}

/// Runs one grouping request and returns the CLI's answer text (which
/// [`super::prompt::parse_reply`] then interprets — this layer knows
/// nothing about the JSON inside). `units` carries the user's `[units]`
/// model/effort tuning; unset fields add no flags, leaving the CLI's own
/// configuration in charge.
///
/// The prompt travels via stdin for both CLIs, never argv: an inventory
/// prompt can approach 200KB (see [`super::prompt`]'s budget), and while
/// Linux's ARG_MAX would probably tolerate that, "probably" is not a
/// property to build on, and stdin also keeps the prompt out of `ps`.
pub fn run(
    cli: &AgentCli,
    units: &crate::config::UnitsConfig,
    prompt: &str,
    timeout: Duration,
) -> Result<String, AgentError> {
    let command = build_command(cli, units);
    let stdout = run_captured(command, prompt, timeout)?;
    Ok(match cli.kind {
        AgentKind::Claude => extract_claude_result(&stdout),
        AgentKind::Codex => {
            let path = last_message_path();
            let from_file = std::fs::read_to_string(&path).ok();
            let _ = std::fs::remove_file(&path);
            match from_file {
                Some(text) if !text.trim().is_empty() => text,
                // Old codex without the flag would have errored out above
                // (unknown argument → non-zero exit), so an empty file
                // here means "ran but wrote nothing" — the transcript is
                // the only thing left to offer.
                _ => stdout,
            }
        }
    })
}

/// The full argv for one grouping request — split from [`run`] so a test
/// can assert on `Command::get_args` without spawning anything.
fn build_command(cli: &AgentCli, units: &crate::config::UnitsConfig) -> Command {
    let mut command = Command::new(&cli.path);
    match cli.kind {
        // Non-interactive print mode. The JSON envelope (rather than
        // plain text output) is deliberate: it cleanly separates the
        // model's answer (`result` field) from any banner/log noise, and
        // its parse failure tells us "this claude is too old / too new"
        // in one place, `extract_claude_result`.
        AgentKind::Claude => {
            command.args(["-p", "--output-format", "json"]);
            if let Some(model) = &units.claude_model {
                command.args(["--model", model]);
            }
            if let Some(effort) = &units.claude_effort {
                command.args(["--effort", effort]);
            }
        }
        // `codex exec` prints a running transcript to stdout, which can
        // contain JSON-looking fragments (including an echo of our own
        // instructions), so the answer is captured via
        // `--output-last-message` into a private file instead of scraped
        // from the transcript. `-` = read the prompt from stdin. Effort
        // goes through `-c` because codex has no dedicated flag for it —
        // the config-override channel is its stable interface.
        AgentKind::Codex => {
            command.arg("exec");
            if let Some(model) = &units.codex_model {
                command.args(["--model", model]);
            }
            if let Some(effort) = &units.codex_effort {
                command.arg("-c");
                command.arg(format!("model_reasoning_effort={effort}"));
            }
            command.arg("--output-last-message");
            command.arg(last_message_path());
            command.arg("-");
        }
    }
    command
}

/// Where codex is told to drop its final message. Scoped to the process id
/// so two concurrent katamari sessions can't clobber each other's answer.
fn last_message_path() -> PathBuf {
    std::env::temp_dir().join(format!("ktmr-groups-{}.txt", std::process::id()))
}

/// `claude -p --output-format json` wraps the answer in a result envelope
/// (`{"type":"result", ..., "result":"<text>"}`). If that shape ever
/// changes, falling back to the raw stdout keeps the pipeline alive —
/// [`super::prompt::parse_reply`] extracts the JSON object out of
/// whatever text it's handed anyway.
fn extract_claude_result(stdout: &str) -> String {
    serde_json::from_str::<serde_json::Value>(stdout)
        .ok()
        .and_then(|v| v.get("result")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| stdout.to_owned())
}

/// Spawn, feed stdin, and wait — but with a deadline. std has no
/// `wait_timeout`, so the child is polled via `try_wait` while two reader
/// threads drain stdout/stderr (draining must be concurrent with the
/// wait: a child that fills its pipe buffer while we only poll `try_wait`
/// would deadlock — it blocks writing, we block waiting).
fn run_captured(
    mut command: Command,
    stdin_data: &str,
    timeout: Duration,
) -> Result<String, AgentError> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(AgentError::Spawn)?;

    // Feeding stdin from its own thread for the same deadlock reason as
    // the readers: a prompt larger than the pipe buffer blocks the writer
    // until the child reads, and the child may not read until it has
    // written something we haven't drained yet.
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let stdin_data = stdin_data.to_owned();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        // A child that exits without reading all of stdin (a crash, or
        // claude rejecting the request early) breaks the pipe; that's the
        // child's story to tell via exit status, not a write error worth
        // propagating here.
        let _ = stdin.write_all(stdin_data.as_bytes());
        drop(stdin);
    });

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout_pipe.read_to_string(&mut buf);
        buf
    });
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = writer.join();
                return Err(AgentError::Timeout(timeout));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AgentError::Spawn(e));
            }
        }
    };

    let _ = writer.join();
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        // The last kilobyte of stderr is where CLIs put the actionable
        // line ("not logged in", "unknown flag"); the front is banners.
        let tail_start = stderr.len().saturating_sub(1024);
        let tail_start = stderr
            .char_indices()
            .map(|(i, _)| i)
            .find(|&i| i >= tail_start)
            .unwrap_or(0);
        return Err(AgentError::Failed {
            status: status.code(),
            stderr_tail: stderr[tail_start..].to_owned(),
        });
    }
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests drive `run_captured` through a real shell rather than a fake
    /// claude/codex — the deadlock- and deadline-handling is the point of
    /// this module, and only a live child process exercises it.
    fn run_sh(script: &str, stdin: &str, timeout: Duration) -> Result<String, AgentError> {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        run_captured(command, stdin, timeout)
    }

    #[test]
    fn captures_stdout_of_a_well_behaved_child() {
        let out = run_sh("cat", "hello", Duration::from_secs(10)).unwrap();
        assert_eq!(out, "hello");
    }

    #[test]
    fn a_large_stdin_does_not_deadlock() {
        // 1 MiB through both pipes — far past any OS pipe buffer, so this
        // hangs forever if writer/readers aren't concurrent with the wait.
        let big = "y".repeat(1024 * 1024);
        let out = run_sh("cat", &big, Duration::from_secs(30)).unwrap();
        assert_eq!(out.len(), big.len());
    }

    #[test]
    fn a_hung_child_is_killed_at_the_deadline() {
        let started = Instant::now();
        let result = run_sh("sleep 30", "", Duration::from_millis(300));
        assert!(matches!(result, Err(AgentError::Timeout(_))));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the deadline, not the child's 30s sleep, must decide when this returns"
        );
    }

    #[test]
    fn a_failing_child_reports_its_stderr_tail() {
        let result = run_sh("echo boom >&2; exit 3", "", Duration::from_secs(10));
        match result {
            Err(AgentError::Failed {
                status,
                stderr_tail,
            }) => {
                assert_eq!(status, Some(3));
                assert!(stderr_tail.contains("boom"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    fn args_of(cli: &AgentCli, units: &crate::config::UnitsConfig) -> Vec<String> {
        build_command(cli, units)
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn default_tuning_passes_katamaris_default_claude_model_and_nothing_else() {
        let cli = AgentCli {
            kind: AgentKind::Claude,
            path: PathBuf::from("claude"),
        };
        let args = args_of(&cli, &crate::config::UnitsConfig::default());
        assert_eq!(
            args,
            [
                "-p",
                "--output-format",
                "json",
                "--model",
                crate::config::DEFAULT_CLAUDE_MODEL,
            ]
        );
    }

    #[test]
    fn all_none_tuning_adds_no_model_or_effort_flags() {
        let cli = AgentCli {
            kind: AgentKind::Claude,
            path: PathBuf::from("claude"),
        };
        // What `claude_model = ""` in config resolves to — see
        // `config::finalize`'s opt-out arm.
        let none = crate::config::UnitsConfig {
            claude_model: None,
            ..Default::default()
        };
        let args = args_of(&cli, &none);
        assert_eq!(args, ["-p", "--output-format", "json"]);
    }

    #[test]
    fn claude_tuning_becomes_model_and_effort_flags() {
        let cli = AgentCli {
            kind: AgentKind::Claude,
            path: PathBuf::from("claude"),
        };
        let units = crate::config::UnitsConfig {
            claude_model: Some("opus".to_owned()),
            claude_effort: Some("high".to_owned()),
            // codex tuning must not leak into a claude invocation.
            codex_model: Some("gpt-5-codex".to_owned()),
            codex_effort: Some("low".to_owned()),
            ..Default::default()
        };
        let args = args_of(&cli, &units);
        assert_eq!(
            args,
            [
                "-p",
                "--output-format",
                "json",
                "--model",
                "opus",
                "--effort",
                "high"
            ]
        );
    }

    #[test]
    fn codex_tuning_becomes_model_flag_and_config_override() {
        let cli = AgentCli {
            kind: AgentKind::Codex,
            path: PathBuf::from("codex"),
        };
        let units = crate::config::UnitsConfig {
            codex_model: Some("gpt-5-codex".to_owned()),
            codex_effort: Some("high".to_owned()),
            claude_model: Some("opus".to_owned()),
            ..Default::default()
        };
        let args = args_of(&cli, &units);
        assert_eq!(
            args[..6],
            [
                "exec",
                "--model",
                "gpt-5-codex",
                "-c",
                "model_reasoning_effort=high",
                "--output-last-message",
            ]
        );
        assert_eq!(args.last().map(String::as_str), Some("-"));
    }

    #[test]
    fn claude_result_envelope_is_unwrapped_with_raw_fallback() {
        let envelope = r#"{"type":"result","result":"{\"units\":[]}"}"#;
        assert_eq!(extract_claude_result(envelope), r#"{"units":[]}"#);
        assert_eq!(extract_claude_result("not json at all"), "not json at all");
    }
}
