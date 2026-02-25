# OpenAI Codex API Reference

Internal reference for ri's OpenAI Codex provider (`openai-codex`).

This documents the reverse-engineered API surface used by `ri-ai/src/openai_codex.rs`,
plus probing results for prompt caching and model discovery behavior.

Last updated: 2026-02-25.

## Endpoint

Primary streaming endpoint:

```
POST https://chatgpt.com/backend-api/codex/responses
```

Notes from probing:
- `stream` must be `true` in the request body.
- `store` must be `false` in the request body.
- Non-conforming requests are rejected with HTTP 400.

Observed 400 responses:
- `{"detail":"Stream must be set to true"}`
- `{"detail":"Store must be set to false"}`

## Authentication

ri uses OAuth2 Authorization Code + PKCE against OpenAI auth endpoints.

Auth constants in `openai_codex.rs`:
- Authorization URL: `https://auth.openai.com/oauth/authorize`
- Token URL: `https://auth.openai.com/oauth/token`
- Client ID: `app_EMoamEEZ73f0CkXaXp7hrann`
- Redirect URI: `http://localhost:1455/auth/callback`
- Scope: `openid profile email offline_access`

Authorization request includes:
- `response_type=code`
- `code_challenge` + `code_challenge_method=S256`
- `state=<random>`
- `id_token_add_organizations=true`
- `codex_cli_simplified_flow=true`
- `originator=ri`

### Token Storage

Credentials are stored at:

```
~/.ri/openai_codex_auth.json
```

Format:

```json
{
  "access_token": "<jwt>",
  "refresh_token": "<token>",
  "expires": 1771999999000,
  "project_id": null,
  "email": null
}
```

`expires` is epoch-ms with a 5-minute early-refresh buffer applied.

### Account ID Header (JWT claim)

The API requires a `chatgpt-account-id` header. ri derives this from the access
JWT claims object at key `"https://api.openai.com/auth"`, then reads
`"chatgpt_account_id"` from that object.

Implemented extraction:

```json
payload["https://api.openai.com/auth"]["chatgpt_account_id"]
```

## Model Discovery

Model listing endpoint:

```
GET https://chatgpt.com/backend-api/codex/models?client_version=<x.y.z>
```

Important behaviors:
- `client_version` query param is required.
- Without it, server returns 400 with a missing query-field error.

Probed behavior (2026-02-25):
- `client_version=0.77.0`: `gpt-5.3-codex` not listed.
- `client_version=0.98.0`: `gpt-5.3-codex` listed.
- `client_version=0.120.0`: `gpt-5.3-codex` listed.

This matches open-source Codex metadata that marks `gpt-5.3-codex` with
`minimal_client_version: 0.98.0`.

### Practical implication for ri

ri can call `gpt-5.3-codex` directly on `/codex/responses` even if an old
`/codex/models` query would hide it. Discovery and execution are not perfectly
aligned when emulating older client versions.

## Models Exposed by ri

`openai_codex.rs` currently registers these models:
- `gpt-5.2`
- `gpt-5.2-codex`
- `gpt-5.3-codex`

Shared model metadata in ri:
- `reasoning = true`
- `context_window = 272000`
- `max_tokens = 128000`
- cost (`USD / 1M tokens`): input `1.75`, output `14.0`, cache-read `0.175`,
  cache-write `0.0`

## Streaming Request

### Required Headers

```
Authorization: Bearer <access_token>
chatgpt-account-id: <account_id_from_jwt>
OpenAI-Beta: responses=experimental
Content-Type: application/json
Accept: text/event-stream
```

### Important Optional Header

```
session_id: <stable_conversation_id>
```

`session_id` is critical for consistent prompt caching behavior (details below).

### Typical Request Body

```json
{
  "model": "gpt-5.3-codex",
  "store": false,
  "stream": true,
  "instructions": "<system prompt>",
  "input": [
    { "role": "user", "content": [ { "type": "input_text", "text": "Hello" } ] }
  ],
  "text": { "verbosity": "medium" },
  "include": ["reasoning.encrypted_content"],
  "tool_choice": "auto",
  "parallel_tool_calls": true,
  "tools": [
    {
      "type": "function",
      "name": "read",
      "description": "Read a file",
      "parameters": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] },
      "strict": null
    }
  ],
  "reasoning": {
    "effort": "medium",
    "summary": "auto"
  },
  "prompt_cache_key": "<stable_key>"
}
```

### Request Field Notes

- System prompt is sent via top-level `instructions`, not as a `role:"system"` input item.
- `include: ["reasoning.encrypted_content"]` enables encrypted reasoning payload replay.
- `reasoning` can be omitted; ri sends it when thinking is enabled.
- `text.verbosity` is accepted by this endpoint (`low|medium|high`).
- `prompt_cache_key` is accepted but not sufficient alone for reliable caching on
  newer codex models (see cache section).
- `max_tokens` from ri `RequestOptions` is currently not forwarded in this provider.

## Thinking Level Mapping (ri -> Codex)

When `thinking != Off`, ri sends:

```json
"reasoning": { "effort": "<mapped>", "summary": "auto" }
```

Mapping:
- `ThinkingLevel::Low` -> `effort: "low"`
- `ThinkingLevel::Medium` -> `effort: "medium"`
- `ThinkingLevel::High` -> `effort: "high"`
- `ThinkingLevel::XHigh` -> `effort: "xhigh"`
- `ThinkingLevel::Off` -> omit `reasoning`

## ri Message Conversion

ri converts conversation history into Responses API `input` items.

### User role

- `ContentBlock::Text` -> `{"type":"input_text","text":...}`
- `ContentBlock::Image` -> `{"type":"input_image","image_url":"data:<mime>;base64,..."}`

### Assistant role

- `ContentBlock::Thinking { sig }`:
  - `sig` stores full encrypted reasoning item JSON.
  - On replay, ri deserializes `sig` and appends item verbatim.
- `ContentBlock::Text` -> synthetic `type:"message"` with `output_text` content.
- `ContentBlock::ToolUse` -> `type:"function_call"`.

### Tool results

`ContentBlock::ToolResult` becomes `type:"function_call_output"` using `call_id`.

## Tool Call ID Semantics

Codex uses two IDs for function calls:
- `call_id`
- item `id`

ri stores tool use IDs as compound:

```
<call_id>|<item_id>
```

This preserves both identifiers across turns.

## Streaming Response Format (SSE)

Codex Responses SSE places the event discriminator in JSON `data.type`.
The SSE `event:` line is not used for dispatch.

### Event Types handled by ri

Error path:
- `error`
- `response.failed`

Lifecycle:
- `response.output_item.added`
- `response.output_item.done`

Reasoning:
- `response.reasoning_summary_text.delta`
- `response.reasoning_summary_part.added`
- `response.reasoning_summary_part.done`

Text:
- `response.output_text.delta`
- `response.refusal.delta`
- `response.content_part.added`

Function arguments:
- `response.function_call_arguments.delta`
- `response.function_call_arguments.done`

Terminal:
- `response.done`
- `response.completed`

Unknown event types are ignored (forward-compatible behavior).

### Typical Item Sequence

For a simple text reply:
1. `response.output_item.added` (`item.type = "message"`)
2. one or more `response.output_text.delta`
3. `response.output_item.done` (message)
4. `response.completed`/`response.done` with usage

For a tool call:
1. `response.output_item.added` (`item.type = "function_call"`)
2. one or more `response.function_call_arguments.delta`
3. optional `response.function_call_arguments.done`
4. `response.output_item.done` (function_call)
5. `response.completed`/`response.done`

For reasoning summaries:
1. `response.output_item.added` (`item.type = "reasoning"`)
2. one or more `response.reasoning_summary_text.delta`
3. optional `response.reasoning_summary_part.done` separators
4. `response.output_item.done` (reasoning item, includes encrypted content)

## Usage Mapping

ri reads usage from terminal response events:

```json
"usage": {
  "input_tokens": 7489,
  "input_tokens_details": { "cached_tokens": 0 },
  "output_tokens": 883,
  "output_tokens_details": { "reasoning_tokens": 661 },
  "total_tokens": 8372
}
```

Mapped to ri `Usage`:
- `input_tokens` <- `usage.input_tokens`
- `output_tokens` <- `usage.output_tokens`
- `cache_read_tokens` <- `usage.input_tokens_details.cached_tokens`
- `cache_write_tokens` <- `0` (field not exposed by this API)
- `extras` <- full `usage` object

## Retry and Error Behavior in ri

`openai_codex.rs` retries retryable failures with exponential backoff:
- Retry statuses: `429, 500, 502, 503, 504`
- Retry attempts: up to 3 retries (4 total attempts)
- Delays: `1000ms`, `2000ms`, `4000ms`

Rate-limit detection:
- HTTP 429, or
- error code/type containing `usage_limit` or `rate_limit`

## Prompt Caching Reverse Engineering (2026-02-25)

### Problem Statement

Observed in ri:
- `gpt-5.3-codex` repeatedly reported `cached_tokens: 0`
- even when sending identical long prompts across turns.

### Hypothesis

Caching needed additional conversation identity beyond prompt text,
likely via header(s) used by official Codex clients.

### Cross-check with open-source Codex CLI

Open-source Codex code paths show both of these are sent per conversation:
- body `prompt_cache_key = conversation_id`
- header `session_id = conversation_id`

That indicated ri was missing at least one required signal.

### Probe Method

- Direct calls to `https://chatgpt.com/backend-api/codex/responses`
- Same model + same long prompt, repeated twice per case
- Cases:
  - no cache identifiers
  - `prompt_cache_key` only
  - `session_id` only
  - both
- Measured `usage.input_tokens_details.cached_tokens`

### Probe Results

Representative runs (fresh random prompt per model/case):

`gpt-5.3-codex`
- none: `[0, 0]`
- `prompt_cache_key` only: `[0, 0]`
- `session_id` only: `[0, >0]`
- both: `[0, >0]`

`gpt-5.2-codex`
- none: `[0, 0]`
- `prompt_cache_key` only: `[0, 0]`
- `session_id` only: `[0, >0]`
- both: `[0, >0]`

`gpt-5.2`
- none: `[0, 0]`
- `prompt_cache_key` only: mostly `[0, 0]` (occasional hit observed)
- `session_id` only: `[0, >0]`
- both: `[0, >0]` (or immediate hit if same session already warmed)

### Conclusion

For Codex Responses API, stable `session_id` is the primary lever for reliable
prompt cache hits. `prompt_cache_key` alone is not reliably effective.

Using both is recommended to match official client behavior.

### Additional Observations

- First request on a fresh session is typically uncached.
- Second identical request on same session usually reports large cached token counts.
- Cached token counts are coarse and often aligned in large chunks.
- Cache behavior can look non-deterministic without explicit session identity.

## ri Implementation Notes

Current ri Codex implementation should do all of the following per turn:
- Derive a stable conversation key (ri now uses earliest non-system message ID).
- Send that key as body `prompt_cache_key`.
- Send that same key as header `session_id`.

This mirrors official Codex client behavior and fixes the persistent
`cached_tokens = 0` symptom for `gpt-5.3-codex` in repeated-turn workloads.

## Debugging Checklist

If cache reads stay at zero:
1. Confirm request includes `session_id` header and it is stable across turns.
2. Confirm request includes `prompt_cache_key` (recommended, even if not sufficient alone).
3. Confirm prompt prefix is truly identical across repeated requests.
4. Confirm `stream=true` and `store=false` (server constraints).
5. Inspect terminal usage event (`response.completed`/`response.done`) and read
   `input_tokens_details.cached_tokens` from raw JSON.
6. Probe with a long prompt (short prompts may not show meaningful cache deltas).

If model discovery seems inconsistent:
1. Query `/backend-api/codex/models?client_version=<version>`.
2. Check model `minimal_client_version` behavior (notably for `gpt-5.3-codex`).
3. Validate direct `/codex/responses` execution separately from model-list visibility.

## Known Unknowns

Not yet fully characterized:
- Exact server-side cache key algorithm.
- Cache retention TTL and invalidation policy.
- Precise interplay between account-level cache and explicit session-keyed cache.
- Billing semantics for cached tokens on chatgpt.com Codex backend.
