#!/usr/bin/env python3
"""Functional probe for the tool-search history repair vendor.

Run with the managed venv's python and PYTHONPATH pointing at a directory
holding the desktop's sitecustomize.py. Verifies, against the INSTALLED wheel,
that strip_unsupported_tool_search_blocks keys on ABSENCE (not defer_loading)
and covers BOTH block shapes Claude Code emits:

  1. bind: the wrapper installed on the 0.37.0 pin. When it did not bind (wheel
     bumped past the pin, or the kill switch is set) prints 'FAIL tsr bound' and
     exits 0 -- the Rust test treats that as self-skip, because a wheel that
     ships the fix upstream leaves this vendor inert by design.
  2. client-side absent: a tool_result whose tool_reference names an ABSENT tool
     is neutralized (the reference is removed) so the request no longer 400s.
     This is the shape the wheel repair does not scan (mcp__headroom__* etc.).
  3. client-side present+deferred: a reference to a present, defer_loading tool is
     KEPT. Deferred is valid per Anthropic's docs.
  4. server-side present+deferred: a tool_search_tool_result referencing a
     present, deferred tool is KEPT (this is the 0.9.8-rc.5 regression: rc.5
     wrongly dropped it).
  5. server-side absent: a tool_search_tool_result referencing an absent tool is
     DROPPED (the wheel's own, correct, behavior with tools UNFILTERED).
  6. isolation: with HEADROOM_TOOL_SEARCH_REPAIR=0 the client-side absent
     reference SURVIVES (the wheel alone misses it), proving the vendor is what
     adds client-side coverage.
"""

import os
import subprocess
import sys

_SEARCH_TOOL = {"type": "tool_search_tool_regex", "name": "tool_search_tool_regex"}


def server_side(tool):
    return [
        {
            "role": "assistant",
            "content": [
                {
                    "type": "server_tool_use",
                    "id": "srv_1",
                    "name": "tool_search_tool_regex",
                    "input": {"pattern": "x"},
                },
                {
                    "type": "tool_search_tool_result",
                    "tool_use_id": "srv_1",
                    "content": {
                        "type": "tool_search_tool_search_result",
                        "tool_references": [{"type": "tool_reference", "tool_name": tool}],
                    },
                },
            ],
        }
    ]


def client_side(tool):
    return [
        {
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "ToolSearch", "input": {"q": "x"}}
            ],
        },
        {
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": [{"type": "tool_reference", "tool_name": tool}],
                }
            ],
        },
    ]


def ref_names(messages):
    names = []
    for m in messages:
        c = m.get("content") if isinstance(m, dict) else None
        if not isinstance(c, list):
            continue
        for b in c:
            if not isinstance(b, dict):
                continue
            if b.get("type") == "tool_search_tool_result":
                cc = b.get("content")
                refs = cc.get("tool_references") if isinstance(cc, dict) else cc
                if isinstance(refs, list):
                    names += [
                        r.get("tool_name") or r.get("name")
                        for r in refs
                        if isinstance(r, dict)
                    ]
            elif b.get("type") == "tool_result" and isinstance(b.get("content"), list):
                names += [
                    r.get("tool_name") or r.get("name")
                    for r in b["content"]
                    if isinstance(r, dict) and r.get("type") == "tool_reference"
                ]
    return names


def run(strip, messages, tools):
    out, removed = strip([dict(m) for m in messages], tools)
    return ref_names(out), removed


def main() -> int:
    import sitecustomize as sc  # noqa: F401  (auto-imported anyway)
    from headroom.proxy import helpers

    strip = helpers.strip_unsupported_tool_search_blocks
    vendor_on = hasattr(sc, "_hd_tsr_client_side")

    present_deferred = [_SEARCH_TOOL, {"name": "CronCreate", "defer_loading": True}, {"name": "Bash"}]
    absent_tools = [_SEARCH_TOOL, {"name": "Bash"}]

    if os.environ.get("HEADROOM_TOOL_SEARCH_REPAIR") == "0":
        # Control: vendor must be OFF and the client-side absent reference must
        # SURVIVE (the wheel alone does not scan that shape).
        if vendor_on:
            print("FAIL kill switch did not disable the vendor")
            return 1
        names, _ = run(strip, client_side("GhostTool"), absent_tools)
        if "GhostTool" not in names:
            print("FAIL control: wheel unexpectedly neutralized client-side ref")
            return 1
        print("OK control: client-side absent reference survives without the vendor")
        return 0

    if not vendor_on:
        print("FAIL tsr bound")
        return 0

    # 2. client-side absent -> neutralized
    names, removed = run(strip, client_side("GhostTool"), absent_tools)
    if "GhostTool" in names or removed <= 0:
        print("FAIL client-side absent not neutralized:", names, "removed", removed)
        return 1

    # 3. client-side present+deferred -> kept
    names, removed = run(strip, client_side("CronCreate"), present_deferred)
    if "CronCreate" not in names:
        print("FAIL client-side deferred+present wrongly dropped:", names)
        return 1

    # 4. server-side present+deferred -> kept (rc.5 would have dropped it)
    names, removed = run(strip, server_side("CronCreate"), present_deferred)
    if "CronCreate" not in names:
        print("FAIL server-side deferred+present wrongly dropped (rc.5 regression):", names)
        return 1

    # 5. server-side absent -> dropped
    names, removed = run(strip, server_side("GhostTool"), absent_tools)
    if "GhostTool" in names or removed <= 0:
        print("FAIL server-side absent not dropped:", names, "removed", removed)
        return 1

    # 6. isolation: kill switch reverts client-side coverage
    env = dict(os.environ)
    env["HEADROOM_TOOL_SEARCH_REPAIR"] = "0"
    control = subprocess.run(
        [sys.executable, os.path.abspath(__file__)],
        env=env,
        capture_output=True,
        text=True,
        timeout=120,
    )
    if control.returncode != 0 or "OK control" not in control.stdout:
        print("FAIL kill-switch control run:", control.stdout.strip(), control.stderr[-300:])
        return 1

    print(
        "OK tool-search repair: client-side absent neutralized, deferred kept "
        "(both shapes), server-side absent dropped, kill switch reverts"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
