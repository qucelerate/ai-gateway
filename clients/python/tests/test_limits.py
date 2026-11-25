"""A successful body over the response limit is a DecodeError, and an error
body is read only up to 1 MiB, sync and async."""

import asyncio
import json
import socket
import threading

import pytest

from qucelerate_ai_gateway import AsyncClient, Client, DecodeError, HTTPError


def read_request(connection: socket.socket) -> None:
    data = b""
    while b"\r\n\r\n" not in data:
        data += connection.recv(65536)
    head, _, rest = data.partition(b"\r\n\r\n")
    length = next(int(line.split(b":")[1]) for line in head.split(b"\r\n") if line.lower().startswith(b"content-length"))
    while len(rest) < length:
        rest += connection.recv(65536)


def serve(status: int, body: bytes) -> str:
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen()

    def run() -> None:
        with listener:
            for _ in range(1):
                connection, _ = listener.accept()
                with connection:
                    read_request(connection)
                    head = f"HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {len(body)}\r\n\r\n"
                    try:
                        connection.sendall(head.encode() + body)
                    except OSError:
                        pass

    threading.Thread(target=run, daemon=True).start()
    return f"http://127.0.0.1:{listener.getsockname()[1]}"


HELLO = [{"role": "user", "content": "hi"}]
BIG = json.dumps({"id": "x" * 4096}).encode()
# A problem that only parses whole: cut at 1 MiB it is not a problem.
HUGE_PROBLEM = json.dumps({"code": "x", "detail": "y" * (2 << 20)}).encode()


def test_sync_limits():
    with Client(serve(200, BIG), "gw_key", max_response_bytes=1024) as client:
        with pytest.raises(DecodeError, match="1024"):
            client.chat("m", HELLO)
    with Client(serve(500, HUGE_PROBLEM), "gw_key") as client:
        with pytest.raises(HTTPError):
            client.chat("m", HELLO)


def test_async_limits():
    async def run() -> None:
        async with AsyncClient(serve(200, BIG), "gw_key", max_response_bytes=1024) as client:
            with pytest.raises(DecodeError, match="1024"):
                await client.embeddings("m", ["a"])
        async with AsyncClient(serve(500, HUGE_PROBLEM), "gw_key") as client:
            with pytest.raises(HTTPError):
                await client.chat("m", HELLO)

    asyncio.run(run())


def test_the_limit_must_be_positive():
    from qucelerate_ai_gateway import ConfigError

    with pytest.raises(ConfigError):
        Client("https://x", "gw_key", max_response_bytes=0)
