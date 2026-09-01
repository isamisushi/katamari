#!/usr/bin/env python3
"""A minimal ACP agent for the E2E suite.

Speaks just enough of ACP v1 (newline-delimited JSON-RPC 2.0 over stdio)
for `tests/e2e/agent_check.rs` (a headless single-turn `ktmr agent-check`
run) and `tests/e2e/agent_panel.rs` (the resident TUI session, many turns
across one process) to prove the whole client loop against a real
subprocess: answer `initialize` and `session/new` (advertising an
`acceptEdits` mode so the client's mode-selection path runs), then on each
`session/prompt` stream a couple of `session/update` notifications, raise
one `session/request_permission` for a fake edit, and — only if the client
grants it — write a marker file into the cwd before finishing the turn
with `end_turn`. The marker file is the point: it exists on disk iff the
permission round trip actually gated the "edit", which is the property
both E2E suites assert. A rejected permission ends the turn with `refusal`
and writes nothing.

Two markers are written on a grant, not one: the always-same-named
`acp-marker.txt` (`agent_check.rs`'s own original assertion, kept exactly
as it was) and a per-turn-unique `acp-marker-<n>.txt` (`agent_panel.rs`'s
multi-turn-on-one-session tests, which need turn N's own witness
independent of turn N-1's without the second turn's grant clobbering the
first turn's file). `n` is a plain in-process counter — this script never
forks, so a module-level counter is exactly as safe as a real per-session
turn counter would be.

The first `agent_message_chunk` (`"reading the review comments"`) is
`agent_check.rs`'s own original text, kept verbatim; a second chunk right
after it echoes the received prompt's own text
(`f"received: {text[:80]}"`) so `agent_panel.rs` can assert on constructed
prompt context (the ask template, or `check::DEFAULT_PROMPT`'s exact text)
without parsing raw JSON-RPC off the wire.

Unlike `fake_lsp_server.py` there are no timing knobs: the ACP client has
no readiness gate to race, so the fixture only needs protocol shape, not
controllable delays. An unrecognized request gets an empty result rather
than being left to hang, for the same reason the LSP fake does it.

One exception to "no timing knobs": a prompt containing the literal
substring `SLOW_CANCELLABLE` (checked as a substring, not an exact match,
since `ui::ask::build_prompt` wraps the reviewer's own text in a diff-block
template) streams one chunk and then blocks — indefinitely, not on a
delay — until the client actually sends `session/cancel`, so
`agent_panel.rs`'s cancel tests can prove a turn was genuinely in flight
(not finished before the cancel keypress landed) without racing a fixed
sleep against test timing.
"""

import json
import sys

_turn = 0


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def read_message():
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            continue
        return json.loads(line)


def send_update(session_id, update):
    send(
        {
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session_id, "update": update},
        }
    )


def handle_prompt(msg):
    global _turn
    _turn += 1
    turn = _turn
    tool_call_id = f"tc-{turn}"
    session_id = msg["params"]["sessionId"]
    prompt_text = ""
    try:
        prompt_text = msg["params"]["prompt"][0]["text"]
    except (KeyError, IndexError, TypeError):
        pass

    if "SLOW_CANCELLABLE" in prompt_text:
        # `agent_panel.rs`'s cancel tests: stream one chunk, then block
        # until the client sends a real `session/cancel` notification —
        # exactly what the real adapter's own `cancel()` waits on
        # `interrupt()` for (see the module docs) — rather than racing a
        # fixed delay against however long the test takes to press the
        # cancel key. A strict block-and-match on `session/cancel` alone is
        # enough: the one test that drives this path sends exactly that,
        # nothing else, while this call is blocked.
        send_update(
            session_id,
            {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "thinking slowly"},
            },
        )
        while True:
            note = read_message()
            if note is None:
                return
            if note.get("method") == "session/cancel":
                break
        send(
            {"jsonrpc": "2.0", "id": msg["id"], "result": {"stopReason": "cancelled"}}
        )
        return

    send_update(
        session_id,
        {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "reading the review comments"},
        },
    )
    send_update(
        session_id,
        {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": f"received: {prompt_text[:80]}"},
        },
    )
    send_update(
        session_id,
        {
            "sessionUpdate": "tool_call",
            "toolCallId": tool_call_id,
            "title": "Edit acp-marker.txt",
            "kind": "edit",
            "status": "pending",
        },
    )
    send(
        {
            "jsonrpc": "2.0",
            "id": 1000 + turn,
            "method": "session/request_permission",
            "params": {
                "sessionId": session_id,
                "toolCall": {"toolCallId": tool_call_id, "title": "Edit acp-marker.txt"},
                "options": [
                    {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                    {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
                ],
            },
        }
    )
    reply = read_message()
    outcome = (reply or {}).get("result", {}).get("outcome", {})
    granted = (
        outcome.get("outcome") == "selected" and outcome.get("optionId") == "allow"
    )
    if granted:
        with open("acp-marker.txt", "w") as f:
            f.write("edited after permission was granted\n")
        with open(f"acp-marker-{turn}.txt", "w") as f:
            f.write("edited after permission was granted\n")
        send_update(
            session_id,
            {
                "sessionUpdate": "tool_call_update",
                "toolCallId": tool_call_id,
                "status": "completed",
            },
        )
        send_update(
            session_id,
            {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "done"},
            },
        )
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"stopReason": "end_turn"}})
    else:
        send_update(
            session_id,
            {
                "sessionUpdate": "tool_call_update",
                "toolCallId": tool_call_id,
                "status": "failed",
            },
        )
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"stopReason": "refusal"}})


def main():
    while True:
        msg = read_message()
        if msg is None:
            return
        method = msg.get("method")
        msg_id = msg.get("id")
        if method == "initialize":
            send(
                {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "result": {
                        "protocolVersion": 1,
                        "agentCapabilities": {},
                        "authMethods": [],
                    },
                }
            )
        elif method == "session/new":
            send(
                {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "result": {
                        "sessionId": "fake-session-1",
                        "modes": {
                            "currentModeId": "default",
                            "availableModes": [
                                {"id": "default", "name": "Default"},
                                {"id": "acceptEdits", "name": "Accept Edits"},
                            ],
                        },
                    },
                }
            )
        elif method == "session/prompt":
            handle_prompt(msg)
        elif msg_id is not None:
            send({"jsonrpc": "2.0", "id": msg_id, "result": {}})


if __name__ == "__main__":
    main()
