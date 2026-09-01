---
title: Reset
description: "`ktmr reset` returns katamari to a fresh-install state, selectively."
---

`ktmr reset` returns katamari to a fresh-install state, selectively — and
with no flags it removes nothing at all: it prints an inventory of every
target, whether it currently exists, where it lives, and which flag would
remove it.

```
ktmr reset                  # report only: what exists where; removes nothing
ktmr reset --cache          # the repo's grouping cache (.katamari/groups.jsonl) + katamari's state dir — update-check cache, the per-terminal kitty-keyboard-protocol probe cache, index caches, and the per-session LSP journals
ktmr reset --units-config   # strip the [units] table from both config files
ktmr reset --servers        # katamari-managed language-server installs (large downloads, hence not part of --cache)
ktmr reset --comments       # .katamari/comments.jsonl — review data, never implied by --all
ktmr reset --reviewed       # .katamari/reviewed.jsonl — reviewed-hunk marks, never implied by --all
ktmr reset --all            # cache + units-config + servers; comments/reviewed marks always take the explicit flag
```

`--units-config` edits surgically: only the `[units]` table is removed
from each config file, everything else — comments and formatting
included — is preserved byte-for-byte, and a file left holding nothing but
whitespace afterward is deleted outright. Review comments are review data,
not cache, which is why `--all` deliberately never touches them —
reviewed-hunk marks (`--reviewed`) get the same treatment, for the same
reason: it's the reviewer's own progress through a diff, not a
regenerable cache (`ktmr reviewed clear` does the identical removal).
After any
run, a `.katamari/` directory left holding only its own generated
`.gitignore` is tidied away entirely — and outside a git repository, the
repo-scoped targets are skipped while the user-level ones (servers, state
dir, home config) still work.
