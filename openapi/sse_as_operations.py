#!/usr/bin/env python3
"""Expose SSE event payloads to OpenAPI diff tools.

The stream operation's response is `text/event-stream` (a binary string), and
its event payloads are named only in the `x-sse-events` extension, which
breaking-change tools such as oasdiff do not follow. This writes a copy of the
document in which every SSE event is also the JSON response of a synthetic
operation, `GET /x-sse-events/{event name}`, so the tool applies its response
rules to each event's payload and to every schema it references.

Usage: sse_as_operations.py SOURCE.json TARGET.json
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

STREAM = "/api/inference/v1/chat/completions/stream"


def with_sse_operations(document: dict) -> dict:
    document = json.loads(json.dumps(document))
    events = document["paths"][STREAM]["post"]["x-sse-events"]
    for event in events:
        path = f"/x-sse-events/{event['name']}"
        if path in document["paths"]:
            raise SystemExit(f"{path} already exists")
        document["paths"][path] = {
            "get": {
                "operationId": f"sse_{event['name'].replace('.', '_')}",
                "responses": {
                    "200": {
                        "description": f"Payload of the `{event['name']}` SSE event",
                        "content": {"application/json": {"schema": event["schema"]}},
                    }
                },
            }
        }
    return document


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        raise SystemExit(__doc__)
    source, target = Path(argv[1]), Path(argv[2])
    document = with_sse_operations(json.loads(source.read_text()))
    target.write_text(json.dumps(document, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
