mod diff;
mod highlight;
mod keymap;
mod ui;
mod vcs;

use anyhow::Result;
use clap::{Parser, Subcommand};
use diff::{DiffFile, DiffLineKind, RenderRow, flatten, parse_unified_diff};
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
    /// Show the working-tree diff against HEAD in a full-screen TUI.
    Diff {
        /// Print parsed render rows as plain text and exit, instead of
        /// launching the TUI. Useful for scripting and for verifying
        /// parsing without a terminal.
        #[arg(long, hide = true)]
        dump: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Diff { dump: false }) {
        Command::Diff { dump } => run_diff(dump),
    }
}

fn run_diff(dump: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let source = GitSource::discover(&cwd)?;
    let files = parse_unified_diff(&source.working_tree_diff()?);

    if dump {
        print!("{}", format_dump(&files));
        return Ok(());
    }

    let repo_root = source.repo_root()?;
    let repo_name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo_root.display().to_string());

    let mut app = ui::App::new(repo_name, files);
    ui::run(&mut app)
}

/// Plain-text rendering of the parsed, flattened diff: one line per file
/// header, hunk header, and content row, with the same line numbers and
/// kinds the TUI would show. Exists so parsing correctness can be checked
/// against `git diff` output without a terminal.
fn format_dump(files: &[DiffFile]) -> String {
    let (total_added, total_deleted) = files.iter().fold((0u32, 0u32), |(a, d), f| {
        let (fa, fd) = f.stat();
        (a + fa, d + fd)
    });

    let mut out = format!(
        "files: {}  +{total_added} -{total_deleted}\n\n",
        files.len()
    );

    for row in flatten(files) {
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
                out.push_str(&format!(
                    "FILE {} [{status}] +{added} -{deleted}\n",
                    file.display_path()
                ));
                if file.is_renamed {
                    out.push_str(&format!(
                        "  renamed from {}\n",
                        file.old_path.as_deref().unwrap_or("?")
                    ));
                }
            }
            RenderRow::HunkHeader { file_idx, hunk_idx } => {
                let hunk = &files[file_idx].hunks[hunk_idx];
                out.push_str(&format!(
                    "  HUNK @@ -{},{} +{},{} @@ {}\n",
                    hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines, hunk.header
                ));
            }
            RenderRow::Line {
                file_idx,
                hunk_idx,
                row_idx,
            } => {
                let row = &files[file_idx].hunks[hunk_idx].rows[row_idx];
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
                out.push_str(&format!(
                    "    LINE {kind:<3} {old:>5} {new:>5}  {}\n",
                    row.text
                ));
            }
        }
    }

    out
}
