#!/usr/bin/env python3
"""Check the client conformance cases against the stable inference contract.

Every request body must be valid for its operation's request schema, every
success body and SSE event for its response schema (read openly, as a v1 client
must), every Problem for the Problem schema, and every expectation must agree
with the responses it is about. Run from anywhere:

    python3 clients/conformance/validate.py
"""

from __future__ import annotations

import importlib.util
import json
import re
import sys
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
SPEC = importlib.util.spec_from_file_location("openapi_generate", ROOT / "openapi" / "generate.py")
assert SPEC and SPEC.loader
generate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(generate)

OPERATIONS = {
    "chat": ("/api/inference/v1/chat/completions", "ChatRequest", "ChatResponse"),
    "chat_stream": ("/api/inference/v1/chat/completions/stream", "ChatRequest", None),
    "embeddings": ("/api/inference/v1/embeddings", "EmbeddingRequest", "EmbeddingResponse"),
}
CASE_KEYS = {"name", "description", "operation", "client", "call", "expect_request", "responses", "expect"}
EXPECT_KEYS = {"attempts", "same_idempotency_key", "sleeps", "result", "events", "error"}
ERROR_KINDS = {"problem", "http", "stream", "protocol"}
MAX_WAIT = 60
DEFAULT_RETRIES = 2


class CaseError(Exception):
    pass


def check(condition: bool, message: str) -> None:
    if not condition:
        raise CaseError(message)


def schema_ok(value: Any, name: str, document: dict, path: str, *, closed: bool) -> None:
    try:
        generate._validate_schema(
            value, {"$ref": f"#/components/schemas/{name}"}, document, path, closed=closed
        )
    except AssertionError as error:
        raise CaseError(str(error)) from error


def parse_sse(text: str) -> list[tuple[str, str]]:
    """Parse text/event-stream as the WHATWG specification does."""
    events: list[tuple[str, str]] = []
    name, data = "", []
    # The last piece after the final line break is an unterminated line (or
    # empty): it never completes an event, so a stream cut before its blank
    # line dispatches nothing.
    for line in re.split(r"\r\n|\r|\n", text)[:-1]:
        if line == "":
            if data:
                events.append((name or "message", "\n".join(data)))
            name, data = "", []
            continue
        if line.startswith(":"):
            continue
        field, _, value = line.partition(":")
        if value.startswith(" "):
            value = value[1:]
        if field == "event":
            name = value
        elif field == "data":
            data.append(value)
    return events


def subset(expected: Any, actual: Any, path: str) -> None:
    if isinstance(expected, dict):
        check(isinstance(actual, dict), f"{path}: expected an object")
        for key, value in expected.items():
            check(key in actual, f"{path}.{key}: missing")
            subset(value, actual[key], f"{path}.{key}")
    elif isinstance(expected, list):
        check(isinstance(actual, list) and len(actual) == len(expected), f"{path}: length differs")
        for index, (left, right) in enumerate(zip(expected, actual)):
            subset(left, right, f"{path}[{index}]")
    else:
        check(expected == actual and type(expected) is type(actual), f"{path}: {expected!r} != {actual!r}")


def retry_policy(errors: dict) -> set[str]:
    return {entry["code"] for entry in errors["codes"] if entry["retry"] == "same_key"}


def wait_for(response: dict) -> int:
    value = response.get("headers", {}).get("retry-after")
    if value is None:
        return 1
    return min(int(value), MAX_WAIT)


def validate_case(case: dict, document: dict, same_key: set[str], statuses: dict[str, int]) -> None:
    check(set(case) <= CASE_KEYS, f"unknown keys {sorted(set(case) - CASE_KEYS)}")
    check(case["operation"] in OPERATIONS, "unknown operation")
    path, request_schema, response_schema = OPERATIONS[case["operation"]]
    call, expect_request, responses, expect = (
        case["call"], case["expect_request"], case["responses"], case["expect"]
    )
    check(set(expect) <= EXPECT_KEYS, f"unknown expect keys {sorted(set(expect) - EXPECT_KEYS)}")

    schema_ok(call["body"], request_schema, document, "call.body", closed=True)
    check(expect_request["method"] == "POST", "every inference call is a POST")
    check(expect_request["path"] == path, "expect_request.path is not the operation's path")
    check(expect_request["body"] == call["body"], "the wire body must equal call.body")
    headers = expect_request["headers"]
    check(headers.get("authorization") == "Bearer gw_conformance_key", "authorization header")
    check(all(name == name.lower() for name in headers), "header names must be lowercase")
    if "idempotency_key" in call:
        check(headers.get("idempotency-key") == call["idempotency_key"], "explicit key not expected")
        check(not expect_request.get("generated_idempotency_key"), "a key was given")
    else:
        check(expect_request.get("generated_idempotency_key") is True, "the client must generate a key")
    if "request_id" in call:
        check(headers.get("x-request-id") == call["request_id"], "explicit request ID not expected")

    check(responses, "a case serves at least one response")
    for index, response in enumerate(responses):
        where = f"responses[{index}]"
        bodies = [key for key in ("json", "sse", "text") if key in response]
        check(len(bodies) == 1, f"{where}: exactly one of json, sse, text")
        check(all(name == name.lower() for name in response.get("headers", {})), f"{where}: lowercase headers")
        status = response["status"]
        if "json" in response and status >= 400:
            schema_ok(response["json"], "Problem", document, f"{where}.json", closed=False)
            check(response["json"]["status"] == status, f"{where}: Problem status differs")
            code = response["json"]["code"]
            if code in statuses:
                check(statuses[code] == status, f"{where}: {code} is {statuses[code]}, not {status}")
        elif "json" in response:
            check(response_schema is not None and status == 200, f"{where}: unexpected success body")
            schema_ok(response["json"], response_schema, document, f"{where}.json", closed=False)
        if "sse" in response:
            check(case["operation"] == "chat_stream" and status == 200, f"{where}: SSE outside a stream")
            text = (HERE / "fixtures" / response["sse"]).read_text()
            validate_stream(text, document, where)

    # Replay the retry policy the README defines and compare with expect.
    retries = case.get("client", {}).get("max_retries", DEFAULT_RETRIES)
    attempts, sleeps = 0, []
    for response in responses:
        attempts += 1
        code = response.get("json", {}).get("code") if response["status"] >= 400 else None
        if code in same_key and attempts <= retries:
            sleeps.append(wait_for(response))
            continue
        break
    check(attempts == len(responses), "responses left unserved by the retry policy")
    last = responses[-1]
    last_code = last.get("json", {}).get("code") if last["status"] >= 400 else None
    check(not (last_code in same_key and attempts <= retries),
          "the last response still requires a retry; serve the response the retry gets")
    check(expect["attempts"] == attempts, f"attempts should be {attempts}")
    check(expect["sleeps"] == sleeps, f"sleeps should be {sleeps}")
    check(expect.get("same_idempotency_key", attempts == 1) is True, "retries reuse the key")

    final = responses[-1]
    outcomes = [key for key in ("result", "error") if key in expect]
    if case["operation"] == "chat_stream":
        check("events" in expect, "a stream case lists its events")
        check(len(outcomes) <= 1, "a stream ends in at most one error")
        if "result" in expect:
            raise CaseError("stream cases use events, not result")
    else:
        check(len(outcomes) == 1 and "events" not in expect, "result or error, not both")
    if "result" in expect:
        check(final["status"] == 200 and "json" in final, "a result needs a final success")
        actual = dict(final["json"])
        if "x-request-id" in final.get("headers", {}):
            actual["request_id"] = final["headers"]["x-request-id"]
        if "usage" not in actual:
            actual["usage"] = None
        subset(expect["result"], actual, "expect.result")
    if "events" in expect and "sse" in final:
        known = {event["name"] for event in stream_events(document)}
        actual = [
            {"event": name, "data": json.loads(data)}
            for name, data in parse_sse((HERE / "fixtures" / final["sse"]).read_text())
            if name in known
        ]
        terminal = [event for event in actual if event["event"] in ("response.completed", "error")]
        yielded = [event for event in actual if event["event"] != "error"]
        subset(expect["events"], yielded, "expect.events")
        error = expect.get("error")
        if terminal and terminal[-1]["event"] == "error":
            check(error and error["kind"] == "stream", "a terminal error event is a stream error")
            check(error["code"] == terminal[-1]["data"]["code"], "stream error code")
            check(error["status"] == terminal[-1]["data"]["status"], "stream error status")
        elif not terminal:
            check(error == {"kind": "protocol"}, "a stream without a terminal event is a protocol error")
        else:
            check(error is None, "a completed stream is not an error")
    if "error" in expect:
        error = expect["error"]
        check(error["kind"] in ERROR_KINDS, "unknown error kind")
        if error["kind"] == "problem":
            check(final["status"] >= 400 and "json" in final, "a problem error needs a final Problem response")
            body = final["json"]
            check(error["status"] == final["status"] and error["code"] == body["code"], "problem fields")
            if "request_id" in error:
                check(error["request_id"] == body["request_id"], "problem request_id")
            header = final.get("headers", {}).get("retry-after")
            if header is None:
                check("retry_after" not in error, "retry_after without a Retry-After header")
            else:
                check(error.get("retry_after") == int(header), "retry_after must equal Retry-After")
        elif error["kind"] == "http":
            check(final["status"] >= 400 and "text" in final, "an http error needs a final non-Problem response")
            check(error == {"kind": "http", "status": final["status"]}, "http error")
        else:
            check(final["status"] == 200 and "sse" in final,
                  f"a {error['kind']} error needs a final stream; its events decide which")


def stream_events(document: dict) -> list[dict]:
    return document["paths"][OPERATIONS["chat_stream"][0]]["post"]["x-sse-events"]


def validate_stream(text: str, document: dict, where: str) -> None:
    events = {event["name"]: event for event in stream_events(document)}
    parsed = [(name, data) for name, data in parse_sse(text) if name in events]
    check(parsed and parsed[0][0] in ("response.started", "error"),
          f"{where}: a stream starts with response.started or a terminal error")
    open_choices, finished = set(), set()
    for name, data in parsed:
        payload = json.loads(data)
        if "choice_index" in payload:
            (finished if name == "choice.finished" else open_choices).add(payload["choice_index"])
        if name == "response.completed":
            check(open_choices <= finished, f"{where}: response.completed before every choice.finished")
    for number, (name, data) in enumerate(parse_sse(text), 1):
        if name not in events:
            continue
        payload = json.loads(data)
        generate_ok = events[name]["schema"]
        try:
            generate._validate_schema(payload, generate_ok, document, f"{where} event {number} ({name})", closed=False)
        except AssertionError as error:
            raise CaseError(str(error)) from error


def validate_errors(errors: dict, document: dict) -> None:
    seen = set()
    for entry in errors["codes"]:
        check(set(entry) == {"code", "status", "reserved", "retry", "description"}, f"{entry}: keys")
        check(re.fullmatch(r"[a-z][a-z0-9_]*", entry["code"]) is not None, f"{entry['code']}: pattern")
        check(entry["code"] not in seen, f"{entry['code']}: duplicate")
        seen.add(entry["code"])
        check(str(entry["status"]) in document["paths"][OPERATIONS["chat"][0]]["post"]["responses"],
              f"{entry['code']}: status {entry['status']} is not documented")
        check(entry["reserved"] in (True, False, "maybe"), f"{entry['code']}: reserved")
        check(entry["retry"] in ("same_key", "new_key", "fix", "none"), f"{entry['code']}: retry")
        if entry["retry"] == "same_key":
            check(entry["reserved"] is False, f"{entry['code']}: a reserved key cannot be reused")


def main() -> int:
    document = json.loads((ROOT / "openapi" / "inference-v1.json").read_text())
    errors = json.loads((HERE / "errors.json").read_text())
    failures = []
    try:
        validate_errors(errors, document)
    except CaseError as error:
        failures.append(f"errors.json: {error}")
    same_key = retry_policy(errors)
    statuses = {entry["code"]: entry["status"] for entry in errors["codes"]}
    cases = sorted((HERE / "cases").glob("*.json"))
    names = set()
    for path in cases:
        case = json.loads(path.read_text())
        try:
            check(case["name"] not in names, "duplicate case name")
            names.add(case["name"])
            check(path.stem[3:] == case["name"].replace("_", "-"), "file name must match the case name")
            validate_case(case, document, same_key, statuses)
        except (CaseError, KeyError, ValueError) as error:
            failures.append(f"{path.name}: {error!r}" if isinstance(error, KeyError) else f"{path.name}: {error}")
    fixtures = {path.name for path in (HERE / "fixtures").glob("*.sse")}
    used = {
        response["sse"]
        for path in cases
        for response in json.loads(path.read_text())["responses"]
        if "sse" in response
    }
    if fixtures - used:
        failures.append(f"unused fixtures: {sorted(fixtures - used)}")
    for failure in failures:
        print(failure, file=sys.stderr)
    if failures:
        return 1
    print(f"conformance suite valid: {len(cases)} cases, {len(errors['codes'])} problem codes")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
