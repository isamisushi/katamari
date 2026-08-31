---
title: jj colocated setup
description: Colocate jj with an existing git repository so katamari's timeline can read its snapshots.
---

katamari's timeline reads jj's own automatic working-copy snapshots — no
katamari-specific setup beyond jj itself being colocated with the existing
git repository:

```
cd your-repo
jj git init --colocate
```

That's it: jj now tracks the same working copy as git (a `.jj` directory
appears alongside `.git`), and every save creates a new jj operation
katamari's timeline can show. During live refresh, katamari itself triggers a
snapshot (`jj util snapshot`) after each burst of edits, so an agent
session leaves one timeline entry per save even though the agent never
runs a jj command. Nothing about `ktmr diff` changes if jj
isn't set up — the timeline (`t`) simply reports it's unavailable. `ktmr
diff -r`/`--from`/`--to` and `ktmr log`'s jj-backed history need this same
colocated setup; `ktmr log` still works without it (falling back to plain
`git log`), just without jj's revsets or the working copy as a browsable
entry.
