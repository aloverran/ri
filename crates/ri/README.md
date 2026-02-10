# ri

Core types, traits, and storage for ri. This is the foundation crate that all other ri crates depend on.

## Modules

**message** -- `Message`, `Role`, `ContentBlock`, `Provenance`, `Usage`, `MessagePool`, `SessionHeader`, `SessionInfo`. The message is the fundamental building block: an immutable content blob with optional provenance (which LLM call produced it). The pool is an in-memory `HashMap<Id, Message>` with insertion-order iteration.

**model** -- `Model` (id, name, context window, max tokens, cost), `ModelCost`, `ThinkingLevel`.

**provider** -- `LlmProvider` trait with `stream(RequestOptions) -> EventStream`. Also `ApiError`, `RequestOptions`, `EventStream`.

**event** -- `StreamEvent`: normalized events from any LLM provider (TextStart/Delta/End, ThinkingStart/Delta/End, ToolCallStart/Delta/End, Done, Error). Also `ToolSchema` for tool definitions sent to the LLM API.

**tool** -- `ToolDef`, `ToolFn`, `ToolOutput`. Tools are plain function pointers, not trait objects.

**filing** -- `SessionFiling`: reads/writes per-session JSONL files, manages the message pool, tracks the active session.

**id** -- `gen_id()`, `gen_session_prefix()`. UUID-based ID generation.
