# katamari demo: reviewing an AI agent's TypeScript refactor with
# ktmr diff — hover, go-to-definition, diagnostics, a review comment,
# and the v0.4.0 additions: `/` search with n, `z o` fold expansion,
# the `?` help window, and a closing `ktmr doctor` health check.
# Recorded against a freshly built local binary (main @ v0.4.0+,
# soft line-wrap on by default) with --show-keys so the bottom-right
# key-display chip shows what's being pressed; the comment-typing
# segment intentionally shows its `[typing…]` masking rather than
# echoing the typed text.
#
# NOTE: the two `Hide`-section paths below (fixture repo cd + PATH) are
# machine-local — update them to wherever your demo repo and freshly
# built target/release live before running `vhs docs/demo.tape`.

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
Type "cd /private/tmp/claude-501/-Users-yuya-Downloads-tako/158c067e-92e3-4775-9fef-8ea1da600422/scratchpad/gif-take2/demo-repo"
Enter
# The on-screen command should read like a normal install (`ktmr diff`),
# not a developer's target/release path — put the fresh build on PATH
# out of frame instead.
Type "export PATH=/Users/yuya/Downloads/katamari/target/release:$PATH"
Enter
Type "clear"
Enter
Show

# Open the diff on the AI's working-tree changes, with the key-display
# overlay on so the recording shows what's being pressed.
Type "ktmr diff --show-keys"
Enter
Sleep 2s

# Navigate to the correct createUser(...) call site and hover it —
# money shot #1: real type info from typescript-language-server.
Type "jjjjjjjjjjjjj"
Sleep 500ms
Tab
Sleep 500ms
Sleep 10s
Type "K"
Sleep 3500ms
Escape
Sleep 500ms

# Go to definition, glance at it (also where the long formatUserSummary
# line lives, showing the new soft-wrap continuation marker), then jump back.
Type "gd"
Sleep 2500ms
Ctrl+O
Sleep 1s

# Jump to the AI's mistake — money shot #2: a real diagnostic on a
# changed line, caught inside the diff.
Type "]d"
Sleep 1500ms
Type "K"
Sleep 3500ms
Escape
Sleep 500ms

# Leave a review comment for the agent to pick up — money shot #3.
Type "c"
Sleep 500ms
Type "wrong arg shape - use the new options object"
Sleep 500ms
Ctrl+S
Sleep 2s

# v0.4.0: search the diff vim-style — money shot #4. `/` prompts on the
# status line, matches highlight as you type, n hops between them.
Type "/"
Sleep 400ms
Type "createUser"
Sleep 1200ms
Enter
Sleep 800ms
Type "n"
Sleep 900ms
Type "n"
Sleep 1200ms
Escape
Sleep 600ms

# v0.4.0: unfold the context git omitted. G lands on the file's trailing
# `···` fold row; z o splices the real source lines in, LSP-ready.
Type "G"
Sleep 800ms
Type "zo"
Sleep 2000ms
Type "zc"
Sleep 1000ms

# v0.4.0: `?` — every binding, live and filterable. Type a few letters
# into its `/` filter, then close.
Type "?"
Sleep 1500ms
Type "/"
Sleep 300ms
Type "fold"
Sleep 1500ms
Escape
Sleep 400ms
Escape
Sleep 600ms

Sleep 1000ms
Type "q"
Sleep 500ms

# v0.4.0 closer: `ktmr doctor` — the checkhealth-style report, ending on
# the live spawn-and-hover probe timings for the repo's language.
Type "clear"
Enter
Type "ktmr doctor"
Enter
Sleep 8s
