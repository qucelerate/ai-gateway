//! An incremental `text/event-stream` parser, as the WHATWG HTML
//! specification defines it: LF, CR, and CRLF line endings, comment lines,
//! `data` split across lines and joined with LF, and chunk boundaries
//! anywhere, including inside a CRLF pair or a multi-byte character.

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RawEvent {
    pub name: String,
    pub data: String,
}

/// The most one unfinished event may buffer: its pending line plus the data
/// lines read so far. Past it the stream fails, so a peer cannot grow the
/// client's memory without limit.
pub(crate) const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// An event grew past [`MAX_EVENT_BYTES`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Oversized;

#[derive(Default)]
pub(crate) struct Parser {
    /// The current line, not yet ended.
    buffer: Vec<u8>,
    /// The previous chunk ended in CR, so a leading LF belongs to it.
    skip_lf: bool,
    name: String,
    /// The data lines so far, joined with LF as they arrive, so the limit
    /// counts every byte held however many lines the event has.
    data: String,
    has_data: bool,
}

impl Parser {
    /// Feed a chunk, adding the events it completes to `events`, even those
    /// before an [`Oversized`] error. Each byte is scanned once, however the
    /// stream is chunked.
    pub fn feed(&mut self, chunk: &[u8], events: &mut Vec<RawEvent>) -> Result<(), Oversized> {
        let mut chunk = chunk;
        if self.skip_lf && !chunk.is_empty() {
            if chunk[0] == b'\n' {
                chunk = &chunk[1..];
            }
            self.skip_lf = false;
        }
        let mut start = 0;
        let mut index = 0;
        while index < chunk.len() {
            match chunk[index] {
                ending @ (b'\n' | b'\r') => {
                    self.take_line(&chunk[start..index], events)?;
                    index += 1;
                    if ending == b'\r' {
                        if index < chunk.len() {
                            if chunk[index] == b'\n' {
                                index += 1;
                            }
                        } else {
                            self.skip_lf = true;
                        }
                    }
                    start = index;
                }
                _ => index += 1,
            }
        }
        self.append(&chunk[start..])
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), Oversized> {
        if self.data.len() + self.buffer.len() + bytes.len() > MAX_EVENT_BYTES {
            return Err(Oversized);
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    fn take_line(&mut self, tail: &[u8], events: &mut Vec<RawEvent>) -> Result<(), Oversized> {
        self.append(tail)?;
        let line = std::mem::take(&mut self.buffer);
        self.line(&line, events);
        Ok(())
    }

    fn line(&mut self, line: &[u8], events: &mut Vec<RawEvent>) {
        let line = String::from_utf8_lossy(line);
        if line.is_empty() {
            if self.has_data {
                let name = std::mem::take(&mut self.name);
                events.push(RawEvent {
                    name: if name.is_empty() {
                        "message".to_owned()
                    } else {
                        name
                    },
                    data: std::mem::take(&mut self.data),
                });
                self.has_data = false;
            } else {
                self.name.clear();
            }
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.find(':') {
            Some(position) => {
                let value = &line[position + 1..];
                (&line[..position], value.strip_prefix(' ').unwrap_or(value))
            }
            None => (line.as_ref(), ""),
        };
        match field {
            "event" => self.name = value.to_owned(),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_in_pieces(text: &[u8], size: usize) -> Vec<RawEvent> {
        let mut parser = Parser::default();
        text.chunks(size)
            .flat_map(|chunk| {
                let mut events = Vec::new();
                parser.feed(chunk, &mut events).unwrap();
                events
            })
            .collect()
    }

    #[test]
    fn every_chunking_gives_the_same_events() {
        let text =
            "event: a\r\ndata: {\"x\":\r\ndata: \"é\"}\r\n\r\n: comment\n\nevent: b\rdata: 2\r\r"
                .as_bytes();
        let whole = parse_in_pieces(text, text.len());
        assert_eq!(
            whole,
            vec![
                RawEvent {
                    name: "a".into(),
                    data: "{\"x\":\n\"é\"}".into()
                },
                RawEvent {
                    name: "b".into(),
                    data: "2".into()
                },
            ]
        );
        for size in 1..text.len() {
            assert_eq!(parse_in_pieces(text, size), whole, "chunk size {size}");
        }
    }

    #[test]
    fn an_event_without_a_blank_line_is_not_dispatched() {
        assert!(parse_in_pieces(b"event: a\ndata: 1\n", 4).is_empty());
    }

    #[test]
    fn an_endless_line_fails_at_the_limit() {
        let mut parser = Parser::default();
        let chunk = vec![b'a'; 1024 * 1024];
        let mut events = Vec::new();
        for _ in 0..16 {
            assert_eq!(parser.feed(&chunk, &mut events), Ok(()));
        }
        assert_eq!(parser.feed(b"a", &mut events), Err(Oversized));
        assert!(events.is_empty());
    }

    #[test]
    fn endless_data_lines_fail_at_the_limit() {
        let mut parser = Parser::default();
        let mut line = b"data: ".to_vec();
        line.extend(vec![b'a'; 1024 * 1024]);
        line.push(b'\n');
        let mut events = Vec::new();
        let mut result = Ok(());
        for _ in 0..17 {
            result = parser.feed(&line, &mut events);
            if result.is_err() {
                break;
            }
        }
        assert_eq!(result, Err(Oversized));
        assert!(events.is_empty());
    }

    #[test]
    fn many_empty_data_lines_count_toward_the_limit() {
        let mut parser = Parser::default();
        let chunk = b"data:\n".repeat(1024 * 1024);
        let mut events = Vec::new();
        let mut result = Ok(());
        for _ in 0..17 {
            result = parser.feed(&chunk, &mut events);
            if result.is_err() {
                break;
            }
        }
        assert_eq!(result, Err(Oversized));
    }

    #[test]
    fn empty_data_lines_join_with_lf() {
        let events = parse_in_pieces(b"data:\ndata:\ndata: x\n\ndata:\n\n", 3);
        let data: Vec<_> = events.iter().map(|event| event.data.as_str()).collect();
        assert_eq!(data, ["\n\nx", ""]);
    }

    #[test]
    fn events_before_an_oversized_one_are_kept() {
        let mut parser = Parser::default();
        let mut chunk = b"data: 1\n\n".to_vec();
        chunk.extend(vec![b'a'; MAX_EVENT_BYTES + 1]);
        let mut events = Vec::new();
        assert_eq!(parser.feed(&chunk, &mut events), Err(Oversized));
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn a_long_event_in_tiny_chunks_is_fast() {
        let mut text = b"event: a\ndata: ".to_vec();
        text.extend(vec![b'x'; 4 * 1024 * 1024]);
        text.extend(b"\n\n");
        let started = std::time::Instant::now();
        let events = parse_in_pieces(&text, 7);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data.len(), 4 * 1024 * 1024);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
