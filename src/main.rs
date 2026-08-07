#[cfg(test)]
mod cjk_regression;
mod comments;
mod config;
mod diff;
mod highlight;
mod keymap;
mod lsp;
mod ui;
mod update;
mod vcs;
mod watch;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use comments::{Comment, CommentAnnotation, CommentIndex, CommentStore, Status as CommentStatus};
use diff::{
    DiffFile, DiffLineKind, RenderRow, SideBySideRow, SideCell, flatten, flatten_side_by_side,
    parse_unified_diff,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
        #[arg(conflicts_with_all = ["revision", "from", "to"])]
        range: Option<String>,
        /// Diff a single jj change against its parent(s) — `jj diff -r
        /// <revset>`'s equivalent. Requires a colocated jj repository (see
        /// `ktmr timeline`'s docs on what "colocated" means here).
        #[arg(short = 'r', long, value_name = "REVSET", conflicts_with_all = ["from", "to", "range", "staged", "watch"])]
        revision: Option<String>,
        /// Diff between two jj revisions — `jj diff --from/--to`'s
        /// equivalent. Either side left unset defaults to `@` (the working
        /// copy), matching jj's own `jj diff --help` documented behavior.
        /// Requires a colocated jj repository.
        #[arg(long, value_name = "REVSET", conflicts_with_all = ["revision", "range", "staged", "watch"])]
        from: Option<String>,
        /// See `--from`.
        #[arg(long, value_name = "REVSET", conflicts_with_all = ["revision", "range", "staged", "watch"])]
        to: Option<String>,
        /// Watch the repository for filesystem changes and refresh the
        /// diff automatically. Working-tree scope only — combining this
        /// with `--staged`, a revision range, or `-r`/`--from`/`--to` is
        /// rejected, since none of those has a stable "current" version for
        /// a filesystem watcher to refresh against the way the working tree
        /// does.
        #[arg(long, conflicts_with_all = ["revision", "from", "to"])]
        watch: bool,
        /// Renders this many frames of the diff pane offscreen (a
        /// `ratatui::backend::TestBackend`, no real terminal involved) and
        /// prints timing, then exits — a headless way to measure
        /// `ui::diff_view`'s rendering cost, most usefully against a very
        /// large synthetic diff, without a human watching a real session.
        /// Reports the first frame's time (a cold highlight cache) and the
        /// steady-state average over every frame after it (memoized —see
        /// `highlight::LineHighlighter`'s cache) separately, since those two
        /// numbers are the whole point of measuring this at all.
        #[arg(long, hide = true, value_name = "N")]
        bench_render: Option<usize>,
        /// Forces the screenkey-style key-display overlay on for this
        /// session, regardless of `[ui] show_keys` in config — see
        /// `ui::key_display`'s module docs. The flag only ever turns this
        /// *on*; there's no `--no-show-keys` to force it off, since config
        /// already defaults to off.
        #[arg(long)]
        show_keys: bool,
    },
    /// Open a single file in a read-only, syntax-highlighted viewer.
    Open {
        /// Path to the file to open.
        file: PathBuf,
        /// See `Diff`'s `--show-keys`.
        #[arg(long)]
        show_keys: bool,
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
        /// See `Diff`'s `--show-keys`.
        #[arg(long)]
        show_keys: bool,
    },
    /// Opens directly into a browsable revision history (see `L` from
    /// `ktmr diff`): jj changes — including the working copy, `@`, as a
    /// real entry — in a colocated jj repository, or `git log` commits plus
    /// a synthetic "local changes" row for the dirty working tree
    /// otherwise. Unlike `ktmr timeline`, this never requires jj: every
    /// repository `ktmr` runs in has git history to show even without it.
    Log {
        /// Print the parsed history list as plain text and exit, instead of
        /// launching the TUI — for headless verification.
        #[arg(long, hide = true)]
        dump: bool,
        /// See `Diff`'s `--show-keys`.
        #[arg(long)]
        show_keys: bool,
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
    /// Read and update review comments left via `ktmr diff`'s compose
    /// overlay (`c`) — the CLI half of M6's comment round trip, for an AI
    /// coding agent (see `skills/katamari-review/SKILL.md`) or a script to
    /// drive without a terminal. Every subcommand operates on
    /// `<repo_root>/.katamari/comments.jsonl` for the repository containing
    /// the current directory.
    Comments {
        #[command(subcommand)]
        action: CommentsCommand,
    },
    /// Installs the bundled `katamari-review` Claude Code skill into the
    /// current repository, so an agent working in it picks up the
    /// `ktmr comments` workflow automatically.
    Skill {
        #[command(subcommand)]
        action: SkillCommand,
    },
    /// Diagnoses and manages the language-server installs `[lsp]
    /// auto_install` (on by default) would otherwise trigger silently the
    /// first time a server is needed — see [`lsp::install`]'s module docs
    /// for the managed-prefix layout and pinned versions.
    Lsp {
        #[command(subcommand)]
        action: LspCommand,
    },
}

#[derive(Subcommand)]
enum LspCommand {
    /// Prints, for each of the five supported languages, where its server
    /// would resolve from today (config override / project-local / PATH /
    /// `mise which` / katamari-managed / not found) and — only when not
    /// found — whether `[lsp] auto_install` would handle it. Read-only:
    /// never downloads or installs anything, and runs fully offline.
    Doctor,
    /// Installs `language`'s pinned server into katamari's managed prefix,
    /// streaming progress lines to stdout. Deliberately ignores whatever
    /// `PATH`/project-local/`rustup which` would otherwise resolve to —
    /// that's the point of running this explicitly rather than waiting for
    /// auto-install; `ktmr lsp doctor` shows the *effective* resolution,
    /// which still prefers those tiers over the managed install. `all` runs
    /// every language, printing why (not failing the whole command) for any
    /// whose toolchain prerequisite is missing (gopls without a go
    /// toolchain, currently the only such case).
    Install { language: LanguageArg },
    /// Re-installs every pinned server whose managed install isn't already
    /// at the current pin — for after a katamari upgrade bumps one of the
    /// version constants in [`lsp::install`]. Reports, per language,
    /// whether it was already current or got (re)installed.
    Update,
}

/// `ktmr lsp install <language>`'s argument — the five languages plus
/// `all`, kebab-free since every variant here is already a single word.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum LanguageArg {
    Rust,
    Typescript,
    Python,
    Go,
    Kotlin,
    All,
}

#[derive(Subcommand)]
enum CommentsCommand {
    /// Lists comments: a human-readable table by default, or one JSON
    /// object per line with `--json` — for a script or agent to parse.
    List {
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum, default_value_t = StatusFilter::Open)]
        status: StatusFilter,
    },
    /// Prints a paste-ready report of comments: markdown grouped by file
    /// (default), or a JSON array with `--format=json`. The markdown form
    /// opens with an instruction line meant to be handed straight to an
    /// AI coding agent as a prompt.
    Export {
        #[arg(long, value_enum, default_value_t = ExportFormat::Md)]
        format: ExportFormat,
        #[arg(long, value_enum, default_value_t = StatusFilter::Open)]
        status: StatusFilter,
    },
    /// Adds a comment programmatically — the scripted/agent equivalent of
    /// the TUI's compose overlay.
    Add {
        /// Repo-relative path, as it appears in `ktmr diff`.
        file: String,
        /// 1-based line number in the file's current working-tree content.
        line: u32,
        body: String,
    },
    /// Marks a comment resolved.
    Resolve { id: String },
    /// Marks a resolved comment open again.
    Reopen { id: String },
}

/// `ktmr comments list/export`'s `--status` filter. `Open` is the default
/// for both — an agent (or a reviewer) asking "what's outstanding" wants
/// open comments, not the full history, unless it says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum StatusFilter {
    Open,
    Resolved,
    All,
}

impl StatusFilter {
    fn matches(self, status: CommentStatus) -> bool {
        match self {
            StatusFilter::Open => status == CommentStatus::Open,
            StatusFilter::Resolved => status == CommentStatus::Resolved,
            StatusFilter::All => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ExportFormat {
    Md,
    Json,
}

#[derive(Subcommand)]
enum SkillCommand {
    /// Copies the bundled `SKILL.md` to
    /// `<repo_root>/.claude/skills/katamari-review/SKILL.md`.
    Install,
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
        revision: None,
        from: None,
        to: None,
        watch: false,
        bench_render: None,
        show_keys: false,
    }) {
        Command::Diff {
            dump,
            layout,
            staged,
            range,
            revision,
            from,
            to,
            watch,
            bench_render,
            show_keys,
        } => run_diff(
            dump,
            layout,
            staged,
            range,
            revision,
            from,
            to,
            watch,
            bench_render,
            show_keys,
        ),
        Command::Open { file, show_keys } => run_open(file, show_keys),
        Command::Timeline {
            dump,
            op,
            snapshot,
            show_keys,
        } => run_timeline(dump, op, snapshot, show_keys),
        Command::Log { dump, show_keys } => run_log(dump, show_keys),
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
        Command::Comments { action } => run_comments(action),
        Command::Skill { action } => run_skill(action),
        Command::Lsp { action } => run_lsp(action),
    }
}

#[allow(clippy::too_many_arguments)] // one flag per `Command::Diff` field;
// see `ui::mod::handle_action`'s identical justification for its own
// too-many-arguments allow — bundling into a struct wouldn't reduce how
// many independent scope choices this function has to resolve.
fn run_diff(
    dump: bool,
    layout: LayoutArg,
    staged: bool,
    range: Option<String>,
    revision: Option<String>,
    from: Option<String>,
    to: Option<String>,
    watch: bool,
    bench_render: Option<usize>,
    show_keys: bool,
) -> Result<()> {
    if watch && (staged || range.is_some()) {
        bail!(
            "--watch only supports the working tree; it cannot be combined with --staged or a revision range"
        );
    }

    let cwd = std::env::current_dir()?;
    let source = GitSource::discover(&cwd)?;
    let repo_root = source.repo_root()?;
    let config = config::load_merged(&repo_root);
    config::install(&config);

    // `revision`/`from`/`to` are jj-only and mutually exclusive with
    // `staged`/`range`/`watch` (see `Command::Diff`'s `conflicts_with_all`
    // attributes) — resolving them first means the `(staged, range)` match
    // below only ever has to handle the git-only scopes it always has.
    // `interactive = false` for every jj revision diff, matching
    // `crate::ui::timeline_view::TimelineView`'s "historical content isn't
    // trustworthy enough to ask a language server about" rule — see
    // `App::interactive`'s docs.
    let (diff_text, scope_label, interactive) = if let Some(revset) = revision.as_deref() {
        let jj_repo = discover_jj_repo_at(&repo_root)?;
        (
            jj_repo.revision_diff(revset)?,
            Some(format!("r: {revset}")),
            false,
        )
    } else if from.is_some() || to.is_some() {
        let jj_repo = discover_jj_repo_at(&repo_root)?;
        let text = jj_repo.revision_range_diff(from.as_deref(), to.as_deref())?;
        let label = format!(
            "{}..{}",
            from.as_deref().unwrap_or("@"),
            to.as_deref().unwrap_or("@")
        );
        (text, Some(label), false)
    } else {
        let text = match (staged, range.as_deref()) {
            (true, Some(_)) => bail!("--staged cannot be combined with a revision range"),
            (true, None) => source.staged_diff()?,
            (false, Some(range)) => source.range_diff(range)?,
            (false, None) => source.working_tree_diff()?,
        };
        (text, None, true)
    };
    let files = parse_unified_diff(&diff_text);

    if dump {
        // Comments always anchor to the working tree, regardless of which
        // scope `--dump` is showing — relocating against the repo's
        // current on-disk content (not whatever `diff_text` above happens
        // to cover) is what `CommentIndex` always does anyway (see
        // `comments::build_index`), so this is correct even for
        // `--staged`/a revision range, not just the plain working-tree
        // diff.
        let loaded = CommentStore::new(&repo_root).load().unwrap_or_default();
        let comments = comments::build_index(&repo_root, &loaded);
        let text = match layout {
            LayoutArg::Unified => format_dump(&files, &comments),
            LayoutArg::Side => format_dump_side_by_side(&files, &comments),
        };
        print!("{text}");
        return Ok(());
    }

    let repo_name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo_root.display().to_string());

    let mut app = App::new(repo_name, repo_root, files);
    app.layout = layout.into();
    app.interactive = interactive;
    app.scope_label = scope_label;

    if let Some(frames) = bench_render {
        return run_bench_render(app, frames);
    }

    let mut stack = ViewStack::new(View::Diff(app));
    let pre_refresh_hook: Option<Box<dyn ui::PreRefreshHook>> =
        watch.then(|| Box::new(ui::NoopPreRefreshHook) as Box<dyn ui::PreRefreshHook>);
    ui::run(
        &mut stack,
        pre_refresh_hook,
        &config,
        show_keys || config.show_keys,
    )
}

/// `ktmr diff --bench-render N` — see that flag's doc comment on `Command::Diff`.
/// Renders `app`'s diff pane `frames` times into an offscreen
/// `ratatui::backend::TestBackend` (120x40, a representative terminal size —
/// no real terminal or event loop involved) and prints the first frame's
/// time separately from the steady-state average over the rest, so a
/// synthetic large-diff benchmark shows `highlight::LineHighlighter`'s
/// per-line memoization actually paying off frame-over-frame, not just a
/// single aggregate number that would hide it.
fn run_bench_render(app: App, frames: usize) -> Result<()> {
    use crate::comments::CommentIndex;
    use crate::highlight::LineHighlighter;
    use crate::lsp::DiagnosticsStore;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

    if frames == 0 {
        bail!("--bench-render needs at least 1 frame");
    }

    const WIDTH: u16 = 120;
    const HEIGHT: u16 = 40;
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend)?;
    let mut highlighter = LineHighlighter::new();
    let diagnostics = DiagnosticsStore::new();
    let comments = CommentIndex::default();
    let layout = ui::diff_view::effective_layout(app.layout, WIDTH);

    let mut first_frame = Duration::ZERO;
    let mut rest_total = Duration::ZERO;
    for i in 0..frames {
        let started = std::time::Instant::now();
        terminal.draw(|frame| {
            ui::diff_view::render(
                frame,
                frame.area(),
                &app,
                &mut highlighter,
                layout,
                &diagnostics,
                &comments,
            );
        })?;
        let elapsed = started.elapsed();
        if i == 0 {
            first_frame = elapsed;
        } else {
            rest_total += elapsed;
        }
    }

    println!(
        "bench-render: {frames} frame(s), {WIDTH}x{HEIGHT} pane, {} diff rows",
        app.rows.len()
    );
    println!(
        "  first frame  (cold highlight cache): {:.3} ms",
        first_frame.as_secs_f64() * 1000.0
    );
    if frames > 1 {
        let steady_avg_ms = rest_total.as_secs_f64() * 1000.0 / (frames - 1) as f64;
        println!(
            "  steady state (memoized, avg of {} frames): {steady_avg_ms:.3} ms/frame",
            frames - 1
        );
    }
    Ok(())
}

fn run_open(file: PathBuf, show_keys: bool) -> Result<()> {
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

    let config = config::load_merged(&git_root);
    config::install(&config);

    let view = FileView::with_hover_target(display_path, &source, Some((absolute_file, git_root)));
    let mut stack = ViewStack::new(View::File(view));
    ui::run(&mut stack, None, &config, show_keys || config.show_keys)
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
    discover_jj_repo_at(&repo_root)
}

/// The repo-root-already-known half of [`discover_jj_repo`] — shared with
/// [`run_diff`]'s `-r`/`--from`/`--to` handling, which has already resolved
/// `repo_root` via its own `GitSource::discover` and would otherwise pay
/// for that `git rev-parse` a second time just to reach this check.
fn discover_jj_repo_at(repo_root: &Path) -> Result<jj::JjRepo> {
    let jj_bin =
        jj::resolve_jj_bin().context("jj not found on PATH; this needs a `jj` binary to read")?;
    jj::JjRepo::detect(repo_root, jj_bin).with_context(|| {
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
fn run_timeline(dump: bool, op: Option<String>, snapshot: bool, show_keys: bool) -> Result<()> {
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

    let config = config::load_merged(jj_repo.repo_root());
    config::install(&config);

    let timeline = TimelineView::new(jj_repo, ui::timeline_view::DEFAULT_OP_LOG_LIMIT)?;
    let mut stack = ViewStack::new(View::Timeline(timeline));
    ui::run(&mut stack, None, &config, show_keys || config.show_keys)
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

/// `ktmr log [--dump]` — opens directly into [`ui::log_view::LogView`], or
/// (hidden, for headless E2E verification) dumps the parsed history list as
/// plain text via [`ui::log_view::format_dump`]. Unlike [`run_timeline`],
/// never requires jj: [`vcs::LogBackend::detect`] falls back to plain git
/// history when no colocated jj repository is found, so this only ever
/// fails the way [`run_diff`]'s plain working-tree case can (not a git
/// repository at all, or `git` missing from `PATH`).
fn run_log(dump: bool, show_keys: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo_root = GitSource::discover(&cwd)?.repo_root()?;
    let backend = vcs::LogBackend::detect(&repo_root);

    if dump {
        let entries = backend.log(ui::log_view::DEFAULT_LOG_LIMIT)?;
        print!("{}", ui::log_view::format_dump(&entries));
        return Ok(());
    }

    let config = config::load_merged(&repo_root);
    config::install(&config);

    let log_view = ui::log_view::LogView::new(backend, ui::log_view::DEFAULT_LOG_LIMIT)?;
    let mut stack = ViewStack::new(View::Log(log_view));
    ui::run(&mut stack, None, &config, show_keys || config.show_keys)
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
    // `lsp-check` is a headless smoke test for the `lsp` module itself
    // (see this command's doc comment) — it has no config file to load, so
    // it always resolves servers the built-in way, with no overrides, and
    // auto-install on (matching the production default, so this command
    // also exercises `resolve_or_install` when a server is missing).
    let manager = lsp::LspManager::new(events_tx, std::sync::Arc::new(HashMap::new()), true);

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
    watch::spawn(repo_root, tx, watch::DEBOUNCE_QUIET)
        .map_err(|e| anyhow::anyhow!("failed to start watcher: {e}"))?;

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

/// The repository root `ktmr comments`/`ktmr skill install` operate
/// against: the same `git`-rooted discovery [`run_diff`] uses, from the
/// current directory — every comments subcommand is meant to be run from
/// anywhere inside the repo being reviewed, the same way `git` commands
/// are.
fn comments_repo_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    GitSource::discover(&cwd)?.repo_root()
}

fn comment_status_label(status: CommentStatus) -> &'static str {
    match status {
        CommentStatus::Open => "open",
        CommentStatus::Resolved => "resolved",
    }
}

fn run_comments(action: CommentsCommand) -> Result<()> {
    match action {
        CommentsCommand::List { json, status } => run_comments_list(json, status),
        CommentsCommand::Export { format, status } => run_comments_export(format, status),
        CommentsCommand::Add { file, line, body } => run_comments_add(file, line, body),
        CommentsCommand::Resolve { id } => run_comments_set_status(id, CommentStatus::Resolved),
        CommentsCommand::Reopen { id } => run_comments_set_status(id, CommentStatus::Open),
    }
}

fn run_comments_list(json: bool, status: StatusFilter) -> Result<()> {
    let repo_root = comments_repo_root()?;
    let filtered: Vec<Comment> = CommentStore::new(&repo_root)
        .load()?
        .into_iter()
        .filter(|c| status.matches(c.status))
        .collect();

    if json {
        for comment in &filtered {
            println!("{}", serde_json::to_string(comment)?);
        }
        return Ok(());
    }

    if filtered.is_empty() {
        println!("(no comments)");
        return Ok(());
    }
    println!("{:<10} {:<9} {:<32} comment", "id", "status", "location");
    for comment in &filtered {
        let location = format!("{}:{}", comment.file, comment.anchor.new_line);
        let first_line = comment.body.lines().next().unwrap_or("");
        println!(
            "{:<10} {:<9} {:<32} {first_line}",
            comment.id,
            comment_status_label(comment.status),
            location,
        );
    }
    Ok(())
}

fn run_comments_export(format: ExportFormat, status: StatusFilter) -> Result<()> {
    let repo_root = comments_repo_root()?;
    let filtered: Vec<Comment> = CommentStore::new(&repo_root)
        .load()?
        .into_iter()
        .filter(|c| status.matches(c.status))
        .collect();

    match format {
        ExportFormat::Json => println!("{}", serde_json::to_string_pretty(&filtered)?),
        ExportFormat::Md => print!("{}", format_export_markdown(&repo_root, &filtered)),
    }
    Ok(())
}

/// Groups `comments` by file (in first-appearance order) and renders each as
/// a `### file:line` heading, the anchored line's current text (relocated
/// against `repo_root`'s on-disk content, so the quoted line matches what
/// the agent will actually see when it opens the file), the comment body,
/// and an id/status footer — directly pasteable into an AI coding agent's
/// prompt, per the milestone spec, opening with an instruction line to that
/// effect.
fn format_export_markdown(repo_root: &Path, comments: &[Comment]) -> String {
    let mut out = String::from(
        "Address the following review comments; after fixing each, run `ktmr comments resolve <id>`.\n\n",
    );

    let mut file_order: Vec<&str> = Vec::new();
    let mut by_file: HashMap<&str, Vec<&Comment>> = HashMap::new();
    for comment in comments {
        if !by_file.contains_key(comment.file.as_str()) {
            file_order.push(comment.file.as_str());
        }
        by_file
            .entry(comment.file.as_str())
            .or_default()
            .push(comment);
    }

    for file in file_order {
        let content = std::fs::read_to_string(repo_root.join(file)).ok();
        let lines: Vec<&str> = content
            .as_deref()
            .map(|s| s.lines().collect())
            .unwrap_or_default();

        for comment in &by_file[file] {
            let (line, quoted) = match comments::relocate(comment, &lines) {
                Some(line) => (
                    line,
                    lines
                        .get((line - 1) as usize)
                        .map_or_else(String::new, |l| l.trim().to_owned()),
                ),
                None => (
                    comment.anchor.new_line,
                    "(line not found — the file has changed since this comment was written)"
                        .to_owned(),
                ),
            };
            out.push_str(&format!("### {file}:{line}\n"));
            out.push_str(&format!("> {quoted}\n\n"));
            out.push_str(&comment.body);
            out.push_str(&format!(
                "\n\n_id: {} · status: {}_\n\n",
                comment.id,
                comment_status_label(comment.status)
            ));
        }
    }
    out
}

fn run_comments_add(file: String, line: u32, body: String) -> Result<()> {
    let repo_root = comments_repo_root()?;
    let content = std::fs::read_to_string(repo_root.join(&file))
        .with_context(|| format!("failed to read {file}"))?;
    let lines: Vec<&str> = content.lines().collect();
    let anchor =
        comments::anchor_for(&lines, line).with_context(|| format!("{file} has no line {line}"))?;

    let comment = Comment {
        id: comments::generate_id(),
        created_at: comments::now_unix(),
        file: file.clone(),
        anchor,
        body,
        status: CommentStatus::Open,
        resolved_at: None,
    };
    CommentStore::new(&repo_root).append_comment(&comment)?;
    println!("added comment {} on {file}:{line}", comment.id);
    Ok(())
}

fn run_comments_set_status(id: String, status: CommentStatus) -> Result<()> {
    let repo_root = comments_repo_root()?;
    let resolved_at = (status == CommentStatus::Resolved).then(comments::now_unix);
    CommentStore::new(&repo_root)
        .set_status(&id, status, resolved_at)
        .with_context(|| format!("failed to update comment {id}"))?;
    println!("{id}: {}", comment_status_label(status));
    Ok(())
}

/// The bundled Claude Code skill, embedded at compile time so `ktmr skill
/// install` works from the installed binary alone — no separate asset to
/// ship or locate on disk.
const SKILL_MD: &str = include_str!("../skills/katamari-review/SKILL.md");

fn run_skill(action: SkillCommand) -> Result<()> {
    match action {
        SkillCommand::Install => run_skill_install(),
    }
}

fn run_skill_install() -> Result<()> {
    let repo_root = comments_repo_root()?;
    let dest_dir = repo_root
        .join(".claude")
        .join("skills")
        .join("katamari-review");
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("failed to create {}", dest_dir.display()))?;
    let dest = dest_dir.join("SKILL.md");
    std::fs::write(&dest, SKILL_MD)
        .with_context(|| format!("failed to write {}", dest.display()))?;
    println!("installed katamari-review skill to {}", dest.display());
    Ok(())
}

const ALL_LANGUAGES: [lsp::adapter::Language; 5] = [
    lsp::adapter::Language::Rust,
    lsp::adapter::Language::TypeScript,
    lsp::adapter::Language::Python,
    lsp::adapter::Language::Go,
    lsp::adapter::Language::Kotlin,
];

fn run_lsp(action: LspCommand) -> Result<()> {
    match action {
        LspCommand::Doctor => run_lsp_doctor(),
        LspCommand::Install { language } => run_lsp_install(language),
        LspCommand::Update => run_lsp_update(),
    }
}

/// The workspace root and `[lsp.servers.<lang>]` overrides `ktmr lsp`'s
/// three subcommands diagnose/install against — resolved the same way a
/// live `ktmr diff`/`ktmr open` session would (current directory's git
/// root, falling back to the directory itself outside a repo), so `doctor`
/// reports exactly what a real session would see.
fn lsp_workspace_root_and_overrides() -> Result<(PathBuf, HashMap<String, config::ServerOverride>)>
{
    let cwd = std::env::current_dir()?;
    let repo_root = GitSource::discover(&cwd)
        .and_then(|source| source.repo_root())
        .unwrap_or(cwd);
    let config = config::load_merged(&repo_root);
    Ok((repo_root, config.lsp_servers))
}

fn language_label(language: lsp::adapter::Language) -> &'static str {
    match language {
        lsp::adapter::Language::Rust => "rust",
        lsp::adapter::Language::TypeScript => "typescript",
        lsp::adapter::Language::Python => "python",
        lsp::adapter::Language::Go => "go",
        lsp::adapter::Language::Kotlin => "kotlin",
    }
}

fn resolved_from_label(from: lsp::adapter::ResolvedFrom) -> &'static str {
    use lsp::adapter::ResolvedFrom;
    match from {
        ResolvedFrom::ConfigOverride => "override",
        ResolvedFrom::ProjectLocal => "project-local",
        ResolvedFrom::Path => "PATH",
        ResolvedFrom::ToolchainWhich => "toolchain (rustup)",
        ResolvedFrom::Mise => "mise",
        ResolvedFrom::KatamariManaged => "katamari-managed",
    }
}

fn run_lsp_doctor() -> Result<()> {
    let (workspace_root, overrides) = lsp_workspace_root_and_overrides()?;
    println!(
        "{:<12} {:<20} {:<13} path / notes",
        "language", "source", "auto-install"
    );
    for language in ALL_LANGUAGES {
        let diagnosis = lsp::adapter::diagnose(language, &workspace_root, &overrides);
        let (source, detail) = match &diagnosis.found {
            Some((from, path)) => (resolved_from_label(*from), path.display().to_string()),
            None => ("not found", String::new()),
        };
        let auto_install = if diagnosis.found.is_some() {
            "n/a"
        } else if diagnosis.installable_if_missing {
            "yes"
        } else {
            "no"
        };
        println!(
            "{:<12} {source:<20} {auto_install:<13} {detail}",
            language_label(diagnosis.language)
        );
    }
    Ok(())
}

fn run_lsp_install(language: LanguageArg) -> Result<()> {
    let languages: Vec<lsp::adapter::Language> = match language {
        LanguageArg::All => ALL_LANGUAGES.to_vec(),
        LanguageArg::Rust => vec![lsp::adapter::Language::Rust],
        LanguageArg::Typescript => vec![lsp::adapter::Language::TypeScript],
        LanguageArg::Python => vec![lsp::adapter::Language::Python],
        LanguageArg::Go => vec![lsp::adapter::Language::Go],
        LanguageArg::Kotlin => vec![lsp::adapter::Language::Kotlin],
    };
    for language in languages {
        println!("=== {} ===", language_label(language));
        match lsp::install::ensure(language, |message| println!("  {message}")) {
            Ok(path) => println!("  installed: {}", path.display()),
            Err(e) => println!("  skipped: {e}"),
        }
    }
    Ok(())
}

fn run_lsp_update() -> Result<()> {
    for language in ALL_LANGUAGES {
        let prefix = lsp::install::prefix_dir();
        if lsp::install::installed_binary_path(&prefix, language).is_some() {
            println!(
                "{}: already at the pinned version",
                language_label(language)
            );
            continue;
        }
        println!(
            "{}: not at the pinned version, (re)installing",
            language_label(language)
        );
        match lsp::install::ensure(language, |message| println!("  {message}")) {
            Ok(path) => println!("  (re)installed: {}", path.display()),
            Err(e) => println!("  skipped: {e}"),
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
/// to make diagnostics appear without a hover) and waits for a
/// `textDocument/publishDiagnostics`-shaped notification for it on
/// `events_rx`, which is how `katamari`'s core value proposition ("did the
/// AI's edit introduce errors") actually surfaces. That covers both models a
/// server might speak: a push-only server (rust-analyzer, TS, most others)
/// sends that notification unsolicited, straight off the transport; a
/// pull-only one (kotlin-lsp — LSP 3.17's `textDocument/diagnostic`, no
/// unsolicited push at all) has [`lsp::LspManager`] pull on its behalf right
/// after `warm_up`'s `didOpen` and republish the answer as a notification of
/// the same shape (see `lsp::manager::apply_pulled_diagnostics`) — so this
/// function needs no separate pull-polling branch of its own; it was
/// already the pull path's intended landing point. kotlin-lsp specifically
/// can leave an early pull empty while it's still indexing the project
/// (tens of seconds on a cold run); the manager re-pulls once indexing
/// reports done via `$/progress end`, which is why this keeps listening
/// past an empty wave the same way it already did for rust-analyzer's
/// fast/flycheck waves, below.
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
/// and kinds the TUI would show, plus one `COMMENT` line per comment
/// anchored (after relocation — see `comments::build_index`) to that row.
/// Exists so parsing correctness — and, since M6, comment placement — can
/// be checked against `git diff` output without a terminal.
///
/// Deliberately ignores `[ui] wrap`: this writes each row's raw text
/// straight to stdout, one row per line, with no notion of a terminal
/// width to wrap against in the first place — no pane, no gutter, no
/// continuation rows, and no `App::row_visual_height` to consult.
fn format_dump(files: &[DiffFile], comments: &CommentIndex) -> String {
    let (total_added, total_deleted) = total_stat(files);
    let mut out = format!(
        "files: {}  +{total_added} -{total_deleted}\n\n",
        files.len()
    );
    for row in flatten(files) {
        out.push_str(&format_render_row(files, row));
        if let RenderRow::Line {
            file_idx,
            hunk_idx,
            row_idx,
        } = row
        {
            out.push_str(&format_comment_markers(
                files, file_idx, hunk_idx, row_idx, comments,
            ));
        }
    }
    out
}

/// Plain-text rendering of the side-by-side pairing: file/hunk headers as in
/// [`format_dump`], but each hunk body row becomes an `OLD`/`NEW` pair (`--`
/// for a filler cell with no counterpart on that side) instead of a flat
/// list — the same grouping `diff_view::render` draws as two columns.
/// Comment markers follow the `NEW` cell only, matching where a comment can
/// ever be anchored (see `App::comment_target`'s docs).
fn format_dump_side_by_side(files: &[DiffFile], comments: &CommentIndex) -> String {
    let (total_added, total_deleted) = total_stat(files);
    let mut out = format!(
        "files: {}  +{total_added} -{total_deleted}  layout=side\n\n",
        files.len()
    );
    let rows = flatten(files);
    for paired in flatten_side_by_side(files) {
        match paired {
            SideBySideRow::Full { flat_idx } => {
                out.push_str(&format_render_row(files, rows[flat_idx]));
                out.push_str(&format_comment_markers_for_flat_row(
                    files, &rows, flat_idx, comments,
                ));
            }
            SideBySideRow::Paired { old, new } => {
                out.push_str("    PAIR\n");
                out.push_str(&format_side_cell("OLD", files, &rows, old));
                out.push_str(&format_side_cell("NEW", files, &rows, new));
                if let SideCell::Line { flat_idx } = new {
                    out.push_str(&format_comment_markers_for_flat_row(
                        files, &rows, flat_idx, comments,
                    ));
                }
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

/// One `      COMMENT <id> [status] <first line of body>` line per comment
/// anchored to `files[file_idx].hunks[hunk_idx].rows[row_idx]`'s current
/// line — empty when there are none. Shared by [`format_dump`] and (via
/// [`format_comment_markers_for_flat_row`]) [`format_dump_side_by_side`], so
/// both dump modes agree on exactly what counts as "annotated."
fn format_comment_markers(
    files: &[DiffFile],
    file_idx: usize,
    hunk_idx: usize,
    row_idx: usize,
    comments: &CommentIndex,
) -> String {
    let file = &files[file_idx];
    let row = &file.hunks[hunk_idx].rows[row_idx];
    let Some(new_line) = row.new_line else {
        return String::new();
    };
    let mut out = String::new();
    for annotation in comments.at(file.display_path(), new_line) {
        out.push_str(&format_comment_marker(annotation));
    }
    out
}

/// As [`format_comment_markers`], addressing the row by its flat index into
/// `rows` (as [`flatten_side_by_side`] does) rather than by
/// `(file_idx, hunk_idx, row_idx)` directly.
fn format_comment_markers_for_flat_row(
    files: &[DiffFile],
    rows: &[RenderRow],
    flat_idx: usize,
    comments: &CommentIndex,
) -> String {
    let RenderRow::Line {
        file_idx,
        hunk_idx,
        row_idx,
    } = rows[flat_idx]
    else {
        return String::new();
    };
    format_comment_markers(files, file_idx, hunk_idx, row_idx, comments)
}

fn format_comment_marker(annotation: &CommentAnnotation) -> String {
    let status = if annotation.detached {
        "detached"
    } else {
        match annotation.status {
            CommentStatus::Open => "open",
            CommentStatus::Resolved => "resolved",
        }
    };
    let first_line = annotation.body.lines().next().unwrap_or("");
    format!("      COMMENT {} [{status}] {first_line}\n", annotation.id)
}
