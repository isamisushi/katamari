//! `ktmr doctor` — a checkhealth-style report answering the question issue
//! #4 was filed over: "is the language server actually working in my repo,
//! and if not, is that katamari's fault, the server's fault, or is it just
//! still indexing?" Everything here is data-first: [`build_report`]
//! assembles a [`HealthReport`] (a `Vec<Section>` of `Vec<Check>`, each
//! [`Check`] an [`Status`]/label/detail triple) by calling into the same
//! logic `ktmr lsp doctor` and a live session already use — never a second,
//! independently-written copy of it — and [`render_text`]/`serde_json` turn
//! that data into either a human-readable report or `--json`. Five
//! sections, always in this order:
//!
//! - **vcs**: is `git` on `PATH`, is the current directory actually inside a
//!   repository, and (only when detected) is this repo colocated-jj.
//! - **config**: does each of the two config files (`~/.config/katamari/`,
//!   `<repo>/.katamari/`) parse cleanly — reusing
//!   [`crate::config::parse_with_warnings`] rather than [`crate::config::load_merged`]'s
//!   own stderr-only warnings, so a parse problem becomes a `Check` a caller
//!   can render or serialize instead of a side effect it has no way to
//!   observe. Also reports the current terminal's kitty-keyboard-protocol
//!   probe fingerprint and cached verdict (see [`crate::ui::probe_cache`]),
//!   the one static, self-diagnosable answer to "why did this launch's
//!   splash linger" — a `warn` on a cache miss (the *next* launch in this
//!   terminal, not necessarily this one, is what would actually probe and
//!   wait) rather than an `error`, since a first launch or one since `ktmr
//!   reset --cache` is completely ordinary, expected state.
//! - **lsp (resolution)**: static, offline — where each of the six built-in
//!   languages' server would resolve from today, plus any
//!   `[lsp.servers.<id>]` custom entry — built on [`crate::lsp::adapter::diagnose`],
//!   the exact function `ktmr lsp doctor`'s table already uses, so the two
//!   commands agree on *how* a language resolves. Unlike `ktmr lsp doctor`
//!   (always one repo-root-wide `workspace_root` for every language), a
//!   built-in language with a file already present in the repo is diagnosed
//!   against *that file's own* workspace root — the same one the live-probe
//!   section would actually spawn against — rather than the repo root; see
//!   [`lsp_resolution_checks`]'s docs for why: without it, a nested
//!   TypeScript/Python project with only a project-local server install
//!   could be reported "not found" here and "running fine" two sections
//!   later, for the identical language, in the identical run.
//! - **agents**: which agent CLI (`claude`/`codex`) the semantic-units
//!   grouping (`u` in `ktmr diff`) would spawn — a PATH probe via
//!   [`crate::groups::agent::detect_all`], the same resolution the feature
//!   itself uses, so the report and the live session can't disagree. A
//!   warning, never an error, when none is found: grouping is an optional
//!   feature, and a katamari without it is still fully functional.
//! - **lsp (live probe)**: the feature's whole point — for every built-in or
//!   custom language with at least one matching file in the repository
//!   (tracked or untracked-and-not-ignored — see [`scan_repo_files`]) *and*
//!   a static resolution, actually spawns the real server (headless, no
//!   config/`--json`/TUI dependency — the same [`crate::lsp::LspManager`]
//!   entry point `ktmr lsp-check` uses) and reports `spawn+initialize` and
//!   `hover round-trip` as separate, timed checks. Never installs anything,
//!   even when `[lsp] auto_install` is on and resolution says a language is
//!   installable — a diagnostic must not mutate the environment it's
//!   diagnosing (`--no-live` skips this section outright; `--language`
//!   narrows it to one language/custom id).
//!
//! Exit code: 0 unless the report contains at least one [`Status::Error`]
//! check (a warning alone is still 0) — see [`exit_code`].

use crate::config::{self, ServerOverride};
use crate::lsp::adapter::{self, Diagnosis, LangKey, Language};
use crate::lsp::manager::ServerState;
use crate::lsp::{self, LspManager};
use crate::ui::probe_cache;
use crate::vcs::git::GitSource;
use crate::vcs::{self, DiffSource, jj};
use anyhow::{Result, bail};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

// --- Report data model -----------------------------------------------------

/// One [`Check`]'s severity. Serializes lowercase (`"ok"`/`"warn"`/`"error"`)
/// to match `--json`'s documented shape; [`Status::tag`] renders the same
/// three strings for the plain-text column, so the two output modes never
/// disagree about what a status is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    Ok,
    Warn,
    Error,
}

impl Status {
    fn tag(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Error => "error",
        }
    }
}

/// One row of the report: a severity, a short name for what was checked, and
/// a free-text detail (a resolved path, an error message, a hint — whatever
/// is most useful for that particular check; empty when the label already
/// says everything there is to say).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Check {
    status: Status,
    label: String,
    detail: String,
}

impl Check {
    fn ok(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: Status::Ok,
            label: label.into(),
            detail: flatten_detail(&detail.into()),
        }
    }

    fn warn(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: Status::Warn,
            label: label.into(),
            detail: flatten_detail(&detail.into()),
        }
    }

    fn error(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: Status::Error,
            label: label.into(),
            detail: flatten_detail(&detail.into()),
        }
    }
}

/// Collapses any run of newlines in `detail` into `"; "` — applied at every
/// [`Check`]'s only construction path (here, rather than at each of the
/// several call sites that happen to pass through external text today: a
/// config file's own TOML parse-error message via [`config_path_check`], a
/// crashed server's raw stderr tail via [`crate::lsp::client::Client::start`]'s
/// `augment_with_stderr`). Doing it here means [`render_check_line`]'s
/// one-line-per-check assumption can never be violated no matter where a
/// future multi-line source gets threaded through a `Check`, and that
/// `--json` and the text renderer read the exact same, already-flattened
/// `detail` string — they can't disagree about what a check's detail says
/// the way they would if flattening only happened at render time.
fn flatten_detail(detail: &str) -> String {
    detail
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Section {
    title: String,
    checks: Vec<Check>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HealthReport {
    sections: Vec<Section>,
}

impl HealthReport {
    /// `(warnings, errors)` across every section — the one pass
    /// [`exit_code`] and the text renderer's summary line both need.
    fn counts(&self) -> (usize, usize) {
        let mut warnings = 0;
        let mut errors = 0;
        for check in self.sections.iter().flat_map(|s| &s.checks) {
            match check.status {
                Status::Warn => warnings += 1,
                Status::Error => errors += 1,
                Status::Ok => {}
            }
        }
        (warnings, errors)
    }
}

/// 0 unless the report contains at least one [`Status::Error`] check — a
/// warning alone still exits 0. Documented on `Command::Doctor` in
/// `main.rs`; the actual process exit happens there, via
/// [`std::process::exit`], since a `Result`-returning `main` has no other
/// way to choose a nonzero code without also printing a redundant "Error:"
/// line over a report that already explained itself.
pub(crate) fn exit_code(report: &HealthReport) -> i32 {
    let (_, errors) = report.counts();
    i32::from(errors > 0)
}

/// Checkhealth-flavored plain text: one line per section title, then one
/// aligned `<tag>  <label>: <detail>` line per check, a blank line between
/// sections, and a summary line at the end (`"all checks passed"`, or e.g.
/// `"2 warnings, 1 error"` — see [`summary_line`]).
pub(crate) fn render_text(report: &HealthReport) -> String {
    let mut out = String::new();
    for section in &report.sections {
        out.push_str(&section.title);
        out.push('\n');
        for check in &section.checks {
            out.push_str(&render_check_line(check));
        }
        out.push('\n');
    }
    out.push_str(&summary_line(report));
    out.push('\n');
    out
}

fn render_check_line(check: &Check) -> String {
    let tag = check.status.tag();
    if check.detail.is_empty() {
        format!("  {tag:<5} {}\n", check.label)
    } else {
        format!("  {tag:<5} {}: {}\n", check.label, check.detail)
    }
}

/// `"all checks passed"` when nothing warned or errored; otherwise
/// `"<N> warning(s)[, <M> error(s)]"` — warnings named first (matching the
/// order a reviewer should triage: a warning might explain an error, not the
/// other way around), errors only when there are any, each correctly
/// singular/plural.
fn summary_line(report: &HealthReport) -> String {
    let (warnings, errors) = report.counts();
    if warnings == 0 && errors == 0 {
        return "all checks passed".to_owned();
    }
    let mut parts = Vec::new();
    if warnings > 0 {
        parts.push(format!(
            "{warnings} warning{}",
            if warnings == 1 { "" } else { "s" }
        ));
    }
    if errors > 0 {
        parts.push(format!(
            "{errors} error{}",
            if errors == 1 { "" } else { "s" }
        ));
    }
    parts.join(", ")
}

// --- CLI-facing entry point -------------------------------------------------

/// `ktmr doctor`'s options, past what clap alone can validate — see
/// `Command::Doctor`'s doc comments in `main.rs` for each flag's meaning.
pub(crate) struct DoctorOptions<'a> {
    pub(crate) no_live: bool,
    pub(crate) language_filter: Option<&'a str>,
}

/// Assembles the full report for a session rooted at `cwd` — the one
/// function `main.rs`'s `run_doctor` calls. Every section after `vcs` probes
/// against `effective_root` (the repository root if `cwd` is inside one,
/// else `cwd` itself, matching `ktmr diff`/`ktmr open`'s own outside-a-repo
/// fallback) rather than bailing outright, so a non-repo directory still
/// gets a full report — just one with the vcs section's `repo root` check
/// reporting `error` (see the verifier duty in the issue #4 spec: "a
/// non-repo directory" is one of the required manual checks, and it expects
/// a report plus a nonzero exit, not a crash).
pub(crate) fn build_report(cwd: &Path, options: DoctorOptions) -> Result<HealthReport> {
    let (vcs_section_result, ctx) = vcs_section(cwd);
    let config = config::load_merged(&ctx.effective_root);

    // Validated eagerly (before deciding whether the live section even
    // runs) so `--no-live --language bogus` still reports the mistake
    // rather than silently ignoring an argument that would have mattered
    // had `--no-live` been left off.
    let filter = options
        .language_filter
        .map(|value| resolve_language_filter(value, &config.lsp_servers))
        .transpose()?;

    // Computed exactly once and threaded to both sections below —
    // `custom_extension_map` warns to stderr about a shadowed/collided
    // custom extension claim (see its docs), and calling it a second time
    // for the same `overrides` would print that warning twice for the
    // identical reason.
    let custom_extensions = adapter::custom_extension_map(&config.lsp_servers);

    // The repo's tracked+untracked files, classified by language — shared by
    // the resolution section (so a built-in language's workspace-root-
    // dependent lookup tier, e.g. TypeScript/Python's project-local search,
    // diagnoses against the same root the live-probe section would actually
    // spawn against — see `lsp_resolution_checks`'s docs) and the live-probe
    // section itself, which used to run this same scan a second time,
    // independently. Only possible with a real repo to scan; outside one,
    // both sections fall back to their repo-agnostic behavior. Scanned once
    // here regardless of `--no-live`, since the resolution section needs it
    // now too, not just the live section.
    let scan: Option<Result<Vec<PathBuf>>> = ctx
        .repo_root
        .as_ref()
        .map(|repo_root| scan_repo_files(&GitSource::at(repo_root.clone())));

    let present: HashMap<LangKey, Vec<PathBuf>> = match &scan {
        Some(Ok(files)) => detect_present_languages(files, &custom_extensions),
        _ => HashMap::new(),
    };

    // The file each present language would actually be probed with, having
    // just confirmed it still exists on disk (see `select_probe_file`'s
    // docs) — no entry for a language whose every candidate file is gone
    // (e.g. an unstaged deletion of the only tracked file of that
    // language).
    let probe_files: HashMap<LangKey, PathBuf> = match &ctx.repo_root {
        Some(repo_root) => present
            .iter()
            .filter_map(|(key, candidates)| {
                select_probe_file(candidates, |p| repo_root.join(p).is_file())
                    .map(|file| (key.clone(), file.clone()))
            })
            .collect(),
        None => HashMap::new(),
    };

    let mut sections = vec![
        vcs_section_result,
        config_section(&ctx.effective_root),
        Section {
            title: "lsp (resolution)".to_owned(),
            checks: lsp_resolution_checks(
                &ctx.effective_root,
                ctx.repo_root.as_deref(),
                &probe_files,
                &config.lsp_servers,
                &custom_extensions,
            ),
        },
        agents_section(&config.units),
    ];

    if !options.no_live {
        sections.push(match (&ctx.repo_root, &scan) {
            (Some(repo_root), Some(Ok(_))) => lsp_live_section(
                repo_root,
                &present,
                &probe_files,
                &config.lsp_servers,
                filter.as_ref(),
            ),
            (Some(_), Some(Err(e))) => Section {
                title: "lsp (live probe)".to_owned(),
                checks: vec![Check::error(
                    "repo file scan",
                    format!("failed to list repository files: {e}"),
                )],
            },
            // `(Some(_), None)`/`(None, _)`: `scan` is always `Some` exactly
            // when `ctx.repo_root` is, so only the "outside a repo" case
            // (`None`, `None`) is actually reachable here.
            _ => Section {
                title: "lsp (live probe)".to_owned(),
                checks: vec![Check::warn(
                    "lsp live probe",
                    "skipped — not inside a git repository",
                )],
            },
        });
    }

    Ok(HealthReport { sections })
}

// --- vcs section -------------------------------------------------------

/// The root every section after `vcs` probes against, plus whether `cwd`
/// was actually inside a git repository at all (kept separately from
/// `effective_root` because the live-probe section needs a real repo to
/// scan files from — a `cwd`-only fallback has no meaningful "tracked
/// files" to union with).
struct RepoContext {
    repo_root: Option<PathBuf>,
    effective_root: PathBuf,
}

fn vcs_section(cwd: &Path) -> (Section, RepoContext) {
    let mut checks = vec![git_binary_check()];
    let (root_check, repo_root) = repo_root_check(cwd);
    checks.push(root_check);
    // Absent jj is deliberately not a check at all in a plain git repo —
    // not even an `ok "not detected"` row — since jj is an optional,
    // per-user setup, not something every katamari repo is expected to
    // have (see the module docs' "only when detected" wording).
    if let Some(root) = &repo_root
        && let Some(check) = jj_check(root)
    {
        checks.push(check);
    }
    let effective_root = repo_root.clone().unwrap_or_else(|| cwd.to_path_buf());
    (
        Section {
            title: "vcs".to_owned(),
            checks,
        },
        RepoContext {
            repo_root,
            effective_root,
        },
    )
}

/// See the module docs' **agents** entry. One row per detected CLI plus,
/// when more than one is present, the first row is the one grouping will
/// actually use — [`crate::groups::agent::detect_all`] returns them in
/// preference order, and preserving that order here is the point.
fn agents_section(units: &config::UnitsConfig) -> Section {
    let found = crate::groups::agent::detect_all();
    // The very resolution grouping itself runs — including the user's
    // `[units] agent` preference — so the "(used for grouping)" marker
    // can't drift from what `u` would actually spawn.
    let chosen = crate::groups::agent::detect_preferring(units.agent.as_deref());
    let checks = if found.is_empty() {
        vec![Check::warn(
            "agent CLI",
            "none found — semantic-units grouping (u) needs `claude` or `codex` on PATH",
        )]
    } else {
        found
            .iter()
            .map(|cli| {
                let is_chosen = chosen.as_ref().is_some_and(|c| c.kind == cli.kind);
                let detail = if is_chosen && found.len() > 1 {
                    format!("{} (used for grouping)", cli.path.display())
                } else {
                    cli.path.display().to_string()
                };
                Check::ok(cli.kind.binary(), detail)
            })
            .collect()
    };
    Section {
        title: "agents".to_owned(),
        checks,
    }
}

fn git_binary_check() -> Check {
    match Command::new("git").arg("--version").output() {
        Ok(output) if output.status.success() => Check::ok(
            "git binary",
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ),
        Ok(output) => Check::error(
            "git binary",
            format!(
                "git --version failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ),
        Err(e) => Check::error("git binary", format!("not found on PATH: {e}")),
    }
}

fn repo_root_check(cwd: &Path) -> (Check, Option<PathBuf>) {
    match GitSource::discover(cwd).and_then(|source| source.repo_root()) {
        Ok(root) => {
            let display = root.display().to_string();
            (Check::ok("repo root", display), Some(root))
        }
        Err(_) => (
            Check::error("repo root", "not inside a git repository — run from a repo"),
            None,
        ),
    }
}

/// `None` when `repo_root` isn't a colocated jj repo — see
/// [`vcs::LogBackend::detect`], the exact detection [`crate::ui::log_view`]
/// uses, called here rather than `ui::mod`'s own `detect_jj_repo` (which is
/// `View`-coupled and has nothing for a CLI command to pass it). Re-resolves
/// the jj binary itself (via [`jj::resolve_jj_bin`]) for the version string
/// below — `LogBackend` has no getter for the path it already found one
/// internally.
fn jj_check(repo_root: &Path) -> Option<Check> {
    if !matches!(vcs::LogBackend::detect(repo_root), vcs::LogBackend::Jj(_)) {
        return None;
    }
    let detail = match jj::resolve_jj_bin() {
        Some(bin) => format!("colocated ({})", jj_binary_version(&bin)),
        // `LogBackend::detect` just resolved a jj binary to get here, so
        // this is only reachable if PATH changed between the two calls —
        // still handled rather than unwrapped, since a CLI diagnostic
        // should never panic on a timing fluke.
        None => "colocated (jj binary version unknown)".to_owned(),
    };
    Some(Check::ok("jj", detail))
}

fn jj_binary_version(jj_bin: &Path) -> String {
    match Command::new(jj_bin).arg("--version").output() {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }
        _ => format!("{} (version unknown)", jj_bin.display()),
    }
}

// --- config section ----------------------------------------------------

fn config_section(effective_root: &Path) -> Section {
    let mut checks = Vec::new();
    if let Some(home_path) = config::home_config_path() {
        checks.push(config_path_check("home config", &home_path));
    }
    let repo_path = effective_root.join(".katamari").join("config.toml");
    checks.push(config_path_check("repo config", &repo_path));
    checks.push(kitty_probe_check());
    Section {
        title: "config".to_owned(),
        checks,
    }
}

/// Reports this terminal's kitty-keyboard-protocol probe cache state — the
/// same fingerprint and lookup [`crate::ui::enable_kitty_keyboard_protocol`]
/// itself would use on the next real `ktmr` launch (see
/// [`crate::ui::probe_cache`]'s module docs), read here purely for display:
/// running `ktmr doctor` never probes or writes anything itself. A hit
/// (either verdict) is `ok` — the next launch in this terminal skips the
/// real probe either way; a miss is `warn`, since it means the *next*
/// launch, not this `doctor` run, will pay crossterm's up-to-2s wait.
///
/// Checked first, ahead of the lookup: inside a terminal multiplexer
/// (`$TMUX`/`$STY` set), `fingerprint` no longer identifies the outer
/// terminal in front of the user (see `probe_cache`'s module docs'
/// **Multiplexers** section) — `run` never looks the cache up there either,
/// so reporting a `hit`/`miss` off this same fingerprint would tell this
/// user a terminal-specific answer that doesn't actually apply to their
/// terminal. This is `warn`, same as a miss, but for a different (and
/// permanent, not one-launch) reason: every launch in this session pays the
/// real probe, by design, not just the next one.
fn kitty_probe_check() -> Check {
    let fingerprint = probe_cache::fingerprint_from_env();
    let label = "kitty keyboard probe cache";
    if probe_cache::multiplexed_from_env() {
        return Check::warn(
            label,
            format!(
                "disabled inside a terminal multiplexer ({fingerprint}) — tmux/screen \
                 overwrite the env vars this cache keys on, so every launch here re-probes \
                 rather than trust a verdict that might belong to a different outer terminal"
            ),
        );
    }
    match probe_cache::look_up(&probe_cache::cache_file_path(), &fingerprint) {
        Some(true) => Check::ok(
            label,
            format!("hit, supported ({fingerprint}) — startup skips the probe"),
        ),
        Some(false) => Check::ok(
            label,
            format!("hit, not supported ({fingerprint}) — startup skips the probe"),
        ),
        None => Check::warn(
            label,
            format!(
                "miss ({fingerprint}) — the next launch in this terminal probes \
                 and may wait up to ~2s before caching a verdict"
            ),
        ),
    }
}

/// One config path's check: missing is `ok` (defaults apply, same as a
/// normal session), a clean parse is `ok`, and any parse/deserialize/
/// unknown-key warning [`config::parse_with_warnings`] collects is `warn` —
/// never `error`, matching [`config::load_merged`]'s own "a bad file
/// degrades, it doesn't fail the session" behavior (see that function's
/// docs): if a broken config file doesn't stop a real session from running,
/// it shouldn't make the doctor report look more dire than a real session
/// would be. The unknown-`keymap`-preset warning is deliberately not
/// checked here — it fires post-merge, with no single file to attribute it
/// to (see `config::finalize`), so it's left to the stderr warning a normal
/// `load_merged` call already prints, rather than pretending this
/// per-path check could name which file caused it.
fn config_path_check(kind: &str, path: &Path) -> Check {
    let label = format!("{kind} ({})", path.display());
    let Ok(text) = std::fs::read_to_string(path) else {
        return Check::ok(label, "not present (defaults)");
    };
    let (_, warnings) = config::parse_with_warnings(path, &text);
    if warnings.is_empty() {
        Check::ok(label, "parsed clean")
    } else {
        Check::warn(label, warnings.join("; "))
    }
}

// --- lsp (resolution) section -------------------------------------------

/// Every built-in language's resolution row (plus, for Java, the JDK note
/// `ktmr lsp doctor` prints beneath it) followed by every custom server's
/// row — built on [`adapter::diagnose`]/[`adapter::install_hint`]/
/// [`adapter::custom_extension_map`], the same functions `ktmr lsp doctor`'s
/// own printer (`main.rs`'s `run_lsp_doctor`/`print_custom_server_doctor`)
/// calls, so the two commands can never disagree about *how* a server
/// resolves.
///
/// What they *can* disagree about, deliberately: `ktmr lsp doctor` always
/// diagnoses every language against one repo-root-wide `effective_root`.
/// This diagnoses a built-in language against `probe_files`' entry for it
/// when one exists (via [`resolution_workspace_root`]) — the exact file
/// [`lsp_live_section`] would actually probe, at the exact workspace root
/// [`is_resolved`] would compute for it — falling back to `effective_root`
/// only when no matching file is present in the repo (or there's no repo to
/// scan at all). Only TypeScript and Python's lookup actually varies by
/// workspace root (see `adapter::diagnose_language`'s project-local tier),
/// so this is a no-op for the other four built-ins — but for those two,
/// it's what keeps this section from ever telling a reviewer "not found"
/// while the very next section, probing the identical language, reports
/// "running fine": see this module's docs for the concrete nested-workspace
/// scenario this closes.
fn lsp_resolution_checks(
    effective_root: &Path,
    repo_root: Option<&Path>,
    probe_files: &HashMap<LangKey, PathBuf>,
    overrides: &HashMap<String, ServerOverride>,
    custom_extensions: &HashMap<String, String>,
) -> Vec<Check> {
    let mut checks: Vec<Check> = crate::ALL_LANGUAGES
        .into_iter()
        .flat_map(|language| {
            let workspace_root =
                resolution_workspace_root(language, effective_root, repo_root, probe_files);
            language_resolution_checks(language, &workspace_root, overrides)
        })
        .collect();
    checks.extend(custom_server_checks(overrides, custom_extensions));
    checks
}

/// The workspace root [`lsp_resolution_checks`] diagnoses `language`
/// against: `probe_files`' entry for it (joined onto `repo_root`, then
/// walked the same way [`is_resolved`] would — see [`adapter::workspace_root`])
/// when one exists, else `effective_root`. Pulled out as its own pure
/// function — no `adapter::diagnose` call inside it — so this specific
/// "which root did we even ask" decision is unit-testable without needing a
/// real project-local server install on disk (see this module's tests).
fn resolution_workspace_root(
    language: Language,
    effective_root: &Path,
    repo_root: Option<&Path>,
    probe_files: &HashMap<LangKey, PathBuf>,
) -> PathBuf {
    repo_root
        .zip(probe_files.get(&LangKey::Builtin(language)))
        .map(|(root, file)| adapter::workspace_root(&root.join(file), root, language))
        .unwrap_or_else(|| effective_root.to_path_buf())
}

fn language_resolution_checks(
    language: Language,
    workspace_root: &Path,
    overrides: &HashMap<String, ServerOverride>,
) -> Vec<Check> {
    let diagnosis = adapter::diagnose(language, workspace_root, overrides);
    let mut checks = vec![diagnosis_check(&diagnosis)];
    if language == Language::Java {
        let (jdk_ok, note) = adapter::java_jdk_status();
        let label = format!("{}: jdk", language.lsp_id());
        checks.push(if jdk_ok {
            Check::ok(label, note)
        } else {
            // A missing or too-old JDK is a real problem, not an `ok` row —
            // see this module's docs/tests on the bug this replaced (the
            // row used to be hardcoded `Check::ok` regardless of what the
            // note actually said, e.g. `ok java: jdk: not found`).
            Check::warn(label, note)
        });
    }
    if language == Language::Go {
        // Same shape as Java's jdk row, for the same reason: gopls is the
        // second server with a runtime prerequisite the resolution row
        // can't see — it spawns and initializes without `go`, then fails
        // every request with an opaque "no views".
        let (go_ok, note) = adapter::go_toolchain_status();
        let label = format!("{}: toolchain", language.lsp_id());
        checks.push(if go_ok {
            Check::ok(label, note)
        } else {
            Check::warn(label, note)
        });
    }
    checks
}

fn diagnosis_check(diagnosis: &Diagnosis) -> Check {
    let label = diagnosis.language.lsp_id().to_owned();
    match &diagnosis.found {
        Some((from, path)) => Check::ok(
            label,
            format!("{} ({})", path.display(), crate::resolved_from_label(*from)),
        ),
        None => Check::warn(
            label,
            adapter::install_hint(diagnosis.language, diagnosis.installable_if_missing),
        ),
    }
}

fn custom_server_checks(
    overrides: &HashMap<String, ServerOverride>,
    custom_extensions: &HashMap<String, String>,
) -> Vec<Check> {
    let mut custom_ids: Vec<&String> = overrides
        .iter()
        .filter(|(_, over)| !over.extensions.is_empty())
        .map(|(id, _)| id)
        .collect();
    custom_ids.sort();
    custom_ids
        .into_iter()
        .map(|id| custom_server_check(id, &overrides[id], custom_extensions))
        .collect()
}

/// The row label for `key` in `ktmr doctor`'s output — a built-in
/// language's own [`Language::lsp_id`], or `"{id} (custom)"` for a
/// `[lsp.servers.<id>]` entry (matching [`custom_server_check`]'s own
/// convention) — so a reviewer correlating rows between the "lsp
/// (resolution)" and "lsp (live probe)" sections for the same server sees
/// the identical label in both, instead of the live-probe section silently
/// dropping the `(custom)` qualifier the resolution section used.
fn key_label(key: &LangKey) -> String {
    match key {
        LangKey::Builtin(language) => language.lsp_id().to_owned(),
        LangKey::Custom(id) => format!("{id} (custom)"),
    }
}

/// One `[lsp.servers.<id>]` custom entry's resolution row — the same two
/// facts `print_custom_server_doctor` shows (whether `command` resolves on
/// `PATH`/as a direct path, via [`crate::locate_custom_command`]; whether
/// its `extensions` claim actually routes anywhere, via
/// [`crate::custom_extension_note`]), reused rather than re-derived so this
/// can never disagree with that table.
fn custom_server_check(id: &str, over: &ServerOverride, active: &HashMap<String, String>) -> Check {
    let label = key_label(&LangKey::Custom(id.to_owned()));
    let note = crate::custom_extension_note(id, over, active);
    match crate::locate_custom_command(&over.command) {
        Some(path) => match note {
            Some(note) => Check::warn(label, format!("{} — {note}", path.display())),
            None => Check::ok(label, path.display().to_string()),
        },
        None => {
            let base = format!("command `{}` not found", over.command);
            match note {
                Some(note) => Check::warn(label, format!("{base}; {note}")),
                None => Check::warn(label, base),
            }
        }
    }
}

// --- extension scan (shared by the live-probe section) ---------------------

/// Every path this repository has, tracked or not — the union
/// [`detect_present_languages`] classifies. Tracked
/// ([`GitSource::tracked_files`]) plus untracked-and-not-ignored
/// ([`GitSource::untracked_files`]): katamari reviews untracked files too
/// (`git diff --no-index` against `/dev/null`, see
/// [`GitSource::untracked_diff`]), and "I just added a new file and the
/// language server stayed silent" is issue #4's own motivating scenario —
/// scanning only tracked files would miss exactly that case.
fn scan_repo_files(git_source: &GitSource) -> Result<Vec<PathBuf>> {
    let mut files = git_source.tracked_files()?;
    files.extend(git_source.untracked_files()?);
    Ok(files)
}

/// Classifies `files` (repo-relative paths) by [`LangKey`], keeping *every*
/// matching file per key — sorted lexicographically, for deterministic
/// output across repeated runs regardless of what order `git ls-files`
/// happened to return them in. Pure and cheap (no filesystem reads beyond
/// what the caller already gathered, and deliberately no existence check —
/// a git-tracked path can be listed and still be gone from disk, e.g. an
/// unstaged deletion; that's [`select_probe_file`]'s job, done separately so
/// this stays a pure classification of what git *says* is here), so the
/// live-probe section's "which languages are even present" decision is
/// unit-testable without a real git repository — see this module's tests.
/// Keeping every candidate (not just the first) is what lets
/// [`select_probe_file`] fall through to the next one when the first is
/// gone, instead of the live-probe section being stuck with a single,
/// possibly-stale choice.
fn detect_present_languages(
    files: &[PathBuf],
    custom_extensions: &HashMap<String, String>,
) -> HashMap<LangKey, Vec<PathBuf>> {
    let mut sorted: Vec<&PathBuf> = files.iter().collect();
    sorted.sort();
    let mut map: HashMap<LangKey, Vec<PathBuf>> = HashMap::new();
    for file in sorted {
        if let Some(key) = LangKey::detect(file, custom_extensions) {
            map.entry(key).or_default().push(file.clone());
        }
    }
    map
}

/// Picks the file the live-probe section will actually probe for one
/// language: the first of `candidates` (already in
/// [`detect_present_languages`]'s deterministic, lexicographic order) that
/// satisfies `exists`. Pulled apart from any real filesystem check so the
/// fallback-to-the-next-candidate logic is unit-testable against a fake
/// predicate (see this module's tests); the real call site in
/// [`build_report`] passes `|p| repo_root.join(p).is_file()`.
///
/// Exists because a tracked file can be deleted from disk without staging
/// the deletion — `git ls-files --cached` keeps listing it regardless — and
/// probing that stale path would misreport a perfectly healthy language
/// server as an LSP I/O failure (`reading .../main.rs: No such file or
/// directory`) rather than skip it with a clear reason. `None` when every
/// candidate is gone.
fn select_probe_file(
    candidates: &[PathBuf],
    mut exists: impl FnMut(&Path) -> bool,
) -> Option<&PathBuf> {
    candidates.iter().find(|candidate| exists(candidate))
}

// --- --language ----------------------------------------------------------

/// `--language`'s runtime validation: a built-in [`Language::lsp_id`] string
/// (`rust`/`typescript`/`python`/`go`/`kotlin`/`java`), or a `[lsp.servers.<id>]`
/// entry that actually claims at least one extension (an id with no
/// `extensions` is a plain built-in override — see [`ServerOverride::extensions`]'s
/// docs — and was never routable in the first place, so it isn't a valid
/// live-probe target either). Not a `clap` `ValueEnum`: a custom id isn't
/// known until config loads, well after clap parses argv (see
/// `Command::Doctor::language`'s doc comment).
fn resolve_language_filter(
    value: &str,
    overrides: &HashMap<String, ServerOverride>,
) -> Result<LangKey> {
    if let Some(language) = builtin_language_by_name(value) {
        return Ok(LangKey::Builtin(language));
    }
    if overrides
        .get(value)
        .is_some_and(|over| !over.extensions.is_empty())
    {
        return Ok(LangKey::Custom(value.to_owned()));
    }
    bail!(
        "--language {value:?}: not one of rust/typescript/python/go/kotlin/java, and no \
         `[lsp.servers.{value}]` with `extensions` set is configured"
    )
}

/// A reverse lookup over [`crate::ALL_LANGUAGES`] rather than a second,
/// hand-maintained string table — the same anti-drift principle
/// [`adapter::is_builtin_language_id`] already applies to this exact
/// question.
fn builtin_language_by_name(value: &str) -> Option<Language> {
    crate::ALL_LANGUAGES
        .into_iter()
        .find(|language| language.lsp_id() == value)
}

// --- lsp (live probe) section --------------------------------------------

/// Safety margin above [`crate::lsp::client::Client::start`]'s own 30s
/// `initialize` timeout — `resolve_or_install` (auto-install always off
/// here, see [`build_report`]'s docs) never blocks on the network before
/// that, so this budget almost never matters in practice; it exists so a
/// genuinely wedged spawn thread can't hang `ktmr doctor` forever.
const SPAWN_BUDGET: Duration = Duration::from_secs(35);
/// The hover round trip's own budget, deliberately shorter than
/// [`SPAWN_BUDGET`]: an already-`Ready` server that never answers a hover is
/// a different, faster-to-suspect problem (still indexing — see
/// [`classify_hover_outcome`]'s wording) than one that never even finished
/// `initialize`.
const HOVER_BUDGET: Duration = Duration::from_secs(20);
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// `present`/`probe_files` are computed once in [`build_report`] (shared
/// with [`lsp_resolution_checks`] — see its docs) rather than scanned here a
/// second time.
fn lsp_live_section(
    repo_root: &Path,
    present: &HashMap<LangKey, Vec<PathBuf>>,
    probe_files: &HashMap<LangKey, PathBuf>,
    overrides: &HashMap<String, ServerOverride>,
    filter: Option<&LangKey>,
) -> Section {
    let title = "lsp (live probe)".to_owned();

    if let Some(filter_key) = filter
        && !present.contains_key(filter_key)
    {
        return Section {
            title,
            checks: vec![Check::warn(
                "lsp live probe",
                format!("no `{filter_key}` files found in this repository — nothing to probe"),
            )],
        };
    }

    let (targets, mut checks) =
        partition_live_targets(present, probe_files, filter, |key, rel_path| {
            is_resolved(key, &repo_root.join(rel_path), repo_root, overrides)
        });

    if targets.is_empty() {
        if checks.is_empty() {
            checks.push(Check::ok(
                "lsp live probe",
                "no built-in or custom language files found in this repository",
            ));
        }
        return Section { title, checks };
    }

    // One shared manager for every target this run (mirroring how a real
    // `ktmr diff`/`ktmr open` session shares one `LspManager` across every
    // language it touches), spawning sequentially — `probe_language` fully
    // resolves one target (spawn, then hover) before the loop moves to the
    // next, so a slow server (jdtls) doesn't compete for attention with the
    // next language's own probe. `shutdown_all` once, at the end, covers
    // every server this run spawned — see `run_lsp_check`'s identical
    // "shut down before exit" rule, which this is bound by too.
    let (events_tx, _events_rx) = std::sync::mpsc::channel();
    let manager = LspManager::new(events_tx, Arc::new(overrides.clone()), false);
    for (key, rel_path) in &targets {
        let abs_file = repo_root.join(rel_path);
        checks.extend(probe_language(
            &manager,
            &key_label(key),
            &abs_file,
            repo_root,
        ));
    }
    manager.shutdown_all();

    Section { title, checks }
}

/// Whether `key`'s server statically resolves — the same question
/// [`lsp_resolution_checks`] answers per built-in language, asked again here
/// (cheaply: filesystem/`PATH` lookups, never a process spawn) because the
/// live-probe section needs a plain `bool` per *present* language/custom id,
/// not a full [`Check`] row. For [`LangKey::Custom`], "resolved" means
/// exactly what [`custom_server_check`] already checks —
/// [`crate::locate_custom_command`] actually finding the configured command
/// on `PATH` or as a direct path — not just a non-empty `command` string
/// ([`adapter::resolve_custom_server`]'s own, narrower notion of "resolved,"
/// meant for building the `Command` to spawn, not for deciding whether to
/// attempt a spawn at all). Using the narrower check here used to mean an
/// unresolved custom server (a typo'd or uninstalled `command`) got spawned
/// anyway instead of skipped with the documented note.
fn is_resolved(
    key: &LangKey,
    abs_file: &Path,
    repo_root: &Path,
    overrides: &HashMap<String, ServerOverride>,
) -> bool {
    match key {
        LangKey::Builtin(language) => {
            let workspace_root = adapter::workspace_root(abs_file, repo_root, *language);
            adapter::diagnose(*language, &workspace_root, overrides)
                .found
                .is_some()
        }
        LangKey::Custom(id) => overrides
            .get(id)
            .is_some_and(|over| crate::locate_custom_command(&over.command).is_some()),
    }
}

/// Splits `present` into probe targets and skip notes — the one piece of
/// live-probe orchestration logic pure enough to unit test directly: given
/// which languages are present, which of them still has a readable file to
/// probe (`probe_files` — see [`select_probe_file`]), and a resolution
/// predicate (a real one calls [`is_resolved`]; a test can inject a fake),
/// this decides who gets probed, who gets a [`unresolved_skip_check`], and
/// who gets a [`no_probe_file_skip_check`] instead, with no process spawning
/// anywhere in it. `filter` narrows to one key (`--language`) the same way
/// [`lsp_live_section`]'s caller already checked it exists in `present`
/// before reaching here.
fn partition_live_targets(
    present: &HashMap<LangKey, Vec<PathBuf>>,
    probe_files: &HashMap<LangKey, PathBuf>,
    filter: Option<&LangKey>,
    mut is_resolved: impl FnMut(&LangKey, &Path) -> bool,
) -> (Vec<(LangKey, PathBuf)>, Vec<Check>) {
    let mut keys: Vec<&LangKey> = present.keys().collect();
    keys.sort_by_key(|key| key.to_string());

    let mut targets = Vec::new();
    let mut checks = Vec::new();
    for key in keys {
        if filter.is_some_and(|wanted| wanted != key) {
            continue;
        }
        let label = key_label(key);
        match probe_files.get(key) {
            Some(rel_path) => {
                if is_resolved(key, rel_path) {
                    targets.push((key.clone(), rel_path.clone()));
                } else {
                    checks.push(unresolved_skip_check(&label));
                }
            }
            None => checks.push(no_probe_file_skip_check(&label)),
        }
    }
    (targets, checks)
}

fn unresolved_skip_check(label: &str) -> Check {
    Check::warn(
        label,
        "present in this repository, but its server didn't resolve — skipped (see \"lsp \
         (resolution)\" above)",
    )
}

/// The skip note for a language that's present in the repo (per
/// [`detect_present_languages`]) but whose every candidate file is gone by
/// the time [`select_probe_file`] goes looking for one to actually read —
/// e.g. a tracked file deleted from disk without staging the deletion, so
/// `git ls-files --cached` still lists it. Kept distinct from
/// [`unresolved_skip_check`] (a resolution problem) since this is a
/// filesystem problem: probing it anyway would misreport a perfectly
/// healthy language server as an LSP I/O failure (see this module's docs on
/// the bug this was pulled out to fix).
fn no_probe_file_skip_check(label: &str) -> Check {
    Check::warn(
        label,
        "present in this repository, but every matching file is missing on disk (e.g. an \
         uncommitted deletion) — skipped",
    )
}

/// First line of `path`'s content, or `""` if the file is empty *or*
/// unreadable — deliberately never a hard failure the way
/// `run_lsp_check`'s own line lookup is (`.with_context(...)?`, `main.rs`
/// ~798-802): a probe file the doctor picked itself (from a real repo scan)
/// being empty is a normal, unremarkable case, not the user-error a manual
/// `ktmr lsp-check <file> <line> <col>` typo would be. `manager.hover`
/// dispatches purely on `line_text`/`line`/`col` with no symbol resolution
/// of its own (see `LspManager::submit`'s docs), so an empty line is a
/// perfectly valid — if uninteresting — hover target: the round trip still
/// completes, which is all this probe checks.
fn probe_line_text(path: &Path) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| content.lines().next().map(str::to_owned))
        .unwrap_or_default()
}

/// One language's whole probe: kicks off a hover request (which is also
/// what makes [`LspManager`] spawn the server at all — it's lazy), waits for
/// spawn+initialize to settle one way or another, and — only if that
/// succeeded — waits for the hover round trip too. Returns one `Check` (just
/// `spawn+initialize`) on a spawn failure/timeout, since there's no
/// connection left to hover through; two checks on success.
fn probe_language(
    manager: &LspManager,
    label: &str,
    abs_file: &Path,
    repo_root: &Path,
) -> Vec<Check> {
    // Per-language progress line to stderr (never stdout, which either the
    // text report or `--json` owns exclusively) — the spec's "so slow ones
    // (jdtls) show progress" requirement, without corrupting either output
    // mode.
    eprintln!("ktmr doctor: probing {label} ({})...", abs_file.display());
    let line_text = probe_line_text(abs_file);
    let hover_rx = manager.hover(abs_file, repo_root, &line_text, 0, 0);

    let spawn_started = Instant::now();
    let spawn_outcome = wait_for_spawn(manager, abs_file, repo_root, spawn_started, SPAWN_BUDGET);
    let spawn_check = classify_spawn_outcome(label, &spawn_outcome);
    if !matches!(spawn_outcome, SpawnOutcome::Ready { .. }) {
        return vec![spawn_check];
    }

    let hover_started = Instant::now();
    let hover_outcome = wait_for_hover(
        &hover_rx,
        manager,
        abs_file,
        repo_root,
        hover_started,
        HOVER_BUDGET,
    );
    let hover_check = classify_hover_outcome(label, &hover_outcome);
    vec![spawn_check, hover_check]
}

fn wait_for_spawn(
    manager: &LspManager,
    file: &Path,
    git_root: &Path,
    started: Instant,
    budget: Duration,
) -> SpawnOutcome {
    let deadline = started + budget;
    loop {
        match manager.state(file, git_root) {
            ServerState::Ready => {
                return SpawnOutcome::Ready {
                    elapsed: started.elapsed(),
                };
            }
            ServerState::Unavailable { reason } | ServerState::Crashed { reason } => {
                return SpawnOutcome::Failed {
                    reason,
                    elapsed: started.elapsed(),
                };
            }
            ServerState::NotStarted | ServerState::Starting | ServerState::Installing { .. } => {}
        }
        if Instant::now() >= deadline {
            return SpawnOutcome::TimedOut {
                elapsed: started.elapsed(),
            };
        }
        std::thread::sleep(PROBE_POLL_INTERVAL);
    }
}

fn wait_for_hover(
    hover_rx: &std::sync::mpsc::Receiver<Result<lsp::HoverResult, lsp::LspError>>,
    manager: &LspManager,
    file: &Path,
    git_root: &Path,
    started: Instant,
    budget: Duration,
) -> HoverOutcome {
    let deadline = started + budget;
    loop {
        if let Ok(result) = hover_rx.try_recv() {
            return match result {
                // Success is "the round trip completed," full stop — even a
                // `None`/empty hover result, per the spec: a null answer
                // still proves the request-response cycle works end to end.
                Ok(_) => HoverOutcome::Completed {
                    elapsed: started.elapsed(),
                },
                Err(e) => HoverOutcome::Failed {
                    reason: e.to_string(),
                },
            };
        }
        let state = manager.state(file, git_root);
        if let ServerState::Unavailable { reason } | ServerState::Crashed { reason } = &state {
            return HoverOutcome::Failed {
                reason: reason.clone(),
            };
        }
        if Instant::now() >= deadline {
            return HoverOutcome::TimedOut {
                elapsed: started.elapsed(),
                state,
            };
        }
        std::thread::sleep(PROBE_POLL_INTERVAL);
    }
}

#[derive(Debug, Clone, PartialEq)]
enum SpawnOutcome {
    Ready { elapsed: Duration },
    Failed { reason: String, elapsed: Duration },
    TimedOut { elapsed: Duration },
}

#[derive(Debug, Clone, PartialEq)]
enum HoverOutcome {
    Completed {
        elapsed: Duration,
    },
    Failed {
        reason: String,
    },
    TimedOut {
        elapsed: Duration,
        state: ServerState,
    },
}

/// Pure: turns a [`SpawnOutcome`] into a `Check`, with no manager/receiver
/// involved — the decision logic [`probe_language`] delegates to, tested
/// directly against synthetic outcomes (see this module's tests) rather
/// than only reachable by actually spawning a server.
fn classify_spawn_outcome(label: &str, outcome: &SpawnOutcome) -> Check {
    let check_label = format!("{label}: spawn+initialize");
    match outcome {
        SpawnOutcome::Ready { elapsed } => Check::ok(
            check_label,
            format!("ready in {:.1}s", elapsed.as_secs_f64()),
        ),
        // `reason` already carries the transport's stderr tail where one is
        // available (see `Client::start`'s `augment_with_stderr` call) —
        // this is the "the stderr-tail wiring makes these actionable" the
        // spec calls out, reused verbatim rather than summarized.
        SpawnOutcome::Failed { reason, .. } => Check::error(check_label, reason.clone()),
        SpawnOutcome::TimedOut { elapsed } => Check::error(
            check_label,
            format!(
                "timed out after {}s waiting to become ready",
                elapsed.as_secs()
            ),
        ),
    }
}

/// As [`classify_spawn_outcome`], for [`HoverOutcome`]. The timeout wording
/// is pinned to the spec's exact phrasing (mentions indexing, names the
/// state) — see this module's tests.
fn classify_hover_outcome(label: &str, outcome: &HoverOutcome) -> Check {
    let check_label = format!("{label}: hover round-trip");
    match outcome {
        HoverOutcome::Completed { elapsed } => Check::ok(
            check_label,
            format!("completed in {:.1}s", elapsed.as_secs_f64()),
        ),
        HoverOutcome::Failed { reason } => Check::error(check_label, reason.clone()),
        HoverOutcome::TimedOut { elapsed, state } => Check::error(
            check_label,
            format!(
                "request timed out after {}s — server may still be indexing; large projects can \
                 take minutes on first open (state: {state:?})",
                elapsed.as_secs()
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- report model / rendering ---------------------------------------

    fn sample_report(statuses: &[Status]) -> HealthReport {
        let checks = statuses
            .iter()
            .enumerate()
            .map(|(i, status)| Check {
                status: *status,
                label: format!("check {i}"),
                detail: format!("detail {i}"),
            })
            .collect();
        HealthReport {
            sections: vec![Section {
                title: "section".to_owned(),
                checks,
            }],
        }
    }

    #[test]
    fn exit_code_is_zero_with_only_ok_and_warn_checks() {
        let report = sample_report(&[Status::Ok, Status::Warn, Status::Warn]);
        assert_eq!(exit_code(&report), 0);
    }

    #[test]
    fn exit_code_is_one_when_any_check_is_error() {
        let report = sample_report(&[Status::Ok, Status::Warn, Status::Error]);
        assert_eq!(exit_code(&report), 1);
    }

    #[test]
    fn render_text_aligns_the_tag_column_and_includes_label_and_detail() {
        let report = sample_report(&[Status::Ok, Status::Warn, Status::Error]);
        let text = render_text(&report);
        assert!(text.contains("  ok    check 0: detail 0"), "{text}");
        assert!(text.contains("  warn  check 1: detail 1"), "{text}");
        assert!(text.contains("  error check 2: detail 2"), "{text}");
    }

    #[test]
    fn render_text_omits_the_colon_when_detail_is_empty() {
        let report = HealthReport {
            sections: vec![Section {
                title: "section".to_owned(),
                checks: vec![Check::ok("bare label", "")],
            }],
        };
        let text = render_text(&report);
        assert!(text.contains("  ok    bare label\n"), "{text}");
        assert!(!text.contains("bare label:"), "{text}");
    }

    #[test]
    fn summary_line_reports_all_checks_passed_when_nothing_warned_or_errored() {
        let report = sample_report(&[Status::Ok, Status::Ok]);
        assert_eq!(summary_line(&report), "all checks passed");
    }

    #[test]
    fn summary_line_singular_and_plural_and_warnings_before_errors() {
        assert_eq!(summary_line(&sample_report(&[Status::Warn])), "1 warning");
        assert_eq!(
            summary_line(&sample_report(&[Status::Warn, Status::Warn])),
            "2 warnings"
        );
        assert_eq!(summary_line(&sample_report(&[Status::Error])), "1 error");
        assert_eq!(
            summary_line(&sample_report(&[Status::Warn, Status::Warn, Status::Error])),
            "2 warnings, 1 error"
        );
    }

    #[test]
    fn json_round_trips_with_stable_lowercase_field_names() {
        let report = sample_report(&[Status::Ok, Status::Warn, Status::Error]);
        let value: serde_json::Value = serde_json::to_value(&report).unwrap();
        let sections = value["sections"].as_array().unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0]["title"], "section");
        let checks = sections[0]["checks"].as_array().unwrap();
        assert_eq!(checks.len(), 3);
        assert_eq!(checks[0]["status"], "ok");
        assert_eq!(checks[1]["status"], "warn");
        assert_eq!(checks[2]["status"], "error");
        assert_eq!(checks[0]["label"], "check 0");
        assert_eq!(checks[0]["detail"], "detail 0");
    }

    // --- flatten_detail / newline-safe Check construction -------------------

    #[test]
    fn flatten_detail_collapses_newline_runs_into_semicolons() {
        // The exact multi-line shape a real `toml` 0.9 deserialize error's
        // `Display` produces (see `config::parse_with_warnings`'s docs).
        let raw = "invalid type: string \"four\", expected usize\nin `ui.tab_width`\n";
        assert_eq!(
            flatten_detail(raw),
            "invalid type: string \"four\", expected usize; in `ui.tab_width`"
        );
    }

    #[test]
    fn flatten_detail_drops_blank_lines_instead_of_doubling_up_the_separator() {
        assert_eq!(
            flatten_detail("line one\n\nline two\n"),
            "line one; line two"
        );
    }

    #[test]
    fn flatten_detail_leaves_a_single_line_untouched() {
        assert_eq!(flatten_detail("ready in 1.4s"), "ready in 1.4s");
        assert_eq!(flatten_detail(""), "");
    }

    #[test]
    fn check_detail_never_contains_a_raw_newline_even_when_constructed_from_multiline_text() {
        // The Check-construction boundary itself, not just the pure helper:
        // proves `Check::warn`/`ok`/`error` all flatten `detail` before it's
        // ever stored, so `render_check_line`'s one-line-per-check
        // assumption holds regardless of which constructor a caller used.
        let warn = Check::warn("label", "line one\nline two\n\nline three");
        assert!(!warn.detail.contains('\n'), "{}", warn.detail);
        assert_eq!(warn.detail, "line one; line two; line three");

        let ok = Check::ok("label", "a\nb");
        assert_eq!(ok.detail, "a; b");

        let error = Check::error("label", "a\nb");
        assert_eq!(error.detail, "a; b");
    }

    // --- extension scan ---------------------------------------------------

    #[test]
    fn detect_present_languages_maps_builtin_extensions_and_keeps_every_file_sorted() {
        let files = vec![
            PathBuf::from("src/z.rs"),
            PathBuf::from("src/a.rs"),
            PathBuf::from("README.md"),
        ];
        let present = detect_present_languages(&files, &HashMap::new());
        assert_eq!(
            present.get(&LangKey::Builtin(Language::Rust)),
            Some(&vec![PathBuf::from("src/a.rs"), PathBuf::from("src/z.rs")]),
            "every matching file is kept, lexicographically sorted"
        );
        assert_eq!(present.len(), 1, "README.md has no detected language");
    }

    #[test]
    fn detect_present_languages_includes_a_custom_extension_case() {
        let files = vec![PathBuf::from("app.rb"), PathBuf::from("Gemfile")];
        let custom_extensions = HashMap::from([("rb".to_owned(), "ruby".to_owned())]);
        let present = detect_present_languages(&files, &custom_extensions);
        assert_eq!(
            present.get(&LangKey::Custom("ruby".to_owned())),
            Some(&vec![PathBuf::from("app.rb")])
        );
        assert_eq!(present.len(), 1, "Gemfile has no extension to route on");
    }

    #[test]
    fn detect_present_languages_is_empty_for_no_recognized_extensions() {
        let files = vec![PathBuf::from("README.md"), PathBuf::from("LICENSE")];
        assert!(detect_present_languages(&files, &HashMap::new()).is_empty());
    }

    // --- select_probe_file -------------------------------------------------

    #[test]
    fn select_probe_file_picks_the_first_existing_candidate_in_order() {
        let candidates = vec![
            PathBuf::from("a.rs"),
            PathBuf::from("b.rs"),
            PathBuf::from("c.rs"),
        ];
        // "a.rs" is present but doesn't exist (e.g. an unstaged deletion) —
        // the next candidate in deterministic order wins.
        let selected = select_probe_file(&candidates, |p| p != Path::new("a.rs"));
        assert_eq!(selected, Some(&PathBuf::from("b.rs")));
    }

    #[test]
    fn select_probe_file_returns_the_only_candidate_when_it_exists() {
        let candidates = vec![PathBuf::from("only.rs")];
        let selected = select_probe_file(&candidates, |_| true);
        assert_eq!(selected, Some(&PathBuf::from("only.rs")));
    }

    #[test]
    fn select_probe_file_returns_none_when_every_candidate_is_gone() {
        let candidates = vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")];
        let selected = select_probe_file(&candidates, |_| false);
        assert_eq!(selected, None);
    }

    // --- --language ---------------------------------------------------

    #[test]
    fn resolve_language_filter_accepts_a_builtin_language_name() {
        let key = resolve_language_filter("rust", &HashMap::new()).unwrap();
        assert_eq!(key, LangKey::Builtin(Language::Rust));
    }

    #[test]
    fn resolve_language_filter_accepts_a_configured_custom_id() {
        let overrides = HashMap::from([(
            "ruby".to_owned(),
            ServerOverride {
                command: "solargraph".to_owned(),
                extensions: vec!["rb".to_owned()],
                ..ServerOverride::default()
            },
        )]);
        let key = resolve_language_filter("ruby", &overrides).unwrap();
        assert_eq!(key, LangKey::Custom("ruby".to_owned()));
    }

    #[test]
    fn resolve_language_filter_rejects_an_unknown_id() {
        let err = resolve_language_filter("totally-unknown", &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("totally-unknown"));
    }

    #[test]
    fn resolve_language_filter_rejects_a_custom_id_with_no_extensions_configured() {
        // An id with no `extensions` is a plain built-in override (or an
        // inert entry) — never a routable custom language, so it must not
        // pass as a live-probe target either.
        let overrides = HashMap::from([(
            "rust".to_owned(),
            ServerOverride {
                command: "/opt/bin/rust-analyzer".to_owned(),
                ..ServerOverride::default()
            },
        )]);
        // "rust" is still accepted, but as the *built-in* language, not
        // because of the override entry.
        let key = resolve_language_filter("rust", &overrides).unwrap();
        assert_eq!(key, LangKey::Builtin(Language::Rust));

        let overrides = HashMap::from([(
            "myserver".to_owned(),
            ServerOverride {
                command: "myserver".to_owned(),
                ..ServerOverride::default()
            },
        )]);
        assert!(resolve_language_filter("myserver", &overrides).is_err());
    }

    // --- probe_line_text ---------------------------------------------------

    #[test]
    fn probe_line_text_returns_the_first_line_of_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, "first line\nsecond line\n").unwrap();
        assert_eq!(probe_line_text(&path), "first line");
    }

    #[test]
    fn probe_line_text_is_empty_for_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").unwrap();
        assert_eq!(probe_line_text(&path), "");
    }

    #[test]
    fn probe_line_text_is_empty_for_a_missing_file() {
        let path = PathBuf::from("/nonexistent/definitely-not-here.txt");
        assert_eq!(probe_line_text(&path), "");
    }

    // --- live-probe classification (pure, no spawning) ----------------------

    #[test]
    fn classify_spawn_outcome_ready_reports_ok_with_timing() {
        let outcome = SpawnOutcome::Ready {
            elapsed: Duration::from_millis(1400),
        };
        let check = classify_spawn_outcome("rust", &outcome);
        assert_eq!(check.status, Status::Ok);
        assert_eq!(check.label, "rust: spawn+initialize");
        assert_eq!(check.detail, "ready in 1.4s");
    }

    #[test]
    fn classify_spawn_outcome_failed_reports_error_with_the_reason() {
        let outcome = SpawnOutcome::Failed {
            reason: "rust-analyzer not found on PATH".to_owned(),
            elapsed: Duration::from_millis(5),
        };
        let check = classify_spawn_outcome("rust", &outcome);
        assert_eq!(check.status, Status::Error);
        assert_eq!(check.detail, "rust-analyzer not found on PATH");
    }

    #[test]
    fn classify_spawn_outcome_timed_out_reports_error() {
        let outcome = SpawnOutcome::TimedOut {
            elapsed: Duration::from_secs(35),
        };
        let check = classify_spawn_outcome("java", &outcome);
        assert_eq!(check.status, Status::Error);
        assert!(
            check.detail.contains("timed out after 35s"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn classify_hover_outcome_completed_reports_ok_regardless_of_hover_content() {
        let outcome = HoverOutcome::Completed {
            elapsed: Duration::from_millis(200),
        };
        let check = classify_hover_outcome("rust", &outcome);
        assert_eq!(check.status, Status::Ok);
        assert_eq!(check.label, "rust: hover round-trip");
        assert_eq!(check.detail, "completed in 0.2s");
    }

    #[test]
    fn classify_hover_outcome_failed_reports_error() {
        let outcome = HoverOutcome::Failed {
            reason: "lsp transport closed".to_owned(),
        };
        let check = classify_hover_outcome("go", &outcome);
        assert_eq!(check.status, Status::Error);
        assert_eq!(check.detail, "lsp transport closed");
    }

    #[test]
    fn classify_hover_outcome_timed_out_mentions_indexing_and_the_state() {
        let outcome = HoverOutcome::TimedOut {
            elapsed: Duration::from_secs(20),
            state: ServerState::Ready,
        };
        let check = classify_hover_outcome("kotlin", &outcome);
        assert_eq!(check.status, Status::Error);
        assert!(
            check.detail.contains("timed out after 20s"),
            "{}",
            check.detail
        );
        assert!(check.detail.contains("indexing"), "{}", check.detail);
        assert!(check.detail.contains("state: Ready"), "{}", check.detail);
    }

    // --- unresolved_skip_check / diagnosis_check / custom_server_check ------

    #[test]
    fn unresolved_skip_check_is_a_warn_mentioning_skipped() {
        let check = unresolved_skip_check("go");
        assert_eq!(check.status, Status::Warn);
        assert_eq!(check.label, "go");
        assert!(check.detail.contains("skipped"), "{}", check.detail);
    }

    #[test]
    fn diagnosis_check_is_ok_when_found() {
        let diagnosis = Diagnosis {
            language: Language::Rust,
            found: Some((
                adapter::ResolvedFrom::Path,
                PathBuf::from("/usr/bin/rust-analyzer"),
            )),
            installable_if_missing: true,
        };
        let check = diagnosis_check(&diagnosis);
        assert_eq!(check.status, Status::Ok);
        assert_eq!(check.label, "rust");
        assert!(
            check.detail.contains("/usr/bin/rust-analyzer"),
            "{}",
            check.detail
        );
        assert!(check.detail.contains("PATH"), "{}", check.detail);
    }

    #[test]
    fn diagnosis_check_is_warn_with_a_hint_when_not_found() {
        let diagnosis = Diagnosis {
            language: Language::Go,
            found: None,
            installable_if_missing: true,
        };
        let check = diagnosis_check(&diagnosis);
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("gopls"), "{}", check.detail);
    }

    #[test]
    fn custom_server_check_is_ok_when_the_command_resolves_and_routes_cleanly() {
        let over = ServerOverride {
            command: "/bin/sh".to_owned(), // guaranteed to exist and be a direct path
            extensions: vec!["rb".to_owned()],
            ..ServerOverride::default()
        };
        let active = HashMap::from([("rb".to_owned(), "ruby".to_owned())]);
        let check = custom_server_check("ruby", &over, &active);
        assert_eq!(check.status, Status::Ok);
        assert_eq!(check.detail, "/bin/sh");
    }

    #[test]
    fn custom_server_check_is_warn_when_the_command_is_missing() {
        let over = ServerOverride {
            command: "totally-nonexistent-lsp-binary-xyz".to_owned(),
            extensions: vec!["rb".to_owned()],
            ..ServerOverride::default()
        };
        let active = HashMap::from([("rb".to_owned(), "ruby".to_owned())]);
        let check = custom_server_check("ruby", &over, &active);
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("not found"), "{}", check.detail);
    }

    #[test]
    fn custom_server_check_is_warn_when_extensions_dont_route_anywhere() {
        let over = ServerOverride {
            command: "/bin/sh".to_owned(),
            extensions: vec!["rs".to_owned()], // shadowed by the built-in Rust server
            ..ServerOverride::default()
        };
        let active = HashMap::new(); // nothing claims .rs — built-in Rust owns it
        let check = custom_server_check("myruby", &over, &active);
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("shadowed"), "{}", check.detail);
    }

    #[test]
    fn key_label_matches_custom_server_checks_own_convention() {
        assert_eq!(key_label(&LangKey::Builtin(Language::Rust)), "rust");
        assert_eq!(
            key_label(&LangKey::Custom("ruby".to_owned())),
            "ruby (custom)"
        );
    }

    // --- is_resolved ----------------------------------------------------

    #[test]
    fn is_resolved_for_custom_is_false_when_the_command_does_not_actually_resolve() {
        // `resolve_custom_server` alone (the old check) only fails on an
        // *empty* command — a wrong-but-non-empty command like this one
        // would have passed it. `is_resolved` must actually check the
        // command resolves, the same way `custom_server_check` does, or an
        // unresolved custom server gets live-probed instead of skipped.
        let overrides = HashMap::from([(
            "ruby".to_owned(),
            ServerOverride {
                command: "totally-nonexistent-lsp-binary-xyz".to_owned(),
                extensions: vec!["rb".to_owned()],
                ..ServerOverride::default()
            },
        )]);
        let key = LangKey::Custom("ruby".to_owned());
        assert!(!is_resolved(
            &key,
            Path::new("/repo/app.rb"),
            Path::new("/repo"),
            &overrides
        ));
    }

    #[test]
    fn is_resolved_for_custom_is_true_when_the_command_actually_resolves() {
        let overrides = HashMap::from([(
            "ruby".to_owned(),
            ServerOverride {
                command: "/bin/sh".to_owned(), // guaranteed to exist and be a direct path
                extensions: vec!["rb".to_owned()],
                ..ServerOverride::default()
            },
        )]);
        let key = LangKey::Custom("ruby".to_owned());
        assert!(is_resolved(
            &key,
            Path::new("/repo/app.rb"),
            Path::new("/repo"),
            &overrides
        ));
    }

    #[test]
    fn is_resolved_for_custom_is_false_with_no_matching_override() {
        let key = LangKey::Custom("ruby".to_owned());
        assert!(!is_resolved(
            &key,
            Path::new("/repo/app.rb"),
            Path::new("/repo"),
            &HashMap::new()
        ));
    }

    // --- resolution_workspace_root (resolution/live-probe agreement) --------

    #[test]
    fn resolution_workspace_root_falls_back_to_effective_root_with_no_probe_file() {
        let root = resolution_workspace_root(
            Language::TypeScript,
            Path::new("/effective"),
            Some(Path::new("/repo")),
            &HashMap::new(),
        );
        assert_eq!(root, PathBuf::from("/effective"));
    }

    #[test]
    fn resolution_workspace_root_falls_back_to_effective_root_outside_a_repo() {
        let probe_files = HashMap::from([(
            LangKey::Builtin(Language::TypeScript),
            PathBuf::from("frontend/app.ts"),
        )]);
        let root = resolution_workspace_root(
            Language::TypeScript,
            Path::new("/effective"),
            None,
            &probe_files,
        );
        assert_eq!(root, PathBuf::from("/effective"));
    }

    #[test]
    fn resolution_workspace_root_uses_the_probe_files_workspace_when_present() {
        // The concrete scenario the finding this fixes describes: a nested
        // TypeScript project (its own `package.json`) under a repo root
        // that has nothing at the top level to mark it — the resolution
        // section must diagnose against the *nested* root, exactly like the
        // live-probe section's `is_resolved` would, not the (deliberately
        // different, and wrong for this purpose) flat `effective_root`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("frontend")).unwrap();
        std::fs::write(dir.path().join("frontend").join("package.json"), "{}").unwrap();
        let probe_files = HashMap::from([(
            LangKey::Builtin(Language::TypeScript),
            PathBuf::from("frontend/app.ts"),
        )]);
        let root = resolution_workspace_root(
            Language::TypeScript,
            dir.path(), // effective_root — deliberately not the expected answer
            Some(dir.path()),
            &probe_files,
        );
        assert_eq!(root, dir.path().join("frontend"));
    }

    // --- language_resolution_checks (Java jdk sub-check status) -------------

    #[test]
    fn language_resolution_checks_java_jdk_subcheck_status_matches_java_jdk_status() {
        let checks =
            language_resolution_checks(Language::Java, Path::new("/repo"), &HashMap::new());
        let jdk_check = checks
            .iter()
            .find(|c| c.label == "java: jdk")
            .expect("a `java: jdk` row is always present for the Java language");
        let (ok, note) = adapter::java_jdk_status();
        assert_eq!(jdk_check.detail, note, "{jdk_check:?}");
        assert_eq!(
            jdk_check.status,
            if ok { Status::Ok } else { Status::Warn },
            "a `not found`/too-old note must never be tagged `ok`: {jdk_check:?}"
        );
    }

    #[test]
    fn language_resolution_checks_go_toolchain_subcheck_status_matches_go_toolchain_status() {
        // Go's twin of the jdk sub-check test above: gopls initializes fine
        // without a `go` binary and then fails every request with "no
        // views", so the toolchain row must exist and must never tag a
        // not-found note `ok`.
        let checks = language_resolution_checks(Language::Go, Path::new("/repo"), &HashMap::new());
        let toolchain_check = checks
            .iter()
            .find(|c| c.label == "go: toolchain")
            .expect("a `go: toolchain` row is always present for the Go language");
        let (ok, note) = adapter::go_toolchain_status();
        assert_eq!(toolchain_check.detail, note, "{toolchain_check:?}");
        assert_eq!(
            toolchain_check.status,
            if ok { Status::Ok } else { Status::Warn },
            "a not-found note must never be tagged `ok`: {toolchain_check:?}"
        );
    }

    // --- partition_live_targets (pure orchestration logic) ------------------

    #[test]
    fn partition_live_targets_splits_resolved_from_unresolved() {
        let present = HashMap::from([
            (
                LangKey::Builtin(Language::Rust),
                vec![PathBuf::from("a.rs")],
            ),
            (LangKey::Builtin(Language::Go), vec![PathBuf::from("b.go")]),
        ]);
        let probe_files = HashMap::from([
            (LangKey::Builtin(Language::Rust), PathBuf::from("a.rs")),
            (LangKey::Builtin(Language::Go), PathBuf::from("b.go")),
        ]);
        let (targets, checks) = partition_live_targets(&present, &probe_files, None, |key, _| {
            matches!(key, LangKey::Builtin(Language::Rust))
        });
        assert_eq!(
            targets,
            vec![(LangKey::Builtin(Language::Rust), PathBuf::from("a.rs"))]
        );
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Warn);
        assert!(checks[0].detail.contains("skipped"), "{}", checks[0].detail);
    }

    #[test]
    fn partition_live_targets_orders_deterministically_by_display() {
        let present = HashMap::from([
            (
                LangKey::Custom("zeta".to_owned()),
                vec![PathBuf::from("z.zz")],
            ),
            (
                LangKey::Builtin(Language::Rust),
                vec![PathBuf::from("a.rs")],
            ),
        ]);
        let probe_files = HashMap::from([
            (LangKey::Custom("zeta".to_owned()), PathBuf::from("z.zz")),
            (LangKey::Builtin(Language::Rust), PathBuf::from("a.rs")),
        ]);
        let (targets, _) = partition_live_targets(&present, &probe_files, None, |_, _| true);
        assert_eq!(
            targets,
            vec![
                (LangKey::Builtin(Language::Rust), PathBuf::from("a.rs")),
                (LangKey::Custom("zeta".to_owned()), PathBuf::from("z.zz")),
            ],
            "\"rust\" sorts before \"zeta\""
        );
    }

    #[test]
    fn partition_live_targets_honors_a_filter() {
        let present = HashMap::from([
            (
                LangKey::Builtin(Language::Rust),
                vec![PathBuf::from("a.rs")],
            ),
            (LangKey::Builtin(Language::Go), vec![PathBuf::from("b.go")]),
        ]);
        let probe_files = HashMap::from([
            (LangKey::Builtin(Language::Rust), PathBuf::from("a.rs")),
            (LangKey::Builtin(Language::Go), PathBuf::from("b.go")),
        ]);
        let filter = LangKey::Builtin(Language::Go);
        let (targets, checks) =
            partition_live_targets(&present, &probe_files, Some(&filter), |_, _| true);
        assert_eq!(
            targets,
            vec![(LangKey::Builtin(Language::Go), PathBuf::from("b.go"))]
        );
        assert!(
            checks.is_empty(),
            "the filtered-out language gets no row at all"
        );
    }

    #[test]
    fn partition_live_targets_emits_a_distinct_skip_note_when_every_candidate_file_is_gone() {
        let present = HashMap::from([(
            LangKey::Builtin(Language::Rust),
            vec![PathBuf::from("a.rs")],
        )]);
        // No entry for the Rust key at all — `select_probe_file` found
        // nothing that still exists on disk.
        let probe_files = HashMap::new();
        let (targets, checks) = partition_live_targets(&present, &probe_files, None, |_, _| true);
        assert!(targets.is_empty(), "{targets:?}");
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Warn);
        assert!(checks[0].detail.contains("missing"), "{}", checks[0].detail);
    }

    // --- config_path_check ---------------------------------------------------

    #[test]
    fn config_path_check_is_ok_not_present_for_a_missing_file() {
        let check = config_path_check("repo config", Path::new("/nonexistent/config.toml"));
        assert_eq!(check.status, Status::Ok);
        assert_eq!(check.detail, "not present (defaults)");
    }

    #[test]
    fn config_path_check_is_ok_parsed_clean_for_a_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[ui]\ntab_width = 8\n").unwrap();
        let check = config_path_check("repo config", &path);
        assert_eq!(check.status, Status::Ok);
        assert_eq!(check.detail, "parsed clean");
    }

    #[test]
    fn config_path_check_is_warn_with_the_message_for_a_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not_a_real_field = 1\n").unwrap();
        let check = config_path_check("repo config", &path);
        assert_eq!(check.status, Status::Warn);
        assert!(
            check.detail.contains("not_a_real_field"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn config_path_check_never_stores_a_raw_newline_for_a_deserialize_error() {
        // `toml` 0.9's own deserialize-error `Display` embeds real
        // newlines (e.g. `invalid type: string "four", expected usize\nin
        // \`ui.tab_width\`\n`) — this is the end-to-end guarantee `Check`'s
        // constructors exist to provide: whatever `toml` prints, the stored
        // detail is always one line.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[ui]\ntab_width = \"four\"\n").unwrap();
        let check = config_path_check("repo config", &path);
        assert_eq!(check.status, Status::Warn);
        assert!(!check.detail.contains('\n'), "{}", check.detail);
    }

    // --- vcs section pieces ---------------------------------------------------

    #[test]
    fn git_binary_check_is_ok_when_git_is_on_path() {
        // Every test in this suite already assumes `git` is on `PATH` (see
        // e.g. `vcs::git`'s own fixtures) — this just pins that this
        // specific check agrees.
        let check = git_binary_check();
        assert_eq!(check.status, Status::Ok);
        assert!(!check.detail.is_empty());
    }

    #[test]
    fn repo_root_check_is_error_outside_a_git_repository() {
        let dir = tempfile::tempdir().unwrap();
        let (check, root) = repo_root_check(dir.path());
        assert_eq!(check.status, Status::Error);
        assert!(root.is_none());
    }

    #[test]
    fn jj_check_is_none_for_a_plain_git_repo_with_no_jj() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(jj_check(dir.path()).is_none());
    }
}
