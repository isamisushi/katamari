---
title: Quickstart
description: Open a diff, review with LSP-backed hover and go-to-definition, leave comments, and switch scope without restarting.
---

Run inside any git repository:

```
ktmr diff              # working tree vs HEAD, refreshed live by default
ktmr diff --no-watch   # working tree vs HEAD, without live refresh
ktmr diff --staged     # staged (index) changes vs HEAD
ktmr diff <rev>        # one commit's own changes
ktmr diff <a>..<b>     # a revision range
ktmr diff --pr 123     # a GitHub pull request, via your logged-in `gh`
```

`--no-watch` is a working-tree-only opt-out; staged and pinned historical
diffs are already static. A diff opened through a *moving* revision —
`HEAD`, a branch name, or a jj revset like `@` — is the one exception: it
re-resolves live, so amending the commit it points at (`git commit
--amend`, `jj describe`/`squash`) updates the open diff in place, cursor
and scroll preserved. A diff opened by commit hash never changes.

In a colocated jj repository (see [jj colocated setup](/katamari/jj-colocated-setup/)),
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

`ktmr diff --pr <number>` opens a GitHub pull request's diff against its
actual base — no branch to fetch, no checkout, nothing written into the
repository. It runs through your own logged-in
[GitHub CLI](https://cli.github.com) (`gh pr diff` under the hood), so
private repositories and GitHub Enterprise work exactly as well as your
`gh` does — katamari deliberately ships no GitHub client, tokens, or
HTTP code of its own, the same philosophy as review units spawning your
own `claude`/`codex` instead of bundling an LLM client. `gh`'s own
errors surface directly: a missing login says `gh auth login`, an
ambiguous multi-remote repo says `gh repo set-default`. Like any other
historical scope the snapshot is read-only — hover/go-to-definition and
comments are unavailable on it — and the status bar labels it
`PR #123`, so what's on screen is never ambiguous. Also reachable
mid-session: the scope menu (`o`) has a **GitHub PR…** entry that asks
for the number and fetches in the background.

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
themselves if missing — see [Language servers](/katamari/language-servers/) if a
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
config (see [Configuration](/katamari/configuration/)) to turn the offer off
entirely; `ktmr skill install` keeps working as an explicit command
regardless.

`ktmr skill install --user` installs the skill once for every repository
instead: just `~/.agents/skills/katamari-review` and the
`~/.claude/skills/katamari-review` symlink to it, under your home directory
rather than a repo — no `AGENTS.md`/`CLAUDE.md`, since a home directory has
no single project for either to point at. It works from outside any git
repo (there's no repo root to find), is idempotent the same way the
per-repo command is, and refreshes stale content the same way too. A repo
whose `$HOME` already carries the skill this way is never offered the
first-comment prompt above, since it already has what the prompt would
install. Running a plain `ktmr skill install` inside a particular repo
still works on top of this and remains the repo-shared default — the two
scopes don't conflict, since the per-repo skill/`.claude` symlink and the
`--user` one live at different paths.

`ktmr open <file>` opens a single file read-only, with the same
highlighting/hover/go-to-definition as the diff view — useful for reading
code a diff jumped you into without leaving katamari.

If the repository has a colocated jj repo (see
[jj colocated setup](/katamari/jj-colocated-setup/)), `t` from `ktmr diff` opens the
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
- **GitHub PR…** opens the same kind of input for a pull request number
  (`#123` pasted straight from GitHub works too) — the mid-session twin
  of `ktmr diff --pr`. The `gh` fetch runs in the background: the diff
  already on screen stays put and fully interactive until the PR's text
  actually lands, and a failure (not logged in, no GitHub remote, no
  such PR) surfaces `gh`'s own message in the status bar instead of
  touching the current view.

Swapping to anything other than the working tree pauses live refresh's
refresh loop (the watcher itself keeps running) until you swap back;
`.katamari/comments.jsonl`'s own watcher is unaffected either way.

Finally, `u` groups the current diff into ordered review units — katamari's
stacked-PR-like reading order — and `Enter` on one scopes the whole session
to it. That's the next section.
