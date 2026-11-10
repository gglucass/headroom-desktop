"""Exercise the Codex TLS retry vendor against the managed wheel."""

import asyncio
import errno
import ssl

import httpx
import sitecustomize


async def main():
    assert "codex_tls_retry" in sitecustomize._hd_bound

    async def fail(_client, _request, *args, **kwargs):
        raise ssl.SSLError(errno.EPERM, "Operation not permitted")

    sitecustomize._hd_tls_orig_send = fail
    async with httpx.AsyncClient() as client:
        codex = client.build_request(
            "POST", "https://chatgpt.com/backend-api/codex/responses"
        )
        try:
            await client.send(codex)
        except httpx.ConnectError as exc:
            assert exc.request is codex
        else:
            raise AssertionError("Codex TLS EPERM did not become retryable")

        unrelated = client.build_request("POST", "https://example.com/responses")
        try:
            await client.send(unrelated)
        except ssl.SSLError:
            pass
        else:
            raise AssertionError("Unrelated TLS error was changed")

    print("OK Codex TLS EPERM is retryable")


asyncio.run(main())
