#!/usr/bin/env python3
"""Functional probe for the proxied guarded-upstream vendor.

Run with the managed venv's python and PYTHONPATH pointing at a directory holding
the desktop's sitecustomize.py, with HEADROOM_ALLOW_PROXIED_GUARDED_UPSTREAMS set
as the desktop sets it. Offline: DNS is stubbed and the proxy is never dialled.

  1. bind: the vendor wrapped the 0.39.0 refusing transport. Not bound ->
     'FAIL pgu bound', exit 0 (the Rust test self-skips).
  2. a guarded x-headroom-base-url upstream on a proxy route is forwarded.
  3. a direct route still pins (PinnedAddressBackend), and a transport with no
     pool still refuses.
  4. the guard still rejects a name that resolves to an internal address.

With the flag off, prints 'REFUSED' for check 2 and exits 0.
"""

import asyncio
import os
import socket

import httpx


def answer(ip):
    return lambda *a, **k: [(None, None, None, None, (ip, 443))]


async def main() -> None:
    import sitecustomize  # noqa: F401

    from headroom.proxy import upstream_guard

    try:
        from headroom.proxy import upstream_pinning as up
    except ImportError:  # pre-0.39.0 wheel: nothing to vendor
        print("FAIL pgu bound")
        return

    flag_on = os.environ.get("HEADROOM_ALLOW_PROXIED_GUARDED_UPSTREAMS") == "1"
    bound = up.GuardedUpstreamRefusingTransport.handle_async_request.__name__ == "_hd_pgu_handle"
    if flag_on and not bound:
        print("FAIL pgu bound")
        return

    socket.getaddrinfo = answer("8.8.8.8")
    upstream_guard.clear_validated_addresses()
    assert upstream_guard.is_safe_upstream_url("https://api.x.ai/v1")

    client = up.install_upstream_pinning(httpx.AsyncClient(proxy="http://proxy.internal:3128"))
    transport = client._transport_for_url(httpx.URL("https://api.x.ai/v1"))
    hosts = []

    async def record(request):
        hosts.append(request.url.host)
        return httpx.Response(200)

    transport._inner.handle_async_request = record
    try:
        await transport.handle_async_request(httpx.Request("POST", "https://api.x.ai/v1"))
    except up.UnpinnableUpstreamError:
        print("REFUSED")
        if flag_on:
            raise SystemExit("FAIL proxied guarded upstream refused")
        return
    finally:
        await client.aclose()
    assert hosts == ["api.x.ai"], hosts
    print("OK   proxied guarded upstream forwarded")

    direct = up.install_upstream_pinning(httpx.AsyncClient())
    assert isinstance(direct._transport._pool._network_backend, up.PinnedAddressBackend)
    await direct.aclose()
    poolless = up.GuardedUpstreamRefusingTransport(httpx.MockTransport(record), "no pool")
    try:
        await poolless.handle_async_request(httpx.Request("POST", "https://api.x.ai/v1"))
        raise SystemExit("FAIL poolless transport forwarded")
    except up.UnpinnableUpstreamError:
        pass
    print("OK   direct route pinned, poolless still refused")

    socket.getaddrinfo = answer("10.0.0.5")
    upstream_guard.clear_validated_addresses()
    assert not upstream_guard.is_safe_upstream_url("https://internal.example/v1")
    print("OK   internal answer still refused")


if __name__ == "__main__":
    asyncio.run(main())
