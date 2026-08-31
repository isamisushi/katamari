---
title: Configuration
description: The full TOML configuration reference, merged from built-in defaults, home config, and repo config.
---

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
# Action name (kebab-case; see the keybindings table in the Keybindings
# chapter for what each one does) -> key-sequence notation, overriding the
# preset above.
# Notation: "C-x" (Ctrl), "M-x" (Alt/Meta), "Space"/"Esc"/"Enter"/"Tab"/
# "BackTab"/"Backspace"/arrows/Home/End/PageUp/PageDown as named keys, a
# bare character otherwise, space-separated for a multi-key sequence.
quit = "Z Z"
next-hunk = "C-n"

[lsp]
# Whether a missing language server is silently downloaded/built into
# katamari's own prefix instead of just reporting an install hint. Default
# true; see the Language servers chapter for what each language's strategy
# is.
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
# built-in support for — see the Language servers chapter's "Custom
# language servers" section.
command = "/opt/homebrew/bin/rust-analyzer"
args = []
# extensions/root_markers/language_id/initialization_options — all
# optional, all snake_case, normally only set on a *custom* id (a built-in
# override like this `rust` entry needs none of them). See the Language
# servers chapter's "Custom language servers" section for what each does
# and a worked example. A whole
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
# Shows the key-display overlay chip described in the Keybindings chapter
# by default, without needing `--show-keys` on every invocation. Default
# false.
show_keys = false
# Enables mouse capture (wheel scrolling — see "Mouse" in the Keybindings
# chapter). Default true; set false to leave the terminal's own
# click-and-drag text selection working instead.
mouse = true
# Resting the pointer on an eligible code symbol or changed-file tree row
# shows details after ~400ms, independently of click/wheel/right-click
# support above — see "Mouse" in the Keybindings chapter. Default true;
# only ever has anything to act on while `mouse` (above) is also true.
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
# install it (see the Quickstart chapter). Default true; false turns off the
# offer entirely. `ktmr skill install` always keeps working as an explicit
# command either way.
offer_install = true

[units]
# Review units (see the Review units chapter): which agent CLI `u` spawns to
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
