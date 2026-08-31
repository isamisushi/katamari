---
title: Review units
description: How `u` groups a tangled diff into ordered, stacked-PR-like review units through your own agent CLI.
---

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
[Configuration](/katamari/configuration/) for the keys and their exact CLI-flag
mapping, and `ktmr reset --units-config` ([Reset](/katamari/reset/)) to remove
the block and get the picker back.
