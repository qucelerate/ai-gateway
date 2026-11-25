from __future__ import annotations

import asyncio
from collections import deque
from typing import Any, AsyncIterator, Awaitable, Callable, Deque, Iterable, Mapping, Optional, Sequence, Tuple

import httpx

from . import _base
from ._client import _decode
from ._errors import TransportError
from ._types import Batch, BatchList, ChatResponse, EmbeddingResponse, FileObject, MessageLike, StreamEvent, ToolChoice, ToolLike


class AsyncClient:
    """An asyncio client for the Gateway inference API, with the same calls
    and behavior as `Client`. Use ``async with``, or ``await close()``.

    Cancelling a call (for example with ``asyncio.wait_for``) closes its
    connection.
    """

    def __init__(
        self,
        base_url: str,
        api_key: str,
        *,
        max_retries: int = 2,
        timeout: Optional[float] = None,
        http_client: Optional[httpx.AsyncClient] = None,
        max_response_bytes: int = _base.DEFAULT_MAX_RESPONSE_BYTES,
        _sleep: Callable[[float], Awaitable[None]] = asyncio.sleep,
    ) -> None:
        self._settings = _base.Settings(base_url, api_key, max_retries, timeout, max_response_bytes)
        self._http = http_client or httpx.AsyncClient(timeout=httpx.Timeout(self._settings.timeout, connect=10.0))
        self._owns_http = http_client is None
        self._sleep = _sleep

    def __repr__(self) -> str:
        return f"AsyncClient({self._settings!r})"

    async def close(self) -> None:
        if self._owns_http:
            await self._http.aclose()

    async def __aenter__(self) -> "AsyncClient":
        return self

    async def __aexit__(self, *exc: Any) -> None:
        await self.close()

    async def chat(
        self,
        model: str,
        messages: Sequence[MessageLike],
        *,
        temperature: Optional[float] = None,
        max_tokens: Optional[int] = None,
        tools: Optional[Sequence[ToolLike]] = None,
        tool_choice: Optional[ToolChoice] = None,
        parallel_tool_calls: Optional[bool] = None,
        idempotency_key: Optional[str] = None,
        request_id: Optional[str] = None,
    ) -> ChatResponse:
        body = _base.chat_body(model, messages, temperature, max_tokens, tools, tool_choice, parallel_tool_calls)
        response, content = await self._send(_base.CHAT_PATH, body, idempotency_key, request_id, stream=False)
        return _decode(ChatResponse.from_dict, response, content)

    async def embeddings(
        self,
        model: str,
        input: Iterable[str],
        *,
        idempotency_key: Optional[str] = None,
        request_id: Optional[str] = None,
    ) -> EmbeddingResponse:
        body = _base.embedding_body(model, input)
        response, content = await self._send(
            _base.EMBEDDINGS_PATH, body, idempotency_key, request_id, stream=False
        )
        return _decode(EmbeddingResponse.from_dict, response, content)

    async def chat_stream(
        self,
        model: str,
        messages: Sequence[MessageLike],
        *,
        temperature: Optional[float] = None,
        max_tokens: Optional[int] = None,
        tools: Optional[Sequence[ToolLike]] = None,
        tool_choice: Optional[ToolChoice] = None,
        parallel_tool_calls: Optional[bool] = None,
        idempotency_key: Optional[str] = None,
        request_id: Optional[str] = None,
    ) -> "AsyncChatStream":
        """Stream a chat completion: ``async for event in await client.chat_stream(...)``.
        Use ``async with`` or iterate to the end to close it."""
        body = _base.chat_body(model, messages, temperature, max_tokens, tools, tool_choice, parallel_tool_calls)
        response, _ = await self._send(_base.STREAM_PATH, body, idempotency_key, request_id, stream=True)
        return AsyncChatStream(response)

    async def upload_batch_file(self, content: bytes, *, filename: str = "batch.jsonl", request_id: Optional[str] = None) -> FileObject:
        """Upload a batch input file: JSONL, one request per line, each
        ``{"custom_id", "method": "POST", "url", "body"}``."""
        response, body = await self._plain(
            "POST", _base.FILES_PATH, None, request_id, write=False,
            files={"file": (filename, content, "application/jsonl")}, data={"purpose": "batch"},
        )
        return _decode(FileObject.from_dict, response, body)

    async def create_batch(
        self,
        input_file_id: str,
        *,
        endpoint: str = "/v1/chat/completions",
        metadata: Optional[Mapping[str, str]] = None,
        idempotency_key: Optional[str] = None,
        request_id: Optional[str] = None,
    ) -> Batch:
        """Create a batch from an uploaded file. The whole batch is checked,
        routed and reserved when it is created, or refused with nothing
        held. Retry with the same `idempotency_key` to get the same batch."""
        response, body = await self._plain(
            "POST", _base.BATCHES_PATH, idempotency_key, request_id, write=True,
            json=_base.batch_body(input_file_id, endpoint, metadata),
        )
        return _decode(Batch.from_dict, response, body)

    async def batch(self, batch_id: str, *, request_id: Optional[str] = None) -> Batch:
        """A batch, with the gateway's account of how it runs and its cost."""
        response, body = await self._plain("GET", f"{_base.BATCHES_PATH}/{_base.path_segment(batch_id)}", None, request_id, write=False)
        return _decode(Batch.from_dict, response, body)

    async def list_batches(self, *, after: Optional[str] = None, limit: Optional[int] = None, request_id: Optional[str] = None) -> BatchList:
        """The API key's service account's batches, newest first."""
        params = {key: value for key, value in (("after", after), ("limit", limit)) if value is not None}
        response, body = await self._plain("GET", _base.BATCHES_PATH, None, request_id, write=False, params=params)
        return _decode(BatchList.from_dict, response, body)

    async def cancel_batch(self, batch_id: str, *, request_id: Optional[str] = None) -> Batch:
        """Ask a batch to stop: lines not yet run end without a charge."""
        response, body = await self._plain("POST", f"{_base.BATCHES_PATH}/{_base.path_segment(batch_id)}/cancel", None, request_id, write=False)
        return _decode(Batch.from_dict, response, body)

    async def file_content(self, file_id: str, *, request_id: Optional[str] = None) -> bytes:
        """A file's content: a batch's output or error file is JSONL, one
        line per request line, matched by `custom_id`."""
        _, body = await self._plain("GET", f"{_base.FILES_PATH}/{_base.path_segment(file_id)}/content", None, request_id, write=False)
        return body

    async def _plain(
        self,
        method: str,
        path: str,
        idempotency_key: Optional[str],
        request_id: Optional[str],
        *,
        write: bool,
        **request: Any,
    ) -> Tuple[httpx.Response, bytes]:
        """A files or batches call, retried only as `_send` retries."""
        headers = _base.plain_headers(self._settings, idempotency_key, request_id, write=write)
        url = self._settings.base_url + path
        retries = 0
        while True:
            try:
                built = self._http.build_request(method, url, headers=headers, timeout=self._settings.timeout, **request)
                response = await self._http.send(built, stream=True)
            except httpx.HTTPError as error:
                raise TransportError(str(error)) from error
            limit = self._settings.max_response_bytes if response.is_success else _base.MAX_ERROR_BODY_BYTES
            reader = _base.BodyReader(limit)
            try:
                async for chunk in response.aiter_bytes():
                    if not reader.add(chunk):
                        break
            except httpx.HTTPError as error:
                raise TransportError(str(error)) from error
            finally:
                await response.aclose()
            if response.is_success:
                if reader.exceeded:
                    raise _base.oversized(limit)
                return response, reader.content()
            error = _base.error_from(response.status_code, response.headers, reader.content())
            wait = _base.retry_wait(error, retries, self._settings.max_retries)
            if wait is None:
                raise error
            retries += 1
            await self._sleep(wait)

    async def _send(
        self,
        path: str,
        body: Mapping[str, Any],
        idempotency_key: Optional[str],
        request_id: Optional[str],
        *,
        stream: bool,
    ) -> Tuple[httpx.Response, bytes]:
        headers, content = _base.prepare(self._settings, body, idempotency_key, request_id)
        url = self._settings.base_url + path
        retries = 0
        while True:
            request = self._http.build_request(
                "POST", url, headers=headers, content=content,
                timeout=_base.stream_timeout(self._settings.timeout) if stream else self._settings.timeout,
            )
            try:
                response = await self._http.send(request, stream=True)
            except httpx.HTTPError as error:
                raise TransportError(str(error)) from error
            if response.is_success and stream:
                return response, b""
            limit = self._settings.max_response_bytes if response.is_success else _base.MAX_ERROR_BODY_BYTES
            reader = _base.BodyReader(limit)
            try:
                async for chunk in response.aiter_bytes():
                    if not reader.add(chunk):
                        break
            except httpx.HTTPError as error:
                raise TransportError(str(error)) from error
            finally:
                await response.aclose()
            if response.is_success:
                if reader.exceeded:
                    raise _base.oversized(limit)
                return response, reader.content()
            error = _base.error_from(response.status_code, response.headers, reader.content())
            wait = _base.retry_wait(error, retries, self._settings.max_retries)
            if wait is None:
                raise error
            retries += 1
            await self._sleep(wait)


class AsyncChatStream:
    """The events of a streamed chat completion, for ``async for``."""

    def __init__(self, response: httpx.Response) -> None:
        self._response = response
        self.request_id: Optional[str] = response.headers.get("x-request-id")
        self._decoder = _base.EventDecoder()
        self._pending: Deque[StreamEvent] = deque()
        self._chunks: Optional[AsyncIterator[bytes]] = None
        self._error: Optional[Exception] = None

    def __aiter__(self) -> "AsyncChatStream":
        return self

    async def __anext__(self) -> StreamEvent:
        try:
            return await self._next()
        except BaseException:
            await self.aclose()
            raise

    async def _next(self) -> StreamEvent:
        while True:
            if self._pending:
                return self._pending.popleft()
            if self._error is not None:
                error, self._error = self._error, None
                raise error
            if self._decoder.done:
                await self.aclose()
                raise StopAsyncIteration
            if self._chunks is None:
                self._chunks = self._response.aiter_bytes()
            try:
                chunk = await self._chunks.__anext__()
            except StopAsyncIteration:
                self._decoder.end()
                continue
            except httpx.HTTPError as error:
                raise TransportError(str(error)) from error
            try:
                self._pending.extend(self._decoder.feed(chunk))
            except _base._PendingStreamError as pending:
                self._pending.extend(pending.events)
                self._error = pending.error

    async def aclose(self) -> None:
        await self._response.aclose()

    async def __aenter__(self) -> "AsyncChatStream":
        return self

    async def __aexit__(self, *exc: Any) -> None:
        await self.aclose()
