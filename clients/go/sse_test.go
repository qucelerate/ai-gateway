package gateway

import (
	"bytes"
	"errors"
	"reflect"
	"testing"
	"time"
)

func parseInPieces(t *testing.T, text []byte, size int) []sseEvent {
	t.Helper()
	var parser sseParser
	var events []sseEvent
	for start := 0; start < len(text); start += size {
		end := min(start+size, len(text))
		more, oversized := parser.feed(text[start:end])
		if oversized {
			t.Fatalf("oversized at chunk size %d", size)
		}
		events = append(events, more...)
	}
	return events
}

func TestEveryChunkingGivesTheSameEvents(t *testing.T) {
	text := []byte("event: a\r\ndata: {\"x\":\r\ndata: \"é\"}\r\n\r\n: comment\n\nevent: b\rdata: 2\r\r")
	want := []sseEvent{{"a", "{\"x\":\n\"é\"}"}, {"b", "2"}}
	for size := 1; size <= len(text); size++ {
		if got := parseInPieces(t, text, size); !reflect.DeepEqual(got, want) {
			t.Fatalf("chunk size %d: %v", size, got)
		}
	}
}

func TestAnEventWithoutABlankLineIsNotDispatched(t *testing.T) {
	if got := parseInPieces(t, []byte("event: a\ndata: 1\n"), 4); len(got) != 0 {
		t.Fatal(got)
	}
}

func TestAnEndlessLineFailsAtTheLimit(t *testing.T) {
	var parser sseParser
	chunk := bytes.Repeat([]byte("a"), 1<<20)
	for range 16 {
		if _, oversized := parser.feed(chunk); oversized {
			t.Fatal("oversized too early")
		}
	}
	if events, oversized := parser.feed([]byte("a")); !oversized || len(events) != 0 {
		t.Fatal("expected oversized")
	}
}

func TestEndlessDataLinesFailAtTheLimit(t *testing.T) {
	var parser sseParser
	line := append(append([]byte("data: "), bytes.Repeat([]byte("a"), 1<<20)...), '\n')
	for range 17 {
		if _, oversized := parser.feed(line); oversized {
			return
		}
	}
	t.Fatal("expected oversized")
}

func TestManyEmptyDataLinesCountTowardTheLimit(t *testing.T) {
	var parser sseParser
	chunk := bytes.Repeat([]byte("data:\n"), 1<<20)
	for range 17 {
		if _, oversized := parser.feed(chunk); oversized {
			return
		}
	}
	t.Fatal("expected oversized")
}

func TestEmptyDataLinesJoinWithLF(t *testing.T) {
	if got := parseInPieces(t, []byte("data:\ndata:\ndata: x\n\ndata:\n\n"), 3); !reflect.DeepEqual(got, []sseEvent{{"message", "\n\nx"}, {"message", ""}}) {
		t.Fatalf("%q", got)
	}
}

func TestEventsBeforeAnOversizedOneAreKept(t *testing.T) {
	var parser sseParser
	chunk := append([]byte("data: 1\n\n"), bytes.Repeat([]byte("a"), maxEventBytes+1)...)
	events, oversized := parser.feed(chunk)
	if !oversized || len(events) != 1 {
		t.Fatalf("%v %v", oversized, events)
	}
}

func TestALongEventInTinyChunksIsLinear(t *testing.T) {
	text := append([]byte("event: a\ndata: "), bytes.Repeat([]byte("x"), 4<<20)...)
	text = append(text, "\n\n"...)
	started := time.Now()
	events := parseInPieces(t, text, 7)
	if len(events) != 1 || len(events[0].data) != 4<<20 {
		t.Fatal("wrong events")
	}
	if time.Since(started) > 5*time.Second {
		t.Fatal("too slow")
	}
}

const started = "event: response.started\ndata: {\"model\":\"m\"}\n\n"

func streamOf(text string) *Stream {
	return &Stream{body: nopCloser{bytes.NewReader([]byte(text))}}
}

func TestAnUnknownEventWithNonJSONDataIsSkipped(t *testing.T) {
	stream := streamOf(started + "event: ping\ndata: keepalive\n\n" +
		"event: response.completed\ndata: {\"model\":\"m\",\"usage\":null}\n\n")
	var names []string
	for stream.Next() {
		names = append(names, stream.Event().EventName())
	}
	if stream.Err() != nil || !reflect.DeepEqual(names, []string{"response.started", "response.completed"}) {
		t.Fatalf("%v %v", names, stream.Err())
	}
}

func TestAMalformedErrorEventKeepsEarlierEvents(t *testing.T) {
	for _, data := range []string{"not json", "[]", `{"code":"x","status":"bad"}`, `{"status":502}`} {
		stream := streamOf(started + "event: error\ndata: " + data + "\n\n")
		count := 0
		for stream.Next() {
			count++
		}
		var decodeError *DecodeError
		if count != 1 || !errors.As(stream.Err(), &decodeError) {
			t.Errorf("%s: %d events, %v", data, count, stream.Err())
		}
	}
}

func TestAnOversizedEventIsAProtocolError(t *testing.T) {
	stream := streamOf(started + string(bytes.Repeat([]byte("a"), maxEventBytes+1)))
	count := 0
	for stream.Next() {
		count++
	}
	if count != 1 || !errors.Is(stream.Err(), ErrProtocol) {
		t.Fatalf("%d %v", count, stream.Err())
	}
}

type nopCloser struct{ *bytes.Reader }

func (nopCloser) Close() error { return nil }
