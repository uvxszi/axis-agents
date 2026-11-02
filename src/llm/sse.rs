//! A minimal Server-Sent Events parser over a byte stream.
//!
//! Bytes are buffered until a full line arrives, so multibyte UTF-8
//! sequences split across network chunks are never decoded halfway.

#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    data: Vec<String>,
}

impl SseParser {
    /// Feed bytes; returns the `data` payloads of every event completed by
    /// them (multi-line data joined with `\n`).
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                    self.data.clear();
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                self.data
                    .push(rest.strip_prefix(' ').unwrap_or(rest).to_owned());
            }
            // `event:`, `id:`, `retry:` and `:` comments are not needed:
            // payloads carry their own `type`.
        }
        events
    }

    /// Flush a final event that was not followed by a blank line.
    pub fn finish(&mut self) -> Option<String> {
        if !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            let line = String::from_utf8_lossy(&rest).trim().to_owned();
            if let Some(data) = line.strip_prefix("data:") {
                self.data.push(data.trim_start().to_owned());
            }
        }
        if self.data.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.data).join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_events_across_chunks() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"data: {\"a\"").is_empty());
        let events = parser.push(b":1}\n\n: comment\n\ndata: [DONE]\n\n");
        assert_eq!(events, vec!["{\"a\":1}", "[DONE]"]);
    }

    #[test]
    fn utf8_split_mid_character() {
        let mut parser = SseParser::default();
        let bytes = "data: héllo\n\n".as_bytes();
        let (a, b) = bytes.split_at(8); // inside the two-byte é
        assert!(parser.push(a).is_empty());
        assert_eq!(parser.push(b), vec!["héllo"]);
    }

    #[test]
    fn crlf_and_multiline() {
        let mut parser = SseParser::default();
        let events = parser.push(b"event: x\r\ndata: one\r\ndata: two\r\n\r\n");
        assert_eq!(events, vec!["one\ntwo"]);
        parser.push(b"data: tail");
        assert_eq!(parser.finish().as_deref(), Some("tail"));
    }
}
