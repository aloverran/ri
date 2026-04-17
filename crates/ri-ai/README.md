# ri-ai

LLM provider implementations. Turns `RequestOptions` into HTTP requests, sends them, and parses the SSE response stream into `StreamEvent`s.

Currently supports Anthropic (Messages API), Google Gemini (Cloud Code Assist API), and OpenAI Codex (ChatGPT Responses API).

## What's here

**anthropic** -- Builds Anthropic Messages API requests. Handles two auth modes: API key (`x-api-key` header) and OAuth (`Bearer` token with claude-code beta headers). For OAuth, tool names are mapped to/from Claude Code's PascalCase convention. Thinking configuration tracks Anthropic's split API: adaptive-effort mode for Opus 4.6+ and Sonnet 4.6+, budget-tokens mode for older reasoning models (Haiku 4.5, Sonnet 4, Sonnet 4.5).

**gemini** -- Builds Google Cloud Code Assist requests. Three variants: `Cli` (cloudcode-pa.googleapis.com), `Antigravity` (daily sandbox endpoint for Gemini 3), and `ApiKey` (generativelanguage.googleapis.com). Handles Gemini's `thoughtSignature` requirement -- tool calls from other providers without valid signatures are converted to descriptive text. Thinking levels map to Gemini's `thinkingLevel` strings for Gemini 3, or `thinkingBudget` tokens for older models.

**openai_codex** -- OpenAI Codex via the ChatGPT Responses API (`chatgpt.com/backend-api/codex/responses`). OAuth2 PKCE via auth.openai.com with a local HTTP callback. Uses the Responses API format (instructions + typed input items, encrypted reasoning replay). Tool call IDs are compound (`call_id|item_id`).

**turn** -- `Turn`: call a provider and accumulate the streamed response into content blocks. Thin wrapper over `LlmProvider::stream()` + `StreamAccumulator`. The fundamental "call the LLM once" building block.

**registry** -- Provider factories, model catalog, and resolution. `all_providers()`, `resolve(model_id)`, `available_model_ids()`. Code-defined, no JSON config.

**sse** -- Shared SSE parser used by all providers. Handles standard SSE framing (event/data fields, blank-line delimiters), partial chunks, CRLF normalization. Each provider interprets the parsed `SseEvent` payloads independently.

**creds** -- Credential persistence. OAuth tokens stored in `~/.ri/auth.json` with per-provider sections. Refresh/access tokens with expiry tracking.

**gemini_auth** -- Google OAuth PKCE flows and project discovery.

## Depends on

ri. External: reqwest, tokio, serde, futures, bytes, sha2, rand, base64, chrono, uuid, async-stream.
