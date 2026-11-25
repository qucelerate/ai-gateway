import time

import pytest

from qucelerate_ai_gateway._base import EventDecoder, _PendingStreamError, stream_timeout
from qucelerate_ai_gateway._errors import DecodeError, ProtocolError
from qucelerate_ai_gateway._sse import MAX_EVENT_BYTES, OversizedEvent, SseParser

TEXT = "event: a\r\ndata: {\"x\":\r\ndata: \"é\"}\r\n\r\n: comment\n\nevent: b\rdata: 2\r\r".encode()


def parse(text: bytes, size: int):
    parser = SseParser()
    events = []
    for start in range(0, len(text), size):
        parser.feed(text[start : start + size], events)
    return events


def test_every_chunking_gives_the_same_events():
    whole = parse(TEXT, len(TEXT))
    assert whole == [("a", '{"x":\n"é"}'), ("b", "2")]
    for size in range(1, len(TEXT)):
        assert parse(TEXT, size) == whole, size


def test_an_event_without_a_blank_line_is_not_dispatched():
    assert parse(b"event: a\ndata: 1\n", 4) == []


def test_an_endless_line_fails_at_the_limit():
    parser = SseParser()
    events = []
    chunk = b"a" * (1024 * 1024)
    for _ in range(16):
        parser.feed(chunk, events)
    with pytest.raises(OversizedEvent):
        parser.feed(b"a", events)
    assert events == []


def test_endless_data_lines_fail_at_the_limit():
    parser = SseParser()
    events = []
    line = b"data: " + b"a" * (1024 * 1024) + b"\n"
    with pytest.raises(OversizedEvent):
        for _ in range(17):
            parser.feed(line, events)
    assert events == []


def test_many_empty_data_lines_count_toward_the_limit():
    parser = SseParser()
    with pytest.raises(OversizedEvent):
        for _ in range(17):
            parser.feed(b"data:\n" * (1024 * 1024), [])


def test_empty_data_lines_join_with_lf():
    assert [data for _, data in parse(b"data:\ndata:\ndata: x\n\ndata:\n\n", 3)] == ["\n\nx", ""]


def test_events_before_an_oversized_one_are_kept():
    events = []
    with pytest.raises(OversizedEvent):
        SseParser().feed(b"data: 1\n\n" + b"a" * (MAX_EVENT_BYTES + 1), events)
    assert events == [("message", "1")]


def test_the_decoder_ends_the_stream_with_a_protocol_error():
    started = b'event: response.started\ndata: {"model":"m"}\n\n'
    with pytest.raises(_PendingStreamError) as caught:
        EventDecoder().feed(started + b"a" * (MAX_EVENT_BYTES + 1))
    assert [event.event for event in caught.value.events] == ["response.started"]
    assert isinstance(caught.value.error, ProtocolError)


def test_a_long_event_in_tiny_chunks_is_linear():
    text = b"event: a\ndata: " + b"x" * (1024 * 1024) + b"\n\n"
    started = time.monotonic()
    events = parse(text, 7)
    assert len(events) == 1 and len(events[0][1]) == 1024 * 1024
    assert time.monotonic() - started < 10


STARTED = b'event: response.started\ndata: {"model":"m"}\n\n'
DELTA = b'event: choice.delta\ndata: {"choice_index":0,"content_delta":"a","refusal_delta":null,"annotations":[]}\n\n'


def test_an_unknown_event_with_non_json_data_is_skipped():
    events = EventDecoder().feed(STARTED + b"event: ping\ndata: keepalive\n\n" + DELTA)
    assert [event.event for event in events] == ["response.started", "choice.delta"]


@pytest.mark.parametrize("data", [b"not json", b"[]", b'{"code":"x","status":"bad"}', b'{"status":502}'])
def test_a_malformed_error_event_keeps_earlier_events(data):
    with pytest.raises(_PendingStreamError) as caught:
        EventDecoder().feed(STARTED + DELTA + b"event: error\ndata: " + data + b"\n\n")
    assert [event.event for event in caught.value.events] == ["response.started", "choice.delta"]
    assert isinstance(caught.value.error, DecodeError)


def test_a_known_event_with_a_non_object_payload_is_a_decode_error():
    with pytest.raises(_PendingStreamError) as caught:
        EventDecoder().feed(STARTED + b"event: choice.delta\ndata: []\n\n")
    assert isinstance(caught.value.error, DecodeError)


def test_streams_keep_finite_connect_write_and_pool_limits():
    timeout = stream_timeout(120.0)
    assert timeout.read is None
    assert (timeout.connect, timeout.write, timeout.pool) == (10.0, 120.0, 120.0)
