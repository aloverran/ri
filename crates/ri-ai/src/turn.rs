//! A single LLM turn: call the provider and accumulate the response.
//!
//! `Turn` wraps the provider's event stream and an internal `StreamAccumulator`.
//! The caller polls events with `next()` (for real-time display or logging),
//! then calls `finish()` to extract the completed content blocks and usage.

use futures::StreamExt;
use tracing::Instrument;
use ri::{
    ApiError, ContentBlock, EventStream, LlmProvider, RequestOptions,
    StreamAccumulator, StreamEvent, Usage,
};

/// A single LLM turn in progress.
///
/// Wraps the streaming response and accumulates content blocks internally.
/// Call `next()` repeatedly to drive the stream, then `finish()` to get
/// the assembled result.
///
/// Carries a tracing span that covers the turn's lifetime -- from the
/// initial API call through streaming to finish. The span is entered
/// on each async poll via `.instrument()`, never held across awaits.
pub struct Turn {
    stream: EventStream,
    acc: StreamAccumulator,
    span: tracing::Span,
}

impl Turn {
    /// Start a turn by calling the provider.
    pub async fn start(
        provider: &dyn LlmProvider,
        opts: RequestOptions,
    ) -> Result<Self, ApiError> {
        let span = tracing::info_span!(
            "llm_turn",
            provider = provider.id(),
            model = %opts.model.id,
        );
        let stream = provider.stream(opts).instrument(span.clone()).await?;
        Ok(Self {
            stream,
            acc: StreamAccumulator::new(),
            span,
        })
    }

    /// Poll the next stream event. Each event is also fed to the internal
    /// accumulator. Returns None when the stream is exhausted.
    pub async fn next(&mut self) -> Option<Result<StreamEvent, ApiError>> {
        let item = self.stream.next().instrument(self.span.clone()).await?;
        if let Ok(ref event) = item {
            self.acc.feed(event);
        }
        Some(item)
    }

    /// Consume the turn and return the accumulated content blocks and usage.
    /// Call after the stream is exhausted (next() returned None).
    pub fn finish(self) -> (Vec<ContentBlock>, Option<Usage>) {
        let span = self.span;
        let (content, usage) = self.acc.finish();
        if let Some(ref u) = usage {
            tracing::info!(
                parent: &span,
                u.input_tokens, u.output_tokens,
                u.cache_read_tokens, u.cache_write_tokens,
                "turn complete",
            );
        }
        (content, usage)
    }
}
