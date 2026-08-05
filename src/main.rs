mod diff;
mod highlight;
mod keymap;
mod lsp;
mod ui;
mod vcs;
mod watch;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use diff::{
    DiffFile, DiffLineKind, RenderRow, SideBySideRow, SideCell, flatten, flatten_side_by_side,
    parse_unified_diff,
};
use std::path::PathBuf;
use ui::app::Layout;
use ui::timeline_view::TimelineView;
use ui::{App, FileView, View, ViewStack};
use vcs::DiffSource;
use vcs::git::GitSource;
use vcs::jj;

#[derive(Parser)]
#[command(
    name = "ktmr",
    version,
    about = "Terminal diff review for AI-coding-agent workflows"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show a diff in a full-screen TUI. Defaults to the working tree
    /// against HEAD; `--staged` or a revision narrows the scope.
    Diff {
        /// Print parsed render rows as plain text and exit, instead of
        /// launching the TUI. Useful for scripting and for verifying
        /// parsing without a terminal.
        #[arg(long, hide = true)]
        dump: bool,
        /// With `--dump`, print the side-by-side pairing instead of the
        /// unified row sequence. Also sets the TUI's initial layout.
        #[arg(long, value_enum, hide = true, default_value_t = LayoutArg::Unified)]
        layout: LayoutArg,
        /// Show staged (index) changes against HEAD instead of the working
        /// tree. Cannot be combined with a revision.
        #[arg(long)]
        staged: bool,
        /// A single revision (that commit's own changes against its
        /// parent) or a `<rev>..<rev>` / `<rev>...<rev>` range, passed to
        /// git. Defaults to the working tree.
        range: Option<String>,
        /// Watch the repository for filesystem changes and refresh the
        /// diff automatically. Working-tree scope only — combining this
        /// with `--staged` or a revision range is rejected, since neither
        /// has a stable "current" version for a filesystem watcher to
        /// refresh against the way the working tree does.
        #[arg(long)]
        watch: bool,
    },
    /// Open a single file in a read-only, syntax-highlighted viewer.
    Open {
        /// Path to the file to open.
        file: PathBuf,
    },
    /// Opens directly into the jj snapshot timeline (see `t` from `ktmr
    /// diff`). Requires a colocated jj repository — a `.jj` directory
    /// alongside `.git`, with `jj` on PATH — and fails with a clear error
    /// otherwise, rather than the silent-fallback behavior `t` uses when
    /// it's just one option among several in a live session.
    Timeline {
        /// Print the snapshot list as plain text and exit, instead of
        /// launching the TUI — for headless verification.
        #[arg(long, hide = true)]
        dump: bool,
        /// With `--dump`, print this operation's diff against its
        /// immediate predecessor in the snapshot list, instead of the list
        /// itself. Accepts any prefix of a full operation id, as printed by
        /// a plain `--dump`.
        #[arg(long, hide = true, requires = "dump")]
        op: Option<String>,
        /// Triggers a working-copy snapshot via `JjRepo::snapshot` — the
        /// same call `JjPreRefreshHook` makes before every watch-mode
        /// refresh — and prints whether it created a new operation, then
        /// exits. For headless E2E verification of the snapshot trigger
        /// without a live `--watch` session.
        #[arg(long, hide = true, conflicts_with = "dump")]
        snapshot: bool,
    },
    /// Spawns a language server, requests hover (or, with a mode flag,
    /// go-to-definition/references/diagnostics) at one position, prints the
    /// result, and exits — an E2E smoke test for the `lsp` module runnable
    /// without a terminal. Not part of the reviewing workflow the rest of
    /// this CLI supports, so it's hidden from `--help`.
    #[command(hide = true)]
    LspCheck {
        /// Path to the file to check.
        file: PathBuf,
        /// 1-based line number, as an editor would show it.
        line: u32,
        /// 1-based display column within that line.
        col: usize,
        /// `textDocument/definition` instead of hover.
        #[arg(long)]
        definition: bool,
        /// `textDocument/references` instead of hover.
        #[arg(long)]
        references: bool,
        /// Opens the file and waits for a `textDocument/publishDiagnostics`
        /// notification instead of requesting anything at `line`/`col` —
        /// `line`/`col` are still required by the command's shape but
        /// unused in this mode.
        #[arg(long)]
        diagnostics: bool,
    },
    /// Runs the working-tree watcher (no TUI) against a repository and
    /// prints each flushed batch of changes as one line, then exits — an
    /// E2E smoke test for the `watch` module runnable without a terminal,
    /// the same role `lsp-check` plays for `lsp`. Not part of the reviewing
    /// workflow, so hidden from `--help`.
    #[command(hide = true)]
    WatchCheck {
        /// Repository (or any directory inside one) to watch. Defaults to
        /// the current directory.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Exit successfully after this many flushed batches.
        #[arg(long, default_value_t = 3)]
        flushes: usize,
        /// Exit with an error if that many flushes haven't arrived within
        /// this many seconds.
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
    },
}

/// `--layout`'s value space in clap-friendly form. `ui::app::Layout` is the
/// type the rest of the program actually uses; this exists only so clap has
/// something with kebab-case `ValueEnum` variants to parse `--layout=side`
/// into.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum LayoutArg {
    Unified,
    Side,
}

impl From<LayoutArg> for Layout {
    fn from(arg: LayoutArg) -> Self {
        match arg {
            LayoutArg::Unified => Layout::Unified,
            LayoutArg::Side => Layout::SideBySide,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Diff {
        dump: false,
        layout: LayoutArg::Unified,
        staged: false,
        range: None,
        watch: false,
    }) {
        Command::Diff {
            dump,
            layout,
            staged,
            range,
            watch,
        } => run_diff(dump, layout, staged, range, watch),
        Command::Open { file } => run_open(file),
        Command::Timeline { dump, op, snapshot } => run_timeline(dump, op, snapshot),
        Command::LspCheck {
            file,
            line,
            col,
            definition,
            references,
            diagnostics,
        } => run_lsp_check(file, line, col, definition, references, diagnostics),
        Command::WatchCheck {
            dir,
            flushes,
            timeout_secs,
        } => run_watch_check(dir, flushes, timeout_secs),
    }
}

fn run_diff(
    dump: bool,
    layout: LayoutArg,
    staged: bool,
    range: Option<String>,
    watch: bool,
) -> Result<()> {
    if watch && (staged || range.is_some()) {
        bail!(
            "--watch only supports the working tree; it cannot be combined with --staged or a revision range"
        );
    }

    let cwd = std::env::current_dir()?;
    let source = GitSource::discover(&cwd)?;

    let diff_text = match (staged, range.as_deref()) {
        (true, Some(_)) => bail!("--staged cannot be combined with a revision range"),
        (true, None) => source.staged_diff()?,
        (false, Some(range)) => source.range_diff(range)?,
        (false, None) => source.working_tree_diff()?,
    };
    let files = parse_unified_diff(&diff_text);

    if dump {
        let text = match layout {
            LayoutArg::Unified => format_dump(&files),
            LayoutArg::Side => format_dump_side_by_side(&files),
        };
        print!("{text}");
        return Ok(());
    }

    let repo_root = source.repo_root()?;
    let repo_name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo_root.display().to_string());

    let mut app = App::new(repo_name, repo_root, files);
    app.layout = layout.into();
    let mut stack = ViewStack::new(View::Diff(app));
    let pre_refresh_hook: Option<Box<dyn ui::PreRefreshHook>> =
        watch.then(|| Box::new(ui::NoopPreRefreshHook) as Box<dyn ui::PreRefreshHook>);
    ui::run(&mut stack, pre_refresh_hook)
}

fn run_open(file: PathBuf) -> Result<()> {
    let source = std::fs::read_to_string(&file)
        .with_context(|| format!("failed to read {}", file.display()))?;
    let display_path = file.display().to_string();

    let absolute_file = std::env::current_dir()
        .map(|cwd| cwd.join(&file))
        .unwrap_or_else(|_| file.clone());
    // Best-effort: a file opened outside a git repository (or when `git`
    // itself is unavailable) still opens read-only exactly as before —
    // hovering it just won't have anywhere to send a request, since
    // `LspManager` has no workspace root to bound its `Cargo.toml` search
    // by. Falling back to the file's own directory keeps that search from
    // wandering arbitrarily far up the real filesystem.
    let git_root = GitSource::discover(absolute_file.parent().unwrap_or(&absolute_file))
        .and_then(|source| source.repo_root())
        .unwrap_or_else(|_| {
            absolute_file
                .parent()
                .unwrap_or(&absolute_file)
                .to_path_buf()
        });

    let view = FileView::with_hover_target(display_path, &source, Some((absolute_file, git_root)));
    let mut stack = ViewStack::new(View::File(view));
    ui::run(&mut stack, None)
}

/// Resolves the jj repository for the current directory, the same way
/// [`run_diff`] resolves a `GitSource` — via `git`'s own idea of the
/// repository root, so `ktmr timeline` finds the same repo `ktmr diff`
/// would in the same directory. Fails with a message naming exactly what's
/// missing (no `jj` on PATH, or no colocated `.jj`), since unlike `t` inside
/// a live session — which falls back to a status-bar hint because there's a
/// diff already on screen to fall back *to* — this command has nothing else
/// to show.
fn discover_jj_repo() -> Result<jj::JjRepo> {
    let cwd = std::env::current_dir()?;
    let repo_root = GitSource::discover(&cwd)?.repo_root()?;
    let jj_bin = jj::resolve_jj_bin()
        .context("jj not found on PATH; `ktmr timeline` needs a `jj` binary to read")?;
    jj::JjRepo::detect(&repo_root, jj_bin).with_context(|| {
        format!(
            "no colocated jj repository at {} (expected a `.jj` directory alongside `.git`)",
            repo_root.display()
        )
    })
}

/// `ktmr timeline [--dump [--op <id>]]` — opens directly into
/// [`TimelineView`], or (hidden, for headless E2E verification) dumps the
/// snapshot list or one operation's diff as plain text. See
/// [`discover_jj_repo`] for what "requires a jj repo" means here.
fn run_timeline(dump: bool, op: Option<String>, snapshot: bool) -> Result<()> {
    let jj_repo = discover_jj_repo()?;

    if snapshot {
        let created = jj_repo.snapshot()?;
        println!(
            "{}",
            if created {
                "snapshot: created a new operation"
            } else {
                "snapshot: no changes since the last snapshot"
            }
        );
        return Ok(());
    }

    if dump {
        return run_timeline_dump(&jj_repo, op);
    }

    let timeline = TimelineView::new(jj_repo, ui::timeline_view::DEFAULT_OP_LOG_LIMIT)?;
    let mut stack = ViewStack::new(View::Timeline(timeline));
    ui::run(&mut stack, None)
}

/// Prints the snapshot list (`op_id  unix_time  description`, one per
/// line, newest first) or, with `op` set, that operation's diff against its
/// immediate predecessor in the list — the plain-text shape an E2E test
/// script can assert against without a terminal, mirroring what `ktmr diff
/// --dump` already does for the parser.
fn run_timeline_dump(jj_repo: &jj::JjRepo, op: Option<String>) -> Result<()> {
    let ops = jj_repo.snapshot_ops(ui::timeline_view::DEFAULT_OP_LOG_LIMIT)?;

    let Some(op_prefix) = op else {
        if ops.is_empty() {
            println!("(no snapshots yet)");
        }
        for entry in &ops {
            println!(
                "{}  {}  {}",
                entry.op_id, entry.time_unix, entry.description
            );
        }
        return Ok(());
    };

    let idx = ops
        .iter()
        .position(|o| o.op_id.starts_with(&op_prefix))
        .with_context(|| {
            format!(
                "no snapshot with id prefix {op_prefix:?} among the {} loaded",
                ops.len()
            )
        })?;
    let Some(previous) = ops.get(idx + 1) else {
        bail!(
            "{} is the oldest loaded snapshot; nothing earlier to diff against",
            ops[idx].op_id
        );
    };
    let diff = jj_repo.op_diff(&previous.op_id, &ops[idx].op_id)?;
    print!("{diff}");
    Ok(())
}

/// `ktmr lsp-check <file> <line> <col> [--definition|--references|--diagnostics]`
/// — an E2E smoke test for the whole `lsp` module without a terminal: spawns
/// a server through the same [`lsp::LspManager`] the TUI uses, waits for it
/// to become `Ready` (printing `$/progress` notifications and state
/// transitions along the way, since a cold `rust-analyzer` can take real
/// time to index), performs the requested check, prints the result, and
/// shuts the server down before exiting — see
/// [`lsp::LspManager::shutdown_all`]'s docs for why that last step isn't
/// optional. Defaults to hover when none of the mode flags are set.
fn run_lsp_check(
    file: PathBuf,
    line: u32,
    col: usize,
    definition: bool,
    references: bool,
    diagnostics: bool,
) -> Result<()> {
    if [definition, references, diagnostics]
        .iter()
        .filter(|set| **set)
        .count()
        > 1
    {
        bail!("--definition, --references, and --diagnostics are mutually exclusive");
    }

    let absolute_file = std::env::current_dir()?.join(&file);
    let content = std::fs::read_to_string(&absolute_file)
        .with_context(|| format!("failed to read {}", absolute_file.display()))?;
    let line_text = content
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .with_context(|| format!("{} has no line {line}", absolute_file.display()))?
        .to_owned();

    let git_root = GitSource::discover(absolute_file.parent().unwrap_or(&absolute_file))
        .and_then(|source| source.repo_root())
        .unwrap_or_else(|_| {
            absolute_file
                .parent()
                .unwrap_or(&absolute_file)
                .to_path_buf()
        });

    println!("file:     {}", absolute_file.display());
    println!("git root: {}", git_root.display());
    println!("target:   line {line} col {col}: {line_text}");
    println!();

    let (events_tx, events_rx) = std::sync::mpsc::channel();
    let manager = lsp::LspManager::new(events_tx);

    let line0 = line.saturating_sub(1);
    let col0 = col.saturating_sub(1);

    if diagnostics {
        run_diagnostics_check(&manager, &events_rx, &absolute_file, &git_root);
    } else if definition {
        run_definition_check(
            &manager,
            &events_rx,
            &absolute_file,
            &git_root,
            &line_text,
            line0,
            col0,
        );
    } else if references {
        run_references_check(
            &manager,
            &events_rx,
            &absolute_file,
            &git_root,
            &line_text,
            line0,
            col0,
        );
    } else {
        run_hover_check(
            &manager,
            &events_rx,
            &absolute_file,
            &git_root,
            &line_text,
            line0,
            col0,
        );
    }

    manager.shutdown_all();
    Ok(())
}

/// `ktmr watch-check --dir <dir> [--flushes N] [--timeout-secs S]` — an E2E
/// smoke test for the whole `watch` module without a terminal: starts the
/// same [`watch::spawn`] the TUI's `--watch` mode uses, prints each flushed
/// batch as one line (change kind and path, one entry per change, sorted
/// for deterministic output a test script can diff against), and exits
/// once `flushes` have arrived or `timeout_secs` have passed without them.
fn run_watch_check(dir: PathBuf, flushes: usize, timeout_secs: u64) -> Result<()> {
    let repo_root = dir.canonicalize().unwrap_or(dir);
    println!("watching: {}", repo_root.display());

    let (tx, rx) = std::sync::mpsc::channel();
    watch::spawn(repo_root, tx).map_err(|e| anyhow::anyhow!("failed to start watcher: {e}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut seen = 0usize;
    while seen < flushes {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            bail!(
                "watch-check: timed out after {timeout_secs}s waiting for {flushes} flush(es); saw {seen}"
            );
        }
        match rx.recv_timeout(remaining) {
            Ok(watch::WatchSignal::Pending) => println!("[pending]"),
            Ok(watch::WatchSignal::Flushed(batch)) => {
                seen += 1;
                let mut lines: Vec<String> = batch
                    .changes
                    .iter()
                    .map(|c| format!("{:?} {}", c.kind, c.path.display()))
                    .collect();
                lines.sort();
                println!("[flush {seen}] {}", lines.join(", "));
            }
            // A plain timeout just means no signal arrived within
            // `remaining` — the top of the loop re-checks the real
            // deadline and bails with a clear message once it's actually
            // passed, so this iterates rather than misreporting an
            // ordinary wait as the watcher having died.
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("watch-check: watcher thread ended unexpectedly")
            }
        }
    }
    Ok(())
}

/// The very first request against a freshly resolved key is also what makes
/// `LspManager` spawn the server at all (it's lazy — nothing starts until
/// something asks for it). That request reaches the server the instant the
/// `initialize` handshake finishes, which is well before rust-analyzer's
/// background workspace indexing catches up, so an immediate empty answer
/// isn't necessarily "nothing there" — it can just as easily mean "asked
/// too early." Rather than trust the first answer, every `run_*_check`
/// function below re-issues its request every `RETRY_INTERVAL` (printing
/// state/progress in between, via `events_rx`) until either `is_final`
/// judges an answer real or the overall deadline passes — the one piece of
/// polling/retry machinery every mode built on a request/response (i.e.
/// everything but `--diagnostics`, which waits on a notification instead)
/// shares.
const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);
const CHECK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

fn poll_request<T>(
    manager: &lsp::LspManager,
    events_rx: &std::sync::mpsc::Receiver<lsp::ServerEvent>,
    file: &std::path::Path,
    git_root: &std::path::Path,
    mut issue: impl FnMut() -> std::sync::mpsc::Receiver<Result<T, lsp::LspError>>,
    is_final: impl Fn(&T) -> bool,
) -> Result<T, lsp::LspError> {
    let deadline = std::time::Instant::now() + CHECK_DEADLINE;
    let mut pending = Some(issue());
    let mut last_attempt = std::time::Instant::now();
    let mut last_state = None;
    let mut best = None;

    loop {
        if let Some(rx) = &pending
            && let Ok(result) = rx.try_recv()
        {
            let final_now = matches!(&result, Ok(v) if is_final(v));
            best = Some(result);
            pending = None;
            if final_now {
                return best.expect("just assigned");
            }
        }

        if pending.is_none() && last_attempt.elapsed() >= RETRY_INTERVAL {
            println!("[retry] requesting again");
            pending = Some(issue());
            last_attempt = std::time::Instant::now();
        }

        if let Ok(event) = events_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            print_progress_or_close(&event);
        }

        let state = manager.state(file, git_root);
        if last_state.as_ref() != Some(&state) {
            println!("[state] {state:?}");
            last_state = Some(state);
        }

        if std::time::Instant::now() > deadline {
            return best.unwrap_or_else(|| {
                Err(lsp::LspError::Io(
                    "timed out waiting for a response after 60s".to_owned(),
                ))
            });
        }
    }
}

fn print_progress_or_close(event: &lsp::ServerEvent) {
    match &event.event {
        lsp::LspEvent::Notification { method, params } if method == "$/progress" => {
            if let Some(text) = lsp::progress_status_text(params) {
                println!("[progress] {text}");
            }
        }
        lsp::LspEvent::Closed { reason } => {
            println!("[lsp] transport closed: {reason:?}");
        }
        _ => {}
    }
}

fn run_hover_check(
    manager: &lsp::LspManager,
    events_rx: &std::sync::mpsc::Receiver<lsp::ServerEvent>,
    file: &std::path::Path,
    git_root: &std::path::Path,
    line_text: &str,
    line0: u32,
    col0: usize,
) {
    let result = poll_request(
        manager,
        events_rx,
        file,
        git_root,
        || manager.hover(file, git_root, line_text, line0, col0),
        Option::is_some,
    );
    println!();
    match result {
        Ok(Some(hover)) => {
            println!("--- hover ---");
            println!("{}", ui::hover_popup::plain_text(&hover.contents));
        }
        Ok(None) => println!("--- hover ---\n(no hover information at this position)"),
        Err(e) => println!("--- hover error ---\n{e}"),
    }
}

fn run_definition_check(
    manager: &lsp::LspManager,
    events_rx: &std::sync::mpsc::Receiver<lsp::ServerEvent>,
    file: &std::path::Path,
    git_root: &std::path::Path,
    line_text: &str,
    line0: u32,
    col0: usize,
) {
    let result = poll_request(
        manager,
        events_rx,
        file,
        git_root,
        || manager.definition(file, git_root, line_text, line0, col0),
        definition_is_final,
    );
    println!();
    match result {
        Ok(Some(response)) => {
            let locations = ui::navigation::definition_locations(response);
            println!("--- definition ({} location(s)) ---", locations.len());
            for location in &locations {
                print_location(location);
            }
        }
        Ok(None) => println!("--- definition ---\n(no definition found at this position)"),
        Err(e) => println!("--- definition error ---\n{e}"),
    }
}

/// A plain `Option::is_some` check (good enough for hover, where any
/// `Some(Hover)` is a meaningful answer) isn't quite right for
/// go-to-definition: a still-indexing rust-analyzer can answer an early
/// request with `Some(GotoDefinitionResponse::Array(vec![]))` — a
/// definite-looking "no results" that's actually just "not ready yet,"
/// indistinguishable from a real empty answer by shape alone. Treating only
/// a non-empty `Array` (or the `Scalar`/`Link` shapes, which can't be
/// "empty" the same way) as final, and retrying through `None`/empty-`Array`
/// until [`poll_request`]'s deadline, matches what hover already does for
/// its own "answered too early" case — see `run_lsp_check`'s doc comment.
fn definition_is_final(result: &lsp::DefinitionResult) -> bool {
    match result {
        Some(lsp_types::GotoDefinitionResponse::Array(locations)) => !locations.is_empty(),
        Some(_) => true,
        None => false,
    }
}

fn run_references_check(
    manager: &lsp::LspManager,
    events_rx: &std::sync::mpsc::Receiver<lsp::ServerEvent>,
    file: &std::path::Path,
    git_root: &std::path::Path,
    line_text: &str,
    line0: u32,
    col0: usize,
) {
    let result = poll_request(
        manager,
        events_rx,
        file,
        git_root,
        || manager.references(file, git_root, line_text, line0, col0),
        |result: &lsp::ReferencesResult| result.as_ref().is_some_and(|locs| !locs.is_empty()),
    );
    println!();
    match result {
        Ok(Some(locations)) => {
            println!("--- references ({} location(s)) ---", locations.len());
            for location in &locations {
                print_location(location);
            }
        }
        Ok(None) => println!("--- references ---\n(no references found at this position)"),
        Err(e) => println!("--- references error ---\n{e}"),
    }
}

fn print_location(location: &lsp_types::Location) {
    let path = lsp::client::uri_to_path(&location.uri)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| location.uri.as_str().to_owned());
    println!(
        "  {path}:{}:{} - {}:{}",
        location.range.start.line + 1,
        location.range.start.character + 1,
        location.range.end.line + 1,
        location.range.end.character + 1,
    );
}

/// `--diagnostics` doesn't request anything at `line`/`col` — it opens the
/// file (via [`lsp::LspManager::warm_up`], the same mechanism the TUI uses
/// to make diagnostics appear without a hover) and waits for the server to
/// push a `textDocument/publishDiagnostics` notification for it, which is
/// how `katamari`'s core value proposition ("did the AI's edit introduce
/// errors") actually surfaces — there is no request/response form of this
/// in LSP.
fn run_diagnostics_check(
    manager: &lsp::LspManager,
    events_rx: &std::sync::mpsc::Receiver<lsp::ServerEvent>,
    file: &std::path::Path,
    git_root: &std::path::Path,
) {
    let target_uri = match lsp::client::file_uri(file) {
        Ok(uri) => uri,
        Err(e) => {
            println!("--- diagnostics error ---\n{e}");
            return;
        }
    };

    let summary = manager.warm_up(std::slice::from_ref(&file.to_path_buf()), git_root);
    println!(
        "[warm-up] opened {} of {} eligible file(s)",
        summary.opened, summary.total_eligible
    );

    // rust-analyzer (and most servers) publish diagnostics in (at least)
    // two waves: a fast one from its own semantic analysis, often empty for
    // an error only `rustc`/`cargo check` itself would catch, followed by a
    // slower one once its background `cargo check` ("flycheck") finishes
    // compiling the crate. Breaking on the *first* notification for this
    // URI would report a false "zero diagnostics" for exactly the errors
    // this mode most wants to catch — so this keeps listening past an empty
    // notification, updating `received` each time, and only stops early on
    // a non-empty one. A longer deadline than the request-based checks'
    // `CHECK_DEADLINE`, since a cold flycheck run compiles the whole crate
    // graph rather than answering from an already-parsed AST.
    const DIAGNOSTICS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);
    let deadline = std::time::Instant::now() + DIAGNOSTICS_DEADLINE;
    let mut last_state = None;
    let mut received: Option<Vec<lsp_types::Diagnostic>> = None;

    while std::time::Instant::now() < deadline {
        if let Ok(event) = events_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            match &event.event {
                lsp::LspEvent::Notification { method, params } if method == "$/progress" => {
                    if let Some(text) = lsp::progress_status_text(params) {
                        println!("[progress] {text}");
                    }
                }
                lsp::LspEvent::Notification { method, params }
                    if method == "textDocument/publishDiagnostics" =>
                {
                    if let Some(parsed) = lsp::parse_publish_diagnostics(params) {
                        if parsed.uri.as_str() == target_uri.as_str() {
                            let is_final = !parsed.diagnostics.is_empty();
                            received = Some(parsed.diagnostics);
                            if is_final {
                                break;
                            }
                            println!("[diagnostics] (empty wave received; still waiting)");
                        } else {
                            println!(
                                "[diagnostics] (ignoring notification for a different file) {}",
                                parsed.uri.as_str()
                            );
                        }
                    }
                }
                lsp::LspEvent::Closed { reason } => {
                    println!("[lsp] transport closed: {reason:?}");
                }
                _ => {}
            }
        }

        let state = manager.state(file, git_root);
        if last_state.as_ref() != Some(&state) {
            println!("[state] {state:?}");
            last_state = Some(state);
        }
    }

    println!();
    match received {
        Some(diagnostics) if diagnostics.is_empty() => {
            println!("--- diagnostics ---\n(server published zero diagnostics for this file)");
        }
        Some(diagnostics) => {
            println!("--- diagnostics ({}) ---", diagnostics.len());
            for d in &diagnostics {
                let severity = d
                    .severity
                    .map(|s| format!("{s:?}"))
                    .unwrap_or_else(|| "?".to_owned());
                println!(
                    "  [{severity}] {}:{}-{}:{}  {}{}",
                    d.range.start.line + 1,
                    d.range.start.character + 1,
                    d.range.end.line + 1,
                    d.range.end.character + 1,
                    d.message,
                    d.source
                        .as_deref()
                        .map(|s| format!("  ({s})"))
                        .unwrap_or_default(),
                );
            }
        }
        None => println!(
            "--- diagnostics ---\n(no publishDiagnostics notification arrived within {}s)",
            DIAGNOSTICS_DEADLINE.as_secs()
        ),
    }
}

/// Plain-text rendering of one flattened row, shared by [`format_dump`] and
/// [`format_dump_side_by_side`] for the rows that render identically in
/// both layouts (file/hunk headers, binary notices): only hunk bodies
/// differ between unified and side-by-side, since side-by-side pairs them
/// up instead of listing them one after another.
fn format_render_row(files: &[DiffFile], row: RenderRow) -> String {
    match row {
        RenderRow::FileHeader { file_idx } => {
            let file = &files[file_idx];
            let (added, deleted) = file.stat();
            let status = if file.is_new {
                "new"
            } else if file.is_deleted {
                "deleted"
            } else if file.is_renamed {
                "renamed"
            } else {
                "modified"
            };
            let mut out = format!(
                "FILE {} [{status}] +{added} -{deleted}\n",
                file.display_path()
            );
            if file.is_renamed {
                out.push_str(&format!(
                    "  renamed from {}\n",
                    file.old_path.as_deref().unwrap_or("?")
                ));
            }
            out
        }
        RenderRow::BinaryNotice { .. } => "  BINARY (contents not shown)\n".to_owned(),
        RenderRow::HunkHeader { file_idx, hunk_idx } => {
            let hunk = &files[file_idx].hunks[hunk_idx];
            format!(
                "  HUNK @@ -{},{} +{},{} @@ {}\n",
                hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines, hunk.header
            )
        }
        RenderRow::Line {
            file_idx,
            hunk_idx,
            row_idx,
        } => format_line_row(&files[file_idx].hunks[hunk_idx].rows[row_idx]),
    }
}

fn format_line_row(row: &diff::DiffRow) -> String {
    let kind = match row.kind {
        DiffLineKind::Context => "ctx",
        DiffLineKind::Add => "add",
        DiffLineKind::Del => "del",
    };
    let old = row
        .old_line
        .map_or_else(|| "-".to_owned(), |n| n.to_string());
    let new = row
        .new_line
        .map_or_else(|| "-".to_owned(), |n| n.to_string());
    format!("    LINE {kind:<3} {old:>5} {new:>5}  {}\n", row.text)
}

fn total_stat(files: &[DiffFile]) -> (u32, u32) {
    files.iter().fold((0u32, 0u32), |(a, d), f| {
        let (fa, fd) = f.stat();
        (a + fa, d + fd)
    })
}

/// Plain-text rendering of the parsed, flattened unified diff: one line per
/// file header, hunk header, and content row, with the same line numbers
/// and kinds the TUI would show. Exists so parsing correctness can be
/// checked against `git diff` output without a terminal.
fn format_dump(files: &[DiffFile]) -> String {
    let (total_added, total_deleted) = total_stat(files);
    let mut out = format!(
        "files: {}  +{total_added} -{total_deleted}\n\n",
        files.len()
    );
    for row in flatten(files) {
        out.push_str(&format_render_row(files, row));
    }
    out
}

/// Plain-text rendering of the side-by-side pairing: file/hunk headers as in
/// [`format_dump`], but each hunk body row becomes an `OLD`/`NEW` pair (`--`
/// for a filler cell with no counterpart on that side) instead of a flat
/// list — the same grouping `diff_view::render` draws as two columns.
fn format_dump_side_by_side(files: &[DiffFile]) -> String {
    let (total_added, total_deleted) = total_stat(files);
    let mut out = format!(
        "files: {}  +{total_added} -{total_deleted}  layout=side\n\n",
        files.len()
    );
    let rows = flatten(files);
    for paired in flatten_side_by_side(files) {
        match paired {
            SideBySideRow::Full { flat_idx } => {
                out.push_str(&format_render_row(files, rows[flat_idx]))
            }
            SideBySideRow::Paired { old, new } => {
                out.push_str("    PAIR\n");
                out.push_str(&format_side_cell("OLD", files, &rows, old));
                out.push_str(&format_side_cell("NEW", files, &rows, new));
            }
        }
    }
    out
}

fn format_side_cell(label: &str, files: &[DiffFile], rows: &[RenderRow], cell: SideCell) -> String {
    let SideCell::Line { flat_idx } = cell else {
        return format!("      {label}  --\n");
    };
    let RenderRow::Line {
        file_idx,
        hunk_idx,
        row_idx,
    } = rows[flat_idx]
    else {
        unreachable!("flatten_side_by_side only ever addresses RenderRow::Line entries");
    };
    let row = &files[file_idx].hunks[hunk_idx].rows[row_idx];
    let kind = match row.kind {
        DiffLineKind::Context => "ctx",
        DiffLineKind::Add => "add",
        DiffLineKind::Del => "del",
    };
    let num = if label == "OLD" {
        row.old_line
    } else {
        row.new_line
    };
    let num = num.map_or_else(|| "-".to_owned(), |n| n.to_string());
    format!("      {label}  {kind:<3} {num:>5}  {}\n", row.text)
}
