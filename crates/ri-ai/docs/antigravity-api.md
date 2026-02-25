# Antigravity API Reference

Internal reference for ri's Antigravity provider (`google-antigravity`). Documents the
sandbox Cloud Code Assist API surface used to access frontier Gemini models.

See also: `gemini-cli-api.md` for the production variant (Gemini 2.5 models). Both
providers share the same wire protocol but differ in endpoint, models, thinking
control, and a few request fields.

Last updated: 2026-02-25.

## Endpoint

The Antigravity sandbox endpoint URL is defined in `gemini_auth.rs`. It shares the
same API surface as the production endpoint (documented in `gemini-cli-api.md`) but
serves frontier models (Gemini 3.x) that are not available on production.

For ri's Antigravity-specific model IDs, the endpoints serve **disjoint model sets**
(probed 2026-02-23):
- 2.5 models on Antigravity: **503** "No capacity available for model"
- Antigravity model IDs on production: **403** Permission Denied or **404**

**Note (2026-02-25):** Google's gemini-cli v0.30.0 uses a *different set of Gemini 3
model IDs* (e.g. `gemini-3-pro-preview`, `gemini-3-flash-preview`) that are available
on the production endpoint and/or the public Gemini API. These `-preview` IDs are
distinct from the Antigravity IDs (`gemini-3.1-pro-high`, `gemini-3-flash`). See
`gemini-cli-api.md` "Gemini 3 on Production" section for details.

## Authentication

OAuth2 via Google accounts with PKCE. OAuth app credentials, scopes, and redirect
URIs are in `gemini_auth.rs`.

Standard Google OAuth2 flow:
1. Build authorization URL with PKCE challenge
2. User authorizes in browser, callback delivers code
3. Exchange code for `access_token` + `refresh_token`

The access token is short-lived (~1 hour). Refresh using the standard Google
`refresh_token` grant.

### Credential Storage

ri stores credentials at `~/.ri/gemini_antigravity_auth.json`:

```json
{
  "access_token": "ya29.a0...",
  "refresh_token": "1//04...",
  "expires": 1771886493639,
  "project_id": "<project_id>",
  "email": "user@gmail.com"
}
```

## Project Discovery

Before making model requests, you need a project ID. This is obtained during login
via the `loadCodeAssist` endpoint.

```
POST <endpoint>/v1internal:loadCodeAssist
Authorization: Bearer <token>
Content-Type: application/json

{
  "metadata": {
    "ideType": "IDE_UNSPECIFIED",
    "platform": "PLATFORM_UNSPECIFIED",
    "pluginType": "GEMINI"
  }
}
```

Response includes `cloudaicompanionProject` -- the project ID used in all subsequent
requests. If this endpoint fails, ri falls back to the production endpoint, then to
the default project ID defined in `gemini_auth.rs`.

## Streaming Request

```
POST <endpoint>/v1internal:streamGenerateContent?alt=sse
```

### Required Headers

```
Authorization: Bearer <oauth_access_token>
Content-Type: application/json
User-Agent: antigravity/<version> darwin/arm64
```

### User-Agent Version Gating

The server checks the version number in the User-Agent header and gates access to
newer models. This is a server-side proxy check -- requests with a too-old version
receive a 200 OK with a canned rejection message from the proxy, not a proper error.

Known version requirements (as of 2026-02-23):
- `>= 1.18.0` -- required for Gemini 3.1 Pro models
- `< 1.18.0` -- Gemini 3.1 Pro returns: "not available on this version"
- Any version -- Gemini 3 Flash works

When debugging model access issues, the version gate is the first thing to check.
The symptom is a 200 response containing a short text rejection instead of actual
model output.

### Optional Headers

These are sent by ri but haven't been confirmed as strictly required:

```
X-Goog-Api-Client: gl-node/22.17.0
Client-Metadata: {"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}
```

### Request Body

The body is a JSON object with a specific nesting structure. The actual Gemini API
payload lives inside `request`, wrapped by routing metadata.

```json
{
  "project": "<project_id>",
  "model": "gemini-3.1-pro-high",
  "request": {
    "contents": [
      { "role": "user", "parts": [{ "text": "Hello" }] }
    ],
    "systemInstruction": {
      "parts": [{ "text": "You are a helpful assistant." }]
    },
    "generationConfig": {
      "maxOutputTokens": 21845,
      "thinkingConfig": {
        "includeThoughts": true,
        "thinkingLevel": "HIGH"
      }
    },
    "tools": [
      {
        "functionDeclarations": [
          {
            "name": "read",
            "description": "Read a file",
            "parameters": {
              "type": "object",
              "properties": { "path": { "type": "string" } },
              "required": ["path"]
            }
          }
        ]
      }
    ]
  },
  "userAgent": "antigravity",
  "requestType": "agent",
  "requestId": "agent-<epoch_ms>-<random>"
}
```

### Top-Level Fields

| Field | Required | Value | Description |
|---|---|---|---|
| `project` | Yes | string | GCP project ID from `loadCodeAssist` |
| `model` | Yes | string | Model ID (see Available Models below) |
| `request` | Yes | object | The Gemini API request payload |
| `userAgent` | Yes | `"antigravity"` | Must be this literal string for the sandbox endpoint |
| `requestType` | Yes | `"agent"` | Must be `"agent"` for Antigravity |
| `requestId` | No | string | Unique trace ID, format: `agent-<epoch_ms>-<random>` |

### `request.contents` -- Message History

Standard Gemini message format. Roles are `user` and `model`.

User text message:
```json
{ "role": "user", "parts": [{ "text": "What is 2+2?" }] }
```

Model text response (when echoing back in history):
```json
{ "role": "model", "parts": [{ "text": "Four." }] }
```

Model thinking (echoed back with signature for verification):
```json
{
  "role": "model",
  "parts": [
    { "thought": true, "text": "I need to calculate...", "thoughtSignature": "<base64>" },
    { "text": "Four." }
  ]
}
```

Model tool call (echoed back in history):
```json
{
  "role": "model",
  "parts": [
    { "functionCall": { "name": "read", "args": { "path": "./file.txt" } } }
  ]
}
```

Tool result (sent as a user message):
```json
{
  "role": "user",
  "parts": [
    {
      "functionResponse": {
        "name": "read",
        "response": { "output": "file contents here" }
      }
    }
  ]
}
```

Tool error:
```json
{
  "role": "user",
  "parts": [
    {
      "functionResponse": {
        "name": "read",
        "response": { "error": "file not found" }
      }
    }
  ]
}
```

### `request.generationConfig.thinkingConfig` -- Thinking Control

All Antigravity models use named thinking levels:

```json
{ "includeThoughts": true, "thinkingLevel": "HIGH" }
```

**Omitting thinkingConfig entirely:** The model still thinks internally (you'll see
`thoughtsTokenCount` in usage) but thought text is not included in the response.
Only the `thoughtSignature` is returned.

### `request.tools` -- Tool Declarations

```json
{
  "tools": [{
    "functionDeclarations": [
      {
        "name": "bash",
        "description": "Execute a shell command",
        "parameters": {
          "type": "object",
          "properties": {
            "command": { "type": "string", "description": "The command to run" }
          },
          "required": ["command"]
        }
      }
    ]
  }]
}
```

Tool declarations use JSON Schema for `parameters`. All declarations go inside a
single `functionDeclarations` array inside a single `tools` array element.

## Streaming Response Format (SSE)

The response is a stream of Server-Sent Events. Each `data:` line contains a complete
JSON object. A typical model turn produces 2-3 chunks.

### Chunk Sequence

**1. Thinking chunk** (if `includeThoughts: true` and the model thinks):

```json
{
  "response": {
    "candidates": [{
      "content": {
        "role": "model",
        "parts": [{ "thought": true, "text": "I need to calculate..." }]
      }
    }],
    "usageMetadata": { "promptTokenCount": 13, "totalTokenCount": 13 },
    "modelVersion": "gemini-3.1-pro-high",
    "responseId": "<id>"
  },
  "traceId": "<trace>",
  "metadata": {}
}
```

**2. Content chunk** (text or tool call, with thought signature):

Text response:
```json
{
  "response": {
    "candidates": [{
      "content": {
        "role": "model",
        "parts": [{ "text": "Four." }]
      }
    }],
    "usageMetadata": {
      "promptTokenCount": 13,
      "candidatesTokenCount": 2,
      "totalTokenCount": 141,
      "thoughtsTokenCount": 126
    },
    "modelVersion": "gemini-3.1-pro-high",
    "responseId": "<id>"
  }
}
```

Tool call:
```json
{
  "response": {
    "candidates": [{
      "content": {
        "role": "model",
        "parts": [{
          "thoughtSignature": "<base64>",
          "functionCall": { "name": "read", "args": { "path": "./test.txt" } }
        }]
      }
    }],
    "usageMetadata": { ... }
  }
}
```

**3. Finish chunk** (with `finishReason`):

```json
{
  "response": {
    "candidates": [{
      "content": { "role": "model", "parts": [{ "text": "" }] },
      "finishReason": "STOP"
    }],
    "usageMetadata": {
      "promptTokenCount": 13,
      "candidatesTokenCount": 2,
      "totalTokenCount": 141,
      "thoughtsTokenCount": 126
    }
  }
}
```

### Response Field Reference

**`response.candidates[0].content.parts[]`:**

| Field | Type | Description |
|---|---|---|
| `text` | string | Text content from the model |
| `thought` | bool | If `true`, this part is internal thinking (not shown to user) |
| `thoughtSignature` | string | Opaque base64 blob. Must be preserved and echoed back in subsequent turns for thought verification |
| `functionCall` | object | Tool invocation: `{ "name": string, "args": object }` |
| `functionCall.id` | string | Optional tool call ID (not always present; ri generates synthetic IDs when missing) |

**`response.candidates[0].finishReason`:**

| Value | Meaning |
|---|---|
| `STOP` | Normal completion |
| `MAX_TOKENS` | Hit output token limit |
| Other values | Various error/safety stops |

**`response.usageMetadata`:**

| Field | Description |
|---|---|
| `promptTokenCount` | Input tokens (includes cached) |
| `candidatesTokenCount` | Output tokens (excluding thinking) |
| `thoughtsTokenCount` | Thinking tokens consumed |
| `cachedContentTokenCount` | Tokens served from cache |
| `totalTokenCount` | Sum of all token types |

**Top-level response fields:**

| Field | Description |
|---|---|
| `traceId` | Server-side trace identifier |
| `metadata` | Currently empty object |

## Available Models

Probed 2026-02-23. Model availability changes without notice.

### Working

| Model ID | thinkingLevel support | Notes |
|---|---|---|
| `gemini-3.1-pro-high` | LOW, MEDIUM, HIGH | Requires UA >= 1.18.0 |
| `gemini-3.1-pro-low` | LOW, MEDIUM, HIGH | Requires UA >= 1.18.0. Lower latency variant |
| `gemini-3-flash` | MINIMAL, LOW, MEDIUM, HIGH | Works with any UA version |

### Deprecated

| Model ID | Behavior |
|---|---|
| `gemini-3-pro` | 404 |
| `gemini-3-pro-high` | 200 + "Gemini 3 Pro is no longer available" |
| `gemini-3-pro-low` | 200 + "Gemini 3 Pro is no longer available" |

### Does Not Exist (on Antigravity endpoint)

| Model ID | Status |
|---|---|
| `gemini-3.1-pro` (no suffix) | 404 |
| `gemini-3.1-flash` | 404 |
| `gemini-3.1-pro-preview` | 404 |

**Note**: `gemini-3-pro-preview` and `gemini-3-flash-preview` (the gemini-cli model
IDs) were NOT probed on the Antigravity endpoint. They likely return 404 here since
Antigravity uses its own model ID scheme (`-high`/`-low` suffixes).

### thinkingLevel Support Matrix

| Level | 3.1-pro-high | 3.1-pro-low | 3-flash |
|---|---|---|---|
| `MINIMAL` | 400 error | 400 error | OK |
| `LOW` | OK | OK | OK |
| `MEDIUM` | OK | OK | OK |
| `HIGH` | OK | OK | OK |

## ri Thinking Level Mapping

ri exposes five thinking levels. All Antigravity models use `thinkingLevel` strings.

| ri level | Pro models | Flash models |
|---|---|---|
| `off` | no thinkingConfig | no thinkingConfig |
| `low` | `"LOW"` | `"LOW"` |
| `medium` | `"HIGH"` (clamped) | `"MEDIUM"` |
| `high` | `"HIGH"` | `"HIGH"` |
| `xhigh` | `"HIGH"` (capped) | `"HIGH"` (capped) |

Pro models don't support `MINIMAL`, and ri clamps `medium` up to `HIGH` for Pro
because the quality difference between LOW and HIGH is more meaningful than a
non-existent MEDIUM tier.

## Debugging

### Model returns "not available on this version"
The User-Agent version is too old. Bump the version string in `gemini.rs`.
The server parses the semver from `antigravity/<version> darwin/arm64`.

### 404 "Requested entity was not found"
The model ID doesn't exist. Check the Available Models table.
Model IDs are case-sensitive and must include the `-high`/`-low` suffix for Pro.

### 400 "Thinking level X is not supported for this model"
That thinkingLevel value isn't valid for this model. Pro models reject `MINIMAL`.

### 200 but empty/weird response
Check if `requestType` is set to `"agent"` and `userAgent` is `"antigravity"`.
Missing these fields may route to a different backend.

### Token refresh failing
The refresh token may have been revoked. Re-run the login flow (`/login` in ri).

### Probing new models

Use this curl template to test whether a model ID exists. Replace `<endpoint>`,
`<token>`, and `<project_id>` from your credential file and `gemini_auth.rs`.

```bash
curl -s "<endpoint>/v1internal:streamGenerateContent?alt=sse" \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -H "User-Agent: antigravity/1.18.0 darwin/arm64" \
  -d '{
    "project": "<project_id>",
    "model": "<model_id_to_test>",
    "request": {
      "contents": [{"role": "user", "parts": [{"text": "Say hi"}]}],
      "generationConfig": {"maxOutputTokens": 20}
    },
    "userAgent": "antigravity",
    "requestType": "agent"
  }'
```

Reading the response:
- 200 with model output -- model exists and works
- 200 with "not available on this version" -- model exists, bump UA version
- 200 with "no longer available" -- model is deprecated
- 404 -- model ID doesn't exist on this endpoint

If a new model returns "not available", try incrementing the UA version in steps
(e.g. 1.19.0, 1.20.0, 2.0.0) to find the minimum required version.
