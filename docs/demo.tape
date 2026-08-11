# katamari demo: reviewing an AI agent's tangled TypeScript change with
# ktmr diff — v0.5.x's review units first (`u` groups the diff into a
# stacked-PR-like reading order, Enter scopes to one unit), then the LSP
# money shots inside it: a real diagnostic on the agent's mistake, a
# review comment for the agent to pick up, and hover on the refactored
# signature found via `/` search. Recorded with --show-keys so the
# bottom-right chip shows what's being pressed; the comment-typing
# segment intentionally shows its `[typing…]` masking.
#
# Kept deliberately free of dead air: the units panel opens instantly
# because the grouping cache is seeded before recording (see below), and
# the LSP warm-up overlaps the units segment instead of being waited out
# on camera.
#
# To re-record:
#   1. docs/demo-fixture.sh <dir>   — build the throwaway demo repo
#   2. cd <dir> && ktmr diff --dump-units   — seed the units cache once
#      (needs `claude` or `codex`; --regroup to reroll bad labels)
#   3. update the two machine-local Hide paths below (fixture dir + the
#      release build's PATH), then `vhs docs/demo.tape`
#
# NOTE: the two `Hide`-section paths below are machine-local.

Output demo.gif

Require ktmr

Set Shell "bash"
Set FontSize 20
Set Width 1370
Set Height 760
Set Padding 15
Set Theme "Dracula"
Set WindowBar "Rings"
Set BorderRadius 8
Set Framerate 15
Set TypingSpeed 45ms

Hide
Type "cd /tmp/claude-0/-root-projects-github-com-isamisushi-katamari/bdb95d7a-0ff1-4bc3-bfb0-4a61bd3f61ca/scratchpad/demo-repo"
Enter
# The on-screen command should read like a normal install (`ktmr diff`),
# not a developer's target/release path — put the fresh build on PATH
# out of frame instead.
Type "export PATH=/root/projects/github.com/isamisushi/katamari/target/release:$PATH"
Enter
Type "clear"
Enter
Show

# Open the diff on the AI's working-tree changes, with the key-display
# overlay on so the recording shows what's being pressed.
Type "ktmr diff --show-keys"
Enter
Sleep 2.5s

# Money shot #1: `u` — the tangled diff regrouped into ordered review
# units, instantly (cached). Foundations first, lockfile churn dead last.
Type "u"
Sleep 3s

# Scope to the call-site unit and read it as its own diff — the pinned
# banner names where we are in the stack.
Type "jj"
Sleep 800ms
Enter
Sleep 2.2s

# Money shot #2: the agent's mistake, caught inside the scoped unit — a
# real diagnostic from typescript-language-server on a changed line.
Type "]d"
Sleep 1.2s
Type "K"
Sleep 3.2s
Escape
Sleep 500ms

# Money shot #3: leave a review comment for the agent to pick up.
Type "c"
Sleep 500ms
Type "still the old positional call - use the options object"
Sleep 500ms
Ctrl+S
Sleep 1.8s

# Esc widens back from the unit to the full diff.
Escape
Sleep 1.2s

# Money shot #4: `/` search finds createUser, `n` hops to the migrated
# call site, hover shows the refactored signature — real type info,
# inside the diff.
Type "/"
Sleep 400ms
Type "createUser"
Sleep 1s
Enter
Sleep 700ms
Type "n"
Sleep 500ms
Type "n"
Sleep 700ms
Type "K"
Sleep 3.2s
Escape
Sleep 500ms

Type "q"
Sleep 800ms
