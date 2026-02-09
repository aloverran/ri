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

## The pool

The pool is the set of all messages. In memory, it's a `HashMap<Id, Message>`. It's the palette you compose from.

Core operations:

- `put(message) -> Id` -- add a message to the pool
- `get(id) -> Message` -- retrieve a message by ID
- `run(input_ids, model) -> Id` -- call LLM with the given messages, write the output as a new message with provenance, return its ID

The pool doesn't know about sessions, conversations, agents, or tools. It's a bag of messages with a "derive new message via LLM" operation.

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
Layer 0: Pool (ri-store)
  - Messages (with optional provenance)
  - HashMap<Id, Message> in memory
  - The universal data model

Layer 0.5: Filing (ri-store)
  - Per-session JSONL files
  - Loading files into the pool
  - Writing new messages to the active file
  - Application-pluggable filing strategy

Layer 1: Provider (ri-ai)
  - LLM API abstraction
  - Streaming SSE
  - Provider-specific request formatting (Anthropic, OpenAI, etc.)
  - OAuth, API keys
  - Takes Vec<Message>, returns streamed Response

Layer 2: Agent / Strategy (ri-core)
  - The agent loop (compose messages, call LLM, execute tools, repeat)
  - Context strategy (how to select and arrange messages for each LLM call)
  - Tool execution
  - Session tracking (linear chain of derived messages)

Layer 3: Application (ri-cli)
  - CLI parsing, run modes (interactive, print, RPC)
  - TUI / display
  - User interaction (input, output, commands)
  - Config resolution (models.json, settings, auth)
  - Resource loading (context files, skills, prompts)
```

Each layer depends only on the layers below it.

## Crate structure

```
ri/
  crates/
    ri-store/       # Layer 0 + 0.5: Message pool, filing, on-disk format
    ri-core/        # Layer 2: Agent loop, strategy, tool trait, event system, types
    ri-ai/          # Layer 1: LLM providers (Anthropic, etc.), streaming, auth
    ri-tools/       # Layer 2: Built-in tool implementations
  ri-cli/           # Layer 3: CLI entry point, modes, config, TUI
```

### ri-store

The message pool and filing system. Handles:

- Message type definition (id, role, content, provenance)
- In-memory pool (HashMap of messages)
- Filing: read/write per-session JSONL files
- Pool queries: get by ID, find by criteria, walk provenance chains

Does NOT handle: LLM API calls, tool execution, context strategy, agent loop logic.

This crate defines the universal data model. It is small (~300-400 lines) and should be finished early -- it changes rarely once the message format is stable.

### ri-core

Agent types and the agent loop. Handles:

- ContentBlock types (text, image, tool_use, tool_result, thinking)
- The Tool trait and ToolResult types
- AgentEvent system (broadcast events to observers: TUI, RPC, logging)
- The agent loop: compose context -> call LLM -> execute tools -> repeat
- Context strategy: how to select messages from the pool for each LLM call

Depends on ri-store (to read/write messages) and ri-ai (to call LLMs).

### ri-ai

LLM provider abstraction. Handles:

- LlmProvider trait: stream(messages, options) -> Stream<Event>
- Anthropic provider: SSE parsing, request body construction, OAuth tool name remapping
- Model definitions and model registry
- API key resolution (env var, shell command, literal, OAuth)

Does NOT handle: message storage, tool execution, agent loop.

### ri-tools

Built-in tool implementations: bash, read, write, edit, find, grep, ls. Each implements the Tool trait from ri-core. These are simple, mostly-finished modules.

### ri-cli

Application entry point. Wires everything together:

- CLI argument parsing (clap)
- Config resolution (models.json, settings.json, auth.json)
- Resource loading (context files, skills, prompts)
- System prompt construction
- Run modes: interactive (REPL), print (single-shot), RPC (JSON-RPC over stdio)
- Display / TUI
- Session management (creating sessions, naming, listing)

## The agent loop and the pool

The agent loop is the coding agent's core behavior. It orchestrates the pool, the provider, and the tools:

```
1. User provides input
2. Strategy composes the next LLM call's input:
   - Selects messages from the pool (system prompt, conversation history, tool results)
   - May transform messages (summarize old ones, strip tool calls, inject context)
   - Produces an ordered list of message IDs
3. Any NEW messages (not yet in pool) are written to pool + active session file
4. Provider is called with the resolved messages
5. Response is streamed (events emitted for display)
6. Response message is written to pool + active session file (with provenance)
7. If response contains tool calls:
   a. Tools are executed
   b. Tool results are written as new messages to pool + active session file
   c. Go to step 2
8. If no tool calls: done, wait for user input
```

The strategy (step 2) is where all context management intelligence lives. It's a function that takes the pool and returns a list of message IDs. For a simple agent, it returns "all messages from this session in order." For an advanced agent, it might pull from multiple sessions, summarize old context, or inject dynamic information.

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
