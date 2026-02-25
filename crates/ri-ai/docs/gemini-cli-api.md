# Gemini CLI API Reference

Internal reference for ri's Gemini CLI provider (`google-gemini-cli`). Documents the
production Cloud Code Assist API surface used to access standard Gemini models.

See also: `antigravity-api.md` for the sandbox/frontier variant. Both providers share
the same wire protocol (SSE streaming, request envelope) but differ in endpoint,
models, thinking control, and response metadata.

Probed live on 2026-02-23. Updated 2026-02-25 with findings from gemini-cli v0.30.0
source analysis.

## Relationship to Antigravity

Both providers hit the Cloud Code Assist API. The Cli variant uses the **production**
endpoint with standard Gemini models (2.x). Antigravity uses a **sandbox** endpoint
with frontier models (3.x). In the code, both are the same `GeminiProvider` struct
parameterized by `GeminiVariant`.

For ri's Antigravity model IDs, the endpoints serve **disjoint model sets**
(probed 2026-02-23):
- 2.5 Pro/Flash on Antigravity: **503** "No capacity available"
- 3.x Antigravity models on production: **403** Permission Denied or **404**

**However**, Google's gemini-cli v0.30.0 uses a *different set of Gemini 3 model IDs*
(with `-preview` suffixes) that are available on the production endpoint and/or the
public Gemini API. See "Gemini 3 on Production -- the Preview Model IDs" below.

| Aspect | Cli (this doc) | Antigravity |
|---|---|---|
| Endpoint | production | sandbox |
| Models (ri) | 2.5 Pro, 2.5 Flash, 2.0 Flash | 3.1 Pro, 3 Flash |
| Models (gemini-cli) | 2.5 Pro/Flash/Flash-Lite + 3.x `-preview` | (not used) |
| Thinking control | `thinkingBudget` (2.5) / `thinkingLevel` (3.x) | `thinkingLevel` (named level) |
| `requestType` field | optional (works with or without) | required (`"agent"`) |
| `userAgent` field | `"ri-coding-agent"` | `"antigravity"` |
| HTTP User-Agent | no version gating | version-gated |
| System prompt | passthrough | injected bridge prompt |
| Tool call history | native `functionCall` | text-converted |
| Response extras | `trafficType`, `createTime`, modality details | none of these |
| Rate limits | aggressive (~31s cooldown, model-specific) | more lenient |
| OAuth scopes | 3 | 5 |

## Authentication

Same PKCE OAuth2 flow as Antigravity -- different app credentials. All constants are
in `gemini_auth.rs`.

| Parameter | Value |
|---|---|
| Credential file | `~/.ri/gemini_cli_auth.json` |
| Redirect URI | `http://localhost:8085/oauth2callback` |
| Local server port | 8085 |
| Scopes | `cloud-platform`, `userinfo.email`, `userinfo.profile` |

Antigravity requires two additional scopes (`cclog`, `experimentsandconfigs`). The Cli
variant does not.

### Credential Storage

Same format as Antigravity:

```json
{
  "access_token": "ya29.a0...",
  "refresh_token": "1//04...",
  "expires": 1771886493639,
  "project_id": "<project_id>",
  "email": "user@gmail.com"
}
```

Token lifetime is ~1 hour with a 5-minute pre-expiry refresh buffer.

## Project Discovery

On login, ri discovers the GCP project via `loadCodeAssist` on the production endpoint
only (Antigravity tries both endpoints). The response includes tier information:

```json
{
  "currentTier": {
    "id": "standard-tier",
    "name": "Gemini Code Assist",
    "description": "Unlimited coding assistant with the most powerful Gemini models",
    "userDefinedCloudaicompanionProject": true
  },
  "cloudaicompanionProject": "<project_id>"
}
```

If `loadCodeAssist` fails, the Cli variant has a fallback that Antigravity does not:
it calls `onboardUser` with `tierId: "free-tier"` to provision a new project.

```
POST <endpoint>/v1internal:onboardUser
Authorization: Bearer <token>
Content-Type: application/json

{
  "tierId": "free-tier",
  "metadata": {
    "ideType": "IDE_UNSPECIFIED",
    "platform": "PLATFORM_UNSPECIFIED",
    "pluginType": "GEMINI"
  }
}
```

## Streaming Request

```
POST <endpoint>/v1internal:streamGenerateContent?alt=sse
```

### Headers

```
Authorization: Bearer <oauth_access_token>
Content-Type: application/json
User-Agent: google-cloud-sdk vscode_cloudshelleditor/0.1
```

Unlike Antigravity, there is **no version gating** on the User-Agent. The string
mimics the Gemini CLI tool's own user-agent.

### Request Body

Same envelope as Antigravity (see `antigravity-api.md` for full schema) with these
differences:

```json
{
  "project": "<project_id>",
  "model": "gemini-2.5-pro",
  "request": { ... },
  "userAgent": "ri-coding-agent",
  "requestId": "ri-<epoch_ms>-<random>"
}
```

- **`userAgent`**: `"ri-coding-agent"` (Antigravity uses `"antigravity"`)
- **`requestType`**: optional on production (Antigravity requires `"agent"`)
- **`requestId`** prefix: `"ri-"` (Antigravity uses `"agent-"`)

The `request` object inside is standard Gemini API: `contents`, `systemInstruction`,
`generationConfig`, `tools`. See the Antigravity doc for the full content format
(roles, tool calls, tool results, images) -- it is identical.

### System Prompt

The Cli variant passes the system prompt through directly. Antigravity prepends a
hardcoded identity prompt and a bridge prompt to override it. This is not needed for
Cli models since they do not have a baked-in agent persona.

## Thinking Control

This is the main behavioral difference between the two providers.

### Gemini 2.5 models use `thinkingBudget` (token count)

```json
{
  "generationConfig": {
    "maxOutputTokens": 21845,
    "thinkingConfig": {
      "includeThoughts": true,
      "thinkingBudget": 16384
    }
  }
}
```

**Confirmed constraints (probed 2026-02-23):**
- `thinkingBudget` range: **128 to 32768** (inclusive)
- `thinkingBudget: 0` rejected: "The model does not support setting thinking_budget to 0"
- `thinkingBudget: 1` rejected: "thinking_budget is out of range; supported values are integers from 128 to 32768"
- `thinkingLevel` (named levels) rejected: "thinking_level is not supported by this model"
- Both 2.5 Pro and 2.5 Flash reject `thinkingLevel` identically

### Gemini 2.0 Flash does not support thinking

Any `thinkingConfig` on 2.0-flash returns 400: "thinking is not supported by this
model."

### ri Thinking Level Mapping

| ri level | `thinkingBudget` |
|---|---|
| `off` | no thinkingConfig |
| `low` | 2048 |
| `medium` | 8192 |
| `high` | 16384 |
| `xhigh` | 32768 |

Contrast with Antigravity's Gemini 3 models, which use named `thinkingLevel` strings
(`"LOW"`, `"MEDIUM"`, `"HIGH"`).

### Thinking behavior without thinkingConfig

When `thinkingConfig` is omitted entirely, 2.5 models still think internally.
`thoughtsTokenCount` will be nonzero in usage metadata, but no thought text or
signature is returned. Same behavior as Antigravity.

### `includeThoughts: false` with budget

Model thinks (budget is honored, `thoughtsTokenCount` reported) but thought text is
hidden from response. No `thoughtSignature` returned either.

### Thinking response format

When `includeThoughts: true`:
- Thinking chunks arrive as separate SSE events with `"thought": true` on the part
- `thoughtSignature` appears on the final content/tool-call part (not on thought parts)
- For tool calls, `thoughtSignature` is on the same part as `functionCall`
- `thoughtsTokenCount` in `usageMetadata` reports thinking tokens consumed

## Tool Call History

Gemini 2.5 models handle tool calls in message history natively. Prior tool calls are
echoed back as standard `functionCall` parts:

```json
{
  "role": "model",
  "parts": [
    { "functionCall": { "name": "read", "args": { "path": "./file.txt" } } }
  ]
}
```

Antigravity's Gemini 3 models have a bug/limitation where echoing `functionCall` parts
in history causes issues, so ri converts them to descriptive text. This workaround is
not needed for Cli models.

### Tool call response structure

Tool calls carry `thoughtSignature` on the `functionCall` part:

```json
{
  "thoughtSignature": "<base64>",
  "functionCall": { "name": "read_file", "args": { "path": "./test.txt" } }
}
```

There is no separate `functionCall.id` -- ri generates synthetic IDs.

## Available Models

### ri Models (Probed 2026-02-23)

Only exact model IDs work for ri's Cli variant.

#### Working

| Model ID | Thinking | trafficType | Notes |
|---|---|---|---|
| `gemini-2.5-pro` | thinkingBudget 128-32768 | PROVISIONED_THROUGHPUT | Primary model |
| `gemini-2.5-flash` | thinkingBudget 128-32768 | PROVISIONED_THROUGHPUT | Faster, cheaper |
| `gemini-2.0-flash` | none | ON_DEMAND | Non-thinking, aggressive rate limits |

#### Not found (404)

`gemini-2.5-pro-preview-06-05`, `gemini-2.5-flash-preview-05-20`, `gemini-2.5-pro-exp`,
`gemini-2.5-flash-exp`, `gemini-2.5-pro-latest`, `gemini-exp-1206`, `gemini-pro`,
`gemini-1.5-pro`, `gemini-1.5-flash`, `gemini-2.0-pro`, `gemini-3-flash`,
`gemini-3-pro`, `gemini-3-pro-high`, `gemini-3-pro-low`.

**Note**: `gemini-3-pro-preview` and `gemini-3-flash-preview` were NOT probed on this
endpoint. These are the model IDs the gemini-cli app uses (see below).

#### Permission denied (403)

`gemini-3.1-pro-high`, `gemini-3.1-pro-low` -- these exist on the server but require
Antigravity-level access. They are not available via the Cli auth.

#### ri Registry vs Reality

Our registry currently has `gemini-2.5-pro` and `gemini-2.5-flash`. Missing:
`gemini-2.0-flash` (works but non-thinking, very aggressive rate limits, may not be
worth adding).

## Gemini 3 on Production -- the Preview Model IDs

Updated 2026-02-25 from analysis of `@google/gemini-cli` v0.30.0 source.

Google's gemini-cli app supports Gemini 3 models using a **different set of model IDs**
from the Antigravity sandbox. These are defined in `gemini-cli-core/config/models.js`:

| gemini-cli constant | Model ID | Notes |
|---|---|---|
| `PREVIEW_GEMINI_MODEL` | `gemini-3-pro-preview` | Default Gemini 3 Pro |
| `PREVIEW_GEMINI_3_1_MODEL` | `gemini-3.1-pro-preview` | Used when 3.1 experiment enabled |
| `PREVIEW_GEMINI_3_1_CUSTOM_TOOLS_MODEL` | `gemini-3.1-pro-preview-customtools` | API key auth variant |
| `PREVIEW_GEMINI_FLASH_MODEL` | `gemini-3-flash-preview` | Gemini 3 Flash |

These `-preview` IDs are completely distinct from the Antigravity IDs ri uses
(`gemini-3.1-pro-high`, `gemini-3.1-pro-low`, `gemini-3-flash`). They were not probed
in the 2026-02-23 analysis.

### How Gemini CLI Routes Gemini 3

The gemini-cli has two backend paths depending on auth type:

| Auth type | Backend class | Endpoint | Gemini 3 access |
|---|---|---|---|
| Login with Google (OAuth) | `CodeAssistServer` | `cloudcode-pa.googleapis.com` | Gated by quota buckets |
| Gemini API Key | `GoogleGenAI` (`@google/genai`) | `generativelanguage.googleapis.com` | Always enabled |
| Vertex AI | `GoogleGenAI` (`@google/genai`) | `aiplatform.googleapis.com` | Always enabled |

For **API key / Vertex AI users**, requests go through the **public Gemini API** (not
Cloud Code Assist), where these preview models are available. The
`hasAccessToPreviewModel` flag is unconditionally set `true` at `config.js:631`.

For **OAuth users**, access depends on `retrieveUserQuota` returning buckets with
preview model IDs. This is a server-side rollout by Google -- the code at
`config.js:919-921` checks `quota.buckets.some(b => isPreviewModel(b.modelId))`.

### TUI Model Selection

The ModelDialog (`ModelDialog.js`) conditionally shows Gemini 3 options based on
`config.getHasAccessToPreviewModel()`:
- If true: shows "Auto (Gemini 3)" and the preview models in Manual mode
- If false: shows only "Auto (Gemini 2.5)" and 2.5 models

An experiment flag `GEMINI_3_1_PRO_LAUNCHED` (flag ID 45760185) controls whether
the TUI resolves `gemini-3-pro-preview` vs `gemini-3.1-pro-preview`.

### Implications for ri

ri currently accesses Gemini 3 via the Antigravity sandbox with separate credentials
and model IDs. If `gemini-3-pro-preview` / `gemini-3-flash-preview` work on the
production Cloud Code Assist endpoint, ri could unify both providers into one, using
production credentials for everything. This would eliminate the need for separate
Antigravity auth, the bridge prompt hack, and the UA version gating workaround.

Probing `gemini-3-pro-preview` and `gemini-3-flash-preview` on the production endpoint
with Cli auth would confirm this.

## Streaming Response Format

Same SSE structure as Antigravity (see `antigravity-api.md`) with additional metadata
fields in the production response:

### Extra fields vs Antigravity

```json
{
  "response": {
    "usageMetadata": {
      "trafficType": "PROVISIONED_THROUGHPUT",
      "promptTokensDetails": [{ "modality": "TEXT", "tokenCount": 10 }],
      "candidatesTokensDetails": [{ "modality": "TEXT", "tokenCount": 1 }]
    },
    "createTime": "2026-02-23T22:31:39.632448Z"
  },
  "metadata": {
    "remoteContext": { "ragState": "RAG_DISABLED" }
  }
}
```

| Field | Description |
|---|---|
| `trafficType` | `"PROVISIONED_THROUGHPUT"` (2.5 models) or `"ON_DEMAND"` (2.0-flash) |
| `createTime` | ISO timestamp of when the response was created |
| `promptTokensDetails` | Token count broken down by modality (TEXT, IMAGE, etc.) |
| `candidatesTokensDetails` | Same for output tokens |
| `metadata.remoteContext.ragState` | Always `"RAG_DISABLED"` in our usage |

These fields are not present in Antigravity responses. ri's SSE parser ignores them.

## Rate Limits

The production endpoint has **aggressive per-model rate limits**:
- Cooldown period: ~31 seconds after exhaustion
- Model-specific: hitting 2.5-pro limit does not affect 2.5-flash
- 2.0-flash appears to have an even more restrictive quota
- Error format: top-level JSON (not SSE), code 429 `RESOURCE_EXHAUSTED`

```json
{
  "error": {
    "code": 429,
    "message": "You have exhausted your capacity on this model. Your quota will reset after 31s.",
    "status": "RESOURCE_EXHAUSTED"
  }
}
```

Note: rate limit errors come as **top-level JSON**, not SSE `data:` lines. Our parser
needs to handle this (it currently does via the non-SSE error path in `interpret_sse`).

## Debugging

### "Unknown model" / provider not resolving
The Cli provider requires a credential file at `~/.ri/gemini_cli_auth.json`. If it
does not exist, `is_authenticated()` returns false and the provider's models will not
resolve. Run `/login` to authenticate.

### Token refresh failing
Same as Antigravity -- re-run `/login`.

### 429 rate limits
Wait for the cooldown period indicated in the error message. These are per-model.

### Project discovery failing
If `loadCodeAssist` and `onboardUser` both fail, set `GOOGLE_CLOUD_PROJECT` or
`GOOGLE_CLOUD_PROJECT_ID` environment variable manually.

### 403 on model access
The model exists but requires different auth tier. Antigravity-specific model IDs
(e.g. `gemini-3.1-pro-high`) need Antigravity credentials. The `-preview` model IDs
(e.g. `gemini-3-pro-preview`) may work with Cli auth -- needs probing.
