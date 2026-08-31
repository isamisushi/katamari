# Introduction

A terminal diff-review tool. `ktmr diff` shows a `git diff` with syntax
highlighting, hover/go-to-definition/find-references/diagnostics from real
language servers, and inline review comments an AI coding agent can read
back and address — and `u` regroups a big tangled diff into ordered,
stacked-PR-like review units, without creating a single branch. All of it
without leaving the terminal.

![demo](assets/demo.gif)

- **Semantic review units** — `u` groups the diff's hunks into ordered,
  stacked-PR-like units (the refactor first, then the feature built on it,
  tests with the code they cover), each reviewable as its own scoped
  diff — derived and read-only, through the `claude`/`codex` CLI you
  already have, no branches created, nothing written outside `.katamari/`
  (see [Review units](./review-units.md))
- **LSP inside the diff** — hover, go-to-definition, find-references, and
  live diagnostics on changed lines (Rust / TypeScript / Python / Go /
  Kotlin / Java; servers auto-install on first use; `[lsp.servers.<id>]`
  wires up a custom server for any other filetype)
- **Live refresh** — working-tree diffs refresh as an agent's edits land on
  disk by default; pass `--no-watch` for a static session
- **Any scope of change** — working tree, staged, one commit, a range, a jj
  change or revset, or a GitHub pull request by number (`--pr`, through
  your own `gh`); browse and pick from `ktmr log`, or switch mid-session
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
[review units](./review-units.md) recover the same "read it in dependency
order, one concern at a time" property as a derived, read-only view over
the diff you already have — grouped through your own `claude`/`codex` CLI,
with nothing created and nothing rewritten.

For repositories using [jj](https://github.com/jj-vcs/jj) colocated with
git, katamari also keeps a timeline of jj's automatic working-copy
snapshots, so you can step back through every version of the working tree
an agent's session passed through, not just the version currently on disk.

