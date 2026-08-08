# katamari

A terminal diff-review tool. `ktmr diff` shows a `git diff` with syntax
highlighting, hover/go-to-definition/find-references/diagnostics from real
language servers, and inline review comments an AI coding agent can read
back and address — all without leaving the terminal.

![demo](docs/demo.gif)

- **LSP inside the diff** — hover, go-to-definition, find-references, and
  live diagnostics on changed lines (Rust / TypeScript / Python / Go /
  Kotlin / Java; servers auto-install on first use)
- **Watch mode** — the diff refreshes as an agent's edits land on disk
- **Any unit of change** — working tree, staged, one commit, a range, a jj
  change or revset; browse and pick from `ktmr log`, or switch mid-session
  with a popup (`o`)
- **jj snapshot timeline** — step through every save of an agent's session,
  not just the final state
- **Comment round-trip** — leave comments in the TUI, an agent reads and
  resolves them via `ktmr comments`
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
reads and updates through `ktmr comments`, and `ktmr diff --watch`
refreshes the diff live as the agent's edits land on disk — no restart, no
re-running a command by hand.

For repositories using [jj](https://github.com/jj-vcs/jj) colocated with
git, katamari also keeps a timeline of jj's automatic working-copy
snapshots, so you can step back through every version of the working tree
an agent's session passed through, not just the version currently on disk.

## Install

### Homebrew

```
brew install isamisushi/tap/katamari
```

Installs two identical binaries, `katamari` and `ktmr` (the short name is
what the rest of this document uses). Prebuilt for macOS (Apple Silicon
and Intel) and Linux (x86_64 and aarch64); each [release](https://github.com/isamisushi/katamari/releases)
also has raw binary archives for those targets if you'd rather skip
Homebrew.

### From source

Needs a Rust toolchain (any recent stable):

```
git clone git@github.com:isamisushi/katamari.git
cd katamari
cargo install --path .
```

If this repository itself is managed with [mise](https://mise.jdx.dev/),
`mise.toml` already pins a Rust toolchain and defines `mise run
build`/`test`/`lint`/`fmt` tasks for working on katamari's own source;
none of that is required to build or run it.

## Quickstart

Run inside any git repository:

```
ktmr diff              # working tree vs HEAD
ktmr diff --staged     # staged (index) changes vs HEAD
ktmr diff <rev>        # one commit's own changes
ktmr diff <a>..<b>     # a revision range
ktmr diff --watch      # refresh automatically as files change on disk
```

In a colocated jj repository (see [jj colocated setup](#jj-colocated-setup)),
`ktmr diff` also takes jj revsets, matching `jj diff`'s own flags so jj
muscle memory transfers directly:

```
ktmr diff -r <revset>          # one change's own diff, e.g. -r @ or -r @-
ktmr diff --from <a> --to <b>  # diff between two revisions
ktmr diff --from <a>           # --to defaults to @, like `jj diff` itself
```

`-r`/`--from`/`--to` are mutually exclusive with each other, with the plain
git revision/range argument, and with `--staged`/`--watch` — a revision diff
shows historical content, not necessarily what's on disk right now, so
hover/go-to-definition/find-references are unavailable on it (the status bar
names the scope being shown, e.g. `r: <id>` or `<from>..<to>`, so this is
never ambiguous on screen).

`ktmr log` opens a browsable revision history instead of a single diff: jj
changes (including the working copy, `@`, as a real entry) in a colocated jj
repo, or `git log` commits — plus a synthetic "local changes" row while the
working tree is dirty — otherwise. `j`/`k` select, `Enter` opens the
selected revision's diff (the local-changes row opens the same interactive
working-tree diff a plain `ktmr diff` would), `v` starts a 2-point range
selection (a second `Enter` diffs between the two), `q`/`Esc` closes. Also
reachable mid-session from `ktmr diff` with `L`.

Inside the diff: `K` hovers the identifier under the cursor, `gd`/`gr` go
to its definition/references, `]d`/`[d` jump between diagnostics. Language
servers spawn lazily, the first time something asks, and auto-install
themselves if missing — see [Language servers](#language-servers) if a
feature reports a server as unavailable.

Wherever git omits unchanged context — between two hunks, before the first
one, or after the last — a dim fold row shows `··· N unchanged lines ···`
(vimdiff's own convention). `zo` with the cursor on it expands the whole gap
in place as ordinary lines, `zc` folds it back; comments, hover, and
diagnostics work on the unfolded lines exactly like any other row.
Expanding reads the file straight off disk, so it's only available on a
diff whose new side is the live working tree (a plain `ktmr diff`) — fold
rows still show everywhere else (staged, a revision diff, `ktmr log`), `zo`
there just explains why it can't expand.

Leave a review comment on the current line with `c`, then `C-s` to save.
An AI coding agent addresses them with:

```
ktmr comments list --json           # or: ktmr comments export --format=md
ktmr comments resolve <id>          # after addressing one (reopen <id> undoes it)
ktmr comments add <file> <line> <body>   # leave a comment from a script/agent
```

...making the requested changes and resolving each comment it handles —
resolutions show up live in an open `ktmr diff` session. `ktmr skill install`
writes that exact loop into the current repository as a small, tool-agnostic
harness, so an agent picks it up automatically rather than needing the
workflow spelled out in every prompt. Four pieces, none of them ever
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
than discarding it), and any of the three links (`.claude/skills/*`,
`CLAUDE.md`) that already points somewhere else is left alone with a
warning.

The first time you save a comment (`C-s`) in a repository that doesn't have
the full harness installed yet, `ktmr diff` offers to install it right
there: the status bar shows `comment: saved · press y to install the Claude
Code review skill (ktmr skill install)`. `y` installs it and reports each
piece's outcome in the status bar; any other key dismisses the offer — and,
since it wasn't `y`, is then handled normally (a `j` right after dismissing
still moves the cursor, for instance). Offered at most once per session
either way — but a repo that only has *some* of the four pieces (e.g. one
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

Swapping to anything other than the working tree pauses `--watch`'s
refresh loop (the watcher itself keeps running) until you swap back;
`.katamari/comments.jsonl`'s own watcher is unaffected either way.

## Keybindings

Vim bindings are the default; set `keymap = "emacs"` in config (see
below) for the emacs column. `q` quits either way.

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
| Jump back / forward | `C-o` / `C-i`\* | `C-o` / `C-i`\* |
| Next / prev symbol on line | `Tab` / `BackTab` | `Tab` / `BackTab` |
| Confirm / cancel | `Enter` / `Esc` | `Enter` / `Esc` |
| Toggle sidebar | `b` | `b` |
| Toggle unified/side-by-side | `s` | `s` |
| Toggle timeline | `t` | `t` |
| Toggle log view | `L` | `L` |
| Open scope menu | `o` | `o` |
| Open help | `?` | `?` |
| Toggle range-select (timeline/log) | `v` | `C-Space` |
| Add comment | `c` | `C-c C-c` |
| (in comment compose) newline / save / cancel | `Enter` / `C-s` / `Esc` | same |
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

[watch]
# Debounce window for `--watch`: how long a burst of filesystem changes
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
# install it (see "Comments" above). Default true; false turns off the
# offer entirely. `ktmr skill install` always keeps working as an explicit
# command either way.
offer_install = true
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
can legitimately come back empty.

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
ktmr lsp doctor              # where each language's server resolves from today (no installs triggered)
ktmr lsp install <language>  # force an install into katamari's managed prefix (rust/typescript/python/go/kotlin/java/all)
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
katamari's timeline can show. In watch mode, katamari itself triggers a
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
what proves the `C-o/C-t` fallback is what a reviewer actually sees there.

```
mise run e2e-tmux
```

Both are self-contained: they build their own throwaway git fixtures in a
tempdir and point `$HOME`/`$XDG_CONFIG_HOME`/`$XDG_DATA_HOME` at another
tempdir, so a test run never touches your real `~/.config/katamari` or the
katamari-managed language-server install prefix.
