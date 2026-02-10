# ri-ai

LLM provider implementations. Turns `RequestOptions` into HTTP requests, sends them, and parses the SSE response stream into `StreamEvent`s.

Currently supports Anthropic (Messages API) and Google Gemini (Cloud Code Assist API, both standard and Antigravity variants).

## What's here

**Provider enum** — `Provider::Anthropic { api_key }` and `Provider::Gemini { variant, token, project_id }`. Implements `LlmProvider` from ri-core. The flow is: `build_request()` produces an `ApiRequest` (url, headers, body as plain data), `http::send()` fires it and returns a byte stream, `event_stream()` wraps that in a provider-specific SSE interpreter.

**anthropic** — Builds Anthropic Messages API requests. Handles two auth modes: API key (`x-api-key` header) and OAuth (`Bearer` token with claude-code beta headers). For OAuth, tool names are mapped to/from Claude Code's PascalCase convention. Supports thinking configuration: adaptive mode for Opus 4.6, budget-based for other reasoning models.

**gemini** — Builds Google Cloud Code Assist requests. Two variants: `Cli` (cloudcode-pa.googleapis.com) and `Antigravity` (daily sandbox endpoint for Gemini 3). Handles Gemini's `thoughtSignature` requirement — tool calls from other providers without valid signatures are converted to descriptive text to avoid API errors. Thinking levels map to Gemini's `thinkingLevel` strings for Gemini 3, or `thinkingBudget` tokens for older models.

**sse** — Shared SSE parser used by both providers. Handles standard SSE framing (event/data fields, blank-line delimiters), partial chunks, CRLF normalization, and multi-line data. Each provider interprets the parsed `SseEvent` payloads independently.

**http** — `send(ApiRequest) -> ByteStream`. Single function that fires a request via reqwest and returns the streaming response. Parses HTTP error responses into typed `ApiError` variants.

**auth** — OAuth flows for Anthropic and Google. `auth::anthropic` does PKCE authorization code flow against claude.ai. `auth::google` does PKCE flow with a local HTTP callback server (port 8085 for Gemini CLI, 51121 for Antigravity), including project discovery via the Cloud Code Assist API. `auth::pkce` provides the shared verifier/challenge utilities. `OAuthCredentials` holds refresh/access tokens with expiry tracking.

## Depends on

ri-store, ri-core. External: reqwest, tokio, serde, futures, bytes, sha2, rand, base64, chrono, uuid.
