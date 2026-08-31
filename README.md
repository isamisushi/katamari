# katamari

A terminal diff-review tool. `ktmr diff` shows a `git diff` with syntax
highlighting, hover/go-to-definition/find-references/diagnostics from real
language servers, and inline review comments an AI coding agent can read
back and address — and `u` regroups a big tangled diff into ordered,
stacked-PR-like review units, without creating a single branch. All of it
without leaving the terminal.

![demo](docs/demo.gif)

**Full manual: [isamisushi.github.io/katamari](https://isamisushi.github.io/katamari/)**
(the same content also lives in-repo under [`docs/src/`](docs/src/)).
This page is a short landing page — install, a minimal quickstart, and
where to read more.

## Why

Reviewing an AI agent's changes usually means either reading a raw `git
diff` with no semantic information, or opening a full IDE for a change
that's often small and mechanical. katamari sits in between: LSP is wired
directly into the diff, so you can hover a changed identifier or jump to
its definition without leaving it. It also assumes the underlying workflow
is "agent edits, you review, you leave comments, the agent addresses them"
rather than "you edit" — comments live in a plain file an agent reads
through `ktmr comments`, and the diff refreshes live as edits land on
disk. And since an agent's whole task usually lands as one tangled diff,
`u` recovers a stacked-PR-like reading order — foundations first, then
what's built on them — as a derived, read-only view, with nothing branched
or rewritten. For repositories using [jj](https://github.com/jj-vcs/jj)
colocated with git, katamari also keeps a timeline of jj's automatic
working-copy snapshots, so you can step back through every version of the
working tree an agent's session passed through, not just what's on disk
now. See [Why](https://isamisushi.github.io/katamari/introduction.html#why)
for the full case.

## Install

Every method installs two identical binaries, `katamari` and `ktmr` (the
short name is what the rest of this document uses). macOS and Linux; no
distro packages (apt/dnf/pacman) yet.

**Homebrew:**

```
brew install isamisushi/tap/katamari
```

**Install script** — detects your platform, no Rust toolchain needed:

```
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/isamisushi/katamari/releases/latest/download/katamari-installer.sh | sh
```

**mise**, via its `ubi` backend:

```
mise use -g "ubi:isamisushi/katamari[exe=ktmr]"
```

Prebuilt binaries (with a per-target archive table), building from source,
and running under Windows/WSL are all covered in the
[Installation chapter](https://isamisushi.github.io/katamari/installation.html).

## Quickstart

Run inside any git repository:

```
ktmr diff              # working tree vs HEAD, refreshed live by default
ktmr diff --staged     # staged (index) changes vs HEAD
ktmr diff --pr 123     # a GitHub pull request, via your logged-in `gh`
```

Inside the diff: `j`/`k` move, `K` hovers the identifier under the cursor,
`gd` goes to its definition, `/` searches, `c` then `C-s` leaves a review
comment on the current line (or a `V` visual range), `u` groups the diff
into ordered review units. `o` opens a popup to switch scope mid-session
(working tree, staged, a revision, another PR) without restarting; `?`
opens a filterable list of every live binding.

Point an agent at what you left:

```
ktmr comments list --json    # or: ktmr comments export --format=md
ktmr comments resolve <id>   # after it addresses one
```

`ktmr skill install` writes that comment → address → resolve loop into the
repository as a small, tool-agnostic skill an agent picks up automatically
(`ktmr skill install --user` does the same once for every repository under
your home directory instead). katamari also offers to install it the first
time you save a comment in a repo that doesn't have it yet.

The full [Quickstart chapter](https://isamisushi.github.io/katamari/quickstart.html)
covers every scope `ktmr diff` opens (staged, a commit, a range, a jj
revset, `--pr <number>`), `ktmr log`, the LSP Inspector, fold expansion,
and the scope-menu popup (`o`) for switching mid-session.

## Features

- **Semantic review units** (`u`) — the diff's hunks grouped into ordered,
  stacked-PR-like units, through the `claude`/`codex` CLI you already
  have, derived and read-only
- **LSP inside the diff** — hover, go-to-definition, find-references, and
  live diagnostics on changed lines (Rust / TypeScript / Python / Go /
  Kotlin / Java, plus any language via `[lsp.servers.<id>]`)
- **Live refresh** — working-tree diffs update as an agent's edits land on
  disk, no restart
- **jj support** — colocated jj revsets, and a snapshot timeline through
  every save of an agent's session
- **GitHub PR review** — `ktmr diff --pr 123` opens a pull request's diff
  through your own `gh`, no checkout
- **Your keybindings** — vim (default) and emacs presets, every action
  remappable

See the [full manual](https://isamisushi.github.io/katamari/) for
[Review units](https://isamisushi.github.io/katamari/review-units.html),
[Keybindings](https://isamisushi.github.io/katamari/keybindings.html),
[Language servers](https://isamisushi.github.io/katamari/language-servers.html),
[Health check](https://isamisushi.github.io/katamari/health-check.html),
[Reset](https://isamisushi.github.io/katamari/reset.html), and
[jj colocated setup](https://isamisushi.github.io/katamari/jj-colocated-setup.html).

## Configuration

TOML, merged from built-in defaults → `~/.config/katamari/config.toml` →
`<repo_root>/.katamari/config.toml` (later wins per field):

```toml
keymap = "vim"          # or "emacs"

[keys]
quit = "Z Z"             # override any action's key

[units]
agent = "claude"         # which CLI `u` spawns to group the diff
```

Every field, with its default and what it does, is documented in the
[Configuration chapter](https://isamisushi.github.io/katamari/configuration.html).

## Development

`mise run test` runs the unit suite plus a PTY-driven integration suite
(`tests/e2e.rs`) that drives the real compiled `ktmr` binary and asserts
on the rendered terminal screen; `mise run e2e-tmux` is a second,
real-terminal-emulator tier for the one thing a PTY can't check for real:
the kitty-keyboard-protocol fallback. Both build their own throwaway git
fixtures and point `$HOME`/`$XDG_CONFIG_HOME`/`$XDG_DATA_HOME` at a
tempdir, so a test run never touches your real config or language-server
installs. See the
[Development chapter](https://isamisushi.github.io/katamari/development.html)
for the full picture.

## License

MIT, see [LICENSE](LICENSE).
