---
title: Health check
description: "`ktmr doctor`'s checkhealth-style report for diagnosing VCS, config, and LSP issues."
---

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
  doctor` prints (see [Language servers](/katamari/language-servers/)), folded in
  as checks: where each of the six built-in languages' server resolves
  from today, plus every `[lsp.servers.<id>]` custom entry.
- **agents** — which agent CLIs (`claude`, `codex`) are on `PATH` for
  [review units](/katamari/review-units/)' grouping and, when more than one is,
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
