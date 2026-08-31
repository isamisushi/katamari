---
title: Keybindings
description: Every keybinding in the vim and emacs presets, plus mouse support and diff search.
---

Vim bindings are the default; set `keymap = "emacs"` in config (see
[Configuration](/katamari/configuration/)) for the emacs column. `q` quits katamari from anywhere — a pushed
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
Terminal.app, which doesn't implement the protocol). Detection asks the
terminal directly and, on some terminals, can take a couple of seconds to
hear nothing back — so the answer is cached per terminal (`$XDG_STATE_HOME/
katamari/kitty-probe.json`, keyed by `TERM`/`TERM_PROGRAM`/
`TERM_PROGRAM_VERSION`) after the first launch in it; every later launch in
that same terminal skips the wait entirely. `ktmr reset --cache` clears the
cache if you switch terminal emulators and want it re-detected.
`M-Left`/`M-Right` are unconditional aliases for back/forward in
both cases — the always-available, terminal-agnostic pair — while `C-i`
itself is simply left unbound when the terminal can't distinguish it from
Tab. `Ctrl-]` and `Ctrl-t` have no default binding at all: katamari has one
general jump history rather than a separate vim-style tag stack, so there's
no second "go back" key to bind.

## Mouse

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
(preset plus any `[keys]` override, see [Configuration](/katamari/configuration/)),
never a hardcoded reference the
table above could drift out of sync with. `j`/`k`/arrows/`C-n`/`C-p` (and
`PageDown`/`PageUp`/`C-d`/`C-u`, and `gg`/`G` for top/bottom) scroll; `/`
starts a filter that narrows the list live as you type, matching against
each row's description, its config name, and its key; `Enter` keeps the
filter and returns to scrolling. `Esc` while typing the filter clears it
and returns to scrolling; `Esc` while scrolling the list closes the window
outright, same as `q`/`?`, whether or not a filter is still narrowing it.
The window is modal while open — every key goes to it, not whatever view
is underneath.

## Search

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
between hunks — see `zo`/`zc` in [Quickstart](/katamari/quickstart/)) isn't
matched until you unfold it, at which point a confirmed search's matches
recompute automatically over the newly revealed rows.

Any binding can be overridden per action; see `[keys]` in
[Configuration](/katamari/configuration/).

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
