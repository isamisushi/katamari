# katamari

**[The only terminal diff viewer with a language server inside
it.](https://isamisushi.github.io/katamari/compared-to-other-tools/)**
`ktmr diff` shows a `git diff` with hover/go-to-definition/find-references/live
diagnostics from real language servers, a resident coding-agent session
you can ask about any selection, review comments it reads back and
resolves, and `u` to regroup a tangled diff into ordered, stacked-PR-like
review units — without creating a single branch. All of it without
leaving the terminal.

![demo](docs/demo.gif)

**Full manual: [isamisushi.github.io/katamari](https://isamisushi.github.io/katamari/)**
(the same content also lives in-repo under [`docs/src/`](docs/src/)).

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
[Installation chapter](https://isamisushi.github.io/katamari/installation/),
which also covers [updating](https://isamisushi.github.io/katamari/installation/#updating)
(`ktmr self-update` for install-script installs).

## Quickstart

Run inside any git repository:

```
ktmr diff              # working tree vs HEAD, refreshed live by default
ktmr diff --staged     # staged (index) changes vs HEAD
ktmr diff --pr 123     # a GitHub pull request, via your logged-in `gh`
ktmr diff --branch     # this branch vs its detected base, live-refreshed
```

Inside the diff: `j`/`k` move, `K` hovers the identifier under the cursor,
`gd` goes to its definition, `/` searches, `c` then `C-s` leaves a review
comment on the current line (or a `V` visual range), `a` asks a resident
agent session about the line or selection, `u` groups the diff into
ordered review units. `o` opens a popup to switch scope mid-session
(working tree, staged, branch vs base, a revision, another PR) without
restarting; `?` opens a filterable list of every live binding.

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

The full [Quickstart chapter](https://isamisushi.github.io/katamari/quickstart/)
covers every scope `ktmr diff` opens (staged, a commit, a range, a jj
revset, `--pr <number>`), `ktmr log`, the LSP Inspector, fold expansion,
and the scope-menu popup (`o`) for switching mid-session.

## Features

- **LSP inside the diff** — hover, go-to-definition, find-references, and
  live diagnostics on changed lines (Rust / TypeScript / Python / Go /
  Kotlin / Java, plus any language via `[lsp.servers.<id>]`), on a live
  working tree or `HEAD` — read-only on a fixed commit, `--staged`, a jj
  range, or `--pr`
- **Ask the agent** (`a`/`A`/`p`) — question a resident coding-agent session
  about any selection, ask follow-ups in the same session, push every open
  comment in one message, `C-g` cancels an in-flight turn safely, and
  every edit stays gated on your own `y`/`n`
- **Durable, resolvable comments** — leave one on a line or a `V` range; it
  lands in a plain, git-trackable `.katamari/comments.jsonl` an agent
  lists, addresses, and resolves via `ktmr comments`, picked up live with
  no restart
- **Reviewed-hunk state** (`r`/`R`) — mark a hunk reviewed and it
  collapses, content-addressed so the mark survives a rebase or reorder:
  remember what you've reviewed — only what the agent rewrote comes back,
  kept in `.katamari/reviewed.jsonl`
- **Semantic review units** (`u`) — turn a tangled diff into a stack you
  can actually read, without touching a branch: hunks grouped into
  ordered, stacked-PR-like units through the `claude`/`codex` CLI you
  already have, derived and read-only
- **git and jj, equally** — colocated jj revsets and a snapshot timeline
  through every save of an agent's session, the same LSP/comments/units
  feature set either way
- **Any scope, PR and branch included** — `--pr 123` opens a GitHub pull
  request through your own `gh`, no checkout; `--branch`/`B` reviews the
  current branch against its auto-detected base, no network, live-followed
- **Live refresh** — working-tree diffs update as an agent's edits land on
  disk, no restart
- **Your keybindings** — vim (default) and emacs presets, every action
  remappable

See the [full manual](https://isamisushi.github.io/katamari/) for
[Review units](https://isamisushi.github.io/katamari/review-units/),
[Keybindings](https://isamisushi.github.io/katamari/keybindings/),
[Language servers](https://isamisushi.github.io/katamari/language-servers/),
[Health check](https://isamisushi.github.io/katamari/health-check/),
[Reset](https://isamisushi.github.io/katamari/reset/), and
[jj colocated setup](https://isamisushi.github.io/katamari/jj-colocated-setup/).

## Why

Reviewing an AI agent's changes usually means either reading a raw `git
diff` with no semantic information, or opening a full IDE for a change
that's often small and mechanical. katamari sits in between. See
[Why](https://isamisushi.github.io/katamari/#why) for the
full case.

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
[Configuration chapter](https://isamisushi.github.io/katamari/configuration/).

## Development

`mise run test` runs the unit suite plus a PTY-driven e2e suite; see the
[Development chapter](https://isamisushi.github.io/katamari/development/)
for the full picture.

## License

MIT, see [LICENSE](LICENSE).
