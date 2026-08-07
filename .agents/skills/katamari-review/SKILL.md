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
  (e.g. to flag something you noticed but aren't fixing right now).
- `ktmr comments reopen <id>` — undo an accidental resolve.

## Notes

- Comment anchors are always `file:line` in the *working tree*, never a
  commit or a diff hunk — they describe "this line, as it exists on disk
  right now."
- All of this reads and writes a single file,
  `<repo_root>/.katamari/comments.jsonl` — there's no daemon or server
  involved, so these commands work the same whether or not a `ktmr diff`
  session happens to be open.
