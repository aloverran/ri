# Fix Stack: Thinking Budget vs Max Tokens Conflict

## Changes ordered by stack depth

### 1. Make the issue obvious: Request body validation (observability)

**What:** Add a validation check after `build_request_body` that detects and reports constraint violations before sending the request.

**Where:** `crates/ri-ai/src/anthropic.rs`, in the `stream()` method, after `build_request_body()` returns.

**Why:** Currently, the invalid request is sent to the API, which returns a generic error. A local check would produce a precise error like: `"thinking budget_tokens (4096) must be less than max_tokens (4096) for model claude-3-7-sonnet-20250219"`. This makes the issue self-diagnosing.

**What it looks like:**
```rust
// After build_request_body
if let (Some(max), Some(budget)) = (
    body["max_tokens"].as_u64(),
    body["thinking"]["budget_tokens"].as_u64(),
) {
    if max <= budget {
        return Err(ProviderError::Other(format!(
            "thinking budget_tokens ({budget}) must be less than max_tokens ({max}). \
             Reduce thinking level or increase model max_tokens."
        )));
    }
}
```

### 2. Fix the bug: Coordinate max_tokens with budget_tokens

**What:** When thinking is enabled with a budget, ensure `max_tokens > budget_tokens`.

**Where:** `crates/ri-ai/src/anthropic.rs`, in `build_request_body()`.

**Approach:** Compute the thinking budget first, then set max_tokens to be at least `budget + min_answer_headroom`. Cap at `model.max_tokens`.

**Concrete logic:**
```
1. Determine thinking budget based on level (1024/4096/16384/32768)
2. Compute base max_tokens = max(model.max_tokens / 3, 4096)
3. If thinking enabled and budget >= max_tokens:
     max_tokens = min(budget + 1024, model.max_tokens)
4. If max_tokens still <= budget:
     Clamp budget to max_tokens - 1
```

The `+ 1024` headroom ensures the model has meaningful output capacity beyond thinking. Capping at `model.max_tokens` prevents exceeding the model's limit. The final clamp is a safety net.

### 3. Model registry: custom models override hardcoded ones

**What:** When a model id in `models.json` matches a hardcoded model, the custom definition should take precedence.

**Where:** `ri-cli/src/main.rs` -- register custom models BEFORE hardcoded defaults, or change `ModelRegistry::find()` to return the last match.

**Why:** This is how the reproduction was initially blocked -- the custom model config was silently ignored. This is a separate bug that compounds the thinking budget issue.

### 4. (Optional) Thinking level CLI flag

**What:** Add a `--thinking` CLI flag to set the thinking level.

**Where:** `ri-cli/src/main.rs` CLI struct + agent config.

**Why:** The thinking level is hardcoded to Medium. Without a CLI flag, users can't test other levels. This isn't strictly needed for the fix but prevents the same class of bug from being re-discovered when thinking levels become configurable.

## Recommended implementation order

1. **Fix #2** (coordinate max_tokens with budget) -- this is the actual bug fix
2. **Fix #1** (validation) -- makes future similar issues self-diagnosing
3. **Fix #3** (model registry override) -- prevents config confusion
4. **Fix #4** (CLI flag) -- nice to have, low priority

## What NOT to change

- Don't add model-specific branching based on which models support interleaved thinking. The code should produce valid requests for ALL models, not rely on beta behaviors.
- Don't change the adaptive thinking path (Opus 4.6) -- it doesn't use budget_tokens and doesn't have this issue.
- Don't add temperature=1.0 enforcement -- that's a separate constraint for thinking-enabled requests, and the API error for it is already clear.
