"""Files and batches calls: paths, headers, bodies and decoding, sync and async."""

import asyncio
import json

import httpx
import pytest

from qucelerate_ai_gateway import AsyncClient, Batch, Client, ConfigError, ProblemError

BATCH = {
    "id": "batch_1", "object": "batch", "endpoint": "/v1/chat/completions", "input_file_id": "file_1",
    "completion_window": "24h", "status": "in_progress", "output_file_id": None, "error_file_id": None,
    "created_at": 1, "expires_at": 2, "request_counts": {"total": 2, "completed": 1, "failed": 0},
    "metadata": {"run": "a"}, "gateway": {"jobs": [{"mode": "emulated"}]}, "unknown": True,
}


def handler(seen):
    def handle(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        path = request.url.path
        if path == "/api/inference/v1/files":
            return httpx.Response(200, json={"id": "file_1", "object": "file", "bytes": 3, "filename": "b.jsonl",
                                             "purpose": "batch", "created_at": 1})
        if path == "/api/inference/v1/files/file_2/content":
            return httpx.Response(200, content=b'{"custom_id":"a"}\n')
        if path == "/api/inference/v1/batches" and request.method == "GET":
            return httpx.Response(200, json={"object": "list", "data": [BATCH], "has_more": True})
        if path == "/api/inference/v1/batches/missing":
            return httpx.Response(404, headers={"content-type": "application/problem+json"},
                                  json={"code": "batch_not_found", "status": 404})
        return httpx.Response(200, json=BATCH)
    return handle


def test_sync_calls():
    seen = []
    client = Client("http://gw", "gw_key", http_client=httpx.Client(transport=httpx.MockTransport(handler(seen))))
    file = client.upload_batch_file(b"{}\n", filename="b.jsonl")
    assert file.id == "file_1" and b'name="purpose"' in seen[-1].content
    batch = client.create_batch("file_1", metadata={"run": "a"}, idempotency_key="k-1")
    sent = seen[-1]
    assert sent.headers["idempotency-key"] == "k-1"
    assert json.loads(sent.content) == {"input_file_id": "file_1", "endpoint": "/v1/chat/completions",
                                        "completion_window": "24h", "metadata": {"run": "a"}}
    assert isinstance(batch, Batch) and not batch.done and batch.gateway["jobs"][0]["mode"] == "emulated"
    assert batch.request_counts.completed == 1
    listed = client.list_batches(limit=1, after="batch_0")
    assert listed.has_more and seen[-1].url.params["after"] == "batch_0"
    assert client.cancel_batch("batch_1").id == "batch_1" and seen[-1].url.path.endswith("/cancel")
    assert "idempotency-key" not in seen[-1].headers
    assert client.file_content("file_2") == b'{"custom_id":"a"}\n'
    with pytest.raises(ProblemError) as raised:
        client.batch("missing")
    assert raised.value.code == "batch_not_found"
    with pytest.raises(ConfigError):
        client.batch("../files")
    with pytest.raises(ConfigError):
        client.create_batch("file_1", endpoint="/v1/responses")


def test_async_calls():
    async def run() -> None:
        seen = []
        transport = httpx.MockTransport(handler(seen))
        async with AsyncClient("http://gw", "gw_key", http_client=httpx.AsyncClient(transport=transport)) as client:
            assert (await client.upload_batch_file(b"{}\n")).purpose == "batch"
            batch = await client.create_batch("file_1")
            assert "idempotency-key" in seen[-1].headers and batch.status == "in_progress"
            assert len((await client.list_batches()).data) == 1
            assert (await client.batch("batch_1")).metadata == {"run": "a"}
            assert (await client.file_content("file_2")).startswith(b"{")

    asyncio.run(run())
