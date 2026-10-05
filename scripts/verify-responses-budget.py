"""Offline regression for the desktop Responses compression budget.

Uses the installed wheel's adapter and executors, with simulated slow inference.
No model downloads, provider requests, or Sentry events.
"""

import argparse
import asyncio
import copy
import os
from pathlib import Path
import re
import runpy
import socket
import threading
import time
from types import MethodType, SimpleNamespace


def deny_network(*args, **kwargs):
    raise AssertionError("budget regression must stay offline")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sitecustomize", type=Path, required=True)
    parser.add_argument("--disabled", action="store_true")
    args = parser.parse_args()
    socket.socket.connect = socket.socket.connect_ex = deny_network
    socket.create_connection = socket.getaddrinfo = deny_network
    os.environ["LITELLM_LOCAL_MODEL_COST_MAP"] = "True"
    os.environ["HEADROOM_SDK"] = "headroom-desktop-proxy"
    os.environ["HEADROOM_RESPONSES_SHARED_BUDGET"] = "0" if args.disabled else "1"
    os.environ["HEADROOM_COMPRESSION_DEADLINE_MS"] = "120"

    from headroom.proxy.server import ProxyConfig, create_app
    from headroom.proxy.handlers import openai as oa
    from headroom.transforms.content_router import (
        ContentRouter, RouterCompressionResult, CompressionStrategy,
    )

    calls = []
    delay = 0.025
    def shortened(content):
        # Retain the real batch envelope/tag markers while shortening each unit.
        return re.sub(r"(item\d+word7)(?: item\d+word\d+)+", r"\1", content)

    # Replace only inference; retain the real adapter, batches, and executor.
    def inference(self, content, *args, **kwargs):
        calls.append(content)
        time.sleep(delay)
        return RouterCompressionResult(
            original=content, compressed=shortened(content),
            strategy_used=CompressionStrategy.KOMPRESS,
        )

    ContentRouter.compress = inference
    runpy.run_path(str(args.sitecustomize))
    if not args.disabled:
        assert ContentRouter.compress.__name__ == "_hd_cb_router_compress", "shim did not bind"
    scope = oa.OpenAIHandlerMixin._compress_openai_responses_payload_in_executor.__globals__

    class TokenCounter:
        def count_text(self, text):
            return len(text.split())

    def make_proxy():
        proxy = create_app(ProxyConfig(
            optimize=False, cache_enabled=False, rate_limit_enabled=False,
            cost_tracking_enabled=False, log_requests=False,
            ccr_inject_tool=False, ccr_handle_responses=False,
            ccr_context_tracking=False, image_optimize=False,
            compression_max_workers=4,
        )).state.proxy
        proxy.openai_pipeline = SimpleNamespace(transforms=[ContentRouter()])
        proxy.openai_provider = SimpleNamespace(get_token_counter=lambda _: TokenCounter())
        def compress(self, payload, **kwargs):
            return self._compress_openai_responses_live_text_units_with_router(
                payload, model=kwargs["model"], request_id=kwargs["request_id"],
            )
        proxy._compress_openai_responses_payload = MethodType(compress, proxy)
        return proxy

    def payload(count, words=180):
        return {"model": "gpt-5", "input": [
            {"type": "function_call_output", "call_id": f"call_{i}",
             "output": " ".join(f"item{i}word{j}" for j in range(words))}
            for i in range(count)
        ]}

    async def run(proxy, source, timeout=0.5):
        return await proxy._compress_openai_responses_payload_in_executor(
            copy.deepcopy(source), model="gpt-5", request_id="offline-budget", timeout=timeout,
        )

    async def checks():
        nonlocal delay
        for parallelism, words, count in ((1, 180, 100), (4, 180, 100),
                                          (1, 25, 1000), (4, 25, 1000)):
            os.environ["HEADROOM_TOOL_OUTPUT_COMPRESSION_PARALLELISM"] = str(parallelism)
            proxy = make_proxy()
            source = payload(count, words)
            calls.clear()
            started = time.perf_counter()
            try:
                # The fixed budget is min(120ms, 0.75 * timeout) either way; a
                # 2s outer timeout only gives a starved runner slack to merge
                # (rc10 CI: 0.42s for 100 units). `changed < count` still proves
                # the budget cut the work. The control needs the tight timeout.
                result = await run(proxy, source, timeout=0.5 if args.disabled else 2.0)
            except asyncio.TimeoutError:
                assert args.disabled, "fixed request still timed out"
                assert proxy._compression_timed_out_in_flight > 0
                # Poll, not one fixed 80ms sleep: on a loaded CI runner the
                # worker thread can go unscheduled that long (failed rc.5 CI).
                n = len(calls)
                for _ in range(100):
                    await asyncio.sleep(0.02)
                    if len(calls) > n:
                        break
                assert len(calls) > n, "control did not reproduce continuing work"
                print(f"PASS control: parallelism={parallelism}, words={words}, "
                      "timeout with continuing worker")
            else:
                assert not args.disabled, "control unexpectedly completed"
                elapsed = time.perf_counter() - started
                changed = sum(a != b for a, b in zip(source["input"], result[0]["input"]))
                assert 0 < changed < count, (changed, len(calls))
                assert result[2] > 0, "completed compression was discarded"
                for original, output in zip(source["input"], result[0]["input"]):
                    assert output["output"] in (original["output"], shortened(original["output"]))
                    assert output["call_id"] == original["call_id"]
                assert proxy._compression_leaked_threads == 0
                assert proxy._compression_timed_out_in_flight == 0
                n = len(calls)
                await asyncio.sleep(0.08)
                assert len(calls) == n, "work continued after response"
                assert await proxy._run_compression_in_executor(lambda: 42, timeout=0.2) == 42
                print(f"PASS fixed: parallelism={parallelism}, words={words}, {elapsed:.3f}s, "
                      f"{changed}/{count} compressed, {result[2]} estimated tokens saved, no worker debt")
            finally:
                proxy._compression_executor.shutdown(wait=True)

        if args.disabled:
            return

        # Fast requests must remain byte-identical to the unwrapped adapter.
        delay = 0
        proxy = make_proxy()
        source = payload(3)
        fixed = await run(proxy, source)
        control_proxy = make_proxy()
        control = control_proxy._compress_openai_responses_payload(copy.deepcopy(source),
            model="gpt-5", request_id="offline-control")
        assert fixed[0] == control[0] and fixed[2] == control[2]
        print("PASS: fast output and estimated savings identical to control")

        os.environ["HEADROOM_COMPRESSION_DEADLINE_MS"] = "0"
        original = scope["_hd_cb_responses"]
        async def unbounded(self, payload, **kwargs):
            assert scope["_hd_cb_budget"].get() is None
            return payload
        scope["_hd_cb_responses"] = unbounded
        try:
            assert await run(proxy, source) == source
        finally:
            scope["_hd_cb_responses"] = original
            os.environ["HEADROOM_COMPRESSION_DEADLINE_MS"] = "120"
        print("PASS: existing zero-deadline opt-out leaves requests unbounded")

        delay = 0.025
        cancelled_proxy = make_proxy()
        calls.clear()
        task = asyncio.create_task(run(cancelled_proxy, payload(100)))
        for _ in range(100):
            if calls:
                break
            await asyncio.sleep(0.005)
        assert calls, "cancellation test never entered the worker"
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass
        # Poll, not one fixed 150ms sleep (failed 0fe6042 CI on a loaded
        # runner); "drained" is proven by the work stopping short of all 100.
        for _ in range(100):
            if cancelled_proxy._compression_in_flight == 0:
                break
            await asyncio.sleep(0.02)
        assert cancelled_proxy._compression_in_flight == 0
        assert cancelled_proxy._compression_timed_out_in_flight == 0
        assert len(calls) < 100, "cancelled request ran every unit"
        cancelled_proxy._compression_executor.shutdown(wait=True)
        print("PASS: request cancellation drains remaining work")

        # Context isolation across both executor hops and cancellation signal.
        async def isolated(cancel):
            event = threading.Event()
            budget = (time.perf_counter() + 2, event)
            token = scope["_hd_cb_budget"].set(budget)
            try:
                def nested():
                    return oa._openai_responses_unit_executor().submit(
                        lambda: scope["_hd_cb_budget"].get()).result()
                assert await proxy._run_compression_in_executor(nested, timeout=1) is budget
                if cancel:
                    event.set()
                    assert proxy.openai_pipeline.transforms[0].compress("untouched").compressed == "untouched"
            finally:
                scope["_hd_cb_budget"].reset(token)
        await asyncio.gather(isolated(True), isolated(False))
        assert scope["_hd_cb_budget"].get() is None

        # Native Kompress receives one shared start time, including nested calls.
        starts = []
        class Compressor:
            _deadline_s = 20
            def _passthrough(self, text, count):
                return text
        def native(self, content, **kwargs):
            starts.append(kwargs["_deadline_started_at"])
            return content
        bounded = scope["_hd_cb_kompress_wrapper"](native)
        event = threading.Event()
        deadline = time.perf_counter() + 1
        token = scope["_hd_cb_budget"].set((deadline, event))
        try:
            bounded(Compressor(), "a")
            bounded(Compressor(), "b", _deadline_started_at=deadline - 30)
            assert starts == [deadline - 20, deadline - 30]
            bounded(Compressor(), content="keyword")
            def native_batch(self, contents, **kwargs):
                return [bounded(self, text, **kwargs) for text in contents]
            batch = scope["_hd_cb_kompress_wrapper"](native_batch, batch=True)
            assert batch(Compressor(), contents=["a", "b"]) == ["a", "b"]
            assert starts[-2:] == [deadline - 20, deadline - 20]
            event.set()
            assert bounded(Compressor(), "unchanged") == "unchanged"
            assert batch(Compressor(), contents=["a", "b"]) == ["a", "b"]
            assert len(starts) == 5
        finally:
            scope["_hd_cb_budget"].reset(token)
        print("PASS: executor context isolation, cancellation, native deadline propagation")
        proxy._compression_executor.shutdown(wait=True)
        control_proxy._compression_executor.shutdown(wait=True)

    asyncio.run(checks())


if __name__ == "__main__":
    main()
