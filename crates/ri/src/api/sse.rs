// Shared SSE (Server-Sent Events) parser.
//
// Both Anthropic and Gemini use standard SSE framing:
//   event: <type>\n      (optional -- Gemini omits this)
//   data: <payload>\n
//   \n                   (blank line = event boundary)
//
// This parser handles the wire format. Each provider interprets
// the payloads independently.

pub struct SseEvent {
    pub event_type: String,
    pub data: String,
}

pub struct SseParser {
    buffer: String,
}

impl SseParser {
    pub fn new() -> Self {
        Self { buffer: String::new() }
    }

    pub fn feed(&mut self, chunk: &str) -> Vec<SseEvent> {
        // Normalize line endings
        self.buffer.push_str(&chunk.replace("\r\n", "\n"));

        let mut events = Vec::new();
        while let Some(pos) = self.buffer.find("\n\n") {
            let block = self.buffer[..pos].to_string();
            self.buffer = self.buffer[pos + 2..].to_string();

            if let Some(event) = parse_block(&block) {
                events.push(event);
            }
        }
        events
    }

    pub fn flush(&mut self) -> Vec<SseEvent> {
        if self.buffer.trim().is_empty() {
            self.buffer.clear();
            return Vec::new();
        }
        let block = std::mem::take(&mut self.buffer);
        match parse_block(&block) {
            Some(event) => vec![event],
            None => Vec::new(),
        }
    }
}

fn parse_block(block: &str) -> Option<SseEvent> {
    let mut event_type = String::new();
    let mut data = String::new();

    for line in block.lines() {
        let line = line.trim();
        if line.starts_with(':') {
            continue; // SSE comment
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event_type = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }

    if event_type.is_empty() && data.is_empty() {
        return None;
    }

    Some(SseEvent { event_type, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_style() {
        let mut p = SseParser::new();
        let events = p.feed("event: message_start\ndata: {\"type\":\"message\"}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "message_start");
        assert_eq!(events[0].data, "{\"type\":\"message\"}");
    }

    #[test]
    fn gemini_style() {
        let mut p = SseParser::new();
        let events = p.feed("data: {\"candidates\":[{}]}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "");
        assert_eq!(events[0].data, "{\"candidates\":[{}]}");
    }

    #[test]
    fn partial_chunks() {
        let mut p = SseParser::new();
        assert!(p.feed("event: ping\n").is_empty());
        assert!(p.feed("data: {}\n").is_empty());
        let events = p.feed("\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "ping");
    }

    #[test]
    fn multi_data_lines() {
        let mut p = SseParser::new();
        let events = p.feed("data: line1\ndata: line2\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn crlf_handling() {
        let mut p = SseParser::new();
        let events = p.feed("event: test\r\ndata: ok\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "test");
    }
}
