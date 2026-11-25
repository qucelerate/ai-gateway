"""An error body cut off mid-read is a TransportError, sync and async."""

import asyncio
import socket
import threading

import pytest

from qucelerate_ai_gateway import AsyncClient, Client, TransportError


@pytest.fixture
def truncating_server():
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen()
    stop = threading.Event()

    def serve() -> None:
        listener.settimeout(0.1)
        while not stop.is_set():
            try:
                connection, _ = listener.accept()
            except OSError:
                continue
            with connection:
                connection.recv(65536)
                connection.sendall(
                    b"HTTP/1.1 503 X\r\ncontent-type: application/problem+json\r\n"
                    b"content-length: 1000\r\n\r\n{\"code\":"
                )

    thread = threading.Thread(target=serve, daemon=True)
    thread.start()
    yield f"http://127.0.0.1:{listener.getsockname()[1]}"
    stop.set()
    thread.join()
    listener.close()


def test_sync_stream(truncating_server):
    with Client(truncating_server, "gw_key") as client:
        with pytest.raises(TransportError):
            client.chat_stream("m", [{"role": "user", "content": "hi"}])


def test_async_stream(truncating_server):
    async def run() -> None:
        async with AsyncClient(truncating_server, "gw_key") as client:
            with pytest.raises(TransportError):
                await client.chat_stream("m", [{"role": "user", "content": "hi"}])

    asyncio.run(run())
