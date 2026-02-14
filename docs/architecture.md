# ri Architecture

## What ri is

ri is a Rust tool for building applications on top of turn-based LLM APIs. Its first application is a coding agent (like Claude Code), but the core data model is general-purpose -- a message pool that any LLM-based tool can build on.

## Foundational insight

An LLM API is a stateless pure function:

```
f(Vec<Message>) -> Response
```

Every call is independent. The API has no memory. All "memory" is in the caller's hands -- it's whatever the caller chooses to put in `Vec<Message>`.

The fundamental object is not the "conversation" or the "session" or the "turn." It is the **message**. Messages are immutable content blobs that live in a shared pool. Some messages are authored (by humans, tools, or code). Some messages are derived (produced by an LLM call). Derived messages carry **provenance** -- a record of which input messages produced them and how.

"Running a turn" is just: assemble some messages from the pool, call the LLM, and the output is a new message (with provenance) added back to the pool. The pool grows. You compose the next call by selecting from the pool again. The pool is a palette of ingredients you craft from.

## Why messages, not sessions or turns

Most agent frameworks treat the "session" or "conversation" as the primary structure: a linear sequence of messages that accumulates over time. This bakes in assumptions:

- Messages are ordered linearly (what about branching, fan-out, cross-pollination?)
- Context grows by accumulation (what about compaction, summarization, rewriting?)
- Each LLM call builds on the previous one (what about independent explorations that merge?)

By making the message the atom and the pool the structure, these assumptions dissolve:

- **Composition is free**: Pull messages from anywhere -- different sessions, different agents, different time periods. Assemble whatever context you want.
- **Compaction is not special**: Want to compress old context? Run an LLM call with `[system: "summarize this", old_msg]`. The output is a new message. Use it instead of the original. The original stays in the pool.
- **Cross-pollination is natural**: Take an insight from exploration A (one message) and inject it into exploration B's context. Just reference its ID.
- **Fan-out/fan-in**: Send the same messages to different models. Compare the output messages. Pick the best. Use it going forward. All just messages in the pool.
- **Agent handoff**: Agent A produces a message. Agent B uses it as input. The message flows through the pool. No special "handoff" mechanism needed.

## The message

Every message has:

- **id**: globally unique identifier (short random string, e.g., `fx_m3`)
- **role**: `system`, `user`, or `assistant` (per LLM API convention)
- **content**: array of content blocks (text, images, tool use, tool results, thinking)

Some messages also have:

- **provenance**: record of the LLM call that produced this message (the stable core)
  - `input`: ordered array of message IDs (the exact context the LLM saw)
  - `model`: which model was used
  - `ts`: when the call completed
  - `usage`: token counts (input, output, cache read, cache write)
- **meta**: open object for application-defined metadata (provider name, duration, cost, thinking level, etc.)

Provenance captures the four things that are fundamental and stable: what went in, what produced it, when, and the token cost. Everything else (provider details, wall-clock time, dollar cost, thinking level) goes in `meta`, which is an open object the store preserves but does not interpret. This lets applications extend the metadata without changing the core format.

Messages without provenance are "authored" -- written by humans, tools, or code.
Messages with provenance are "derived" -- produced by an LLM call.
Both can carry `meta` for application-specific data (e.g., a timestamp on authored messages, or cost tracking on derived ones).

A message is immutable once written. It exists in the pool forever (or until the pool is archived/compacted at the storage level).

## The message pool

The message pool is the set of all messages. In memory, it's a `HashMap<Id, Message>`. It's the palette you compose from.

Core operations:

- `put(message) -> Id` -- add a message to the pool
- `get(id) -> Message` -- retrieve a message by ID
- `run(input_ids, model) -> Id` -- call LLM with the given messages, write the output as a new message with provenance, return its ID

The message pool doesn't know about sessions, conversations, agents, or tools. It's a bag of messages with a "derive new message via LLM" operation.

## Filing: how messages are organized on disk

The pool is the logical model. Filing is the physical model -- how messages are stored in files on disk.

Filing is a separate concern from the pool. The pool doesn't know about files. The filing layer loads messages from files into the pool, and writes new messages to files.

Different applications can use different filing strategies:

- **Chat agent (ri)**: one JSONL file per session, messages filed by which session they belong to
- **Pipeline tool**: one file per pipeline run
- **Test harness**: one file per test suite
- **Research tool**: one file per experiment

For ri, the filing strategy is:

```
~/.ri/sessions/
  2026-02-09_080000_fix-login.jsonl
  2026-02-09_090000_review.jsonl
```

Each file contains messages from one "session." The first line is file-level metadata (session name, working directory, creation time). The rest are messages, one per line.

A message's globally unique ID allows cross-file references. When a message in the review session references a message from the fix-login session (by ID in its provenance.input), the system resolves it from the global pool (which was populated by loading all session files).

### Loading

On startup, the filing layer scans all session files and loads every message into the pool. The pool is the union of all files. This is fast -- a year of heavy use produces ~60-100MB of JSONL, which loads in under a second.

### Writing

New messages are appended to the "active" session file. When a new session starts, a new file is created and becomes the active write destination.

### Session tracking

For the chat agent, a "session" means a linear chain of LLM calls (derived messages) where each call's input typically includes the previous call's output. This is tracked at the application level as a list of message IDs forming the session's turn sequence. This is an application concern, not a pool or filing concern. It can be stored as metadata in the session file or maintained in memory.

## Architecture layers

```
Layer 0: Foundation (ri)
  - Messages (with optional provenance)
  - MessagePool: HashMap<Id, Message> in memory
  - Filing: per-session JSONL files
  - Model, ThinkingLevel, ModelCost
  - LlmProvider trait, RequestOptions, ApiError
  - Tool trait, ToolOutput
  - StreamEvent, ToolSchema
  - StreamAccumulator (pure: StreamEvent -> ContentBlock)

Layer 1: I/O (ri-ai, ri-tools)
  - ri-ai: LLM API implementations (Anthropic, Gemini)
    - Streaming SSE, OAuth, API keys
    - Turn: call provider + accumulate response
  - ri-tools: Built-in tool implementations (bash, read, write, edit)

Layer 2: Application (ri-cli)
  - Agent loop (compose messages, call LLM, execute tools, repeat)
  - CLI parsing, run modes (interactive, print, RPC)
  - TUI / display
  - Config, resource loading
```

Each layer depends only on the layers above it. There is no agent loop crate --
the loop is application-level composition of Turn, Tool, and SessionStore.
Different applications compose these primitives differently (agent loop,
pipeline, fan-out, evaluation harness).

## Crate structure

```
ri/
  crates/
    ri/             # Layer 0: Foundation -- types, pool, filing, traits
    ri-ai/          # Layer 1: LLM providers (Anthropic, Gemini), Turn, auth
    ri-tools/       # Layer 1: Built-in tool implementations
  ri-cli/           # Layer 2: CLI entry point, agent loop, modes, TUI
```

### ri

The foundation crate. Everything the rest of the system depends on:

- Message types: `Message`, `Role`, `ContentBlock`, `Provenance`, `Usage`
- `MessagePool`: in-memory store (HashMap; ordering is tracked externally by the caller)
- `SessionStore`: per-session JSONL file read/write
- Model types: `Model`, `ModelCost`, `ThinkingLevel`
- `LlmProvider` trait and `RequestOptions`
- `ApiError` types (Http, Api, ContextOverflow, RateLimited, StreamParse)
- `Tool` trait, `ToolOutput` (tool interface and results)
- `StreamEvent` (normalized stream events from any provider)
- `ToolSchema` (tool definitions as seen by the LLM API)
- `StreamAccumulator` (pure state machine: feeds on `StreamEvent`s, produces `Vec<ContentBlock>` + `Usage`)

Does NOT handle: LLM API calls, tool execution, agent loop logic.

### ri-ai

LLM provider implementations. Handles:

- Anthropic provider: SSE parsing, request body construction, OAuth tool name remapping
- Gemini provider: Cloud Code Assist API, Antigravity variant
- `Turn`: call a provider and accumulate the streamed response into content blocks. Thin wrapper over `LlmProvider::stream()` + `StreamAccumulator`. The fundamental "call the LLM once" building block.
- Model catalog and registry (code-defined, no JSON config)
- Provider resolution (auth store, env vars, token refresh)
- Login flow registry (OAuth flows for each provider)
- Auth store (~/.ri/auth.json persistence)

Does NOT handle: message storage, tool execution, agent loop.

### ri-tools

Built-in tool implementations: bash, read, write, edit. Each implements the `Tool` trait from ri. These are simple, mostly-finished modules.

### ri-cli

Application entry point. Wires everything together:

- The agent loop: composes `Turn`, tool execution, and `SessionStore` into the standard "call LLM, execute tools, repeat" loop. Returns a stream of `AgentEvent`s -- no callback trait.
- CLI argument parsing (clap)
- Config resolution (settings.json)
- Resource loading (context files, skills, prompts)
- System prompt construction
- Run modes: interactive (REPL with ratatui TUI), print (single-shot), RPC (JSON over stdio)
- Session management (creating sessions, naming, listing)

ri-cli is provider-agnostic. It receives a `Box<dyn LlmProvider>` and `Model` from ri-ai's registry and drives the agent loop without knowing which provider is behind the trait.

## The agent loop and the pool

The agent loop is the coding agent's core behavior. It lives in ri-cli (application code, not infrastructure) and composes the foundation primitives:

```
1. User provides input -> user message written to pool + session file
2. Select messages from the pool for this LLM call (currently: all session messages)
3. Start a Turn with the selected messages
4. Stream events from the Turn (yielded to the caller for display)
5. Turn finishes -> extract content blocks and usage
6. Build assistant message with provenance, write to pool + session file
7. If response contains tool calls:
   a. Execute each tool (tool.run())
   b. Write tool results as a user message to pool + session file
   c. Go to step 2
8. If no tool calls: done, wait for user input
```

The agent loop is ~80 lines of application code that returns `impl Stream<Item = AgentEvent>`. Each consumer (interactive REPL, print mode, RPC mode) iterates the stream and handles events differently. No callback trait is needed.

The message selection in step 2 is where future context management intelligence will live. Currently it's trivial (include all session messages in order). A compacting strategy would summarize old messages, a cross-session strategy would pull from other sessions, etc. These are just different ways to select IDs from the pool.

## Context strategy examples

### Naive (simplest)
Return all messages from the current session in chronological order. No transformation.

### Windowed
Return the system prompt + the last N messages. Drop old ones.

### Compacting
Track approximate token usage. When approaching the context budget:
1. Run a separate LLM call: `[system: "Summarize this conversation concisely", ...old_messages]`
2. The output is a new summary message in the pool
3. Future calls use [system_prompt, summary, recent_messages] instead of the full history

Note: the summarization call is itself an LLM call that produces a message with provenance. It's recorded in the pool like any other. Nothing special at the data model level.

### Cross-session
Pull relevant messages from other sessions. For example, an agent working on feature B might include a summary message from the session that implemented feature A, giving it context about related code changes.

### Custom
Any logic. The strategy is just Rust code that returns `Vec<message ID>`. The pool is the palette it draws from.

## Error handling

**Crash during write**: The filing layer appends one JSONL line per message. If the process crashes mid-write, the last line may be incomplete. On next load: skip incomplete lines (the last line, if malformed). All previous messages are intact.

**LLM call failure**: If the LLM call fails, no derived message is written (the message is only written after the call completes successfully). The pool and files remain consistent.

**Tool execution failure**: Tool errors produce a message with `is_error: true` in the tool_result content block. This is a normal message -- it enters the pool and gets included in the next LLM call's context so the model can see the error and react.

## What's NOT in this design

- **Extensions / plugins**: Not in v1. Strategy is Rust code. Pluggable strategies can come later via traits.
- **Multi-model orchestration**: Each LLM call uses one model (recorded in provenance). Multi-model workflows are just multiple messages with different provenance.model values.
- **Full-text search**: Not in v1. Grep across session files is sufficient at this scale.
- **Encryption / access control**: Not needed. Single-user, personal machine, indie games.
- **Session UI index**: The chat agent will need a concept of "which derived messages form this session's turn sequence." This is an application-level index, not a pool concern. It can be stored as metadata or computed from provenance chains. Deferred to implementation time.
