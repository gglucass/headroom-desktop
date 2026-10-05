#!/usr/bin/env python3
"""Probe the held-Read breakpoint vendor against the INSTALLED wheel.

Drives the real Anthropic handler (create_app + TestClient, upstream mocked as
in the wheel's tests/test_read_maturation_handler_nobust.py) through one
Claude Code conversation: two 1h system breakpoints, tools, client breakpoints
on the last block of the last two messages, a fresh 3 KB Read, then quiet
turns whose JSON tool output the router compresses. The mock reports the
provider caching exactly up to the last forwarded breakpoint, measured with
the prefix tracker's own estimator, so the confirmed prefix advances as in
production (the live log's `freezing first N` stops at the held Read).

Run with the desktop's sitecustomize on PYTHONPATH:

    PYTHONPATH=<pyinject> HEADROOM_SDK=headroom-desktop-proxy \
        <managed python> scripts/verify-held-read-breakpoint.py

Prints one `OK`/`FAIL` line per check and one INFO line per turn. `FAIL hrb
bound` means the vendor did not bind (wheel bumped, or
HEADROOM_HELD_READ_BREAKPOINT=0), which the caller treats as a skip. With the
kill switch set, the wheel's own behaviour shows as `FAIL every turn keeps the
client's breakpoints` (in_total=4 out_total=3, tail 1, 3, 5, ...).
"""

from __future__ import annotations

import copy
import json
import logging
import os
import sys

logging.disable(logging.CRITICAL)
os.environ["HEADROOM_MODE"] = "token"
os.environ["HEADROOM_CCR_BACKEND"] = "memory"  # not the shared ~/.headroom/ccr_store.db
os.environ["HEADROOM_DISABLE_KOMPRESS"] = "1"  # breakpoints are under test, not Kompress

import httpx
from fastapi.testclient import TestClient

import headroom.transforms.read_maturation as rm
from headroom.cache.compression_store import reset_compression_store
from headroom.cache.prefix_tracker import PrefixCacheTracker
from headroom.proxy.helpers import count_cache_breakpoints
from headroom.proxy.server import ProxyConfig, create_app

failures: list[str] = []


def check(ok: bool, label: str) -> None:
    print(("OK   " if ok else "FAIL ") + label)
    if not ok:
        failures.append(label)


check(rm.relocate_cache_breakpoint.__name__ == "_hd_hrb_relocate", "hrb bound")

CC = {"type": "ephemeral", "ttl": "1h"}
SYSTEM = [
    {"type": "text", "text": "You are Claude Code.", "cache_control": CC},
    {"type": "text", "text": "Project instructions. " * 300, "cache_control": CC},
]
TOOLS = [
    {"name": n, "description": n, "input_schema": {"type": "object", "properties": {}}}
    for n in ("Read", "Edit", "Bash")
]
# Line-numbered source like a Claude Code Read, over the 2048-byte hold floor.
READ = "".join(f"{i:>6}\tfn adapter_{i}(x: u32) -> u32 {{ x * {i} + {i * 7} }}\n" for i in range(1, 60))
READ_AT = 10  # message index of the Read's tool_result
TURNS = 8  # quiesce_turns=5: held on turns 0-4, matures on turn 5


def convo(quiet: int) -> list[dict]:
    msgs: list[dict] = [{"role": "user", "content": [{"type": "text", "text": "fix the adapter"}]}]
    for k in range(4):
        out = json.dumps([{"path": f"src/f{k}_{j}.rs", "size": j} for j in range(300)])
        msgs += [
            {"role": "assistant", "content": [{"type": "tool_use", "id": f"h{k}", "name": "Bash", "input": {"command": f"ls {k}"}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": f"h{k}", "content": out}]},
        ]
    msgs += [
        {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_r1", "name": "Read", "input": {"file_path": "/repo/src/a.rs"}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_r1", "content": READ}]},
    ]
    for k in range(quiet):
        out = json.dumps([{"id": j, "status": "ok", "name": f"test_{k}_{j}", "ms": j % 7} for j in range(300)])
        msgs += [
            {"role": "assistant", "content": [{"type": "text", "text": f"step {k}"}, {"type": "tool_use", "id": f"q{k}", "name": "Bash", "input": {"command": f"cargo test {k}"}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": f"q{k}", "content": out}]},
        ]
    for m in msgs[-2:]:
        m["content"][-1]["cache_control"] = dict(CC)
    return msgs


def marks(messages: list[dict]) -> list[tuple[int, int]]:
    return [
        (i, bi)
        for i, m in enumerate(messages)
        if isinstance(m.get("content"), list)
        for bi, b in enumerate(m["content"])
        if isinstance(b, dict) and "cache_control" in b
    ]


def read_form(messages: list[dict]) -> str:
    for b in messages[READ_AT].get("content") or []:
        if isinstance(b, dict) and b.get("tool_use_id") == "toolu_r1":
            c = b.get("content")
            return "verbatim" if c == READ else "matured" if "Retrieve original: hash=" in str(c) else "other"
    return "missing"


frozen_seen: list[int] = []
_apply = rm.ReadMaturationManager.apply


def _recording_apply(self, messages, frozen_message_count=0):
    frozen_seen.append(frozen_message_count)
    return _apply(self, messages, frozen_message_count=frozen_message_count)


rm.ReadMaturationManager.apply = _recording_apply

reset_compression_store()
config = ProxyConfig(
    optimize=True,
    read_maturation=True,
    mode="token",
    cache_enabled=True,
    rate_limit_enabled=False,
    cost_tracking_enabled=False,
    log_requests=False,
)
forwarded: list[dict] = []
sent: list[list[dict]] = []
with TestClient(create_app(config)) as client:
    proxy = client.app.state.proxy

    async def _upstream(method, url, headers, body, stream=False, **kwargs):
        msgs = body.get("messages") or []
        forwarded.append(copy.deepcopy(body))
        bp = max((i for i, _ in marks(msgs)), default=-1)
        cached = sum(PrefixCacheTracker._estimate_message_tokens(msgs[: bp + 1])) if bp >= 0 else 0
        return httpx.Response(
            200,
            json={
                "id": "msg_x",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "ok"}],
                "usage": {"input_tokens": 20, "output_tokens": 2, "cache_read_input_tokens": cached, "cache_creation_input_tokens": 0},
            },
        )

    proxy._retry_request = _upstream
    for turn in range(TURNS):
        msgs = convo(turn)
        sent.append(msgs)
        r = client.post(
            "/v1/messages",
            headers={
                "x-api-key": "test-key",
                "anthropic-version": "2023-06-01",
                "x-headroom-session-id": "hrb-probe",
                "content-type": "application/json",
            },
            json={"model": "claude-sonnet-4-5", "max_tokens": 16, "system": SYSTEM, "tools": TOOLS, "messages": copy.deepcopy(msgs)},
        )
        if r.status_code != 200:
            check(False, f"turn {turn}: handler returned {r.status_code}")

kept = True
forms = []
for turn, (msgs, body) in enumerate(zip(sent, forwarded)):
    cin = count_cache_breakpoints(SYSTEM, msgs, TOOLS)
    cout = count_cache_breakpoints(body.get("system"), body["messages"], body.get("tools"))
    forms.append(read_form(body["messages"]))
    kept = kept and marks(body["messages"]) == marks(msgs) and cout["total"] == cin["total"]
    print(
        f"INFO turn {turn}: in_total={cin['total']} out_total={cout['total']} "
        f"in_last_tail={cin['last_marker_tail']} out_last_tail={cout['last_marker_tail']} "
        f"maturation_frozen={frozen_seen[turn] if turn < len(frozen_seen) else '?'} read={forms[-1]}"
    )

check(len(forwarded) == TURNS, f"all {TURNS} turns forwarded")
check(kept, "every turn keeps the client's breakpoints")
check(forms == ["verbatim"] * 5 + ["matured"] * (TURNS - 5), "Read held 5 turns, then matures")
check(
    any(f > READ_AT for f in frozen_seen[:5]),
    "fixture pushes the confirmed prefix past the held Read",
)

# Below the confirmed prefix only a Read this manager saw holding may change:
# an old Read that was cached verbatim before maturation saw it stays put.
from headroom.config import ReadMaturationConfig

rm.ReadMaturationManager.apply = _apply
mgr = rm.ReadMaturationManager(ReadMaturationConfig(enabled=True, quiesce_turns=1, max_hold_turns=25, min_size_bytes=2048))


def _pair(tid: str, path: str) -> list[dict]:
    return [
        {"role": "assistant", "content": [{"type": "tool_use", "id": tid, "name": "Read", "input": {"file_path": path}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tid, "content": READ}]},
    ]


held, unseen = _pair("toolu_held", "/repo/held.rs"), _pair("toolu_unseen", "/repo/unseen.rs")
quiet = [{"role": "assistant", "content": [{"type": "text", "text": "ok"}]}, {"role": "user", "content": [{"type": "text", "text": "go on"}]}]
first = mgr.apply(held, frozen_message_count=0)  # toolu_held seen holding
# Both quiet now and both inside the confirmed prefix; toolu_unseen was never
# seen by maturation, so it must come through untouched.
later = mgr.apply(held + unseen + quiet, frozen_message_count=4)
check(
    first.holding_msg_indices == [1]
    and "Retrieve original: hash=" in later.messages[1]["content"][0]["content"]
    and later.messages[3] == unseen[1],
    "only the held Read matures inside the confirmed prefix",
)
sys.exit(1 if failures else 0)
