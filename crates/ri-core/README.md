# ri-core

Agent loop, tool definitions, provider trait, stream events, and shared types. This crate defines the interfaces that connect the data layer (ri-store) to the AI layer (ri-ai) and tool layer (ri-tools), and runs the core loop: prompt the LLM, execute tool calls, repeat.

## What's here

**types** — Re-exports all ri-store types (`Message`, `ContentBlock`, `Pool`, etc.) as the canonical import path. Adds `Model` (id, name, context window, max tokens, cost), `ModelCost`, and `ThinkingLevel` (Off/Low/Medium/High/XHigh).

**tool** — `ToolDef` and `ToolFn`. Tools are plain function pointers, not trait objects: `fn(Value, PathBuf, CancellationToken) -> Pin<Box<dyn Future<Output = ToolOutput>>>`. A `ToolDef` bundles the function with its name, description, and JSON Schema parameters.

**event** — `StreamEvent`: normalized events from any LLM provider (TextStart/Delta/End, ThinkingStart/Delta/End, ToolCallStart/Delta/End, Done, Error). Also `ToolSchema` for the tool definitions sent to the LLM API.

**provider** — `LlmProvider` trait with a single method: `stream(RequestOptions) -> EventStream`. Also defines `ApiError` (Http, Api, ContextOverflow, RateLimited, StreamParse, Other) and `RequestOptions` (model, system prompt, messages, tools, thinking level).

**agent** — `run()`: the agent loop as a free function, not a struct. Takes a `RunConfig` (provider, model, system prompt, tools, thinking level, max tokens, cwd), a mutable `Vec<Message>`, an `AgentCallback`, and a `CancellationToken`. Streams the LLM response, accumulates content blocks, builds a `Message` with provenance, then executes any tool calls and loops. The `AgentCallback` trait lets the caller supply ID generation and event handling — interactive mode uses it to persist messages and display output; print/RPC modes use simpler implementations. Emits `AgentEvent`s: TurnStart/End, StreamEvent, ToolStart/End, Error, MessageComplete.

## Depends on

ri-store. External: tokio, serde, futures, eyre, thiserror, chrono, tokio-util.
