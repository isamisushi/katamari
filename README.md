# katamari

A terminal diff-review tool. `ktmr diff` shows a `git diff` with syntax
highlighting, hover/go-to-definition/find-references/diagnostics from real
language servers, and inline review comments an AI coding agent can read
back and address — and `u` regroups a big tangled diff into ordered,
stacked-PR-like review units, without creating a single branch. All of it
without leaving the terminal.

![demo](docs/demo.gif)

- **Semantic review units** — `u` groups the diff's hunks into ordered,
  stacked-PR-like units (the refactor first, then the feature built on it,
  tests with the code they cover), each reviewable as its own scoped
  diff — derived and read-only, through the `claude`/`codex` CLI you
  already have, no branches created, nothing written outside `.katamari/`
  (see [Review units](#review-units))
- **LSP inside the diff** — hover, go-to-definition, find-references, and
  live diagnostics on changed lines (Rust / TypeScript / Python / Go /
  Kotlin / Java; servers auto-install on first use; `[lsp.servers.<id>]`
  wires up a custom server for any other filetype)
- **Live refresh** — working-tree diffs refresh as an agent's edits land on
  disk by default; pass `--no-watch` for a static session
- **Any scope of change** — working tree, staged, one commit, a range, a jj
  change or revset; browse and pick from `ktmr log`, or switch mid-session
  with a popup (`o`)
- **jj snapshot timeline** — step through every save of an agent's session,
  not just the final state
- **Comment round-trip** — leave comments in the TUI, on one line or a `V`
  visual range, and an agent reads and resolves them via `ktmr comments`
- **Mouse, when you want it** — wheel scrolls the pane under the pointer,
  clicks drive the file tree and run go-to-definition, right-click opens a
  context menu, resting the pointer hovers — all optional, keyboard-first
  as ever
- **Find your way around** — `/` searches the diff vim-style (`n`/`N`),
  `zo` unfolds the unchanged context git omitted around hunks, and `?`
  opens a filterable help window showing every live binding
- **`ktmr doctor`** — a checkhealth-style report answering "is the LSP
  actually working in my environment," down to a live spawn-and-hover
  probe of every server
- **Your keybindings** — vim (default) and emacs presets, every action
  remappable; monorepo-aware workspace roots

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
reads and updates through `ktmr comments`, and the working-tree diff refreshes
live as the agent's edits land on disk — no restart, no re-running a command
by hand. Pass `ktmr diff --no-watch` when you want a static working-tree
snapshot instead.

That workflow has a second problem besides missing semantics: an agent
tends to land its whole task as one tangled diff, with no commit structure
worth reading — the refactor, the feature it enabled, the tests, and a
lockfile bump interleaved across files in alphabetical order. The
established fix, stacked PRs, buys back readability by materializing real
branches and PRs before anyone has read the change. katamari's
[review units](#review-units) recover the same "read it in dependency
order, one concern at a time" property as a derived, read-only view over
the diff you already have — grouped through your own `claude`/`codex` CLI,
with nothing created and nothing rewritten.

For repositories using [jj](https://github.com/jj-vcs/jj) colocated with
git, katamari also keeps a timeline of jj's automatic working-copy
snapshots, so you can step back through every version of the working tree
an agent's session passed through, not just the version currently on disk.

## Install

Every method installs two identical binaries, `katamari` and `ktmr` (the
short name is what the rest of this document uses). There are no distro
packages (apt/dnf/pacman) yet — the channels below are the complete list.

### Homebrew (macOS and Linux)

```
brew install isamisushi/tap/katamari
```

### Install script (macOS and Linux)

Detects your platform — on Linux, choosing between the glibc and static
musl builds — and installs without needing a Rust toolchain:

```
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/isamisushi/katamari/releases/latest/download/katamari-installer.sh | sh
```

### Prebuilt binaries (macOS and Linux)

Each [release](https://github.com/isamisushi/katamari/releases) ships a
`tar.xz` per target, each with a matching `.sha256` alongside it:

| OS    | CPU                     | archive                                   |
| ----- | ----------------------- | ----------------------------------------- |
| macOS | Apple Silicon           | `katamari-aarch64-apple-darwin.tar.xz`    |
| macOS | Intel                   | `katamari-x86_64-apple-darwin.tar.xz`     |
| Linux | x86_64 (glibc)          | `katamari-x86_64-unknown-linux-gnu.tar.xz` |
| Linux | aarch64 (glibc)         | `katamari-aarch64-unknown-linux-gnu.tar.xz` |
| Linux | x86_64 (musl, static)   | `katamari-x86_64-unknown-linux-musl.tar.xz` |
| Linux | aarch64 (musl, static)  | `katamari-aarch64-unknown-linux-musl.tar.xz` |

Download, extract, and put the binaries anywhere on `$PATH` — the same
three commands work on both OSes (shown for Linux x86_64; substitute your
archive name from the table):

```
curl -LO https://github.com/isamisushi/katamari/releases/latest/download/katamari-x86_64-unknown-linux-gnu.tar.xz
tar -xJf katamari-x86_64-unknown-linux-gnu.tar.xz
sudo install katamari-x86_64-unknown-linux-gnu/ktmr katamari-x86_64-unknown-linux-gnu/katamari /usr/local/bin/
```

The gnu archives need glibc 2.34 or newer — in distro terms, Ubuntu
22.04, Debian 12, RHEL/Rocky/Alma 9, or anything more recent (`ldd
--version` prints yours). On anything older, and on musl-based distros
like Alpine, use the musl archives: fully static, they run on any Linux.

### mise

If you already use [mise](https://mise.jdx.dev/), its `ubi` backend
installs `ktmr` straight from the same release archives, on any of the
targets above:

```
mise use -g "ubi:isamisushi/katamari[exe=ktmr]"
```

### From source

Any OS with a Rust toolchain (any recent stable) — and the route for
targets without prebuilt archives, like the BSDs:

```
git clone https://github.com/isamisushi/katamari.git
cd katamari
cargo install --path .
```

If this repository itself is managed with [mise](https://mise.jdx.dev/),
`mise.toml` already pins a Rust toolchain and defines `mise run
build`/`test`/`lint`/`fmt` tasks for working on katamari's own source;
none of that is required to build or run it.

### Windows

No prebuilt binaries, and katamari isn't tested natively on Windows — run
it inside [WSL](https://learn.microsoft.com/windows/wsl/) and follow the
Linux instructions there. (A native `cargo install` may compile, but parts
of the tool — `ktmr skill install`'s symlinks, LSP auto-install's
executable-bit handling — assume a Unix filesystem.)

## Quickstart

Run inside any git repository:

```
ktmr diff              # working tree vs HEAD, refreshed live by default
ktmr diff --no-watch   # working tree vs HEAD, without live refresh
ktmr diff --staged     # staged (index) changes vs HEAD
ktmr diff <rev>        # one commit's own changes
ktmr diff <a>..<b>     # a revision range
```

`--no-watch` is a working-tree-only opt-out; staged and pinned historical
diffs are already static. A diff opened through a *moving* revision —
`HEAD`, a branch name, or a jj revset like `@` — is the one exception: it
re-resolves live, so amending the commit it points at (`git commit
--amend`, `jj describe`/`squash`) updates the open diff in place, cursor
and scroll preserved. A diff opened by commit hash never changes.

In a colocated jj repository (see [jj colocated setup](#jj-colocated-setup)),
`ktmr diff` also takes jj revsets, matching `jj diff`'s own flags so jj
muscle memory transfers directly:

```
ktmr diff -r <revset>          # one change's own diff, e.g. -r @ or -r @-
ktmr diff --from <a> --to <b>  # diff between two revisions
ktmr diff --from <a>           # --to defaults to @, like `jj diff` itself
```

`-r`/`--from`/`--to` are mutually exclusive with each other, with the plain
git revision/range argument, and with `--staged`/`--no-watch` — a
revision diff shows historical content, not necessarily what's on disk right
now, so hover/go-to-definition/find-references are unavailable on it (the
status bar names the scope being shown, e.g. `r: <id>` or `<from>..<to>`, so
this is never ambiguous on screen).

`ktmr log` opens a browsable revision history instead of a single diff: jj
changes (including the working copy, `@`, as a real entry) in a colocated jj
repo, or `git log` commits — plus a synthetic "local changes" row while the
working tree is dirty — otherwise. `j`/`k` select, `Enter` opens the
selected revision's diff (the local-changes row opens the same interactive
working-tree diff a plain `ktmr diff` would), `v` starts a 2-point range
selection (a second `Enter` diffs between the two), `Esc` closes back to
whatever's underneath (`q` quits katamari entirely, same as everywhere
else). Also reachable mid-session from `ktmr diff` with `L`.

Inside the diff: `K` hovers the identifier under the cursor, `gd`/`gr` go
to its definition/references, `]d`/`[d` jump between diagnostics, and `I`
opens the live, read-only LSP Inspector. The diff and file views stay fully
interactive while a server resolves, installs, or initializes — cursor
movement, paging, search, comments, help, and quit are never gated on LSP.
Pressing hover/`gd`/`gr` before the relevant server is ready reports that
immediately in the status bar (e.g. "LSP: rust is starting; go to
definition is not ready yet", or the live install/error detail once one
exists) instead of queuing the request — nothing pops a hover or jumps the
view later on its own; press the key again once the status clears. The
inspector keeps one entry per
actual `(language, workspace root)` server, shows lifecycle state (running
means only that `initialize` completed; it does not mean project indexing is
ready), capabilities, progress tokens, process details, stderr, server log
messages, and request/navigation outcomes. `Tab`/`BackTab` cycle the
explicit `Servers`, read-only `Server detail`, and `Journal` focus targets;
the focused pane is marked by a cyan/bold border and title styling. The
Inspector frame shows `Tab`/`BackTab` focus and `I`/`Esc`/`q` close hints;
each pane's bottom border lists only the controls for that pane. In
`Servers`, `j`/`k` and half-page/top/bottom keys select a server. In
`Server detail`, `j`/`k`, half-page, `gg`, and `G` scroll long read-only
details. In `Journal`, `j`/`k`, `Ctrl-u`/`Ctrl-d`, `gg`, and `G` move the
journal cursor/scroll; its border prioritizes `V select` and `y yank` even on
narrow panes. Press `V` to start linewise visual selection, extend it with `j`/`k`, and press `y` to send
the selected records through terminal-native OSC 52; whether the terminal or
multiplexer accepts that sequence depends on its clipboard support/configuration.
`Esc` cancels selection
before closing the inspector. Wrapped display rows belonging to one record
copy that complete record once, so a partial wrapped-row selection never
truncates or duplicates a log message. Oversized selections are refused with
an in-inspector status message. `q`/`I` close it. Language
servers spawn lazily, the first time something asks, and auto-install
themselves if missing — see [Language servers](#language-servers) if a
feature reports a server as unavailable.

Each TUI session also writes one combined, privacy-filtered journal under
`$XDG_STATE_HOME/katamari/lsp/` (or `~/.local/state/katamari/lsp/`). Journal
directories are unique per process; `events-0001.log` segments rotate at
5 MiB, retain at most four segments per session, and inactive sessions are
retained for 2 days within a 100 MiB global budget. Logs contain server
identity/generation, stderr lines and LSP log/show/trace text (each captured
message is bounded at 16 KiB and receives a UTF-8-safe truncation marker when
needed), and method/outcome metadata, but not source lines, document contents,
hover bodies, diagnostics bodies, initialization options, environment
variables, or raw protocol JSON. Active progress is bounded to 64 tokens; the
inspector shows the session directory so the physical segments are
discoverable. Configure this under `[lsp.logging]` with `enabled`,
`segment_bytes`, `segments_per_session`, `total_bytes`, and `max_age_days`.

Wherever git omits unchanged context — between two hunks, before the first
one, or after the last — a dim fold row shows `··· N unchanged lines ···`
(vimdiff's own convention). `zo` with the cursor on it expands the whole gap
in place as ordinary lines, `zc` folds it back; comments, hover, and
diagnostics work on the unfolded lines exactly like any other row.
Expanding reads the file straight off disk, so it's only available on a
diff whose new side is the live working tree (a plain `ktmr diff`) — fold
rows still show everywhere else (staged, a revision diff, `ktmr log`), `zo`
there just explains why it can't expand.

Leave a review comment on the current line with `c`, then `C-s` to save. With
a visual selection active (`V`, above), `c` instead comments the whole
selection as one range, as long as it resolves to a contiguous run of
added/context lines in a single file — the title and status bar read
`path:start-end`; anything else (a deletion, more than one file, a gap)
is refused with the specific reason, leaving the selection in place so you
can adjust it and try again. An AI coding agent addresses either shape with:

```
ktmr comments list --json           # or: ktmr comments export --format=md
ktmr comments list --json --status all   # resolved ones too — list/export both take --status open|resolved|all (default: open)
ktmr comments resolve <id>          # after addressing one (reopen <id> undoes it)
ktmr comments add <file> <line> <body>                    # leave a comment from a script/agent
ktmr comments add <file> <start> <body> --end-line <end>  # ...or on a line range
```

...making the requested changes and resolving each comment it handles —
resolutions show up live in an open `ktmr diff` session. `ktmr skill install`
writes that exact loop into the current repository as a small, tool-agnostic
harness, so an agent picks it up automatically rather than needing the
workflow spelled out in every prompt. Three pieces, none of them ever
overwriting content that isn't theirs:

- **The skill itself**, at `.agents/skills/katamari-review/` — deliberately
  not under `.claude/`, so any agent harness that adopts the same `.agents/`
  convention picks it up for free — with `.claude/skills/katamari-review`
  kept as a relative symlink into it, so Claude Code finds it exactly where
  it already looks.
- **`AGENTS.md`**: a katamari section wrapped in `<!-- katamari:begin -->` /
  `<!-- katamari:end -->` markers. Created fresh if the file doesn't exist;
  appended after whatever's already there if it does (nothing outside the
  markers is ever touched); refreshed in place if a newer katamari version
  changed the wording; a no-op if it's already current.
- **`CLAUDE.md`**: a relative symlink to `AGENTS.md`, created only if
  `CLAUDE.md` doesn't already exist. If it exists as a real file, or a
  symlink pointing somewhere else, it's left completely alone — that's
  assumed to be deliberate, and `ktmr skill install` prints a warning
  instead of guessing.

Re-running the command is always safe — every piece re-checks rather than
blindly rewriting: `SKILL.md` refreshes if a newer katamari version shipped
changes, a pre-existing real `.claude/skills/katamari-review` directory from
before this layout existed is migrated (backing up whatever was there rather
than discarding it), and either of the two links
(`.claude/skills/katamari-review`, `CLAUDE.md`) that already points
somewhere else is left alone with a warning.

The first time you save a comment (`C-s`) in a repository that doesn't have
the full harness installed yet, `ktmr diff` offers to install it right
there: the status bar shows `comment: saved · press y to install the Claude
Code review skill (ktmr skill install)`. `y` installs it and reports each
piece's outcome in the status bar; any other key dismisses the offer — and,
since it wasn't `y`, is then handled normally (a `j` right after dismissing
still moves the cursor, for instance). Offered at most once per session
either way — but a repo that only has *some* of the three pieces (e.g. one
that ran an older katamari's `ktmr skill install`, from before `AGENTS.md`/
`CLAUDE.md` were part of the harness) is still offered the rest once, since
installing is always idempotent. Set `[skill] offer_install = false` in
config (see below) to turn the offer off entirely; `ktmr skill install`
keeps working as an explicit command regardless.

`ktmr open <file>` opens a single file read-only, with the same
highlighting/hover/go-to-definition as the diff view — useful for reading
code a diff jumped you into without leaving katamari.

If the repository has a colocated jj repo (see
[jj colocated setup](#jj-colocated-setup)), `t` from `ktmr diff` opens the
snapshot timeline, or jump there directly with `ktmr timeline`.

`o` from `ktmr diff` opens the scope-picker popup — switch what a live
session is reviewing without restarting `ktmr diff` with new CLI flags.
`j`/`k` select, `Enter` confirms, `Esc` closes:

- **Working tree** / **Staged** swap the current diff in place (cursor
  resets to the top; anchor restoration across two unrelated diffs
  wouldn't mean anything).
- **Log** / **Timeline (jj)** (the latter only offered in a colocated jj
  repo) open the same views `L`/`t` do — listed here too, purely for
  discoverability.
- **Revision…** opens a one-line input: a git rev or `A..B`/`A...B` range
  in a git-only repo, or a jj revset in a colocated one — passed straight
  through to `jj diff -r <input>`, so jj's own operators (`a..b`, `@-`, a
  bookmark name, ...) work exactly as they do on the command line. An
  invalid revision reports the VCS's own error in the status bar and
  leaves whatever was on screen untouched.

Swapping to anything other than the working tree pauses live refresh's
refresh loop (the watcher itself keeps running) until you swap back;
`.katamari/comments.jsonl`'s own watcher is unaffected either way.

Finally, `u` groups the current diff into ordered review units — katamari's
stacked-PR-like reading order — and `Enter` on one scopes the whole session
to it. That's the next section.

## Review units

A one-shot agent session tends to land as one big tangled diff: a
refactor, the feature it enabled, its tests, and a lockfile bump,
interleaved across files in alphabetical order. Stacked-PR tooling solves
that readability problem by materializing real branches and PRs —
reshaping the repository before you've read the change. katamari recovers
the same reading order without touching the repo: press `u` in `ktmr
diff`, and the diff's hunks are grouped into ordered semantic units —
foundations and refactors first, then the code built on them, tests and
docs with the change they cover — each one reviewable as its own scoped
diff. The grouping is derived and read-only: no branches, no rebase,
nothing written outside `.katamari/`.

The grouping runs through an agent CLI you already have — `claude` or
`codex`, spawned headlessly (`claude -p --output-format json` /
`codex exec`) under the account it's already authenticated with. katamari
deliberately has no LLM client and no API key of its own. With neither CLI
installed, `u` says so in the status bar and everything else keeps
working; `ktmr doctor`'s **agents** section shows which CLIs it can see
and which one `u` would spawn.

What the model sees, and what it's allowed to decide, is deliberately
narrow:

- katamari does the deterministic part first: every hunk gets a stable
  content-hash ID, and lockfile/generated noise (`*.lock`,
  `package-lock.json`, `go.sum`, `*.pb.go`, `generated/` path segments,
  ...) is split off before the model ever sees the diff, into a trailing
  "Lockfiles & generated" unit — the part of the diff a reviewer most
  wants to skip past.
- The prompt carries each remaining hunk's file path, add/remove counts,
  hunk header, and its changed lines only — never unchanged context, never
  whole files, and nothing at all from the noise bucket. A huge diff
  degrades deterministically (fewer excerpt lines per hunk) to stay inside
  a fixed prompt budget.
- The model returns only a mapping — unit labels, one-line descriptions,
  an ordering, and which hunk IDs belong to each — never restated diff
  content. katamari then enforces the invariants the UI relies on: a
  hallucinated ID is dropped, an ID claimed twice keeps its first
  placement, and every hunk the model didn't place lands in a trailing
  "Ungrouped" unit — so the units always cover the whole diff, exactly
  once.

The units panel (`u`) lists the units in reading order, each with its
label, hunk count, and the files it touches; `j`/`k` select, and the
panel's bottom row shows the selected unit's one-line description. `Enter`
scopes the diff to that unit: the changed-file list and search narrow to
just its hunks, and a two-row banner plus the status bar pin
`unit 2/5: <label>` above the diff, so the scope is never ambiguous on
screen. `Esc` widens back to the full diff (that takes precedence over its
search-highlight-clear role), `u` reopens the panel with the current unit
preselected — stepping unit-by-unit is two keys — and `U` discards the
cached grouping and asks the agent afresh. Live refresh keeps working
while scoped: units re-anchor to the refreshed diff by content, not line
numbers, and a unit whose hunks were all rewritten away widens back to the
full diff rather than stranding you on an empty view. Switching scope
(working tree ↔ staged ↔ a revision) drops the unit filter — a grouping
describes one diff, not the new one.

Generation runs in the background: the status bar shows `units: asking the
agent CLI …` and the whole TUI stays interactive — expect seconds to
minutes depending on the model, with a hard 3-minute cap, and a result
that arrives after the diff has already changed is discarded with a status
note rather than applied to the wrong diff. Results are cached in
`.katamari/groups.jsonl` (covered by `.katamari/`'s own generated
`.gitignore`), keyed by the hunks' content: reopening the same diff is
instant and never prompts, while any edit to any changed line produces a
new key, so a stale grouping is never shown against a diff it doesn't
describe.

The first `u` that would actually spawn a CLI — no `[units]` config
anywhere, no cached grouping — opens a one-time three-step picker instead:
which CLI, which model, which reasoning effort. The choice is appended as
a `[units]` block to `~/.config/katamari/config.toml` (append-only —
anything hand-written in the file is preserved) and never asked again;
`Esc` abandons it without saving or spawning anything. See `[units]` under
[Config](#config) for the keys and their exact CLI-flag mapping, and
`ktmr reset --units-config` ([Reset](#reset)) to remove the block and get
the picker back.

## Keybindings

Vim bindings are the default; set `keymap = "emacs"` in config (see
below) for the emacs column. `q` quits katamari from anywhere — a pushed
`FileView`/timeline/log/inspector included, never "back" to whatever's
underneath. `Esc` is the generic "get me out of this": it dismisses the
nearest open overlay (a popup, the hover card, the references panel), and
with nothing local left open it pops exactly the one view a `gd`/`L`/`t`/`I`
press pushed, revealing what was underneath — at the root diff, where
there's nothing left to pop, it cancels an active visual selection first,
then widens an active unit scope back to the full diff, then clears a
confirmed search.
`Ctrl-o`/`Ctrl-i` are a separate axis entirely: they retrace *chronological*
cursor history — every significant jump (go to definition/references, a
confirmed search, a diagnostic step, and later a file-tree or mouse jump),
regardless of which feature caused it — not view stacking, so they keep
working exactly the same whether or not `Esc` has popped anything in
between.

The hint bar along the bottom starts collapsed to a handful of essentials
ending in `. more`; `.` expands it to the full list (and back), so the
table below never has to live in your head.

| Action | Vim | Emacs |
| --- | --- | --- |
| Cursor down / up | `j` / `k` | `C-n` / `C-p` |
| Half page down / up | `C-d` / `C-u` | `C-v` / `M-v` |
| Top / bottom | `gg` / `G` | `M-<` / `M->` |
| Next / prev hunk | `]c` / `[c` | `M-n` / `M-p` |
| Expand / collapse fold | `zo` / `zc` | `zo` / `zc` |
| Next / prev file | `]f` / `[f` | `C-x n` / `C-x p` |
| Next / prev diagnostic | `]d` / `[d` | `M-g M-n` / `M-g M-p` |
| Hover | `K` | `C-h` |
| Go to definition | `gd` | `M-.` |
| Find references | `gr` | `M-?` |
| Jump back / forward | `C-o` / `C-i`\* (also `M-Left`/`M-Right`) | `C-o` / `C-i`\* (also `M-Left`/`M-Right`) |
| Search diff / next / prev match | `/` / `n` / `N` | `/` / `n` / `N` |
| Focus next / prev pane | `Tab` / `BackTab` | `Tab` / `BackTab` |
| Next / prev symbol on line | `l` / `h` | `M-f` / `M-b` |
| Confirm / cancel | `Enter` / `Esc` | `Enter` / `Esc` |
| Toggle sidebar | `b` | `b` |
| Toggle directory (files pane) | `Space` | `Space` |
| Toggle unified/side-by-side | `s` | `s` |
| Toggle timeline | `t` | `t` |
| Toggle log view | `L` | `L` |
| Toggle LSP inspector | `I` | `I` |
| Open scope menu | `o` | `o` |
| Toggle units panel | `u` | `u` |
| Regenerate units | `U` | `U` |
| Open help | `?` | `?` |
| Toggle hint bar | `.` | `.` |
| Toggle range-select (timeline/log) | `v` | `C-Space` |
| Visual-line select (diff) | `V` | `V` |
| Yank visual selection (diff) | `y` | `y` |
| Add comment | `c` | `C-c C-c` |
| (in comment compose) newline / save / cancel | `Enter` / `C-s` / `Esc` | same |
| Toggle inline comment bodies | `C` | `C` |
| Quit | `q` | `q` |

\* Jump-forward matches neovim: it's `C-i` in terminals that implement the
[kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
(Ghostty, kitty, WezTerm, iTerm2 3.5+, Alacritty), which lets katamari tell a
literal Tab keypress apart from `Ctrl-i` on the wire — without it they arrive
as the same byte, and Tab already means focus-next-pane, so katamari
uses `M-Right` as jump-forward's canonical binding there instead (notably
Terminal.app, which doesn't implement the protocol). Detected once at
startup. `M-Left`/`M-Right` are unconditional aliases for back/forward in
both cases — the always-available, terminal-agnostic pair — while `C-i`
itself is simply left unbound when the terminal can't distinguish it from
Tab. `Ctrl-]` and `Ctrl-t` have no default binding at all: katamari has one
general jump history rather than a separate vim-style tag stack, so there's
no second "go back" key to bind.

### Mouse

Wheel scrolling works out of the box (`[ui] mouse = true`, the default):
scrolling over the files pane, the diff pane, a pushed file/timeline/log/
inspector view, or an open hover/references/help overlay scrolls whichever
one is under the pointer, without moving the keyboard cursor or changing
which pane has keyboard focus. A click in the changed-files tree selects a
row, jumps the diff to it, and expands/collapses a directory, the same way
`Enter` does from the keyboard. A click in the diff or a pushed file pane
moves the cursor to the clicked row/column; clicking an identifier on an
interactive new-side/context/add row runs go-to-definition (the same
readiness-gated action `gd` does — a click while the server is still
starting shows the same "not ready" status rather than queuing a surprise
later jump), while a gutter, whitespace, a deletion, a side-by-side old
cell, or a non-interactive historical diff only positions the cursor.
Shift-click extends an active visual selection (`V`) instead, without
triggering go-to-definition. Right-click opens a context-aware action menu
(hover/go to definition/find references on an identifier, expand/collapse
on a tree row, add-comment on a diff row, and more depending on what's
under the pointer); its entries follow the same LSP-readiness rules as the
keyboard. Drag selection and double-click word selection are not
implemented yet. While capture is on, a terminal's plain click-and-drag
text selection goes to katamari instead of the terminal — most terminals
still offer native selection by holding Shift while dragging. Set
`[ui] mouse = false` to leave capture off entirely and get plain, unshifted
drag selection back; katamari makes no attempt to emulate selection itself.
Inside tmux, `set -g mouse on` is additionally required for wheel events to
reach katamari at all, regardless of `[ui] mouse`.

Resting the pointer (no click, no button held) on an eligible code symbol
or a changed-file tree row for about 400ms shows details without moving the
keyboard cursor: a code symbol gets the same hover popup `K` would show
(subject to the same LSP-readiness gating — a not-yet-ready server
just stays quiet rather than queuing a surprise popup later), and a tree
row gets a compact status-bar line with its full path, status, `+/-`
stats, or old → new path for a rename, plus a changed-descendant count for
a directory. Moving to another target, leaving the pane, pressing a key,
clicking, scrolling, resizing, or opening any overlay cancels it instantly.
Controlled independently of click/wheel/right-click support via
`[ui] mouse_hover = true` (the default) — `false` stops katamari from
*acting* on pointer motion, not from the terminal *reporting* it:
`EnableMouseCapture` already requests any-motion reporting whenever
`[ui] mouse` is on, regardless of `mouse_hover`.

With a visual selection active (`V`, above), `y` copies it to the terminal
clipboard via OSC 52: each selected line's repo-relative path, old/new line
numbers, and diff marker (` `/`+`/`-`), grouped by file in selection order —
a path re-entered after the selection has moved on gets its header repeated
rather than merged into the earlier group. Structural rows (file/hunk
headers, fold rows) inside the selection are skipped silently. An empty
result or a payload over the 64 KiB pre-encoding bound (the same limit the
LSP inspector's own `V`/`y` uses) is refused with a status message that
leaves the selection in place to trim and retry; a successful copy clears it,
same as pressing `V` again.

`?` from any view opens a floating help window listing every command,
grouped, with its actual key next to it — the bindings shown are live
(preset plus any `[keys]` override below), never a hardcoded reference the
table above could drift out of sync with. `j`/`k`/arrows/`C-n`/`C-p` (and
`PageDown`/`PageUp`/`C-d`/`C-u`, and `gg`/`G` for top/bottom) scroll; `/`
starts a filter that narrows the list live as you type, matching against
each row's description, its config name, and its key; `Enter` keeps the
filter and returns to scrolling. `Esc` while typing the filter clears it
and returns to scrolling; `Esc` while scrolling the list closes the window
outright, same as `q`/`?`, whether or not a filter is still narrowing it.
The window is modal while open — every key goes to it, not whatever view
is underneath.

### Search

`/` in the diff view (not the help window's own filter above) opens a
search prompt on the status bar. Typing narrows incrementally: every match
across every file highlights live, and the cursor jumps to the first match
at or after where you started typing as the query narrows further. `Enter`
confirms — the matches and highlight stay, the prompt closes — and `n`/`N`
then jump to the next/previous match, wrapping around with a "search
wrapped" note. `Esc` while typing cancels, restoring the cursor and scroll
position from before you pressed `/`; `Esc` in the diff view afterward (not
the prompt) clears an already-confirmed search's highlight, vim's `:noh`. A
query matching nowhere shows `no matches: <query>` and returns the cursor to
where `/` was pressed.

Matching is literal substring (no regex), smartcase like vim's own
`'smartcase'`: an all-lowercase query matches either case, but typing even
one uppercase letter makes it case-sensitive. Match granularity is
per-occurrence — a row with three hits is three `n` stops — across every
file in the diff, in file → hunk → line order. Only visible content is
searched: a fold row's hidden, unchanged context (git's own omitted context
between hunks — see `zo`/`zc` above) isn't matched until you unfold it,
at which point a confirmed search's matches recompute automatically over
the newly revealed rows.

Any binding can be overridden per action; see `[keys]` below.

Pass `--show-keys` to `diff`/`open`/`log`/`timeline` (or set `[ui]
show_keys = true` to leave it on by default) to show a small overlay chip
in the content area's bottom-right corner with the most recent key(s)
pressed, in the same notation as the table above (`gd`, `K`, `]d`) —
useful for recordings and pair-review demos so a viewer can see what's
being pressed. A multi-key sequence builds up as you type it (`g` then
`gd`), repeated identical presses collapse (`j ×3`), the chip clears after
~1.5s of inactivity, and while typing into the comment-compose or
revision-input overlays it shows a generic `[typing…]` placeholder instead
of echoing your text.

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

[lsp.logging]
# TUI-only combined session journal; headless doctor/lsp-check do not create one.
enabled = true
segment_bytes = 5242880
segments_per_session = 4
total_bytes = 104857600
max_age_days = 2

[lsp.servers.rust]
# Overrides how a language's server is resolved — highest priority, above
# PATH and every project-local lookup. `<id>` is rust/typescript/python/
# go/kotlin/java for a built-in override, or any other id of your own
# choosing to define a custom server for a filetype katamari has no
# built-in support for — see "Custom language servers" below.
command = "/opt/homebrew/bin/rust-analyzer"
args = []
# extensions/root_markers/language_id/initialization_options — all
# optional, all snake_case, normally only set on a *custom* id (a built-in
# override like this `rust` entry needs none of them). See "Custom
# language servers" below for what each does and a worked example. A whole
# [lsp.servers.<id>] entry replaces wholesale on merge (the repo file's
# entry for one id fully replaces the home file's entry for that same id,
# not a field-by-field merge) — sibling ids are untouched either way. A
# custom id can never claim a file extension one of the six built-in
# languages already owns.

[ui]
# Terminal columns a tab expands to; ColumnMap (cursor/LSP positions) and
# rendering always agree on this value. Default 4.
tab_width = 4
# Above this many changed lines, a diff file's syntax highlighting is
# skipped in favor of plain styling (and it's excluded from LSP warm-up) —
# lockfiles (*.lock, package-lock.json, *.min.js) always skip regardless
# of size. Default 5000.
highlight_max_lines = 5000
# Soft-wrap a content line wider than its pane onto continuation rows
# (marked with ↪) instead of truncating it at the pane edge. Default true;
# set false to restore truncation, e.g. if you rely on it for
# alignment-heavy code.
wrap = true
# Shows the key-display overlay chip described under "Keybindings" above
# by default, without needing `--show-keys` on every invocation. Default
# false.
show_keys = false
# Enables mouse capture (wheel scrolling — see "Mouse" under "Keybindings"
# above). Default true; set false to leave the terminal's own
# click-and-drag text selection working instead.
mouse = true
# Resting the pointer on an eligible code symbol or changed-file tree row
# shows details after ~400ms, independently of click/wheel/right-click
# support above — see "Mouse" under "Keybindings". Default true; only ever
# has anything to act on while `mouse` (above) is also true.
mouse_hover = true

[watch]
# Debounce window for live refresh: how long a burst of filesystem changes
# must go quiet before the diff refreshes. Milliseconds, default 200.
debounce_ms = 200

[update]
# Whether ktmr looks for a newer release: a once-a-day background check
# (never on the critical path — it can't slow a session down or block on a
# flaky network) against GitHub, plus the notices sourced from its cached
# result — a status-bar note at startup and a one-line "vX.Y.Z is
# available — <upgrade command>" hint on stderr when you quit, printed only
# if stderr is a real terminal. Default true; false disables both the
# request and the notices entirely.
check = true

[skill]
# Whether saving your first comment in a repo without the full katamari
# review harness installed (skill + AGENTS.md + CLAUDE.md) offers to
# install it (see "Quickstart" above). Default true; false turns off the
# offer entirely. `ktmr skill install` always keeps working as an explicit
# command either way.
offer_install = true

[units]
# Review units (see "Review units" above): which agent CLI `u` spawns to
# group the diff, and what model/effort it runs with. All optional — with
# no [units] table in either config file, the first `u` that would spawn a
# CLI opens a one-time picker that writes this section for you.
# "claude" or "codex". Unset: whichever is installed, claude first — and a
# preference that isn't installed falls back to that same detection order
# rather than failing.
agent = "claude"
# Passed verbatim to `claude --model` / `--effort`. claude_model defaults
# to "sonnet" even when the key is absent; an explicit "" drops the
# --model flag entirely, deferring to the CLI's own default. An unset
# effort adds no flag.
claude_model = "sonnet"
claude_effort = "high"
# The codex equivalents map to `codex exec --model` and
# `-c model_reasoning_effort=`; unset means no flag, leaving the CLI's own
# configuration in charge.
# codex_model = "gpt-5-codex"
# codex_effort = "medium"
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
| Kotlin | `kotlin-lsp` (JetBrains) | prebuilt archive from JetBrains' CDN, bundling its own JVM — no external Java needed |
| Java | `jdtls` (Eclipse JDT LS) | prebuilt tarball from download.eclipse.org — needs an external JDK 21+ (katamari won't install a JVM) |

kotlin-lsp is JetBrains' own server (the `fwcd/kotlin-language-server`
community project is unmaintained as of this writing) and is still alpha
quality: on a project with no Gradle wrapper yet cached, its first hover,
go-to-definition, or diagnostics pull can take tens of seconds while it
resolves the classpath and indexes the project in the background —
katamari's `ktmr lsp-check` retries handle this the same way they do
rust-analyzer's own cold-start indexing. kotlin-lsp only implements
LSP 3.17's pull model (`textDocument/diagnostic`), never the unsolicited
`publishDiagnostics` push notifications every other server here sends;
katamari's gutter/`]d`/`[d`/`--diagnostics` flow now pulls on Kotlin's
behalf after every open/change and re-publishes the answer through the same
path a push would use, so error/warning highlighting works for Kotlin files
too — the only user-visible difference from a push server is that the very
first diagnostics for a freshly-opened file can lag behind hover/go-to
readiness while indexing finishes, since an early pull during that window
can legitimately come back empty. One more side effect worth knowing about:
importing a Gradle project spawns a Gradle daemon, which — by Gradle's own
design — keeps running after kotlin-lsp itself exits (including after a
`ktmr doctor` probe). That's normal daemon reuse, not a leak of katamari's;
`gradle --stop` (or killing the `GradleDaemon` process) reclaims it.

jdtls needs a JDK 21+ on the machine — `JAVA_HOME` is honored first, then
`PATH`, then `mise which java` — katamari installs the server itself but
never a JVM to run it on; `ktmr lsp doctor` prints a `jdk:` note under the
Java row naming the JDK it found and its version, saying so if the one it
found is too old, or reporting `not found` if there's no JDK anywhere.
First-open indexing can take a while on a large Maven/Gradle repo, during
which hover/go-to-definition may time out until the import finishes. Its
per-workspace index lives under
`$XDG_STATE_HOME/katamari/jdtls-workspaces/` (`~/.local/state/katamari/…`
if unset) — separate from the managed-server install below, and likewise
safe to delete to force a reindex.

Managed installs live under `~/.local/share/katamari/servers/`
(`$XDG_DATA_HOME/katamari/servers/` if set), one version-stamped
subdirectory per server — never touched by anything but katamari, and safe
to delete entirely (everything reinstalls on demand).

`[lsp.servers.<id>]` in config (above) overrides any of these with an
explicit command, taking priority over every lookup including auto-install.
To disable auto-install and just get the old "here's the install command"
status-bar hint instead:

```toml
[lsp]
auto_install = false
```

### Custom language servers

`[lsp.servers.<id>]` isn't limited to overriding one of the six built-in
languages above — any `<id>` of your own choosing defines a server for a
filetype katamari has no built-in support for, as long as it claims at
least one file extension:

```toml
[lsp.servers.ruby]
command = "solargraph"
args = ["stdio"]
extensions = ["rb"]
root_markers = ["Gemfile"]
```

`extensions` is what makes `ruby` claim `.rb` files at all — a custom id
with no `extensions` is just an inert config entry (indistinguishable from
a plain built-in override with none of the new fields set). A leading `.`
is accepted and stripped (`extensions = [".rb"]` and `["rb"]` are
equivalent), and each entry is trimmed of surrounding whitespace. A custom
claim on an extension one of the six built-in languages already owns is
dropped, the built-in always winning; if two custom ids claim the same
extension, whichever id sorts first alphabetically wins instead; and
`extensions` set on an id that's itself one of the six built-in language
names (e.g. `[lsp.servers.rust]`) is ignored outright, since `<id>` is what
decides override-vs-custom — all three cases warn once to stderr, and
`ktmr lsp doctor`'s custom-server table flags an affected entry with a note
saying why its extensions don't route anywhere. `root_markers` finds this
id's workspace root the same way a built-in language's nearest-marker tier
does — the closest ancestor
directory containing one of the listed files, falling back to the
repository root if it's empty or nothing matches; there's no built-in-style
"workspace of workspaces" tier for a custom id; that logic is
adapter-specific knowledge (a Cargo `[workspace]` table, a `go.work`, a
Gradle `settings.gradle`) this module has no way to generalize to a server
it's never heard of. `language_id` overrides the LSP `languageId`
announced in `didOpen`, for the rare server that expects something other
than the `<id>` key itself.

A custom server is never auto-installed — `command` must already be
reachable on its own, and `ktmr lsp doctor` reports it (in a second table
beneath the built-in one) as found or not found the same way it does for a
built-in language; `ktmr lsp install` doesn't support a custom id.

`initialization_options` (TOML, converted to JSON at resolve time) works on
a built-in override too, not just a custom server — e.g. sending
rust-analyzer settings through `[lsp.servers.rust]`.

`ktmr lsp` manages installs directly, without waiting for a server to be
needed:

```
ktmr lsp doctor              # where each language's server resolves from today (no installs triggered) — see "Health check" below for the fuller report
ktmr lsp install <language>  # force an install into katamari's managed prefix (rust/typescript/python/go/kotlin/java/all)
ktmr lsp update              # reinstall any pinned server that's fallen behind the current pin
```

## Health check

`ktmr doctor` is a checkhealth-style report for when something isn't working
and you can't tell whether it's katamari, the language server, or just a slow
first index — the diagnostic surface issue #4 was filed over ("is the LSP
server running as expected in my env?"):

```
ktmr doctor                    # full report: vcs, config, lsp resolution, lsp live probe
ktmr doctor --no-live          # skip the live spawn-and-hover probe; static sections only
ktmr doctor --language rust    # limit the live probe to one language or a [lsp.servers.<id>] id
ktmr doctor --json             # machine-readable: {"sections": [{"title", "checks": [{"status", "label", "detail"}]}]}
```

Five sections, always in this order:

- **vcs** — is `git` on `PATH` (with its version), is the current directory
  actually inside a repository, and (only when one is detected) is it a
  colocated jj repo, with the `jj` binary's version. Absent jj is not a
  warning in a plain git repo.
- **config** — for each of the two config files (`~/.config/katamari/config.toml`,
  `<repo>/.katamari/config.toml`): missing (defaults apply), parsed clean, or
  every parse/unknown-key warning a normal session would otherwise only
  print to stderr.
- **lsp (resolution)** — the same static, offline information `ktmr lsp
  doctor` prints (above), folded in as checks: where each of the six
  built-in languages' server resolves from today, plus every
  `[lsp.servers.<id>]` custom entry.
- **agents** — which agent CLIs (`claude`, `codex`) are on `PATH` for
  [review units](#review-units)' grouping and, when more than one is,
  which one `u` would actually spawn — resolved through the same `[units]`
  preference the TUI uses, so the report can't drift from what `u` does.
  None found is a warning, not an error: grouping is optional, and
  everything else works without it.
- **lsp (live probe)** — the reason this command exists: for every built-in
  or custom language with at least one matching file in the repository
  (tracked or untracked-and-not-ignored) *and* a static resolution, actually
  spawns the real server (headless — no config/`--json`/TUI dependency) and
  reports `spawn+initialize` and `hover round-trip` as separate, timed
  checks — `ok "ready in 1.4s"`, or an actionable error naming what went
  wrong (including the server's own stderr, where available). Never
  installs anything, even with `[lsp] auto_install` on — a diagnostic
  doesn't mutate your environment. A language present in the repo whose
  server didn't resolve gets a `skipped` note instead of a probe attempt.
  Probes run one at a time, with a progress line per language on stderr, so
  a slow one (jdtls) doesn't look stuck.

Exit code is `0` unless at least one check is `error` (warnings alone still
exit `0`) — safe to wire into a script or CI step as a pass/fail gate.

Maintainers: `scripts/release-check.sh` (`mise run release-check` /
`release-check-full`) automates a `ktmr doctor` pass one step further —
building a release binary and running it against a throwaway multi-language
monorepo in an isolated sandbox, so a release is only cut once LSP
auto-install has actually been proven end to end, not just unit-tested. See
`AGENTS.md`'s "Release check" section for usage.

## Reset

`ktmr reset` returns katamari to a fresh-install state, selectively — and
with no flags it removes nothing at all: it prints an inventory of every
target, whether it currently exists, where it lives, and which flag would
remove it.

```
ktmr reset                  # report only: what exists where; removes nothing
ktmr reset --cache          # the repo's grouping cache (.katamari/groups.jsonl) + katamari's state dir — update-check cache, index caches, and the per-session LSP journals
ktmr reset --units-config   # strip the [units] table from both config files
ktmr reset --servers        # katamari-managed language-server installs (large downloads, hence not part of --cache)
ktmr reset --comments       # .katamari/comments.jsonl — review data, never implied by --all
ktmr reset --all            # cache + units-config + servers; comments always take the explicit flag
```

`--units-config` edits surgically: only the `[units]` table is removed
from each config file, everything else — comments and formatting
included — is preserved byte-for-byte, and a file left holding nothing but
whitespace afterward is deleted outright. Review comments are review data,
not cache, which is why `--all` deliberately never touches them. After any
run, a `.katamari/` directory left holding only its own generated
`.gitignore` is tidied away entirely — and outside a git repository, the
repo-scoped targets are skipped while the user-level ones (servers, state
dir, home config) still work.

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
katamari's timeline can show. During live refresh, katamari itself triggers a
snapshot (`jj util snapshot`) after each burst of edits, so an agent
session leaves one timeline entry per save even though the agent never
runs a jj command. Nothing about `ktmr diff` changes if jj
isn't set up — the timeline (`t`) simply reports it's unavailable. `ktmr
diff -r`/`--from`/`--to` and `ktmr log`'s jj-backed history need this same
colocated setup; `ktmr log` still works without it (falling back to plain
`git log`), just without jj's revsets or the working copy as a browsable
entry.

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
what proves the `C-o`/`M-Right` fallback is what a reviewer actually sees
there.

```
mise run e2e-tmux
```

Both are self-contained: they build their own throwaway git fixtures in a
tempdir and point `$HOME`/`$XDG_CONFIG_HOME`/`$XDG_DATA_HOME` at another
tempdir, so a test run never touches your real `~/.config/katamari` or the
katamari-managed language-server install prefix.
