---
title: katamari
description: A terminal diff-review tool with LSP inside the diff, a persistent agent you can ask, durable resolvable comments, and read-only review units.
template: splash
hero:
  tagline: '<a href="/katamari/compared-to-other-tools/">The only terminal diff viewer with a language server inside it.</a> Ask a resident coding-agent session about any selection, leave comments it resolves, remember what you''ve already reviewed, and turn a tangled diff into a stack you can actually read — all without creating a single branch.'
  image:
    html: '<video autoplay loop muted playsinline poster="/katamari/demo-poster.png" width="1370" height="760" style="max-width: min(44rem, 100%); height: auto; border-radius: 0.5rem;" aria-label="katamari demo: reviewing a diff in the terminal, with hover and go-to-definition"><source src="/katamari/demo.webm" type="video/webm" /><source src="/katamari/demo.mp4" type="video/mp4" /></video>'
  actions:
    - text: Get started
      link: /katamari/installation/
      icon: right-arrow
    - text: GitHub
      link: https://github.com/isamisushi/katamari
      icon: external
      variant: minimal
---

**[The only terminal diff viewer with a language server inside
it.](/katamari/compared-to-other-tools/)** `ktmr
diff` shows a `git diff` with hover/go-to-definition/find-references/live
diagnostics from real language servers, a resident coding-agent session
you can ask about any selection, review comments it reads back and
resolves, and `u` to regroup a tangled diff into ordered, stacked-PR-like
review units — without creating a single branch. All of it without
leaving the terminal.

- **LSP inside the diff** — hover, go-to-definition, find-references, and
  live diagnostics on changed lines (Rust / TypeScript / Python / Go /
  Kotlin / Java; servers auto-install on first use; `[lsp.servers.<id>]`
  wires up a custom server for any other filetype) — works on a live
  working tree or `HEAD`; read-only on a fixed commit, `--staged`, a jj
  range, or `--pr`
- **Ask the agent** (`a`/`A`/`p`) — question a resident coding-agent
  session (Claude, via [ACP](https://agentclientprotocol.com)) about any
  selection, watch it work in a streaming transcript panel, ask follow-ups
  in the same session with no re-selecting, and push every open comment to
  it in one message; `C-g` cancels an in-flight turn safely from anywhere,
  and every edit it wants to make still waits on your own `y`/`n` (see
  [Ask the agent](/katamari/keybindings/#ask-the-agent))
- **Durable, resolvable comments** — leave one in the TUI, on one line or a
  `V` visual range; it lands in a plain, git-trackable
  `.katamari/comments.jsonl` an agent lists, addresses, and resolves via
  `ktmr comments`, picked up live with no restart
- **Reviewed-hunk state** (`r`/`R`) — mark the hunk under the cursor
  reviewed and it collapses to a one-line marker; the mark is
  content-addressed, keyed on the hunk's own changed lines rather than its
  position, so it survives a rebase or reorder in the overwhelming
  majority of cases, and only the hunk an agent actually rewrites
  resurfaces unreviewed — persisted in
  `.katamari/reviewed.jsonl`, explicit-keypress-only, never inferred from
  scroll (see [Quickstart](/katamari/quickstart/))
- **Semantic review units** — turn a tangled diff into a stack you can
  actually read, without touching a branch: `u` groups the diff's hunks
  into ordered, stacked-PR-like units (the refactor first, then the
  feature built on it, tests with the code they cover), each reviewable as
  its own scoped diff — derived and read-only, through the `claude`/`codex`
  CLI you already have, no branches created, nothing written outside
  `.katamari/` (see [Review units](/katamari/review-units/))
- **git and jj, equally first-class** — colocated jj revsets, and a
  snapshot timeline through every save of an agent's session — the same
  LSP, comments, and review-units feature set either way
- **Any scope of change** — working tree, staged, one commit, a range, a jj
  change or revset, a GitHub pull request by number (`--pr`, through your
  own `gh`), or the current branch against its automatically detected base
  (`--branch`/`B`, no network, live-followed as either side moves);
  browse and pick from `ktmr log`, or switch mid-session with a popup (`o`)
- **Live refresh** — working-tree diffs refresh as an agent's edits land on
  disk by default; pass `--no-watch` for a static session
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

Agents now write more code, faster, than review capacity can absorb, and
the strain shows up as a verification bottleneck: the scarce resource
isn't writing the change anymore, it's deciding whether to trust and merge
it. Confidence that AI-generated code is actually correct runs low
industry-wide, yet the volume of AI-assisted pull requests keeps climbing
regardless — leaving rubber-stamping as the path of least resistance for
anyone whose review time hasn't scaled to match. Agent-authored diffs also
tend to land large and oddly ordered, nothing like a human's
incrementally-committed history: a refactor, the feature it enabled, the
tests, and a lockfile bump interleaved across files in alphabetical order.
That's precisely the "unreadable as one blob" problem the stacked-PR world
exists to solve — except that fix means materializing real branches and
PRs before anyone's read the change, a team-level commitment most solo
reviewers and small teams don't want for a diff that was never going to
live as separate PRs anyway.

A wave of terminal-native review tools converged independently on the
same shape recently: local, vim-keyed, comment-then-hand-to-agent —
real evidence that a lightweight, no-SaaS review loop that lives where the
agent CLI already lives is something people actually want. What stayed
missing was the thing a reviewer needs to trust a diff enough to comment
on it with authority: the ability to actually navigate the code the way an
editor lets you — jump to a definition, see who else calls this, see the
type — without leaving the diff to open a second window. The gap between
"a nice terminal diff pager" and "an editor's worth of understanding"
stayed wide open the whole time.

katamari treats a language server exactly the way it treats a coding
agent: something it spawns, owns the lifecycle of, and can health-check
(`ktmr doctor`), rather than something shelled out to once and forgotten.
That's the same manager pattern underneath both the LSP subsystem and the
[agent session](/katamari/keybindings/#ask-the-agent) `a`/`A`/`p` talk to,
which is why review gets editor-grade navigation instead of pager-grade
text, and why asking the agent about a selection means one persistent,
protocol-owned session for the whole review rather than a fresh process
per question. The comment store is a plain, git-trackable file with real
status semantics — `.katamari/comments.jsonl`, resolved and reopened
through `ktmr comments` — because a review can span days and multiple
agent invocations, and a comment that isn't durably resolvable isn't
trustworthy enough to walk away from. And [review
units](/katamari/review-units/) are strictly read-only and derived, cached
by content hash, because the whole point is a *better reading order* for a
diff that already exists — never risking the actual git history to get
there.

Put together, that's a single static binary you can drop into any repo
with zero infrastructure: no server, no token, no SaaS bill, no branch
surgery. For repositories using [jj](https://github.com/jj-vcs/jj)
colocated with git, the same binary also keeps a timeline of jj's
automatic working-copy snapshots, so you can step back through every
version of the working tree an agent's session passed through, not just
the version currently on disk.

See [Compared to other tools](/katamari/compared-to-other-tools/) for how
this stacks up against diff pagers, git TUIs, other terminal review tools,
cloud AI reviewers, and stacked-PR platforms — including where katamari is
the wrong tool for the job.
