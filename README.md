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
servers spawn lazily, the first time something asks, and auto-install
themselves if missing — see [Language servers](#language-servers) if a
feature reports a server as unavailable.

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
| Jump back / forward | `C-o` / `C-i`\* | `C-o` / `C-i`\* |
| Next / prev symbol on line | `Tab` / `BackTab` | `Tab` / `BackTab` |
| Confirm / cancel | `Enter` / `Esc` | `Enter` / `Esc` |
| Toggle sidebar | `b` | `b` |
| Toggle unified/side-by-side | `s` | `s` |
| Toggle timeline | `t` | `t` |
| Toggle range-select (timeline) | `v` | `C-Space` |
| Add comment | `c` | `C-c C-c` |
| Toggle inline comment bodies | `C` | `C` |
| Quit | `q` | `q` |

\* Jump-forward matches neovim: it's `C-i` in terminals that implement the
[kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
(Ghostty, kitty, WezTerm, iTerm2 3.5+, Alacritty), which lets katamari tell a
literal Tab keypress apart from `Ctrl-i` on the wire — without it they arrive
as the same byte, and Tab already means next-symbol-on-line, so katamari
falls back to `C-t` there instead (notably Terminal.app, which doesn't
implement the protocol). Detected once at startup; `C-t` also keeps working
as an alias in terminals where `C-i` is active.

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

[lsp]
# Whether a missing language server is silently downloaded/built into
# katamari's own prefix instead of just reporting an install hint. Default
# true; see "Language servers" below for what each language's strategy is.
auto_install = true

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

## Language servers

katamari spawns servers lazily, the first time a file of that language is
opened, and looks for each one in this order: config override →
project-local convention (`node_modules/.bin`, `.venv/bin`) → `PATH` →
`rustup which`/`mise which` → katamari's own managed install (below). If
none of those find anything, katamari **auto-installs it** — downloading
rust-analyzer's prebuilt binary from GitHub releases, or running `npm
install`/`go install` into a private prefix — with progress shown in the
status bar, no confirmation prompt, the same "it just works" experience
VSCode/Zed give you.

| Language | Server | Auto-install strategy |
| --- | --- | --- |
| Rust | `rust-analyzer` | prebuilt binary from GitHub releases |
| TypeScript/JavaScript | `typescript-language-server` | `npm install` (bootstraps a private Node.js runtime first if no `npm` can be found anywhere) |
| Python | `pyright-langserver` | `npm install` (same Node.js bootstrap fallback) |
| Go | `gopls` | `go install`, requires an existing go toolchain — katamari won't install Go itself |

Managed installs live under `~/.local/share/katamari/servers/`
(`$XDG_DATA_HOME/katamari/servers/` if set), one version-stamped
subdirectory per server — never touched by anything but katamari, and safe
to delete entirely (everything reinstalls on demand).

`[lsp.servers.<lang>]` in config (above) overrides any of these with an
explicit command, taking priority over every lookup including auto-install.
To disable auto-install and just get the old "here's the install command"
status-bar hint instead:

```toml
[lsp]
auto_install = false
```

`ktmr lsp` manages installs directly, without waiting for a server to be
needed:

```
ktmr lsp doctor              # where each language's server resolves from today (no installs triggered)
ktmr lsp install <language>  # force an install into katamari's managed prefix (rust/typescript/python/go/all)
ktmr lsp update              # reinstall any pinned server that's fallen behind the current pin
```

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

## Development

`mise run test` runs the unit suite plus `tests/e2e.rs`, an integration
suite that drives the real compiled `ktmr` binary through a PTY (via
`pty-process`) and parses its output with `vt100` — real crossterm parsing,
real kitty-keyboard-protocol negotiation, no terminal emulator required.
Run it on its own with:

```
mise run e2e
```

`mise run e2e-tmux` is a second, real-terminal-emulator tier: it builds a
debug binary and runs `scripts/e2e-tmux.sh`, which drives `ktmr diff` inside
an actual detached tmux session on a private socket — the one thing the PTY
suite can't check for real, since tmux (unlike the PTY suite's fake
terminal) genuinely doesn't speak the kitty keyboard protocol, so this is
what proves the `C-o/C-t` fallback is what a reviewer actually sees there.

```
mise run e2e-tmux
```

Both are self-contained: they build their own throwaway git fixtures in a
tempdir and point `$HOME`/`$XDG_CONFIG_HOME`/`$XDG_DATA_HOME` at another
tempdir, so a test run never touches your real `~/.config/katamari` or the
katamari-managed language-server install prefix.
