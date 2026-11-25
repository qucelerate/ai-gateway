"""Runs every case of clients/conformance against Client and AsyncClient."""

from __future__ import annotations

import asyncio
import json
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Dict, List, Tuple

import pytest

import qucelerate_ai_gateway
from qucelerate_ai_gateway import (
    AsyncClient,
    Client,
    GatewayError,
    HTTPError,
    ProblemError,
    ProtocolError,
    StreamError,
)

SUITE = Path(__file__).resolve().parents[2] / "conformance"
CASES = sorted((SUITE / "cases").glob("*.json"))


class MockGateway:
    """Serves a case's responses in order, one per request, and records them."""

    def __init__(self, responses: List[Dict[str, Any]]) -> None:
        self.responses = list(responses)
        self.requests: List[Tuple[str, str, Dict[str, str], bytes]] = []
        mock = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args: Any) -> None:
                pass

            def do_POST(self) -> None:
                length = int(self.headers.get("content-length", "0"))
                body = self.rfile.read(length)
                headers = {name.lower(): value for name, value in self.headers.items()}
                mock.requests.append((self.command, self.path, headers, body))
                response = mock.responses.pop(0)
                status = response["status"]
                if "json" in response:
                    kind = "application/problem+json" if status >= 400 else "application/json"
                    payload = json.dumps(response["json"]).encode()
                elif "sse" in response:
                    kind = "text/event-stream"
                    payload = (SUITE / "fixtures" / response["sse"]).read_bytes()
                else:
                    kind = "text/plain"
                    payload = response["text"].encode()
                extra = dict(response.get("headers", {}))
                kind = extra.pop("content-type", kind)
                self.send_response(status)
                self.send_header("content-type", kind)
                self.send_header("content-length", str(len(payload)))
                self.send_header("connection", "close")
                for name, value in extra.items():
                    self.send_header(name, value)
                self.end_headers()
                self.wfile.write(payload)
                self.close_connection = True

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True).start()

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


def subset(expected: Any, actual: Any, path: str) -> None:
    if isinstance(expected, dict):
        assert isinstance(actual, dict), f"{path}: expected an object, got {actual!r}"
        for key, value in expected.items():
            assert key in actual, f"{path}.{key}: missing in {actual!r}"
            subset(value, actual[key], f"{path}.{key}")
    elif isinstance(expected, list):
        assert isinstance(actual, list) and len(actual) == len(expected), f"{path}: {actual!r}"
        for index, (left, right) in enumerate(zip(expected, actual)):
            subset(left, right, f"{path}[{index}]")
    else:
        assert expected == actual and type(expected) is type(actual), f"{path}: {expected!r} != {actual!r}"


def check_error(expected: Dict[str, Any], error: BaseException, name: str) -> None:
    kind = expected["kind"]
    if kind == "problem":
        assert isinstance(error, ProblemError), f"{name}: {error!r}"
        assert error.status == expected["status"] and error.code == expected["code"], name
        if "request_id" in expected:
            assert error.request_id == expected["request_id"], name
        if "retry_after" in expected:
            assert error.retry_after == expected["retry_after"], name
    elif kind == "http":
        assert isinstance(error, HTTPError) and error.status == expected["status"], f"{name}: {error!r}"
    elif kind == "stream":
        assert isinstance(error, StreamError), f"{name}: {error!r}"
        assert (error.code, error.status) == (expected["code"], expected["status"]), name
    elif kind == "protocol":
        assert isinstance(error, ProtocolError), f"{name}: {error!r}"
    else:
        raise AssertionError(f"{name}: unknown error kind {kind}")


def arguments(case: Dict[str, Any]) -> Tuple[Dict[str, Any], Dict[str, Any]]:
    body = dict(case["call"]["body"])
    options = {key: case["call"][key] for key in ("idempotency_key", "request_id") if key in case["call"]}
    if case["operation"] == "embeddings":
        return {"model": body["model"], "input": body["input"]}, options
    extra = {key: body[key] for key in ("temperature", "max_tokens", "tools", "tool_choice", "parallel_tool_calls")
             if key in body}
    return {"model": body["model"], "messages": body["messages"], **extra}, options


def run_sync(case: Dict[str, Any], url: str, sleeps: List[float]) -> Tuple[Any, List[Any], Any, Any]:
    client = Client(url, "gw_conformance_key", _sleep=sleeps.append,
                    **({"max_retries": case["client"]["max_retries"]} if "client" in case else {}))
    call, options = arguments(case)
    result, events, failure, stream_id = None, [], None, None
    with client:
        try:
            if case["operation"] == "chat":
                result = client.chat(**call, **options).to_dict()
            elif case["operation"] == "embeddings":
                result = client.embeddings(**call, **options).to_dict()
            else:
                with client.chat_stream(**call, **options) as stream:
                    stream_id = stream.request_id
                    for event in stream:
                        events.append({"event": event.event, "data": event.to_dict()})
        except GatewayError as error:
            failure = error
    return result, events, failure, stream_id


async def run_async(case: Dict[str, Any], url: str, sleeps: List[float]) -> Tuple[Any, List[Any], Any, Any]:
    async def sleep(seconds: float) -> None:
        sleeps.append(seconds)

    client = AsyncClient(url, "gw_conformance_key", _sleep=sleep,
                         **({"max_retries": case["client"]["max_retries"]} if "client" in case else {}))
    call, options = arguments(case)
    result, events, failure, stream_id = None, [], None, None
    async with client:
        try:
            if case["operation"] == "chat":
                result = (await client.chat(**call, **options)).to_dict()
            elif case["operation"] == "embeddings":
                result = (await client.embeddings(**call, **options)).to_dict()
            else:
                async with await client.chat_stream(**call, **options) as stream:
                    stream_id = stream.request_id
                    async for event in stream:
                        events.append({"event": event.event, "data": event.to_dict()})
        except GatewayError as error:
            failure = error
    return result, events, failure, stream_id


@pytest.mark.parametrize("mode", ["sync", "async"])
@pytest.mark.parametrize("path", CASES, ids=[path.stem for path in CASES])
def test_conformance(path: Path, mode: str) -> None:
    case = json.loads(path.read_text())
    name, expect, expect_request = case["name"], case["expect"], case["expect_request"]
    mock = MockGateway(case["responses"])
    sleeps: List[float] = []
    try:
        if mode == "sync":
            result, events, failure, stream_id = run_sync(case, mock.url, sleeps)
        else:
            result, events, failure, stream_id = asyncio.run(run_async(case, mock.url, sleeps))
    finally:
        mock.close()

    if case["operation"] == "chat_stream":
        subset(expect["events"], events, f"{name}.events")
        if failure is None or isinstance(failure, (StreamError, ProtocolError)):
            served = case["responses"][-1].get("headers", {}).get("x-request-id")
            assert stream_id == served, f"{name}: stream request_id {stream_id!r}"
    if "result" in expect:
        assert failure is None, f"{name}: {failure!r}"
        subset(expect["result"], result, f"{name}.result")
    if "error" in expect:
        assert failure is not None, f"{name}: expected an error"
        check_error(expect["error"], failure, name)
    elif failure is not None:
        raise AssertionError(f"{name}: unexpected {failure!r}")

    assert len(mock.requests) == expect["attempts"], f"{name}: attempts"
    keys = set()
    for method, request_path, headers, body in mock.requests:
        assert method == expect_request["method"] and request_path == expect_request["path"], name
        assert headers.get("content-type") == "application/json", name
        for header, value in expect_request["headers"].items():
            assert headers.get(header) == value, f"{name}: header {header}"
        for header in expect_request.get("absent_headers", []):
            assert header not in headers, f"{name}: {header} sent"
        if expect_request.get("generated_idempotency_key"):
            key = headers.get("idempotency-key", "")
            assert 1 <= len(key) <= 255, f"{name}: key {key!r}"
        keys.add(headers.get("idempotency-key"))
        assert json.loads(body) == expect_request["body"], f"{name}: body"
    assert len(keys) == 1, f"{name}: keys {keys}"
    assert sleeps == expect["sleeps"], f"{name}: sleeps {sleeps}"


def test_version_is_exported() -> None:
    pyproject = (Path(__file__).resolve().parent.parent / "pyproject.toml").read_text(encoding="utf-8")
    declared = re.search(r'(?m)^version = "([^"]+)"', pyproject)
    assert declared and qucelerate_ai_gateway.__version__ == declared.group(1)
