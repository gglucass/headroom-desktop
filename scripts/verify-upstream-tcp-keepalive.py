#!/usr/bin/env python3
"""Functional probe for the upstream TCP keepalive vendor.

Run with the managed venv's python and PYTHONPATH pointing at a directory holding
the desktop's sitecustomize.py, with HEADROOM_SDK=headroom-desktop-proxy and
HEADROOM_UPSTREAM_TCP_KEEPALIVE_SECONDS as the test sets it. Offline: the only
connection is to a local server.

  1. bind: the install_upstream_pinning that server.py's startup() calls is the
     vendor's. Not bound -> 'FAIL tka bound', exit 0 (the Rust test self-skips).
  2. a real upstream socket opened by a client built that way carries
     SO_KEEPALIVE and the configured idle time, and the dial is still pinned.
  3. a proxy mount dials through keepalive too, beneath the pinning wrapper.

With HEADROOM_UPSTREAM_TCP_KEEPALIVE_SECONDS=0 prints 'FAIL tka bound', then
'OFF  socket not probed' once the same request's socket shows no keepalive.
"""

import asyncio
import os
import socket

import httpcore
import httpx

IDLE = getattr(socket, "TCP_KEEPIDLE", None) or getattr(socket, "TCP_KEEPALIVE", None)


class Recorder(httpcore.AsyncNetworkBackend):
    def __init__(self):
        self._real = httpcore.AnyIOBackend()
        self.sockets = []

    async def connect_tcp(self, host, port, timeout=None, local_address=None, socket_options=None):
        stream = await self._real.connect_tcp(
            host, port, timeout=timeout, local_address=local_address, socket_options=socket_options
        )
        self.sockets.append(stream.get_extra_info("socket"))
        return stream

    async def connect_unix_socket(self, path, timeout=None, socket_options=None):
        raise NotImplementedError

    async def sleep(self, seconds):
        await asyncio.sleep(seconds)


async def serve():
    async def handle(reader, writer):
        try:
            while True:
                await reader.readuntil(b"\r\n\r\n")
                writer.write(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                await writer.drain()
        except (asyncio.IncompleteReadError, ConnectionError):
            writer.close()

    return await asyncio.start_server(handle, "127.0.0.1", 0)


async def one_socket(client, server):
    """The socket the client opens for one request, recorded beneath every wrapper."""
    pool = client._transport._pool
    layer = pool._network_backend
    while hasattr(layer, "_inner") and hasattr(layer._inner, "_inner"):
        layer = layer._inner
    recorder = Recorder()
    if hasattr(layer, "_inner"):
        layer._inner = recorder
    else:
        pool._network_backend = recorder
    port = server.sockets[0].getsockname()[1]
    assert (await client.get(f"http://127.0.0.1:{port}/v1/messages")).status_code == 200
    assert len(recorder.sockets) == 1, recorder.sockets
    return recorder.sockets[0]


async def main() -> None:
    import sitecustomize  # noqa: F401

    from headroom.proxy import server as srv
    from headroom.proxy import upstream_pinning as up

    idle = int(os.environ.get("HEADROOM_UPSTREAM_TCP_KEEPALIVE_SECONDS", "30"))
    server = await serve()
    bound = srv.install_upstream_pinning.__name__ == "_hd_tka_install"
    if not bound:
        print("FAIL tka bound")
        if idle == 0:
            client = srv.install_upstream_pinning(httpx.AsyncClient(trust_env=False))
            sock = await one_socket(client, server)
            assert sock.getsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE) == 0
            await client.aclose()
            print("OFF  socket not probed")
        server.close()
        return

    client = srv.install_upstream_pinning(httpx.AsyncClient(trust_env=False))
    backend = client._transport._pool._network_backend
    assert isinstance(backend, up.PinnedAddressBackend), type(backend)
    assert type(backend._inner).__name__ == "_HdKeepaliveBackend", type(backend._inner)
    sock = await one_socket(client, server)
    assert sock.getsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE) != 0
    if IDLE is not None:
        assert sock.getsockopt(socket.IPPROTO_TCP, IDLE) == idle
    if hasattr(socket, "TCP_KEEPINTVL"):
        assert sock.getsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPINTVL) == 10
    if hasattr(socket, "TCP_KEEPCNT"):
        assert sock.getsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPCNT) == 6
    await client.aclose()
    print(f"OK   direct upstream socket probed after {idle}s idle, dial still pinned")

    proxied = srv.install_upstream_pinning(
        httpx.AsyncClient(proxy="http://proxy.internal:3128", trust_env=False)
    )
    mounts = [t for t in proxied._mounts.values() if t is not None]
    assert mounts, "no proxy mount"
    for mount in mounts:
        inner = getattr(mount, "_inner", mount)
        assert type(inner._pool._network_backend).__name__ == "_HdKeepaliveBackend"
    await proxied.aclose()
    print("OK   proxy mount dials through keepalive")
    server.close()


asyncio.run(main())
