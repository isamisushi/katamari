# katamari demo: reviewing an AI agent's TypeScript refactor with
# ktmr diff — hover, go-to-definition, diagnostics, and a review comment,
# all without leaving the terminal. Recorded against a freshly built
# local binary (main @ post-745acfe, soft line-wrap on by default).
# Re-recorded with --show-keys so the bottom-right key-display chip is
# visible throughout; the comment-typing segment intentionally shows its
# `[typing…]` masking rather than echoing the typed text.

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
Type "clear"
Enter
Show

# Open the diff on the AI's working-tree changes, with the key-display
# overlay on so the recording shows what's being pressed.
Type "/Users/yuya/Downloads/katamari/target/release/ktmr diff --show-keys"
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

Sleep 1500ms
Type "q"
Sleep 500ms
