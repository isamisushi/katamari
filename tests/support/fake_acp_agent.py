#!/usr/bin/env python3
"""A minimal ACP agent for the E2E suite.

Speaks just enough of ACP v1 (newline-delimited JSON-RPC 2.0 over stdio)
for `tests/e2e/agent_check.rs` to prove the whole client loop against a
real subprocess: answer `initialize` and `session/new` (advertising an
`acceptEdits` mode so the client's mode-selection path runs), then on
`session/prompt` stream a couple of `session/update` notifications, raise
one `session/request_permission` for a fake edit, and — only if the
client grants it — write `acp-marker.txt` into the cwd before finishing
the turn with `end_turn`. The marker file is the point: it exists on disk
iff the permission round trip actually gated the "edit", which is the
property the E2E test asserts. A rejected permission ends the turn with
`refusal` and writes nothing.

Unlike `fake_lsp_server.py` there are no timing knobs: the ACP client has
no readiness gate to race, so the fixture only needs protocol shape, not
controllable delays. An unrecognized request gets an empty result rather
than being left to hang, for the same reason the LSP fake does it.
"""

import json
import sys


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
    session_id = msg["params"]["sessionId"]
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
            "sessionUpdate": "tool_call",
            "toolCallId": "tc-1",
            "title": "Edit acp-marker.txt",
            "kind": "edit",
            "status": "pending",
        },
    )
    send(
        {
            "jsonrpc": "2.0",
            "id": 1000,
            "method": "session/request_permission",
            "params": {
                "sessionId": session_id,
                "toolCall": {"toolCallId": "tc-1", "title": "Edit acp-marker.txt"},
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
        send_update(
            session_id,
            {"sessionUpdate": "tool_call_update", "toolCallId": "tc-1", "status": "completed"},
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
            {"sessionUpdate": "tool_call_update", "toolCallId": "tc-1", "status": "failed"},
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
