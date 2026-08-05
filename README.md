# katamari

A terminal diff-review tool. `ktmr diff` shows a `git diff` with syntax
highlighting, hover/go-to-definition/find-references/diagnostics from real
language servers, and inline review comments an AI coding agent can read
back and address — all without leaving the terminal.

## Why

Reviewing an AI agent's changes usually means either reading a raw `git
diff` with no semantic information (what does this symbol actually
resolve to? does this introduce a type error?), or opening a full IDE for
a change that's often small and mechanical. katamari sits in between: it's
a diff viewer with LSP wired directly into it, so you can hover a changed
identifier or jump to its definition without leaving the diff, and see
compiler/type-checker diagnostics on the changed lines themselves.

It also assumes the underlying workflow is "agent edits, you review, you
leave comments, the agent addresses them" rather than "you edit." Review
comments are stored as a plain file (`.katamari/comments.jsonl`) an agent
reads and updates through `ktmr comments`, and `ktmr diff --watch`
refreshes the diff live as the agent's edits land on disk — no restart, no
re-running a command by hand.

For repositories using [jj](https://github.com/jj-vcs/jj) colocated with
git, katamari also keeps a timeline of jj's automatic working-copy
snapshots, so you can step back through every version of the working tree
an agent's session passed through, not just the version currently on disk.

## Install

```
cargo install --path .
```

Installs two identical binaries, `katamari` and `ktmr` (the short name is
what the rest of this document uses). If this repository itself is
managed with [mise](https://mise.jdx.dev/), `mise.toml` already pins a
Rust toolchain and defines `mise run build`/`test`/`lint`/`fmt` tasks for
working on katamari's own source; none of that is required to build or
run it.

## Quickstart

Run inside any git repository:

```
ktmr diff              # working tree vs HEAD
ktmr diff --staged     # staged (index) changes vs HEAD
ktmr diff <rev>        # one commit's own changes
ktmr diff <a>..<b>     # a revision range
ktmr diff --watch      # refresh automatically as files change on disk
```

Inside the diff: `K` hovers the identifier under the cursor, `gd`/`gr` go
to its definition/references, `]d`/`[d` jump between diagnostics. Language
servers spawn lazily, the first time something asks — see
[LSP server install hints](#lsp-server-install-hints) if a feature reports
a server as unavailable.

Leave a review comment on the current line with `c`, then `C-s` to save.
An AI coding agent addresses them with:

```
ktmr comments list --json     # or: ktmr comments export --format=md
```

...making the requested changes and running `ktmr comments resolve <id>`
for each one it handles — resolutions show up live in an open `ktmr diff`
session. `ktmr skill install` drops a Claude Code skill
(`.claude/skills/katamari-review/SKILL.md`) into the current repository
that teaches an agent this exact loop, so it picks it up automatically
rather than needing the workflow spelled out in every prompt.

`ktmr open <file>` opens a single file read-only, with the same
highlighting/hover/go-to-definition as the diff view — useful for reading
code a diff jumped you into without leaving katamari.

If the repository has a colocated jj repo (see
[jj colocated setup](#jj-colocated-setup)), `t` from `ktmr diff` opens the
snapshot timeline, or jump there directly with `ktmr timeline`.

## Keybindings

Vim bindings are the default; set `keymap = "emacs"` in config (see
below) for the emacs column. `q` quits either way.

| Action | Vim | Emacs |
| --- | --- | --- |
| Cursor down / up | `j` / `k` | `C-n` / `C-p` |
| Half page down / up | `C-d` / `C-u` | `C-v` / `M-v` |
| Top / bottom | `gg` / `G` | `M-<` / `M->` |
| Next / prev hunk | `]c` / `[c` | `M-n` / `M-p` |
| Next / prev file | `]f` / `[f` | `C-x n` / `C-x p` |
| Next / prev diagnostic | `]d` / `[d` | `M-g M-n` / `M-g M-p` |
| Hover | `K` | `C-h` |
| Go to definition | `gd` | `M-.` |
| Find references | `gr` | `M-?` |
| Jump back / forward | `C-o` / `C-t` | `C-o` / `C-t` |
| Next / prev symbol on line | `Tab` / `BackTab` | `Tab` / `BackTab` |
| Confirm / cancel | `Enter` / `Esc` | `Enter` / `Esc` |
| Toggle sidebar | `b` | `b` |
| Toggle unified/side-by-side | `s` | `s` |
| Toggle timeline | `t` | `t` |
| Toggle range-select (timeline) | `v` | `C-Space` |
| Add comment | `c` | `C-c C-c` |
| Toggle inline comment bodies | `C` | `C` |
| Quit | `q` | `q` |

Any binding can be overridden per action; see `[keys]` below.

## Config

TOML, merged in this order (later wins per field, not per file — setting
one field in the repo file doesn't reset the rest of a section back to
whatever the home file or the built-in default had it at):

1. built-in defaults
2. `~/.config/katamari/config.toml`
3. `<repo_root>/.katamari/config.toml`

An unrecognized key warns once to stderr and is otherwise ignored — a
typo or a field from a newer katamari version never stops a session from
starting. All sections are optional.

```toml
# Which built-in keymap preset to start from.
keymap = "vim"  # or "emacs"

[keys]
# Action name (kebab-case; see the keybindings table above for what each
# one does) -> key-sequence notation, overriding the preset above.
# Notation: "C-x" (Ctrl), "M-x" (Alt/Meta), "Space"/"Esc"/"Enter"/"Tab"/
# "BackTab"/"Backspace"/arrows/Home/End/PageUp/PageDown as named keys, a
# bare character otherwise, space-separated for a multi-key sequence.
quit = "Z Z"
next-hunk = "C-n"

[lsp.servers.rust]
# Overrides how a language's server is resolved — highest priority, above
# PATH and every project-local lookup. `<lang>` is rust/typescript/python/go.
command = "/opt/homebrew/bin/rust-analyzer"
args = []

[ui]
# Terminal columns a tab expands to; ColumnMap (cursor/LSP positions) and
# rendering always agree on this value. Default 4.
tab_width = 4
# Above this many changed lines, a diff file's syntax highlighting is
# skipped in favor of plain styling (and it's excluded from LSP warm-up) —
# lockfiles (*.lock, package-lock.json, *.min.js) always skip regardless
# of size. Default 5000.
highlight_max_lines = 5000

[watch]
# Debounce window for `--watch`: how long a burst of filesystem changes
# must go quiet before the diff refreshes. Milliseconds, default 200.
debounce_ms = 200
```

## LSP server install hints

katamari spawns these lazily and reports a specific install hint in the
status bar if one isn't found — this is the same information, up front:

| Language | Server | Install |
| --- | --- | --- |
| Rust | `rust-analyzer` | `rustup component add rust-analyzer` (found via `PATH` or `rustup which`) |
| TypeScript/JavaScript | `typescript-language-server` | `npm i -g typescript-language-server typescript` (project-local `node_modules/.bin` preferred if present) |
| Python | `pyright-langserver` | `npm i -g pyright` (project-local `.venv/bin` preferred if present) |
| Go | `gopls` | `go install golang.org/x/tools/gopls@latest` |

`[lsp.servers.<lang>]` in config (above) overrides any of these with an
explicit command.

## jj colocated setup

katamari's timeline reads jj's own automatic working-copy snapshots — no
katamari-specific setup beyond jj itself being colocated with the existing
git repository:

```
cd your-repo
jj git init --colocate
```

That's it: jj now tracks the same working copy as git (a `.jj` directory
appears alongside `.git`), and every save creates a new jj operation
katamari's timeline can show. Nothing about `ktmr diff` changes if jj
isn't set up — the timeline (`t`) simply reports it's unavailable.
