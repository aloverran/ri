# Refactor: Absorb ri-agent, establish three-layer architecture

## Goal

Eliminate the `ri-agent` crate. Move its useful pieces into `ri` (pure transforms) and `ri-ai` (I/O wrappers). The agent loop becomes application code in `ri-cli`, not infrastructure.

## Why

`ri-agent` currently owns the agent loop (~200 lines), `AgentEvent`, `AgentCallback`, `ContextStrategy`, `RunConfig`, and stream accumulation logic. Most of this is either:

- **Pure data transforms** that belong in `ri` (stream accumulation: `StreamEvent` -> `ContentBlock`)
- **Application-specific composition** that belongs in the app (the loop itself, event display, tool execution wiring)
- **Thin I/O wrappers** that belong in `ri-ai` (calling a provider and collecting the result)

The current `ri-agent` crate forces one composition shape (the agent loop). Non-agent use cases (pipelines, fan-out, evaluation, batch processing) would skip it entirely and lose the useful pieces locked inside.

## Target architecture

```
ri          - Data model + pure transforms
ri-ai       - LLM I/O (providers + turn)
ri-tools    - Tool I/O (bash, read, write, edit)
ri-cli      - Application (agent loop, REPL, modes, config)
```

Each layer depends only on the layers above it in this list. No `ri-agent` crate.

## What moves where

### Into `ri`: StreamAccumulator

A pure state machine that accumulates `StreamEvent`s into `Vec<ContentBlock>` + `Option<Usage>`. No I/O, no async. Both `StreamEvent` and `ContentBlock` are already defined in `ri`.

```rust
// ri/src/accumulator.rs

pub struct StreamAccumulator { ... }

impl StreamAccumulator {
    pub fn new() -> Self;
    /// Feed a stream event. Accumulates content internally.
    pub fn feed(&mut self, event: &StreamEvent);
    /// Consume the accumulator, returning completed content blocks and usage.
    pub fn finish(self) -> (Vec<ContentBlock>, Option<Usage>);
}
```

The accumulation logic is extracted verbatim from the current `run()` function in `ri-agent/src/lib.rs` (the match arms on `StreamEvent` variants that build `text_buf`, `thinking_buf`, `tool_calls`, `content_blocks`, and `usage`). Currently ~80 lines embedded in the loop, becomes a standalone module.

### Into `ri-ai`: Turn

A thin async wrapper: call `provider.stream()`, wrap with `StreamAccumulator`, present as a stream that also collects results.

```rust
// ri-ai/src/turn.rs

pub struct Turn { ... }

impl Turn {
    /// Start a turn by calling the provider.
    pub async fn start(
        provider: &dyn LlmProvider,
        opts: RequestOptions,
    ) -> Result<Self, ApiError>;

    /// Poll the next stream event (for display/observation).
    /// Returns None when the stream is exhausted.
    pub async fn next(&mut self) -> Option<Result<StreamEvent, ApiError>>;

    /// After the stream is exhausted, extract the accumulated result.
    pub fn result(self) -> (Vec<ContentBlock>, Option<Usage>);
}
```

`Turn` wraps `EventStream` + `StreamAccumulator`. Calling `.next()` feeds each event through the accumulator and returns it to the caller. When the stream ends, `.result()` returns what the accumulator collected.

Optionally implement `futures::Stream` on `Turn` so callers can use `StreamExt` methods, but the explicit `.next()` / `.result()` API is the primary interface.

### Into `ri-cli`: the agent loop

The agent loop moves from `ri-agent/src/lib.rs` to `ri-cli`. It becomes application code that composes `Turn`, tool execution, and `SessionStore`. Roughly:

```rust
// ri-cli, somewhere in the application layer

pub async fn agent_loop(
    provider: &dyn LlmProvider,
    model: &Model,
    system_prompt: &str,
    tools: &[Box<dyn Tool>],
    filing: &mut SessionStore,
    session_ids: &mut Vec<String>,
    cancel: CancellationToken,
    // returns a stream of AgentEvents
) -> impl Stream<Item = AgentEvent> {
    async_stream::stream! {
        let tool_schemas = tools.iter().map(|t| t.schema()).collect();
        let tool_map: HashMap<&str, &dyn Tool> = ...;

        loop {
            let input_ids = session_ids.clone(); // or strategy
            let messages = filing.pool.resolve_existing(&input_ids).cloned().collect();
            let opts = RequestOptions { model, system_prompt, messages, tools: tool_schemas, ... };

            let mut turn = ri_ai::Turn::start(provider, opts).await?;
            while let Some(event) = turn.next().await {
                yield AgentEvent::StreamEvent(event);
            }
            let (content, usage) = turn.result();

            // Build + persist assistant message
            let msg = Message { id: filing.next_id(), role: Assistant, content, provenance: ... };
            filing.write_message(msg)?;
            session_ids.push(msg.id);
            yield AgentEvent::MessageComplete(msg);

            // Tool calls
            let calls = extract_tool_calls(&msg.content);
            if calls.is_empty() { break; }

            for (id, name, input) in calls {
                yield AgentEvent::ToolStart { id, name };
                let output = tool_map[name].run(input, cwd, cancel).await;
                yield AgentEvent::ToolEnd { id, output, is_error };
                results.push(ContentBlock::tool_result_text(id, output.text, output.is_error));
            }

            // Persist tool results
            let tool_msg = Message::new(filing.next_id(), Role::User, results);
            filing.write_message(tool_msg)?;
            session_ids.push(tool_msg.id);
        }
    }
}
```

This is ~40 lines of application code. No callback trait. Returns a stream of `AgentEvent` that the REPL / print mode / RPC mode consumes.

### Deleted

- `ri-agent` crate (Cargo.toml, src/lib.rs, README.md)
- `AgentCallback` trait (replaced by stream return)
- `RunConfig` struct (callers pass args directly or define their own config)
- `ContextStrategy` type alias (trivial -- callers just write the selection inline or define their own function)

### Types that move

| Type | From | To | Notes |
|------|------|----|-------|
| `StreamAccumulator` | new | `ri` | New module `accumulator.rs` |
| `Turn` | new | `ri-ai` | New module `turn.rs` |
| `AgentEvent` | `ri-agent` | `ri-cli` | Application-level, not infrastructure |
| `AgentCallback` | `ri-agent` | deleted | Replaced by stream return |
| `RunConfig` | `ri-agent` | deleted | Args passed directly |
| `ContextStrategy` | `ri-agent` | deleted | Inline in application code |
| `naive_strategy()` | `ri-agent` | deleted | Trivial: `session_ids.clone()` |

## Steps

1. **Add `StreamAccumulator` to `ri`.**
   Create `crates/ri/src/accumulator.rs`. Extract the stream accumulation logic from `ri-agent/src/lib.rs` (the `StreamEvent` match arms). Add `pub mod accumulator` and re-export from `ri/src/lib.rs`.

2. **Add `Turn` to `ri-ai`.**
   Create `crates/ri-ai/src/turn.rs`. Implement `Turn::start()`, `.next()`, `.result()` using `StreamAccumulator`. Add `pub mod turn` and re-export from `ri-ai/src/lib.rs`.

3. **Move agent loop to `ri-cli`.**
   Create a module in `ri-cli` (e.g. `agent.rs`) that contains the loop as a stream-returning function. Move `AgentEvent` there. Update `interactive.rs`, `print_mode.rs`, `rpc_mode.rs` to consume the stream instead of implementing `AgentCallback`.

4. **Remove `ri-agent`.**
   Delete `crates/ri-agent/`. Remove from `Cargo.toml` workspace members. Remove from `ri-cli/Cargo.toml` dependencies.

5. **Verify.**
   `cargo check`, `cargo test`. Ensure all three modes (interactive, print, rpc) work.

## What this enables (beyond agents)

All non-agent use cases compose from `Turn` + `MessagePool` + `SessionStore`:

- **Pipeline**: chain `Turn`s, each using the previous result as input
- **Fan-out**: `tokio::join!` over multiple `Turn::start()` calls
- **Evaluation**: `Turn` in a loop over test inputs, collect scores
- **Summarization**: `Turn` with a summary prompt, store result in pool
- **Cross-session**: pull messages from pool by ID, feed to a new `Turn`

None of these need an agent loop crate. They use the same primitives the agent uses.
