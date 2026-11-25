#!/usr/bin/env python3
"""Generate and validate the checked-in Phase 1D OpenAPI artifact."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent
SOURCE = ROOT / "inference-v1.source.json"
TARGET = ROOT / "inference-v1.json"
FIXTURE = ROOT / "fixtures" / "sse" / "chat-completed.sse"
PATHS = {
    "/api/inference/v1/chat/completions",
    "/api/inference/v1/chat/completions/stream",
    "/api/inference/v1/embeddings",
}
# Files and batches (D-021): added to 1.0.0 compatibly.
BATCH_PATHS = {
    "/api/inference/v1/files",
    "/api/inference/v1/files/{id}",
    "/api/inference/v1/files/{id}/content",
    "/api/inference/v1/batches",
    "/api/inference/v1/batches/{id}",
    "/api/inference/v1/batches/{id}/cancel",
}
STATUS_RESPONSES = {
    "200", "400", "401", "403", "404", "409", "413", "415", "422", "429",
    "502", "503", "504",
}
TOKEN_COUNTERS = (
    "input_tokens",
    "output_tokens",
    "cached_input_tokens",
    "reasoning_tokens",
    "other_tokens",
)


def rendered(document: dict) -> str:
    return json.dumps(document, indent=2) + "\n"


def _fail(message: str) -> None:
    raise AssertionError(message)


def _resolve_schema(document: dict, schema: Any, path: str) -> dict | bool:
    if isinstance(schema, bool):
        return schema
    if not isinstance(schema, dict):
        _fail(f"{path}: schema must be an object")
    reference = schema.get("$ref")
    if reference is None:
        return schema
    prefix = "#/components/schemas/"
    if not isinstance(reference, str) or not reference.startswith(prefix):
        _fail(f"{path}: unsupported schema reference {reference!r}")
    name = reference[len(prefix):]
    schemas = document.get("components", {}).get("schemas", {})
    if name not in schemas:
        _fail(f"{path}: unknown schema {reference}")
    resolved = schemas[name]
    while isinstance(resolved, dict) and set(resolved) == {"$ref"}:
        resolved = _resolve_schema(document, resolved, path)
    return resolved


def _type_matches(value: Any, expected: str) -> bool:
    if expected == "null":
        return value is None
    if expected == "boolean":
        return isinstance(value, bool)
    if expected == "integer":
        return isinstance(value, int) and not isinstance(value, bool)
    if expected == "number":
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    if expected == "string":
        return isinstance(value, str)
    if expected == "array":
        return isinstance(value, list)
    if expected == "object":
        return isinstance(value, dict)
    return False


def _validate_schema(
    value: Any, schema: Any, document: dict, path: str, *, closed: bool = True
) -> None:
    """Validate the JSON Schema vocabulary used by SSE payload schemas.

    With `closed` (the default, for checked-in examples and fixtures) an object
    schema without additionalProperties admits only the properties it names.
    Pass `closed=False` to validate as a v1 client must read responses: fields
    the schema does not name are allowed, as the compatibility policy says.
    """

    if isinstance(schema, str):
        schema = {"$ref": schema}
    schema = _resolve_schema(document, schema, path)
    if schema is True:
        return
    if schema is False:
        _fail(f"{path}: payload is forbidden by its schema")

    if "$ref" in schema:
        _validate_schema(value, schema["$ref"], document, path, closed=closed)
    if "const" in schema and value != schema["const"]:
        _fail(f"{path}: expected {schema['const']!r}")
    if "enum" in schema and value not in schema["enum"]:
        _fail(f"{path}: value is not in enum")

    expected = schema.get("type")
    if expected is not None:
        expected_types = expected if isinstance(expected, list) else [expected]
        if not any(_type_matches(value, item) for item in expected_types):
            _fail(f"{path}: expected type {expected!r}")

    for keyword in ("oneOf", "anyOf"):
        alternatives = schema.get(keyword, [])
        if alternatives:
            matches = 0
            for alternative in alternatives:
                try:
                    _validate_schema(value, alternative, document, path, closed=closed)
                except AssertionError:
                    continue
                matches += 1
            if (keyword == "oneOf" and matches != 1) or (keyword == "anyOf" and matches == 0):
                _fail(f"{path}: did not match any {keyword} schema")

    if isinstance(value, str):
        if "minLength" in schema and len(value) < schema["minLength"]:
            _fail(f"{path}: string is shorter than minLength")
        if "maxLength" in schema and len(value) > schema["maxLength"]:
            _fail(f"{path}: string is longer than maxLength")
        if "pattern" in schema and re.search(schema["pattern"], value) is None:
            _fail(f"{path}: string does not match pattern")

    if isinstance(value, (int, float)) and not isinstance(value, bool):
        if "minimum" in schema and value < schema["minimum"]:
            _fail(f"{path}: number is below minimum")
        if "maximum" in schema and value > schema["maximum"]:
            _fail(f"{path}: number is above maximum")

    if isinstance(value, list):
        if "minItems" in schema and len(value) < schema["minItems"]:
            _fail(f"{path}: array is shorter than minItems")
        if "maxItems" in schema and len(value) > schema["maxItems"]:
            _fail(f"{path}: array is longer than maxItems")
        if "items" in schema:
            for index, item in enumerate(value):
                _validate_schema(item, schema["items"], document, f"{path}[{index}]", closed=closed)

    if isinstance(value, dict):
        for name in schema.get("required", []):
            if name not in value:
                _fail(f"{path}: missing required property {name!r}")
        properties = schema.get("properties", {})
        # Response schemas stay open so v1 can add fields compatibly, but the
        # checked-in examples and fixtures must name only documented fields:
        # an object schema without additionalProperties is treated as closed.
        if schema.get("additionalProperties", False if closed else True) is False:
            unknown = set(value) - set(properties)
            if unknown:
                _fail(f"{path}: unknown properties {sorted(unknown)!r}")
        for name, property_schema in properties.items():
            if name in value:
                _validate_schema(value[name], property_schema, document, f"{path}.{name}", closed=closed)
        additional = schema.get("additionalProperties")
        if isinstance(additional, (dict, bool)) and additional is not False:
            for name in set(value) - set(properties):
                _validate_schema(value[name], additional, document, f"{path}.{name}", closed=closed)


def _sse_events(operation: dict, document: dict) -> list[dict]:
    events = operation.get("x-sse-events")
    if not isinstance(events, list) or not events:
        _fail("stream operation must declare x-sse-events")
    names: set[str] = set()
    terminal_seen = False
    for event in events:
        if not isinstance(event, dict) or not isinstance(event.get("name"), str):
            _fail("each x-sse-events entry must have an event name")
        if event["name"] in names:
            _fail(f"duplicate SSE event {event['name']!r}")
        names.add(event["name"])
        schema = event.get("schema")
        if not isinstance(schema, dict) or "$ref" not in schema:
            _fail(f"SSE event {event['name']!r} must reference a schema")
        _resolve_schema(document, schema, f"event {event['name']!r}")
        if not isinstance(event.get("terminal"), bool):
            _fail(f"SSE event {event['name']!r} must declare terminal")
        if terminal_seen and not event["terminal"]:
            _fail("non-terminal SSE event follows a terminal event declaration")
        terminal_seen = terminal_seen or event["terminal"]
    if not terminal_seen:
        _fail("x-sse-events must declare a terminal event")
    return events


def _parse_sse_records(text: str) -> list[tuple[str, Any]]:
    """Parse the strict event/data SSE grammar used by checked-in fixtures.

    A blank line is required between records.  The final record may terminate
    at end-of-stream after its data line, which avoids adding a blank line to
    the fixture file while retaining unambiguous record framing.
    """

    text = text.replace("\r\n", "\n").replace("\r", "\n")
    if text.endswith("\n\n"):
        body = text[:-2]
    elif text.endswith("\n"):
        body = text[:-1]
    else:
        _fail("SSE fixture must end after a complete data line")
    blocks = body.split("\n\n")
    if not blocks or any(not block for block in blocks):
        _fail("SSE fixture contains an empty or improperly framed record")

    records: list[tuple[str, Any]] = []
    for number, block in enumerate(blocks, 1):
        lines = block.split("\n")
        if len(lines) != 2:
            _fail(f"SSE record {number} must contain exactly event and data lines")
        event_line, data_line = lines
        if not event_line.startswith("event: ") or not event_line[7:]:
            _fail(f"SSE record {number} must start with an event line")
        if not data_line.startswith("data: "):
            _fail(f"SSE record {number} must pair its event with one data line")
        try:
            payload = json.loads(data_line[6:])
        except json.JSONDecodeError as error:
            _fail(f"SSE record {number} has invalid JSON data: {error.msg}")
        records.append((event_line[7:], payload))
    return records


def _validate_cumulative_usage(
    previous: dict | None, current: dict | None, path: str
) -> None:
    if previous is None or current is None:
        return
    for counter in TOKEN_COUNTERS:
        before = previous.get(counter)
        after = current.get(counter)
        if before is not None and (after is None or after < before):
            _fail(f"{path}: {counter} is not cumulative")


def validate_sse_stream(text: str, operation: dict, document: dict) -> None:
    events = _sse_events(operation, document)
    records = _parse_sse_records(text)
    by_name = {event["name"]: event for event in events}
    if not records:
        _fail("SSE fixture must contain at least one record")
    first_name = records[0][0]
    first_event = by_name.get(first_name)
    if first_name != "response.started" and not (
        first_name == "error" and first_event is not None and first_event["terminal"]
    ):
        _fail("SSE fixture must begin with response.started or terminal error")
    terminal_seen = False
    start_seen = False
    observed_choices: set[int] = set()
    finished_choices: set[int] = set()
    previous_usage: dict | None = None
    for number, (name, payload) in enumerate(records, 1):
        event = by_name.get(name)
        if event is None:
            _fail(f"SSE record {number} uses undeclared event {name!r}")
        if name == "response.started":
            if start_seen:
                _fail("SSE fixture must not contain duplicate response.started events")
            start_seen = True
        if terminal_seen:
            _fail(f"SSE record {number} appears after a terminal event")
        _validate_schema(payload, event["schema"], document, f"SSE record {number} ({name})")
        if name in ("choice.delta", "tool_call.delta"):
            choice_index = payload["choice_index"]
            observed_choices.add(choice_index)
            if choice_index in finished_choices:
                _fail(f"SSE record {number} emits {name!r} after choice.finished")
        elif name == "choice.finished":
            choice_index = payload["choice_index"]
            if choice_index in finished_choices:
                _fail(f"SSE record {number} duplicates choice.finished")
            finished_choices.add(choice_index)
        elif name == "usage":
            _validate_cumulative_usage(
                previous_usage, payload, f"SSE record {number} ({name})"
            )
            previous_usage = payload
        elif name == "response.completed":
            if not observed_choices.issubset(finished_choices):
                _fail(
                    f"SSE record {number} completes before every observed choice is finished"
                )
            _validate_cumulative_usage(
                previous_usage,
                payload["usage"],
                f"SSE record {number} ({name})",
            )
        terminal_seen = event["terminal"]
    if not terminal_seen:
        _fail("SSE fixture must end with a terminal event")


def _check_examples(value: Any) -> None:
    if isinstance(value, dict):
        assert not ("value" in value and "externalValue" in value)
        for child in value.values():
            _check_examples(child)
    elif isinstance(value, list):
        for child in value:
            _check_examples(child)


def validate(document: dict, *, root: Path | None = None) -> None:
    assert document.get("openapi") == "3.1.0"
    assert set(document.get("paths", {})) == PATHS | BATCH_PATHS
    schemas = document["components"]["schemas"]
    responses = document["components"]["responses"]
    for request_name in ("ChatRequest", "EmbeddingRequest"):
        model = schemas[request_name]["properties"]["model"]
        assert model == {"type": "string", "minLength": 1, "maxLength": 128}
    assert "WWW-Authenticate" in document["components"]["headers"]
    root = ROOT if root is None else Path(root)

    for path, item in document["paths"].items():
        if path in BATCH_PATHS:
            for method, operation in item.items():
                assert method in {"get", "post"}
                assert operation["security"] == [{"gatewayApiKey": []}]
                assert operation["responses"]["401"]["$ref"] == "#/components/responses/Problem401"
                assert operation["responses"]["404"]["$ref"] == "#/components/responses/Problem404"
                assert "200" in operation["responses"]
            create = document["paths"]["/api/inference/v1/batches"]["post"]
            assert create["parameters"][0]["$ref"].endswith("/IdempotencyKey")
            continue
        operation = item["post"]
        assert operation["security"] == [{"gatewayApiKey": []}]
        assert {key for key in operation["responses"] if key != "default"} == STATUS_RESPONSES
        assert operation["parameters"][0]["$ref"].endswith("/IdempotencyKey")
        assert operation["responses"]["401"]["$ref"] == "#/components/responses/Problem401"
        assert operation["responses"]["404"]["$ref"] == "#/components/responses/Problem404"
        if path.endswith("/stream"):
            events = _sse_events(operation, document)
            assert operation["x-resumable"] is False
            stream = operation["responses"]["200"]["content"]["text/event-stream"]
            examples = stream.get("examples", {})
            fixture_paths: set[Path] = set()
            for example_name, example in examples.items():
                if not isinstance(example, dict):
                    _fail(f"SSE example {example_name!r} must be an object")
                if "value" in example:
                    if not isinstance(example["value"], str):
                        _fail(f"SSE example {example_name!r} value must be a string")
                    validate_sse_stream(example["value"], operation, document)
                fixture_ref = example.get("x-fixture")
                if fixture_ref is None:
                    continue
                if "value" not in example:
                    _fail(f"SSE example {example_name!r} with x-fixture must have a value")
                if not isinstance(fixture_ref, str) or Path(fixture_ref).is_absolute():
                    _fail(f"SSE example {example_name!r} has an invalid x-fixture")
                fixture_path = (root / fixture_ref).resolve()
                try:
                    fixture_path.relative_to(root.resolve())
                except ValueError:
                    _fail(f"SSE example {example_name!r} x-fixture escapes OpenAPI root")
                if not fixture_path.is_file():
                    _fail(f"SSE example {example_name!r} x-fixture does not exist: {fixture_ref}")
                fixture_paths.add(fixture_path)
            if not fixture_paths:
                _fail("SSE response example must link a fixture with x-fixture")
            for fixture_path in fixture_paths:
                validate_sse_stream(fixture_path.read_text(), operation, document)
        else:
            assert "application/json" in operation["responses"]["200"]["content"]
    assert "WWW-Authenticate" in responses["Problem401"]["headers"]
    assert "Retry-After" in responses["Problem429"]["headers"]
    _check_examples(document)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    document = json.loads(SOURCE.read_text())
    validate(document)
    output = rendered(document)
    if args.check:
        if TARGET.read_text() != output:
            raise SystemExit("inference-v1.json is stale; run python3 openapi/generate.py")
    else:
        TARGET.write_text(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
