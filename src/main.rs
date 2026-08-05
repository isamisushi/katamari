mod diff;
mod highlight;
mod keymap;
mod lsp;
mod ui;
mod vcs;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use diff::{
    DiffFile, DiffLineKind, RenderRow, SideBySideRow, SideCell, flatten, flatten_side_by_side,
    parse_unified_diff,
};
use std::path::PathBuf;
use ui::app::Layout;
use ui::{App, FileView, View, ViewStack};
use vcs::DiffSource;
use vcs::git::GitSource;

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
    },
    /// Open a single file in a read-only, syntax-highlighted viewer.
    Open {
        /// Path to the file to open.
        file: PathBuf,
    },
    /// Spawns a language server, requests hover at one position, prints the
    /// result, and exits — an E2E smoke test for the `lsp` module runnable
    /// without a terminal. Not part of the reviewing workflow the rest of
    /// this CLI supports, so it's hidden from `--help`.
    #[command(hide = true)]
    LspCheck {
        /// Path to the file to hover in.
        file: PathBuf,
        /// 1-based line number, as an editor would show it.
        line: u32,
        /// 1-based display column within that line.
        col: usize,
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
    }) {
        Command::Diff {
            dump,
            layout,
            staged,
            range,
        } => run_diff(dump, layout, staged, range),
        Command::Open { file } => run_open(file),
        Command::LspCheck { file, line, col } => run_lsp_check(file, line, col),
    }
}

fn run_diff(dump: bool, layout: LayoutArg, staged: bool, range: Option<String>) -> Result<()> {
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
    ui::run(&mut stack)
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
    ui::run(&mut stack)
}

/// `ktmr lsp-check <file> <line> <col>` — an E2E smoke test for the whole
/// `lsp` module without a terminal: spawns a server through the same
/// [`lsp::LspManager`] the TUI uses, waits for it to become `Ready`
/// (printing `$/progress` notifications and state transitions along the
/// way, since a cold `rust-analyzer` can take real time to index), requests
/// hover at the given position, prints the result, and shuts the server
/// down before exiting — see [`lsp::LspManager::shutdown_all`]'s docs for
/// why that last step isn't optional.
fn run_lsp_check(file: PathBuf, line: u32, col: usize) -> Result<()> {
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

    // The very first hover call is also what makes `LspManager` spawn the
    // server at all (it's lazy — nothing starts until something asks for
    // it). That request reaches the server the instant the `initialize`
    // handshake finishes, which is well before rust-analyzer's background
    // workspace indexing catches up, so an immediate `Ok(None)` here isn't
    // necessarily "no hover info" — it can just as easily mean "asked too
    // early." Rather than trust the first answer, this re-issues the
    // request every `RETRY_INTERVAL` (printing state/progress in between)
    // until either a real result arrives or the overall deadline passes,
    // keeping only the most recent answer.
    const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);

    let mut pending = Some(manager.hover(&absolute_file, &git_root, &line_text, line0, col0));
    let mut last_attempt = std::time::Instant::now();
    let mut last_state = None;
    let mut best = None;

    let result = loop {
        if let Some(rx) = &pending
            && let Ok(result) = rx.try_recv()
        {
            let is_final = matches!(result, Ok(Some(_)));
            best = Some(result);
            pending = None;
            if is_final {
                break best.expect("just assigned");
            }
        }

        if pending.is_none() && last_attempt.elapsed() >= RETRY_INTERVAL {
            println!("[retry] requesting hover again");
            pending = Some(manager.hover(&absolute_file, &git_root, &line_text, line0, col0));
            last_attempt = std::time::Instant::now();
        }

        if let Ok(event) = events_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            match event {
                lsp::LspEvent::Notification { method, params } if method == "$/progress" => {
                    if let Some(text) = lsp::progress_status_text(&params) {
                        println!("[progress] {text}");
                    }
                }
                lsp::LspEvent::Closed { reason } => {
                    println!("[lsp] transport closed: {reason:?}");
                }
                _ => {}
            }
        }

        let state = manager.state(&absolute_file, &git_root);
        if last_state.as_ref() != Some(&state) {
            println!("[state] {state:?}");
            last_state = Some(state);
        }

        if std::time::Instant::now() > deadline {
            manager.shutdown_all();
            break best.unwrap_or_else(|| {
                Err(lsp::LspError::Io(
                    "timed out waiting for a hover response after 60s".to_owned(),
                ))
            });
        }
    };

    println!();
    match result {
        Ok(Some(hover)) => {
            println!("--- hover ---");
            println!("{}", ui::hover_popup::plain_text(&hover.contents));
        }
        Ok(None) => println!("--- hover ---\n(no hover information at this position)"),
        Err(e) => println!("--- hover error ---\n{e}"),
    }

    manager.shutdown_all();
    Ok(())
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
