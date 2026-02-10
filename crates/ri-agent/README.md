# ri-agent

The agent loop: compose context from the message pool, stream an LLM response, execute tool calls, persist everything, repeat.

## What's here

**agent** -- `run()`: the agent loop as a free function, not a struct. Takes a `RunConfig` (provider, model, system prompt, tools, thinking level, max tokens, cwd, strategy), a mutable `SessionFiling`, a `Vec<String>` of session message IDs, an `AgentCallback`, and a `CancellationToken`. Streams the LLM response, accumulates content blocks, builds a `Message` with provenance, then executes any tool calls and loops. Emits `AgentEvent`s: TurnStart/End, StreamEvent, ToolStart/End, Error, MessageComplete.

`ContextStrategy` is a function pointer `fn(&MessagePool, &[String]) -> Vec<String>` -- given the pool and current session IDs, return the ordered list of message IDs for the next LLM call. `naive_strategy` returns all session IDs in order.

## Depends on

ri. External: tokio, serde, serde_json, futures, eyre, chrono, tokio-util.
