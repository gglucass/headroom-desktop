"""Offline checks for the desktop cache-integrity observer; no Sentry/provider calls.

Run with managed Python and --sitecustomize pointing to the candidate injection.
--broken-lineage disables the wheel's transient-system lineage (HEADROOM_TRANSIENT_SYSTEM_LINEAGE=0,
native since 0.41.0) to verify detection of the original defect.
"""

import argparse
import copy
import contextvars
import json
import os
from pathlib import Path
import runpy
import socket
from unittest.mock import patch


def deny_network(*args, **kwargs):
    raise AssertionError("cache-integrity checks must stay offline")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sitecustomize", type=Path, required=True)
    parser.add_argument("--broken-lineage", action="store_true")
    parser.add_argument("--capture", type=Path, help="Optional saved three-request incident capture")
    args = parser.parse_args()
    socket.socket.connect = socket.socket.connect_ex = deny_network
    socket.create_connection = socket.getaddrinfo = deny_network
    os.environ["LITELLM_LOCAL_MODEL_COST_MAP"] = "True"
    os.environ["HEADROOM_SDK"] = "headroom-desktop-proxy"
    if args.broken_lineage:
        os.environ["HEADROOM_TRANSIENT_SYSTEM_LINEAGE"] = "0"
    runpy.run_path(str(args.sitecustomize))

    from headroom.cache import prefix_tracker as pt
    from headroom.proxy import cost
    from headroom.proxy.prometheus_metrics import PrometheusMetrics

    fixture = runpy.run_path(str(Path(__file__).with_name("reproduce-transient-system-cache.py")))
    previous, forwarded, current, drifted = fixture["synthetic_pair"]()
    metrics = PrometheusMetrics(stateless=True)

    def stats():
        return cost.build_prefix_cache_stats(metrics, None)["desktop_integrity"]

    def resolve(store, messages, affinity="same"):
        # cache_ttl_seconds as the 0.41.0 Anthropic handler passes it: a wrapper
        # without it raised TypeError on every Anthropic request.
        return store.resolve_tracker("session", "anthropic", messages, affinity,
                                     cache_ttl_seconds=3600)

    def seeded(floor=4):
        store = pt.SessionTrackerStore()
        prior = resolve(store, previous)
        prior.update_from_response(0, 20_000, copy.deepcopy(forwarded),
                                   [20_000 // floor] * 4, copy.deepcopy(previous))
        assert prior.get_frozen_message_count() == floor
        return store, prior

    def complete(tracker, original=current, output=drifted, reads=100, writes=19_900):
        tracker.update_from_response(reads, writes, copy.deepcopy(output),
                                     original_messages=copy.deepcopy(original))

    before = stats()["count"]
    store, prior = seeded()
    selected = resolve(store, current)
    untouched = copy.deepcopy(drifted)
    complete(selected)
    assert drifted == untouched, "observer mutated request data"
    assert selected.get_last_forwarded_messages() == drifted
    assert stats()["count"] == before + 1
    assert stats()["kind"] == ("tracker_reset" if args.broken_lineage else "prefix_rewrite")
    assert stats()["first_changed_message"] == 2
    assert stats()["stable_messages"] == 3
    assert "original source" not in json.dumps(stats())
    print("PASS: confirmed prefix drift detected with numeric-only diagnostics")

    before = stats()["count"]
    store, prior = seeded()
    selected = resolve(store, current)
    repaired = pt.overlay_cached_prefix(copy.deepcopy(drifted), current, previous, forwarded,
                                        confirmed_frozen_count=4)
    complete(selected, output=repaired)
    assert stats()["count"] == before, "stable replay raised an alarm"

    store, prior = seeded(floor=2)
    complete(resolve(store, current))
    assert stats()["count"] == before, "beyond-floor compression raised an alarm"

    # The provider's previous RESPONSE is appended to tracker history, but was
    # not cached input. A rough token floor can overshoot into that last reply.
    store, prior = seeded()
    response = {"role": "assistant", "content": "fresh response " * 200}
    prior.update_from_response(0, 20_000, forwarded + [response], [100] * 5,
                               previous + [response])
    incoming = previous + [response, {"role": "user", "content": "continue"}]
    optimized = copy.deepcopy(forwarded + incoming[4:])
    optimized[4]["content"] = "compressed response"
    complete(resolve(store, incoming), original=incoming, output=optimized)
    assert stats()["count"] == before, "new assistant response treated as previously cached"

    store, prior = seeded()
    complete(resolve(store, current), reads=20_000, writes=500)
    store, prior = seeded()
    complete(resolve(store, current), reads=0, writes=0)
    assert stats()["count"] == before, "cache hit or missing usage raised an alarm"

    store, prior = seeded()
    changed = copy.deepcopy(current)
    changed[0]["content"] = "A different task"
    complete(resolve(store, changed), original=changed)
    store, prior = seeded()
    complete(resolve(store, current, "different-tools"))
    store, prior = seeded()
    branch = copy.deepcopy(current)
    branch[2]["content"][0]["content"] = "changed original tool result"
    complete(resolve(store, branch), original=branch)
    assert stats()["count"] == before, "changed history, affinity, or sibling raised an alarm"

    store, prior = seeded()
    prior.config.cache_ttl_seconds = 1
    prior._last_activity -= 2
    complete(resolve(store, current))
    store, prior = seeded()
    prior.config.cache_ttl_seconds = 1
    with patch("time.monotonic", return_value=100):
        selected = resolve(store, current)
    with patch("time.monotonic", return_value=102):
        complete(selected)
    assert stats()["count"] == before, "expired cache raised an alarm"
    print("PASS: stable replay, beyond-floor compression, hits, history, affinity, TTL")

    # Two concurrent requests must not borrow each other's evidence.
    store_a, _ = seeded()
    store_b, _ = seeded()
    a, b = contextvars.copy_context(), contextvars.copy_context()
    tracker_a = a.run(resolve, store_a, current)
    tracker_b = b.run(resolve, store_b, current)
    b.run(complete, tracker_b, current, repaired)
    a.run(complete, tracker_a)
    assert stats()["count"] == before + 1

    # Observability failure must not stop the tracker from recording a response.
    store, _ = seeded()
    selected = resolve(store, current)
    with patch.object(pt, "_canonicalize_for_prefix_compare", side_effect=RuntimeError("probe")):
        complete(selected)
    assert selected.get_last_forwarded_messages() == drifted
    assert stats()["count"] == before + 1
    print("PASS: concurrent contexts isolated; observer failure does not affect response state")

    from headroom.proxy import server
    assert server.build_prefix_cache_stats(metrics, None)["desktop_integrity"] == stats()
    print("PASS: the server's /stats helper exposes the same diagnostic snapshot")

    if args.capture:
        old, new = json.loads(args.capture.read_text())[-2:]
        store = pt.SessionTrackerStore()
        prior = resolve(store, old["request_messages"])
        prior.update_from_response(0, 1_000_000, old["compressed_messages"],
                                   original_messages=old["request_messages"])
        before = stats()["count"]
        selected = resolve(store, new["request_messages"])
        # Actual cache counts for the captured September 7 failure.
        complete(selected, new["request_messages"], new["compressed_messages"], 7259, 491260)
        assert stats()["count"] == before + 1
        assert stats()["first_changed_message"] == 3
        assert stats()["stable_messages"] == 861
        print("PASS: captured Fable failure detected at historical message 3")


if __name__ == "__main__":
    main()
