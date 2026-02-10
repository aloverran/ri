# ri-core

Shared types, traits, and stream events. This crate defines the vocabulary that connects ri-store (data), ri-ai (providers), ri-tools (tool implementations), and ri-agent (loop).

## What's here

**types** -- Re-exports all ri-store types (`Message`, `ContentBlock`, `Role`, etc.) as a canonical import path. Adds `Model` (id, name, context window, max tokens, cost), `ModelCost`, and `ThinkingLevel` (Off/Low/Medium/High/XHigh).

**tool** -- `ToolDef` and `ToolFn`. Tools are plain function pointers, not trait objects: `fn(Value, PathBuf, CancellationToken) -> Pin<Box<dyn Future<Output = ToolOutput>>>`. A `ToolDef` bundles the function with its name, description, and JSON Schema parameters.

**event** -- `StreamEvent`: normalized events from any LLM provider (TextStart/Delta/End, ThinkingStart/Delta/End, ToolCallStart/Delta/End, Done, Error). Also `ToolSchema` for the tool definitions sent to the LLM API.

**provider** -- `LlmProvider` trait with a single method: `stream(RequestOptions) -> EventStream`. Also defines `ApiError` (Http, Api, ContextOverflow, RateLimited, StreamParse, Other) and `RequestOptions` (model, system prompt, messages, tools, thinking level).

## Depends on

ri-store. External: serde, serde_json, futures, thiserror, tokio-util.
