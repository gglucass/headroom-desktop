#!/usr/bin/env python3
"""Functional probe for the tool-reference-400 "start a new session" hint vendor.

Run with the managed venv's python and PYTHONPATH pointing at a directory holding
the desktop's sitecustomize.py. Exercises the transform (`_hd_hint_apply`) the
vendor exposes, against the installed wheel's starlette:

  1. bind: the vendor installed on the 0.37.0 pin. Not bound -> 'FAIL hint bound',
     exit 0 (Rust test self-skips: a wheel that ships the hint leaves it inert).
  2. a 400 whose body carries the signature gets the Headroom hint appended, the
     body stays valid JSON, and content-length is recomputed.
  3. idempotent: applying twice leaves exactly one hint.
  4. scoped: a non-400, and a 400 without the signature, are returned untouched
     (same object), and a StreamingResponse (no .body) is never rewritten.
  5. framing-agnostic: an SSE-framed error body is hinted too.
"""

import json
import sys


def main() -> int:
    import sitecustomize as sc

    from starlette.responses import Response, StreamingResponse

    if not hasattr(sc, "_hd_hint_apply"):
        print("FAIL hint bound")
        return 0
    apply = sc._hd_hint_apply

    body = json.dumps(
        {
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "Tool reference 'mcp__x__y' not found in available tools",
            },
        }
    ).encode()

    # 2. appended + still valid JSON + content-length fixed
    out = apply(Response(content=body, status_code=400, media_type="application/json"))
    ob = bytes(out.body)
    if b"Headroom:" not in ob:
        print("FAIL hint not appended")
        return 1
    data = json.loads(ob.decode())
    if "start a new session" not in data["error"]["message"]:
        print("FAIL hint not inside the message:", data["error"]["message"][:120])
        return 1
    if out.headers.get("content-length") != str(len(ob)):
        print("FAIL content-length", out.headers.get("content-length"), "vs", len(ob))
        return 1

    # 3. idempotent
    twice = apply(out)
    if bytes(twice.body).count(b"Headroom:") != 1:
        print("FAIL not idempotent")
        return 1

    # 4. scoped: non-400 and unrelated-400 untouched (identity); no crash on a stream
    r200 = Response(content=body, status_code=200)
    if apply(r200) is not r200:
        print("FAIL touched a non-400")
        return 1
    unrelated = Response(content=b'{"error":{"message":"overloaded"}}', status_code=400)
    if apply(unrelated) is not unrelated:
        print("FAIL touched an unrelated 400")
        return 1

    async def _gen():
        yield b""

    stream = StreamingResponse(_gen(), status_code=400)
    if apply(stream) is not stream:
        print("FAIL touched a StreamingResponse")
        return 1

    # 5. SSE-framed error body is hinted too
    sse = (
        b'event: error\ndata: {"type":"error","error":{"message":'
        b'"Tool reference \'z\' not found in available tools"}}\n\n'
    )
    outs = apply(Response(content=sse, status_code=400, media_type="text/event-stream"))
    if b"Headroom:" not in bytes(outs.body):
        print("FAIL SSE-framed body not hinted")
        return 1

    print("OK tool-ref hint: appended, valid, content-length fixed, idempotent, scoped, SSE-safe")
    return 0


if __name__ == "__main__":
    sys.exit(main())
