---
name: katamari-review
description: Use when addressing review comments left by a human reviewer in katamari (ktmr), a terminal diff-review tool — read the comments with `ktmr comments list --json`, address each one, then mark it resolved with `ktmr comments resolve <id>`. Trigger on requests like "address the katamari comments", "handle the review feedback", or a pasted `ktmr comments export` report.
---

# Addressing katamari review comments

katamari (`ktmr`) is a terminal diff-review tool. A human reviewer reads
your diff inside `ktmr diff`, leaves comments anchored to specific
file/line positions, and expects you to address each one and mark it
resolved — this skill is that workflow.

## Workflow

1. List the open comments:

   ```
   ktmr comments list --json
   ```

   Each line is one JSON object:

   ```
   {"id": "a1b2c3d4", "created_at": 1700000000, "file": "src/lib.rs",
    "anchor": {"new_line": 42, "content_hash": ..., "context_hash": ...},
    "body": "this should handle the empty-input case too",
    "status": "open"}
   ```

   `file` is repo-relative; `anchor.new_line` is the 1-based line in the
   **working tree** version of that file at the moment the comment was
   written. Open the file and read that line (and its surroundings) to see
   what the comment is about — if the file has changed a lot since, the
   line number may no longer point at exactly the right spot; use judgment
   and the comment's own wording rather than trusting the number blindly.
   (katamari's own UI relocates comments live as the file changes; this CLI
   output always reports the comment's last known anchor.)

   A comment can also cover a contiguous range instead of one line: when
   that's the case, the object also has an `end_anchor` with the same shape
   as `anchor`, and the two together mean "every line from `anchor.new_line`
   to `end_anchor.new_line`, inclusive." Treat the comment as being about
   that *whole* range — read and address every line in it, not just the
   first one.

2. For each open comment, make the requested change in the working tree.

3. Mark it resolved:

   ```
   ktmr comments resolve <id>
   ```

   The reviewer's `ktmr diff` session picks this up live — no restart
   needed — and shows the comment as resolved (dimmed, struck through) at
   its current position in the diff.

4. If a comment doesn't apply (already fixed, or you disagree with it),
   still resolve it rather than leaving it open silently, and say why in
   your own summary back to the reviewer.

## Other commands

- `ktmr comments list` — human-readable table of open comments. Add
  `--status=all` to include resolved ones, or `--status=resolved` for only
  those.
- `ktmr comments export --format=md` — the same comments as a
  paste-ready markdown report, grouped by file, if you need to hand a
  summary back to the reviewer instead of (or alongside) resolving inline.
- `ktmr comments add <file> <line> <body>` — leave a comment yourself
  (e.g. to flag something you noticed but aren't fixing right now). Add
  `--end-line <line>` to comment on a range instead of a single line, e.g.
  `ktmr comments add src/lib.rs 10 "extract this" --end-line 18`.
- `ktmr comments reopen <id>` — undo an accidental resolve.

## Seeing the diff the way the reviewer does

The reviewer may be reading the diff as ordered *review units* — a
stacked-PR-like grouping of its hunks (foundations and refactors first,
then the code built on them, tests and docs with the change they cover)
that katamari generates through an agent CLI (`claude` or `codex`). You
can print that same grouping headlessly:

```
ktmr diff --dump-units             # the cached grouping if one exists, else generate it
ktmr diff --dump-units --regroup   # always generate afresh
```

The output is a plain-text list: one numbered unit per line with its
label, hunk count, and the files it touches — a trailing `[noise]` unit
collects lockfile/generated churn, and `[ungrouped]` collects anything the
grouping didn't place. Two good uses: structuring a summary back to the
reviewer in the same order they'll read the change in, and checking that
your diff actually decomposes into the story you meant to tell. Reading
the cache (`.katamari/groups.jsonl`) is instant; generating spawns the
agent CLI and can take seconds to minutes (hard cap: three minutes), and
with no `claude`/`codex` on `PATH` it fails with a message saying so
rather than printing a partial answer.

## Notes

- Comment anchors are always `file:line` (or `file:start-end` for a range)
  in the *working tree*, never a commit or a diff hunk — they describe
  "this line (or these lines), as they exist on disk right now."
- All of this reads and writes a single file,
  `<repo_root>/.katamari/comments.jsonl` — there's no daemon or server
  involved, so these commands work the same whether or not a `ktmr diff`
  session happens to be open.
