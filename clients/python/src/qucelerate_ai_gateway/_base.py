"""What the sync and async clients share: request building, the retry rule,
and response decoding. Neither does I/O here."""

from __future__ import annotations

import dataclasses
import json
import uuid
from typing import Any, Dict, Iterable, List, Mapping, Optional, Sequence, Tuple

import httpx

from . import __version__
from ._errors import ConfigError, GatewayError, DecodeError, HTTPError, ProblemError, ProtocolError, StreamError
from ._sse import MAX_EVENT_BYTES, OversizedEvent, SseParser
from ._types import KNOWN_EVENTS, MessageLike, StreamEvent, ToolChoice, ToolLike, decode_event

CHAT_PATH = "/api/inference/v1/chat/completions"
STREAM_PATH = "/api/inference/v1/chat/completions/stream"
EMBEDDINGS_PATH = "/api/inference/v1/embeddings"
MAX_WAIT_SECONDS = 60
MAX_KEY_CHARS = 255
MAX_REQUEST_ID_CHARS = 128
#: An error body is read only this far: enough for its problem.
MAX_ERROR_BODY_BYTES = 1 << 20
#: The default largest successful non-streaming body read.
DEFAULT_MAX_RESPONSE_BYTES = 64 << 20
DEFAULT_TIMEOUT = 120.0

#: Problem codes for which the gateway refused the request before reserving
#: anything, so the same Idempotency-Key is still unused and a retry is safe
#: (`retry: same_key` in clients/conformance/errors.json).
SAME_KEY_RETRY_CODES = frozenset({"rate_limit_exceeded", "concurrency_limit_exceeded", "provider_circuit_open"})


class Settings:
    def __init__(
        self,
        base_url: str,
        api_key: str,
        max_retries: int,
        timeout: Optional[float],
        max_response_bytes: int = DEFAULT_MAX_RESPONSE_BYTES,
    ) -> None:
        if not api_key or not api_key.strip():
            raise ConfigError("an API key is required")
        if max_retries < 0:
            raise ConfigError("max_retries must be 0 or more")
        if max_response_bytes < 1:
            raise ConfigError("max_response_bytes must be positive")
        self.max_response_bytes = max_response_bytes
        url = httpx.URL(base_url)
        if url.scheme not in ("http", "https") or not url.host:
            raise ConfigError(f"base URL must be http or https: {base_url!r}")
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.max_retries = max_retries
        self.timeout = DEFAULT_TIMEOUT if timeout is None else timeout

    def __repr__(self) -> str:
        return f"Settings(base_url={self.base_url!r}, api_key='<redacted>', max_retries={self.max_retries})"


def user_agent() -> str:
    return f"qucelerate-ai-gateway-python/{__version__}"


def _message(message: MessageLike) -> Dict[str, Any]:
    if dataclasses.is_dataclass(message):
        return message.to_dict()  # type: ignore[union-attr]
    wire: Dict[str, Any] = {"role": message["role"], "content": message.get("content")}
    if message.get("tool_calls"):
        wire["tool_calls"] = [_tool_call(call) for call in message["tool_calls"]]
    if message.get("tool_call_id") is not None:
        wire["tool_call_id"] = message["tool_call_id"]
    return wire


def _tool_call(call: Any) -> Dict[str, Any]:
    return call.to_dict() if dataclasses.is_dataclass(call) else dict(call)


def _tool(tool: ToolLike) -> Dict[str, Any]:
    return tool.to_dict() if dataclasses.is_dataclass(tool) else dict(tool)  # type: ignore[union-attr]


def chat_body(
    model: str,
    messages: Sequence[MessageLike],
    temperature: Optional[float],
    max_tokens: Optional[int],
    tools: Optional[Sequence[ToolLike]] = None,
    tool_choice: Optional[ToolChoice] = None,
    parallel_tool_calls: Optional[bool] = None,
) -> Dict[str, Any]:
    body: Dict[str, Any] = {"model": model, "messages": [_message(message) for message in messages]}
    if temperature is not None:
        body["temperature"] = temperature
    if max_tokens is not None:
        body["max_tokens"] = max_tokens
    if tools:
        body["tools"] = [_tool(tool) for tool in tools]
    if tool_choice is not None:
        body["tool_choice"] = tool_choice if isinstance(tool_choice, str) else dict(tool_choice)
    if parallel_tool_calls is not None:
        body["parallel_tool_calls"] = parallel_tool_calls
    return body


def embedding_body(model: str, input: Iterable[str]) -> Dict[str, Any]:
    if isinstance(input, str):
        raise ConfigError("input is a list of strings; wrap a single string in a list")
    return {"model": model, "input": list(input)}


def prepare(
    settings: Settings,
    body: Mapping[str, Any],
    idempotency_key: Optional[str],
    request_id: Optional[str],
) -> Tuple[Dict[str, str], bytes]:
    """The headers and body every attempt of one call sends."""
    if idempotency_key is None:
        idempotency_key = str(uuid.uuid4())
    elif not 1 <= len(idempotency_key) <= MAX_KEY_CHARS:
        raise ConfigError(f"an Idempotency-Key must be 1 to {MAX_KEY_CHARS} characters")
    headers = {
        "authorization": f"Bearer {settings.api_key}",
        "content-type": "application/json",
        "idempotency-key": idempotency_key,
        "user-agent": user_agent(),
    }
    if request_id is not None:
        if len(request_id) > MAX_REQUEST_ID_CHARS:
            raise ConfigError(f"an X-Request-Id must be at most {MAX_REQUEST_ID_CHARS} characters")
        headers["x-request-id"] = request_id
    return headers, json.dumps(body, separators=(",", ":")).encode()


def error_from(status: int, headers: httpx.Headers, content: bytes) -> Exception:
    retry_after: Optional[int] = None
    raw = headers.get("retry-after")
    if raw is not None and raw.strip().isdigit():
        retry_after = int(raw.strip())
    if "json" in headers.get("content-type", ""):
        try:
            body = json.loads(content)
        except ValueError:
            body = None
        if isinstance(body, dict) and isinstance(body.get("code"), str):
            return ProblemError(status, body, retry_after)
    return HTTPError(status, headers.get("x-request-id"))


class BodyReader:
    """Collects at most `limit` bytes of a body; `exceeded` says it was longer."""

    def __init__(self, limit: int) -> None:
        self.limit = limit
        self.chunks: List[bytes] = []
        self.length = 0
        self.exceeded = False

    def add(self, chunk: bytes) -> bool:
        """Keep the chunk; False once the limit is passed, to stop reading."""
        if self.length + len(chunk) > self.limit:
            self.chunks.append(chunk[: self.limit - self.length])
            self.length = self.limit
            self.exceeded = True
            return False
        self.chunks.append(chunk)
        self.length += len(chunk)
        return True

    def content(self) -> bytes:
        return b"".join(self.chunks)


def oversized(limit: int) -> DecodeError:
    return DecodeError(f"the response body exceeded {limit} bytes")


def retry_wait(error: Exception, retries_done: int, max_retries: int) -> Optional[int]:
    """Seconds to wait before retrying with the same key, or None to raise."""
    if isinstance(error, ProblemError) and error.code in SAME_KEY_RETRY_CODES and retries_done < max_retries:
        return min(error.retry_after if error.retry_after is not None else 1, MAX_WAIT_SECONDS)
    return None


def decode_json(content: bytes) -> Any:
    try:
        return json.loads(content)
    except ValueError as error:
        raise DecodeError(f"response is not JSON: {error}") from error


class EventDecoder:
    """Turns SSE bytes into typed events; raises at the terminal error."""

    def __init__(self) -> None:
        self._parser = SseParser()
        self.done = False

    def feed(self, chunk: bytes) -> List[StreamEvent]:
        events: List[StreamEvent] = []
        raws: List[Tuple[str, str]] = []
        oversized = False
        try:
            self._parser.feed(chunk, raws)
        except OversizedEvent:
            oversized = True
        for name, data in raws:
            if self.done:
                break
            if name == "error":
                self.done = True
                raise _PendingStreamError(events, _stream_error(data))
            if name not in KNOWN_EVENTS:
                continue
            try:
                event = decode_event(name, json.loads(data))
            except (ValueError, KeyError, TypeError, AttributeError) as error:
                self.done = True
                raise _PendingStreamError(events, DecodeError(f"{name}: {error!r}")) from error
            if event is None:
                continue
            events.append(event)
            if event.event == "response.completed":
                self.done = True
        if oversized and not self.done:
            self.done = True
            raise _PendingStreamError(events, ProtocolError(f"a stream event exceeded {MAX_EVENT_BYTES} bytes"))
        return events

    def end(self) -> None:
        if not self.done:
            self.done = True
            raise ProtocolError("the stream closed before a terminal event")


def _stream_error(data: str) -> GatewayError:
    """The terminal error event's StreamError, or a DecodeError if malformed."""
    try:
        payload = json.loads(data)
        code, status = payload["code"], payload["status"]
        if not isinstance(code, str) or not isinstance(status, int) or isinstance(status, bool):
            raise TypeError("code must be a string and status an integer")
    except (ValueError, KeyError, TypeError) as error:
        return DecodeError(f"error event: {error!r}")
    return StreamError(code, status)


def stream_timeout(seconds: float) -> httpx.Timeout:
    """No limit between a stream's events, but finite limits to connect, send
    the request and wait for a pooled connection."""
    return httpx.Timeout(None, connect=min(seconds, 10.0), write=seconds, pool=seconds)


class _PendingStreamError(Exception):
    """Events decoded before an error in the same chunk, and the error."""

    def __init__(self, events: List[StreamEvent], error: Exception) -> None:
        self.events = events
        self.error = error


# Files and batches (D-021): OpenAI's Batch API shape.
FILES_PATH = "/api/inference/v1/files"
BATCHES_PATH = "/api/inference/v1/batches"
BATCH_ENDPOINTS = ("/v1/chat/completions", "/v1/embeddings")


def path_segment(value: str) -> str:
    """An id as one path segment: it can never name another path."""
    if not value or "/" in value or "?" in value or "#" in value or value in (".", ".."):
        raise ConfigError(f"not an id: {value!r}")
    return value


def plain_headers(settings: Settings, idempotency_key: Optional[str], request_id: Optional[str], *, write: bool) -> Dict[str, str]:
    """Headers of a files or batches call: a write that creates something
    carries an Idempotency-Key (generated unless given)."""
    headers = {"authorization": f"Bearer {settings.api_key}", "user-agent": user_agent()}
    if write:
        if idempotency_key is None:
            idempotency_key = str(uuid.uuid4())
        if not 1 <= len(idempotency_key) <= MAX_KEY_CHARS:
            raise ConfigError(f"an Idempotency-Key must be 1 to {MAX_KEY_CHARS} characters")
        headers["idempotency-key"] = idempotency_key
    if request_id is not None:
        if len(request_id) > MAX_REQUEST_ID_CHARS:
            raise ConfigError(f"an X-Request-Id must be at most {MAX_REQUEST_ID_CHARS} characters")
        headers["x-request-id"] = request_id
    return headers


def batch_body(input_file_id: str, endpoint: str, metadata: Optional[Mapping[str, str]]) -> Dict[str, Any]:
    if endpoint not in BATCH_ENDPOINTS:
        raise ConfigError(f"endpoint must be one of {', '.join(BATCH_ENDPOINTS)}")
    body: Dict[str, Any] = {"input_file_id": input_file_id, "endpoint": endpoint, "completion_window": "24h"}
    if metadata:
        body["metadata"] = dict(metadata)
    return body
