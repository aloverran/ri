# Test Harness Deficiencies Report: Thinking Budget vs Max Tokens

## How this issue was found and reproduced

1. Read the issue description on GitHub
2. Traced the code manually to understand the numeric relationships
3. Wrote a standalone Rust program to compute all value combinations
4. Created a `models.json` with a reasoning model to trigger the code path
5. Ran ri and observed the API error
6. Used curl to verify API behavior with/without beta headers

This was a 20+ minute investigation. A well-designed test harness would have caught this at build time or made the reproduction trivial.

## Deficiencies

### 1. No request body validation (the big one)

The most impactful improvement would be a validation pass on the constructed request body before sending it. `build_request_body` produces a `serde_json::Value` that goes directly to `reqwest`. There's no intermediate step that checks invariants.

A `validate_request_body(body: &Value, model: &Model)` function could check:
- `max_tokens > budget_tokens` when thinking is enabled
- `max_tokens <= model.max_tokens`
- Tool names are non-empty
- System prompt is present when expected

This is not a test -- it's runtime validation that makes invalid states produce clear errors locally instead of opaque API errors remotely.

### 2. build_request_body is not independently testable

`build_request_body` is a private function that takes `&Model`, `&[Message]`, `&CompletionOptions`, and `bool`. It's pure (no side effects) and returns `Value`. This is the ideal shape for unit testing, but it's private.

Making it `pub(crate)` (or `#[cfg(test)] pub`) would allow tests that construct a `Model` with specific `max_tokens`, set thinking levels, and assert the output JSON satisfies the API constraints. This is the lowest-effort, highest-coverage change.

### 3. No dry-run mode (from previous report, still missing)

Report 001 recommended a `--dry-run` flag. This issue would have been caught instantly with `ri --dry-run --model claude-3-7-sonnet-20250219 --prompt "test"` by inspecting the JSON output.

### 4. RUST_LOG=debug shows the body, but too late

The debug log at `anthropic.rs:596` does show the request body, which is how we confirmed the values. But:
- It logs AFTER the body is constructed and BEFORE the error response
- The body is on a single line, hard to read
- There's no structured output mode

A `RUST_LOG=ri_ai::anthropic=trace` that pretty-prints the request body would be a low-cost improvement.

### 5. No model-specific integration test harness

The current test harness doesn't exercise different model configurations. The only existing tests are for session management. There's no way to run "build a request for model X with thinking level Y and verify it's valid" without launching the full binary.

A minimal integration test would be:
```rust
#[test]
fn reasoning_model_request_has_valid_budget() {
    let model = Model { reasoning: true, max_tokens: 8192, .. };
    let options = CompletionOptions { thinking_level: Some(ThinkingLevel::Medium), .. };
    let body = build_request_body(&model, &[], &options, false);
    let max_tokens = body["max_tokens"].as_u64().unwrap();
    let budget = body["thinking"]["budget_tokens"].as_u64().unwrap();
    assert!(max_tokens > budget, "max_tokens ({max_tokens}) must be > budget ({budget})");
}
```

This is the type of test that has near-zero maintenance cost (it tests a pure function against a documented API constraint) and catches this exact class of bug.

### 6. Model registry doesn't warn about collisions

When `models.json` defines a model with the same id as a hardcoded model, `find()` silently returns the hardcoded one. This makes custom model configuration unreliable. The registry should either:
- Log a warning when a model id is registered twice
- Let custom models override hardcoded ones (last-write-wins)

## Bird's Eye (Blue Sky)

The ideal test harness for this project would have:

1. **Request body property tests**: For every model configuration, verify the API constraints are satisfied. This is a pure function test with no network access needed.

2. **A "request catalog" CLI**: `ri catalog --model X --thinking Y` that dumps the exact request JSON without sending it. Useful both for debugging and as test fixture generation.

3. **Model configuration validation on startup**: When models.json is loaded, check that each reasoning model's max_tokens is compatible with at least the default thinking level. Fail loudly at startup, not at request time.

## Worm's Eye (Tactical)

The single most impactful change: make `build_request_body` testable (pub visibility) and add one assertion per thinking level per model size class. This would have caught this bug, the Sonnet 4 edge case, and any future thinking budget changes, all with about 10 lines of test code per scenario.
