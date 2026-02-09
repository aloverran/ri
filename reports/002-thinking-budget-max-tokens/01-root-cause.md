# Root Cause Report: Thinking Budget vs Max Tokens Conflict

## Issue
GitHub issue #14: `build_request_body` can produce API requests where `budget_tokens >= max_tokens`, violating the Anthropic API constraint that `max_tokens` must be strictly greater than `budget_tokens`.

## Reproduction

**Setup:** `~/.ri/models.json`:
```json
{
  "providers": {
    "anthropic": {
      "baseUrl": "https://api.anthropic.com",
      "api": "anthropic-messages",
      "models": [{
        "id": "claude-3-7-sonnet-20250219",
        "name": "Claude 3.7 Sonnet",
        "reasoning": true,
        "maxTokens": 8192
      }]
    }
  }
}
```

**Command:**
```sh
cargo run --release -- --mode print --model claude-3-7-sonnet-20250219 --prompt "Say hello"
```

**Result:**
```
API error (400): invalid_request_error: `max_tokens` must be greater than
`thinking.budget_tokens`.
```

**Values sent:** `max_tokens: 4096`, `budget_tokens: 4096` (equality violates strict >).

## Root Cause Chain

### Layer 1: No coordination between max_tokens and budget_tokens

`build_request_body` (anthropic.rs:155-237) computes these two values independently:

```rust
// Line ~158: max_tokens computed first
let max_tokens = options.max_tokens.unwrap_or_else(|| (model.max_tokens / 3).max(4096));

// Lines ~220-228: budget computed independently, much later
let budget = match level {
    ThinkingLevel::Low => 1024,
    ThinkingLevel::Medium => 4096,
    ThinkingLevel::High => 16384,
    ThinkingLevel::XHigh => 32768,
    ThinkingLevel::Off => unreachable!(),
};
```

Neither value knows about the other. The invariant `max_tokens > budget_tokens` is never enforced.

### Layer 2: The `/3` heuristic collapses into budget range

`max_tokens = max(model.max_tokens / 3, 4096)` produces 4096 for any model with `max_tokens <= 12288`. This means the computed max_tokens exactly equals the Medium thinking budget (4096), producing the equality violation.

**Concrete table:**

| model.max_tokens | computed max_tokens | Medium budget | Violation? |
|---|---|---|---|
| 8192 | 4096 | 4096 | Yes (==) |
| 12288 | 4096 | 4096 | Yes (==) |
| 16384 | 5461 | 4096 | No |
| 16384 | 5461 | 16384 (High) | Yes |
| 65536 | 21845 | 32768 (XHigh) | Yes |

### Layer 3: Interleaved thinking beta masks the bug for Sonnet 4 only

ri always sends the `interleaved-thinking-2025-05-14` beta header. This beta relaxes the `max_tokens > budget_tokens` constraint, but **only for Claude 4+ models** (Sonnet 4, Opus 4.6). For Claude 3.7 Sonnet, the constraint is still enforced even with the beta.

**Verified via curl:**
- `claude-sonnet-4-20250514` + interleaved beta + `budget==max_tokens`: OK
- `claude-3-7-sonnet-20250219` + interleaved beta + `budget==max_tokens`: Error

This means the bug is **latent for Sonnet 4** but **active for Claude 3.7 Sonnet** and any future non-Claude-4 reasoning model.

### Layer 4: Hardcoded thinking level and missing configuration

- `ThinkingLevel::Medium` is hardcoded in `main.rs:160`
- No CLI flag or interactive command exists to change it
- Session scaffolding exists (`ThinkingLevelChange` entry type) but is unused
- If/when thinking level becomes configurable, High/XHigh will trigger the bug for many more models

### Layer 5: Model registry collision

A secondary issue discovered during reproduction: if `models.json` defines a model with the same id as a hardcoded model (e.g., `claude-sonnet-4-20250514`), `ModelRegistry::find()` returns the hardcoded one (added first) and silently ignores the custom definition. This makes it impossible to override `reasoning: false` -> `true` for the default model via configuration.

## Why Sonnet 4 hides the bug

For the default model `claude-sonnet-4-20250514`:
- `model.reasoning = false` (hardcoded in main.rs)
- Thinking params are never added to the request body
- The bug code path is never reached

Even if reasoning were true, the interleaved thinking beta would mask the error for Sonnet 4. The bug only surfaces for non-Sonnet-4 reasoning models.

## Summary

The root cause is that `max_tokens` and `budget_tokens` are computed independently with no coordination or invariant enforcement. The `/3` heuristic for max_tokens frequently produces values in the same range as thinking budgets, creating constraint violations for any non-Claude-4 reasoning model.
