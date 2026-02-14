// Shared SSE (Server-Sent Events) parser and event stream driver.
//
// Both Anthropic and Gemini use standard SSE framing:
//   event: <type>\n      (optional -- Gemini omits this)
//   data: <payload>\n
//   \n                   (blank line = event boundary)
//
// This module handles the wire format. Each provider implements
// `SseInterpreter` to translate payloads into normalized StreamEvents.
// `drive_sse_stream` wires the parser to an interpreter, producing
// an EventStream.

use futures::StreamExt;
use ri::{ApiError, StreamEvent};

// -- HTTP dispatch --

pub async fn send(
    builder: reqwest::RequestBuilder,
) -> Result<impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send, ApiError> {
    let response = builder.send().await.map_err(|e| ApiError::Http(e.to_string()))?;
    let status = response.status().as_u16();

    if status >= 400 {
        let body = response.text().await.unwrap_or_default();
        return Err(parse_http_error(status, &body));
    }

    Ok(response.bytes_stream())
}

fn parse_http_error(status: u16, body: &str) -> ApiError {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();

    if let Some(error) = parsed.get("error") {
        let error_type = error["type"].as_str().unwrap_or("unknown");
        let message = error["message"].as_str().unwrap_or(body).to_string();

        if error_type == "rate_limit_error" || status == 429 {
            return ApiError::RateLimited { retry_after_ms: 5000 };
        }
        if message.contains("token") && (message.contains("exceed") || message.contains("limit")) {
            return ApiError::ContextOverflow { used: 0, limit: 0 };
        }
        return ApiError::Api { status, message: format!("{}: {}", error_type, message) };
    }

    if status == 429 || body.contains("RESOURCE_EXHAUSTED") {
        return ApiError::RateLimited { retry_after_ms: 5000 };
    }

    ApiError::Api { status, message: body.to_string() }
}

// -- SSE parsing --

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

/// Trait for SSE payload interpreters. Each provider implements this
/// to translate raw SSE events into normalized StreamEvents.
pub trait SseInterpreter: Send {
    fn interpret(&mut self, sse: &SseEvent) -> Vec<Result<StreamEvent, ApiError>>;
    /// Called after all SSE data has been consumed. Emit trailing events
    /// (e.g. usage/done if the stream ended without an explicit stop signal).
    fn finish(&mut self) -> Vec<Result<StreamEvent, ApiError>> { Vec::new() }
}

/// Convert a byte stream (from an HTTP response) into a stream of
/// normalized StreamEvents by parsing SSE frames and interpreting
/// them with the given interpreter.
pub fn drive_sse_stream(
    bytes: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    mut interpreter: impl SseInterpreter + 'static,
) -> impl futures::Stream<Item = Result<StreamEvent, ApiError>> + Send {
    async_stream::stream! {
        let mut parser = SseParser::new();
        tokio::pin!(bytes);

        while let Some(chunk) = bytes.next().await {
            match chunk {
                Ok(data) => {
                    let text = String::from_utf8_lossy(&data);
                    for sse in parser.feed(&text) {
                        for event in interpreter.interpret(&sse) {
                            yield event;
                        }
                    }
                }
                Err(e) => {
                    yield Err(ApiError::Http(e.to_string()));
                    return;
                }
            }
        }

        for sse in parser.flush() {
            for event in interpreter.interpret(&sse) {
                yield event;
            }
        }
        for event in interpreter.finish() {
            yield event;
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
