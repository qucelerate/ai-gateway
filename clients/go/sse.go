package gateway

import (
	"bytes"
	"strings"
)

// maxEventBytes is the most one unfinished event may buffer: its pending
// line plus the data lines read so far. Past it the stream fails, so a peer
// cannot grow the client's memory without limit.
const maxEventBytes = 16 << 20

// sseEvent is one dispatched server-sent event.
type sseEvent struct {
	name string
	data string
}

// sseParser parses text/event-stream incrementally, as the WHATWG HTML
// specification defines it: LF, CR and CRLF line endings, comment lines,
// data split across lines and joined with LF, and chunk boundaries anywhere.
// Each byte is scanned once, however the stream is chunked.
type sseParser struct {
	// line is the current line, not yet ended.
	line    []byte
	skipLF  bool
	name    string
	data    []byte
	hasData bool
}

// feed parses a chunk, returning the events it completed. It reports
// oversized when an event passed maxEventBytes; the events before it are
// still returned.
func (p *sseParser) feed(chunk []byte) (events []sseEvent, oversized bool) {
	if p.skipLF && len(chunk) > 0 {
		if chunk[0] == '\n' {
			chunk = chunk[1:]
		}
		p.skipLF = false
	}
	for len(chunk) > 0 {
		end := bytes.IndexAny(chunk, "\r\n")
		if end < 0 {
			break
		}
		if !p.append(chunk[:end]) {
			return events, true
		}
		p.dispatch(&events)
		next := end + 1
		if chunk[end] == '\r' {
			if next < len(chunk) {
				if chunk[next] == '\n' {
					next++
				}
			} else {
				p.skipLF = true
			}
		}
		chunk = chunk[next:]
	}
	if !p.append(chunk) {
		return events, true
	}
	return events, false
}

func (p *sseParser) append(part []byte) bool {
	if len(p.data)+len(p.line)+len(part) > maxEventBytes {
		return false
	}
	p.line = append(p.line, part...)
	return true
}

func (p *sseParser) dispatch(events *[]sseEvent) {
	line := strings.ToValidUTF8(string(p.line), "�")
	p.line = p.line[:0]
	if line == "" {
		if p.hasData {
			name := p.name
			if name == "" {
				name = "message"
			}
			*events = append(*events, sseEvent{name: name, data: string(p.data)})
		}
		p.name, p.data, p.hasData = "", p.data[:0], false
		return
	}
	if strings.HasPrefix(line, ":") {
		return
	}
	field, value, _ := strings.Cut(line, ":")
	value = strings.TrimPrefix(value, " ")
	switch field {
	case "event":
		p.name = value
	case "data":
		// One buffer, joined with LF as it grows, so the limit counts every
		// byte held, however many lines the event has.
		if p.hasData {
			p.data = append(p.data, '\n')
		}
		p.data = append(p.data, value...)
		p.hasData = true
	}
}
