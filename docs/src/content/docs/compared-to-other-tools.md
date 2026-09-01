---
title: Compared to other tools
description: An honest, dated comparison against diff pagers, git TUIs, terminal review peers, cloud AI reviewers, and stacked-PR platforms — and where katamari is the wrong tool.
---

A factual snapshot as of September 2026. Terminal review tools in particular
are moving fast; if you're reading this much later, re-verify the specific
claims below rather than trusting them at face value.

## Where katamari is ahead

Four things, as far as this survey found, no other tool in any of these
categories does at the same time:

|  | Diff pagers (delta, difftastic) | git TUIs (lazygit, gitui) | Terminal review peers (lumen, revdiff, tuicr, Hunk) | Cloud AI reviewers (CodeRabbit, Copilot code review) | Stacked-PR platforms (Graphite, GitHub Stacked PRs) | **katamari** |
|---|---|---|---|---|---|---|
| **LSP inside the diff** — hover, go-to-definition, find-references, live diagnostics on changed lines | No | No (delta's own pager coloring only) | No | No | No | **Yes** — 6 languages plus custom servers, auto-install, `ktmr doctor` |
| **LLM review units** — read-only regrouping of an existing diff into an ordered, stacked-PR-like reading order | No | No | Partial — lumen's "stacked" mode is raw commit order, not a model-computed grouping | No | N/A — real branch/commit surgery at authoring time, not a read-only regroup of a diff that already exists | **Yes** — read-only, cached by content hash, whole-diff coverage guaranteed |
| **Protocol-owned persistent agent session** — one agent process spawned and held for the whole review, streaming output, every edit human-gated | N/A | N/A | One-shot CLI invocation or pipe per hand-off, not a held session (closest: Hunk's skill can inspect a live session to reload/apply comments, but that's not a conversational session the reviewer directly drives) | N/A — the reviewer *is* the hosted model, not a session you converse with | N/A | **Yes** — an [ACP](https://agentclientprotocol.com) session spawned once and kept alive for as long as `ktmr diff` runs |
| **Reviewed-hunk state** — remembering what you've already reviewed, in a way that survives the diff changing under you | No | No | Partial — a 2026 wave of local seen-state trackers (e.g. seendiff) picked up the idea, but the ones surveyed here mark on scroll rather than an explicit action | No — nothing to remember across sessions when the review is one hosted pass | Partial — GitHub's own file-level "Viewed" checkbox (unchanged since 2019, clears on a force-push); Gerrit patchsets and Reviewable do real interdiff, but both are platform/SaaS-bound | **Yes** — hunk-granular, content-addressed, explicit-keypress-only, local |

**LSP inside the diff** is a subsystem, not a feature flag: spawn/lifecycle
management, per-language capability negotiation, an auto-install pipeline,
a health-check story. The terminal review tools are architected around
reading and exporting *text* diffs; wiring a real language-server session
into that is a different order of project. Cloud reviewers have an even
stronger reason not to bother — their product is "the AI reads the code so
you don't have to," and a live session for a *human's* interactive
navigation cuts against that pitch entirely.

**LLM review units** need a validation layer most annotate-and-export
tools have no reason to build: reconciling model output against the real
diff so hunks are never dropped, duplicated, or hallucinated into
nonexistent IDs. A broken regroup is worse than no regroup. For the
stacked-PR platforms, a read-only, throwaway regroup would undercut their
actual business, which is getting teams onto real stacked branches that
each get independently CI'd and merged.

**A protocol-owned agent session** means `a`/`A`/`p` all talk to the same
running process for the length of your review — ask a follow-up and it
remembers the earlier question, instead of every hand-off starting cold.
Peers that already added a comment-to-agent loop mostly ship it as a
one-shot invocation: a shell-escape into an agent prompt, a stdout pipe on
quit, a browser-storage export. None hold a live, protocol-driven
conversation with human-gated permissions the way katamari's [Ask the
agent](/katamari/keybindings/#ask-the-agent) does.

**Reviewed-hunk state** is the newest of the four, and the closest
comparisons are still moving. GitHub's own "Viewed" checkbox has been
file-granular since 2019 and still clears on a force-push — precisely the
case an agent's iterating session hits on every rewrite. Gerrit patchsets
and Reviewable both do real interdiff between revisions, but neither
exists outside its own platform. A 2026 wave of local, terminal-native
tools — seendiff among them — picked up seen-state tracking for git diffs
directly, which is real convergence worth naming; the ones surveyed here
mark a line seen as you scroll past it, though, which risks marking
something you scrolled by without reading. katamari's marks are
hunk-granular and keyed on the hunk's own content, so a rebase or reorder
doesn't lose them and an agent's rewrite of one hunk resurfaces only that
hunk — and they're set by an explicit `r`/`R`/`m` keypress only, never
inferred from scroll position, in a local `.katamari/reviewed.jsonl` with
no forge account involved.

Where katamari is *behind*: peers like lumen and tuicr can write real
comments and reviews back to a forge (tuicr to four platforms, via their
native CLIs); katamari's own PR support is deliberately read-only (see
below).

## When katamari is the wrong tool

- **You need to write reviews back to a forge.** `ktmr diff --pr` reads a
  pull request's diff through your own `gh` — there's no API client, no
  posting comments or approvals back, and no GitLab/Bitbucket/Azure DevOps
  support at all. If pushing review state back to the forge matters, use
  **tuicr** (writes real reviews to four platforms via their native CLIs)
  or the forge's own web UI.
- **You need real-time, multi-reviewer collaboration.**
  `.katamari/comments.jsonl` is a plain file with no locking and no
  accounts — "team" review means committing the file and letting git merge
  it. For actual concurrent multi-reviewer review, use **GitHub's** (or
  another forge's) native review flow.
- **You want an AI to review the PR for you.** katamari has no opinionated
  reviewer model of its own: review units reorder a diff you still read
  yourself, and the agent session only acts on what you ask it — it never
  generates review comments unprompted. If what you actually want is "an
  AI reads my PR and tells me what's wrong with it," that's **CodeRabbit**,
  **Copilot code review**, or **Cursor BugBot**, not katamari.
- **You want a full git porcelain.** No interactive rebase UI, no stash
  management, no merge-conflict resolution, no branch operations —
  katamari isn't trying to be that. **lazygit** or **gitui** is the right
  tool for doing all your git work in one TUI.
- **You want real stacked-PR merge mechanics.** Review units are
  organizational and disposable, never separately mergeable PRs. A team
  that wants actual small-PR-at-a-time merge mechanics — each unit its own
  branch, its own CI run, its own merge — wants **Graphite**, GitHub's
  native Stacked PRs, or Gerrit.

A few smaller things worth knowing before you commit to it for a given
repo or platform: LSP support is six languages and a hard stop — Rust,
TypeScript/JavaScript, Python, Go, Kotlin, Java, plus a hand-written
`[lsp.servers.<id>]` entry for anything else with no auto-install; Windows
support is WSL-only, no native build; and hover/go-to-definition/
diagnostics and comment-composing only work on a live working-tree diff
(or a moving ref like `HEAD`/`@`) — a fixed commit, `--staged`, a jj range,
or a `--pr` scope is read-only for those (asking the agent still works
everywhere, including a historical or PR diff).

None of this is a hedge — it's the same list a week of actual use would
turn up, and pointing you at the tool that's actually built for the job
you have is more useful than a page that pretends katamari does
everything.
