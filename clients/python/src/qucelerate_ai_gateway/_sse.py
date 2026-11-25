"""An incremental text/event-stream parser, as the WHATWG HTML specification
defines it: LF, CR and CRLF line endings, comment lines, data split across
lines and joined with LF, and chunk boundaries anywhere."""

from __future__ import annotations

import re
from typing import List, Tuple

#: The most one unfinished event may buffer: its pending line plus the data
#: lines read so far. Past it the stream fails, so a peer cannot grow the
#: client's memory without limit.
MAX_EVENT_BYTES = 16 * 1024 * 1024

_LINE_END = re.compile(rb"\r\n|\r|\n")


class OversizedEvent(Exception):
    """An event grew past MAX_EVENT_BYTES."""


class SseParser:
    def __init__(self) -> None:
        # The current line, not yet ended.
        self._buffer = bytearray()
        self._skip_lf = False
        self._name = ""
        # The data lines so far, joined with LF as they arrive, so the limit
        # counts every byte held however many lines the event has.
        self._data = bytearray()
        self._has_data = False

    def feed(self, chunk: bytes, events: List[Tuple[str, str]]) -> None:
        """Feed a chunk, appending the (event name, data) pairs it completes to
        events, even those before an OversizedEvent. Each byte is scanned once,
        however the stream is chunked."""
        if self._skip_lf and chunk:
            if chunk[:1] == b"\n":
                chunk = chunk[1:]
            self._skip_lf = False
        start = 0
        for match in _LINE_END.finditer(chunk):
            self._append(chunk, start, match.start())
            line = bytes(self._buffer)
            self._buffer.clear()
            self._line(line, events)
            start = match.end()
            if match.group() == b"\r" and start == len(chunk):
                self._skip_lf = True
        self._append(chunk, start, len(chunk))

    def _append(self, chunk: bytes, start: int, end: int) -> None:
        if len(self._data) + len(self._buffer) + end - start > MAX_EVENT_BYTES:
            raise OversizedEvent()
        self._buffer.extend(chunk[start:end])

    def _line(self, raw: bytes, events: List[Tuple[str, str]]) -> None:
        line = raw.decode("utf-8", errors="replace")
        if line == "":
            if self._has_data:
                events.append((self._name or "message", self._data.decode("utf-8", errors="replace")))
            self._name, self._has_data = "", False
            self._data.clear()
            return
        if line.startswith(":"):
            return
        field, _, value = line.partition(":")
        if value.startswith(" "):
            value = value[1:]
        if field == "event":
            self._name = value
        elif field == "data":
            if self._has_data:
                self._data += b"\n"
            self._data += value.encode()
            self._has_data = True
