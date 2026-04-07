# ri

Core types, traits, and storage for ri. This is the foundation crate that all other ri crates depend on.

## Modules

**model** -- The core data model. `Message` and `Context` are the two primitives: a message is an immutable content blob, a context is an immutable object (ordered message list + parent links + metadata, forming a DAG). Also: `MessageId`, `ContextId`, `SessionId` (ID newtypes), `Role`, `ContentBlock`, `Usage`, `gen_id()`.

**store** -- Persistence. `Pool` (in-memory message/context store), `Session` (named pointer to a context), `SessionHeader`, `Store` (pool + JSONL file management: load, write, checkpoint).

**stream** -- `StreamEvent`: normalized incremental events from any LLM provider during response streaming.

**accumulator** -- `StreamAccumulator`: pure state machine that converts `StreamEvent`s into `Vec<ContentBlock>` + `Usage`.

**provider** -- Contracts for LLM providers and tools. `Model`, `ModelCost`, `ThinkingLevel`, `LlmProvider` trait, `RequestOptions`, `ApiError`, `EventStream`, `AuthMethod`, `Tool` trait, `ToolSchema`, `ToolOutput`.
