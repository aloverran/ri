# ri

Core types, traits, and storage for ri. This is the foundation crate that all other ri crates depend on.

## Modules

**message** -- The atomic building block. `MessageId`, `SessionId`, `StepId` (ID newtypes), `Role`, `ContentBlock` (typed content within a message), `StreamEvent` (incremental form of content blocks during streaming), `Usage`, `Message`, `gen_id()`.

**store** -- The object pool and persistence layer. `Pool` (in-memory message/step store), `Context` (ordered message ID list), `Step` (history DAG node), `Session` (named pointer to a step), `SessionHeader`, `Store` (pool + JSONL file management: load, write, checkpoint).

**stream** -- `StreamEvent`: normalized incremental events from any LLM provider during response streaming.

**accumulator** -- `StreamAccumulator`: pure state machine that converts `StreamEvent`s into `Vec<ContentBlock>` + `Usage`.

**provider** -- Contracts for LLM providers and tools. `Model`, `ModelCost`, `ThinkingLevel`, `LlmProvider` trait, `RequestOptions`, `ApiError`, `EventStream`, `AuthMethod`, `Tool` trait, `ToolSchema`, `ToolOutput`, `ToolContext`.
