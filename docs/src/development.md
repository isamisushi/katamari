# Development

`mise run test` runs the unit suite plus `tests/e2e.rs`, an integration
suite that drives the real compiled `ktmr` binary through a PTY (via
`pty-process`) and parses its output with `vt100` — real crossterm parsing,
real kitty-keyboard-protocol negotiation, no terminal emulator required.
Run it on its own with:

```
mise run e2e
```

`mise run e2e-tmux` is a second, real-terminal-emulator tier: it builds a
debug binary and runs `scripts/e2e-tmux.sh`, which drives `ktmr diff` inside
an actual detached tmux session on a private socket — the one thing the PTY
suite can't check for real, since tmux (unlike the PTY suite's fake
terminal) genuinely doesn't speak the kitty keyboard protocol, so this is
what proves the `C-o`/`M-Right` fallback is what a reviewer actually sees
there.

```
mise run e2e-tmux
```

Both are self-contained: they build their own throwaway git fixtures in a
tempdir and point `$HOME`/`$XDG_CONFIG_HOME`/`$XDG_DATA_HOME` at another
tempdir, so a test run never touches your real `~/.config/katamari` or the
katamari-managed language-server install prefix.
