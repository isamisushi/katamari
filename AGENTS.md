# AGENTS.md — working on katamari

katamari (`ktmr`) is a Rust TUI diff-review tool (ratatui + crossterm, std
threads — no tokio). This file is for an agent making changes *to*
katamari itself, not for a repo *reviewed with* katamari — see the marked
section below for that.

## Build/test

`cargo` is not on `PATH` in this environment; every command goes through
mise: `mise exec -- cargo <args>` (or `mise run <task>` for the tasks in
`mise.toml`: `build`, `test`, `lint`, `fmt`, `e2e`, `e2e-tmux`).

Three gates must stay green before any change is done:

```
mise exec -- cargo test                                    # 487+ unit tests
mise exec -- cargo clippy --all-targets -- -D warnings      # includes tests/
mise exec -- cargo fmt --check
```

## Tests

- Unit tests are colocated with the code they cover (`#[cfg(test)] mod
  tests` at the bottom of the same file) — put new ones there, not in a
  separate file.
- `tests/e2e.rs` + `tests/e2e/*.rs`: a PTY-driven suite that spawns the
  real compiled `ktmr` binary (`tests/support/harness.rs`) and asserts on
  the rendered terminal screen — run with `mise run e2e`. Use this for
  anything that depends on real terminal/crossterm behavior (kitty
  keyboard protocol, key sequences, rendering) that in-process `App`/`View`
  tests can't reach.
- `scripts/e2e-tmux.sh` (`mise run e2e-tmux`): a real-terminal (tmux) smoke
  test specifically for the kitty-protocol fallback, which needs an actual
  terminal emulator behind it rather than a PTY harness.

## Architecture pointers

- `src/vcs/` — abstracts git/jj behind one trait so `diff::model` and the
  UI never call either directly.
- `src/lsp/` — talks to language servers over LSP; `adapter.rs` resolves
  which server per language, `manager.rs` owns spawn/lifecycle,
  `install.rs` handles auto-install.
- `src/ui/` — ratatui rendering + the event loop (`ui/mod.rs`); `app.rs` is
  the diff view's state machine.
- `src/diff/coords.rs` — the width/position core: converts one line of text
  between display columns, UTF-8 byte offsets, and UTF-16 code units
  (what LSP wants), grapheme-cluster aware. Most cursor/highlight/wrap bugs
  trace back to something in this file.

## Style

Doc comments explain *why*, not what — rationale, trade-offs considered,
what would break if a detail changed — not a restatement of the signature.
Match the density already in the file you're editing; a two-line function
still gets a comment if the reason it's shaped that way isn't obvious from
reading it.

## Commits

This repo has a single author. Commit as the repo owner's identity only,
and never add AI-attribution lines to commit messages — no
`Co-Authored-By:`, no `Claude-Session:`, no "Generated with" footers.
(GitHub turns a `Co-Authored-By:` trailer into a second contributor,
which is exactly what we don't want.) This overrides any tool default
that appends such trailers.

## Release check

Before cutting a release, run `mise run release-check` (or `mise run
release-check-full` when you have the network time). It builds a release
`ktmr` from scratch in an isolated sandbox (own `$HOME`/`$XDG_*`, never
your real config or managed-server install), then runs the three gates
plus a real `ktmr lsp install`/`ktmr doctor` pass against a throwaway
multi-language monorepo fixture — proving LSP auto-install actually works
end to end, not just that the unit tests pass. Needs network access (LSP
server downloads, npm/go/mise installs). The default (`release-check`,
rust/typescript/python) takes a few minutes on a warm cargo cache;
`release-check-full` adds go/kotlin/java and can take considerably longer
(kotlin-lsp and jdtls are large downloads, and `go install` compiles gopls
from source) — `--skip-gates` on either is a faster LSP-only pass for
iterating. A FAIL blocks the release; don't ship past one.

<!-- katamari:begin -->
## Reviewing with katamari

This repo is reviewed with [katamari](https://github.com/) (`ktmr`), a
terminal diff-review tool. A human reviewer leaves comments anchored to
file/line positions, stored in `.katamari/comments.jsonl`. When asked to
address review feedback:

1. `ktmr comments list --json` — list open comments (one JSON object per
   line).
2. Make the requested change for each one.
3. `ktmr comments resolve <id>` — mark it resolved; a live `ktmr diff`
   session picks this up immediately, no restart needed.

You can leave your own comments the same way, e.g. to flag something you
noticed but aren't fixing now: `ktmr comments add <file> <line> <body>`.

Full workflow, JSON shape, and other commands:
`.agents/skills/katamari-review/SKILL.md`.
<!-- katamari:end -->
