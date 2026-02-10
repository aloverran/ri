# ri-store

Message pool and session filing. This is the data layer -- everything in ri is a message, and this crate defines what a message is, stores them in memory, and persists them as JSONL files.

See [store-format.md](../../docs/store-format.md) for the on-disk format specification.

## What's here

**types** -- `Message`, `ContentBlock`, `Role`, `Provenance`, `Usage`, `SessionHeader`. A message has an id, role, content blocks, and optionally provenance (which records the LLM call that produced it) and metadata. Content blocks are tagged: `Text`, `Thinking`, `Image`, `ToolUse`, `ToolResult`, and `Unknown` (catch-all for forward compat). Unknown fields are preserved on round-trip via `#[serde(flatten)]` on every type.

**pool** -- `MessagePool`: a `HashMap<String, Message>` with insertion-order tracking. Core operations: `put`, `get`, `iter`, `resolve` (bulk ID lookup), `derived_from` (find messages produced from a given input). The pool is the in-memory universe of all messages.

**filing** -- `SessionFiling`: loads JSONL session files into the pool and writes new messages to the active session file. One file per session, append-only. File header (first line) is session metadata; subsequent lines are messages. Handles crash recovery by skipping malformed lines (warns and continues). Generates prefixed message IDs (e.g. `fixlog_a3f7b2_1`) for human readability.

**id** -- `gen_id()`: UUID v4 as a hex string without dashes.

## Depends on

Nothing in the ri workspace. External: serde, serde_json, chrono, uuid, dirs, eyre.
