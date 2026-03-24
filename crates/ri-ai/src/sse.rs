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

use std::fmt;

use futures::StreamExt;
use ri::{ApiError, StreamEvent};

// -- HTTP error type --

/// HTTP-level error from a provider API. Carries the status code and raw
/// response body so diagnostics survive the `BoxError` journey through `ApiError`.
///
/// Shared by all providers that use `sse::send` (Anthropic, Gemini).
/// Providers with custom HTTP handling (Codex) define their own.
#[derive(Debug)]
pub struct HttpApiError {
    pub status: u16,
    pub body: String,
}

impl fmt::Display for HttpApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Pretty-print if the body is valid JSON; proxies and CDNs may
        // return HTML or plain text during outages, so fall back to raw.
        let pretty = serde_json::from_str::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or_else(|| self.body.clone());

        if let Some(error) = serde_json::from_str::<serde_json::Value>(&self.body)
            .ok()
            .as_ref()
            .and_then(|p| p.get("error"))
        {
            let error_type = error["type"].as_str().unwrap_or("unknown");
            let message = error["message"].as_str().unwrap_or(&self.body);
            return write!(f, "HTTP {}: {}: {}\n\nRaw response:\n{}",
                self.status, error_type, message, pretty);
        }

        write!(f, "HTTP {}\n\nRaw response:\n{}", self.status, pretty)
    }
}

impl std::error::Error for HttpApiError {}

// -- HTTP dispatch --

pub async fn send(
    builder: reqwest::RequestBuilder,
) -> Result<impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send, ApiError> {
    let response = builder.send().await
        .map_err(|e| ApiError::other(e))?;
    let status = response.status().as_u16();

    if status >= 400 {
        let body = response.text().await.unwrap_or_default();
        return Err(classify_http_error(status, &body));
    }

    Ok(response.bytes_stream())
}

/// Wrap an HTTP error response in the appropriate `ApiError` variant.
/// Classification reads from the raw body; formatting lives in `HttpApiError::Display`.
fn classify_http_error(status: u16, body: &str) -> ApiError {
    let err = HttpApiError { status, body: body.to_string() };

    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(error) = parsed.get("error") {
            let error_type = error["type"].as_str().unwrap_or("unknown");
            let message = error["message"].as_str().unwrap_or("");

            if error_type == "rate_limit_error" || status == 429 {
                return ApiError::rate_limited(5000, err);
            }
            if message.contains("token") && (message.contains("exceed") || message.contains("limit")) {
                return ApiError::context_overflow(err);
            }
            return ApiError::other(err);
        }
    }

    if status == 429 || body.contains("RESOURCE_EXHAUSTED") {
        return ApiError::rate_limited(5000, err);
    }

    ApiError::other(err)
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
                    yield Err(ApiError::other(e));
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
