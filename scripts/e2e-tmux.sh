#!/usr/bin/env bash
# Real-terminal-emulator smoke test for the M9b/M10 kitty-keyboard-protocol
# fallback: drives `ktmr diff` inside an actual detached tmux session (a
# genuine terminal emulator, not `vt100`'s parser reading a raw PTY the way
# `tests/e2e.rs` does) on a private socket, and checks the one thing only a
# real terminal without kitty support can prove: that the fallback hint
# (`C-o/C-t`) is what a reviewer actually sees in a tool tmux doesn't
# support the kitty protocol in.
#
# Usage: scripts/e2e-tmux.sh [path-to-ktmr-binary]
#   Defaults to target/debug/ktmr, relative to the repo root this script
#   lives in — run `mise run e2e-tmux` rather than invoking this directly
#   unless you already have a debug build.
#
# Exits nonzero with a captured pane dump on any failure. Always tears down
# its tmux server and tempdir, via a trap, regardless of how it exits.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BIN="${1:-"$REPO_ROOT/target/debug/ktmr"}"
if [[ ! -x "$BIN" ]]; then
    echo "e2e-tmux: binary not found or not executable: $BIN" >&2
    echo "e2e-tmux: build it first (e.g. \`mise run build\`) or pass a path" >&2
    exit 1
fi
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

TMUX_BIN="${TMUX_BIN:-tmux}"
if ! command -v "$TMUX_BIN" >/dev/null 2>&1; then
    echo "e2e-tmux: tmux not found on PATH (set TMUX_BIN=/path/to/tmux)" >&2
    exit 1
fi

SOCKET="katamari-e2e-$$"
SESSION="ktmr-e2e"
WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/katamari-e2e-tmux.XXXXXX")"
HOMEDIR="$WORKDIR/home"
REPO="$WORKDIR/repo"
mkdir -p "$HOMEDIR" "$REPO"

cleanup() {
    "$TMUX_BIN" -L "$SOCKET" kill-server >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

pane() {
    "$TMUX_BIN" -L "$SOCKET" capture-pane -p -t "$SESSION"
}

fail() {
    echo "=== e2e-tmux FAILED: $1 ===" >&2
    echo "--- pane dump ---" >&2
    pane >&2 || true
    echo "--- end pane dump ---" >&2
    exit 1
}

# Polls `pane | grep -qF "$1"` every 100ms up to $2 seconds (default 10),
# via `fail` naming what was expected on timeout.
wait_for_pane_text() {
    local needle="$1"
    local timeout_s="${2:-10}"
    local deadline=$((SECONDS + timeout_s))
    while ! pane 2>/dev/null | grep -qF "$needle"; do
        if (( SECONDS >= deadline )); then
            fail "expected pane text not found within ${timeout_s}s: $needle"
        fi
        sleep 0.1
    done
}

# --- fixture repo: plain-text content only, same reasoning as
# tests/support/fixture.rs — no .rs/.ts/.py/.go, so no language server (and
# no network) is ever involved. ---------------------------------------
git -C "$REPO" init -q
cat >"$REPO/README.md" <<'EOF'
# Sample project

This is line two.
This is line three.
EOF
git -C "$REPO" -c user.email=e2e@katamari.test -c user.name="katamari e2e" add -A
git -C "$REPO" -c user.email=e2e@katamari.test -c user.name="katamari e2e" commit -q -m "initial commit"
cat >"$REPO/README.md" <<'EOF'
# Sample project

This is line two, updated for the e2e-tmux smoke test.
This is line three.
EOF

# --- launch --------------------------------------------------------------
# Isolated $HOME/$XDG_* — never touch the real ~/.config/katamari or the
# katamari-managed LSP install prefix, matching `tests/support::Harness`'s
# env isolation.
export HOME="$HOMEDIR"
export XDG_CONFIG_HOME="$HOMEDIR/config"
export XDG_DATA_HOME="$HOMEDIR/data"

"$TMUX_BIN" -L "$SOCKET" new-session -d -s "$SESSION" -x 100 -y 30 -c "$REPO" "$BIN"

wait_for_pane_text "README.md"

# (a) tmux has no kitty keyboard protocol — the hint bar must show the
# always-available fallback binding, not the kitty-only C-i alias.
if ! pane | grep -qF "C-o/C-t"; then
    fail "expected hint bar to show the C-o/C-t fallback (tmux has no kitty protocol)"
fi

# (b) a known diff line renders.
if ! pane | grep -qF "updated for the e2e-tmux"; then
    fail "expected the changed README.md line to render in the diff"
fi

# (c) resize to 60 columns -> hints re-wrap onto a second bulleted line.
"$TMUX_BIN" -L "$SOCKET" resize-window -t "$SESSION" -x 60 -y 30
sleep 0.3 # let the resize event reach ktmr and redraw
BULLET="$(printf '\xc2\xb7')" # U+00B7 MIDDLE DOT, ui::hints::LINE_PREFIX's bullet
hint_lines="$(pane | awk -v b="$BULLET" '{line=$0; sub(/^[ \t]+/, "", line); if (index(line, b) == 1) c++} END {print c+0}')"
if [[ "$hint_lines" -lt 2 ]]; then
    fail "expected the hint bar to wrap onto >1 line at 60 columns, found $hint_lines"
fi

# (d) q quits cleanly.
"$TMUX_BIN" -L "$SOCKET" send-keys -t "$SESSION" q
deadline=$((SECONDS + 10))
while "$TMUX_BIN" -L "$SOCKET" has-session -t "$SESSION" >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
        fail "session did not end within 10s of sending q"
    fi
    sleep 0.1
done

echo "e2e-tmux: ok"
