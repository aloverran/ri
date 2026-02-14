//! A single LLM turn: call the provider and accumulate the response.
//!
//! `Turn` wraps the provider's event stream and an internal `StreamAccumulator`.
//! The caller polls events with `next()` (for real-time display or logging),
//! then calls `finish()` to extract the completed content blocks and usage.

use futures::StreamExt;
use ri::{
    ApiError, ContentBlock, EventStream, LlmProvider, RequestOptions,
    StreamAccumulator, StreamEvent, Usage,
};

/// A single LLM turn in progress.
///
/// Wraps the streaming response and accumulates content blocks internally.
/// Call `next()` repeatedly to drive the stream, then `finish()` to get
/// the assembled result.
pub struct Turn {
    stream: EventStream,
    acc: StreamAccumulator,
}

impl Turn {
    /// Start a turn by calling the provider.
    pub async fn start(
        provider: &dyn LlmProvider,
        opts: RequestOptions,
    ) -> Result<Self, ApiError> {
        let stream = provider.stream(opts).await?;
        Ok(Self {
            stream,
            acc: StreamAccumulator::new(),
        })
    }

    /// Poll the next stream event. Each event is also fed to the internal
    /// accumulator. Returns None when the stream is exhausted.
    pub async fn next(&mut self) -> Option<Result<StreamEvent, ApiError>> {
        let item = self.stream.next().await?;
        if let Ok(ref event) = item {
            self.acc.feed(event);
        }
        Some(item)
    }

    /// Consume the turn and return the accumulated content blocks and usage.
    /// Call after the stream is exhausted (next() returned None).
    pub fn finish(self) -> (Vec<ContentBlock>, Option<Usage>) {
        self.acc.finish()
    }
}
