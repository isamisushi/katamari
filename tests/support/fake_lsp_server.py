#!/usr/bin/env python3
"""A minimal, deliberately-delayed language server for the E2E suite.

Speaks just enough of LSP-over-stdio (`Content-Length` framing, JSON-RPC
2.0) for `tests/e2e/lsp_readiness.rs`'s readiness coverage (issue #11):
answer `initialize` — after sleeping `sys.argv[1]` seconds first, so a test
has a real, controllable window to press actions against a server that
hasn't come up yet — declare `definitionProvider`, then answer
`textDocument/definition` with no result after sleeping `sys.argv[2]`
seconds, so the same fixture can also prove movement stays responsive
while a *Ready* server is deliberately slow to answer a real request.
`sys.argv[3] == "1"` (issue #12) switches that last answer to a real
`Location` in a sibling file instead, for a PTY test that needs an actual
`FileView` push to prove `Esc` pops it — see
`support::fixture::lsp_readiness_repo_with_definition_target`'s docs.
Everything else (`initialized`, `textDocument/didOpen`, `shutdown`/`exit`)
is handled just well enough that katamari's client doesn't see anything it
would treat as a protocol violation; an unrecognized request gets an empty
result rather than being left to hang, so a future test extending this
fixture doesn't deadlock by surprise.

Deliberately plain, dependency-free Python (stdlib only) rather than a
compiled fixture binary: this script isn't shipped in `Cargo.toml`'s
`[[bin]]` list (see that file — `cargo-dist` packages every `[[bin]]`
target it finds there for release, and a test-only stub has no business in
a release archive) or built by `cargo test` at all; `tests/support::fixture::lsp_readiness_repo`
invokes it by absolute path via `python3` on `$PATH`, the same way a real
custom `[lsp.servers.<id>]` entry names an already-installed interpreter or
binary (see README.md's "Custom language servers" section) — nothing here
is katamari-specific enough to justify a compiled fixture instead.
"""

import json
import sys
import time


def read_message():
    """One `Content-Length`-framed JSON-RPC message, or `None` on EOF —
    exactly what happens once katamari closes stdin on shutdown, at which
    point this script should just exit rather than raise."""
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.decode("ascii", errors="replace").rstrip("\r\n")
        if line == "":
            break
        if ":" in line:
            key, _, value = line.partition(":")
            headers[key.strip().lower()] = value.strip()
    length = int(headers.get("content-length", "0"))
    body = sys.stdin.buffer.read(length)
    if not body:
        return None
    return json.loads(body.decode("utf-8"))


def write_message(obj):
    body = json.dumps(obj).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()


def main():
    init_delay = float(sys.argv[1]) if len(sys.argv) > 1 else 0.0
    definition_delay = float(sys.argv[2]) if len(sys.argv) > 2 else 0.0
    # Issue #12: when set, `textDocument/definition` answers with a real
    # `Location` in a sibling file (`other.stub`, next to whichever file
    # was asked about) instead of `None` — see
    # `support::fixture::lsp_readiness_repo_with_definition_target`'s docs
    # for why a *second* file, not just a non-null response, is what a
    # genuine `FileView`-push test needs. Off by default so issue #11's
    # existing not-ready/no-result assertions never have to think about it.
    definition_target = len(sys.argv) > 3 and sys.argv[3] == "1"

    while True:
        message = read_message()
        if message is None:
            return
        method = message.get("method")
        message_id = message.get("id")

        if method == "initialize":
            time.sleep(init_delay)
            write_message(
                {
                    "jsonrpc": "2.0",
                    "id": message_id,
                    "result": {
                        "capabilities": {"definitionProvider": True},
                        "serverInfo": {"name": "fake-lsp-server", "version": "0.0.0"},
                    },
                }
            )
        elif method == "textDocument/definition":
            time.sleep(definition_delay)
            if definition_target:
                # Derive `other.stub`'s URI from the request's own
                # `textDocument.uri` rather than hardcoding a path this
                # script was never told (no repo root is passed on argv) —
                # the two files always sit side by side, so swapping the
                # last path segment is exact, not a guess.
                uri = message["params"]["textDocument"]["uri"]
                base = uri.rsplit("/", 1)[0]
                write_message(
                    {
                        "jsonrpc": "2.0",
                        "id": message_id,
                        "result": {
                            "uri": f"{base}/other.stub",
                            "range": {
                                "start": {"line": 0, "character": 0},
                                "end": {"line": 0, "character": 0},
                            },
                        },
                    }
                )
            else:
                # `None` — no definition found — is enough to prove a
                # request actually dispatched and answered; issue #11's
                # readiness contract never depends on a real navigation
                # target.
                write_message({"jsonrpc": "2.0", "id": message_id, "result": None})
        elif method == "shutdown":
            write_message({"jsonrpc": "2.0", "id": message_id, "result": None})
        elif method == "exit":
            return
        elif message_id is not None:
            # A request this fixture doesn't otherwise implement — answer
            # instead of leaving the client waiting on it forever.
            write_message({"jsonrpc": "2.0", "id": message_id, "result": None})
        # Notifications (`initialized`, `textDocument/didOpen`, ...) carry
        # no `id` and expect no response at all.


if __name__ == "__main__":
    main()
