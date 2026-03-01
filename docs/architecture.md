# ri Architecture

## What ri is

ri is a Rust tool for building applications on top of turn-based LLM APIs. Its first application is a coding agent (like Claude Code), but the core data model is general-purpose -- a message pool and context DAG that any LLM-based tool can build on.

## Foundational insight

An LLM API is a stateless pure function:

```
f(Vec<Message>) -> Response
```

Every call is independent. The API has no memory. All "memory" is in the caller's hands -- it's whatever the caller chooses to put in `Vec<Message>`.

The fundamental building blocks are **messages** (immutable content blobs) and **contexts** (immutable objects -- an ordered list of message references, parent links, and metadata). Messages live in a shared pool. A context resolved against the pool gives you `Vec<Message>`, which is what you hand to the LLM.

Contexts form a DAG through their parent links. "Running a turn" is: resolve a context's messages, call the LLM, add the output as a new message, and create a new context with the updated message list. The system is just messages, contexts, and the algebra on them.

## Data model

### Messages

A message is an immutable content blob (role + content blocks) that lives in the pool. Messages are context-free -- they carry no information about which LLM call produced them or what context they were part of.

Every message has:

- **id**: globally unique identifier (`MessageId` newtype)
- **role**: `system`, `user`, or `assistant` (per LLM API convention)
- **content**: array of typed content blocks (text, thinking, image, tool_use, tool_result, error)
- **meta**: optional open object for application-defined metadata

Messages are authored (by humans, tools, or code) or derived (produced by an LLM call). Both are the same type.

### Contexts

A context is an immutable object: an ordered list of message references, parent links, and metadata. It's the second primitive alongside Message.

```rust
pub struct Context {
    pub id: ContextId,
    pub messages: Vec<MessageId>,
    pub parents: Vec<ContextId>,
    pub meta: Option<serde_json::Value>,
}
```

Contexts form a DAG through their parent links. Resolved against the pool, `messages` gives you `Vec<Message>` -- what you hand to the LLM. A context is a freestanding immutable object. The system is just contexts and the operations that create new ones from old ones.

### Sessions

A session is a named pointer to a context. Like a git branch, it tells you where you are in the history.

Every session has:

- **name**: human-readable label (e.g. "fix-login")
- **file_id**: the `SessionId` that locates the session file on disk
- **head**: the `ContextId` this session currently points to
- **cwd**: the working directory associated with this session
- **parent**: optional parent session ID (for sub-agent sessions)
- **ts**: creation timestamp

Sessions advance by writing new contexts and moving the head pointer. This is an atomic append operation (append context line + head line to the JSONL file).

### The git analogy

A useful reference point, though ri's model is its own thing:

| Git | ri |
|-----|----|
| Blob | Message |
| Commit | Context |
| Branch | Session |
| Repository | Pool + Store |

The key similarity is that content (messages/blobs) is separate from structure (contexts/commits), and structure forms a DAG. The key difference is that ri's model is simpler -- there's no separate Tree object, just two types and a pointer.

## Why this model

### Messages and contexts are the two primitives

You need exactly two things to call an LLM: messages (content) and a context (which messages, in what order). Everything else in ri -- sessions, the DAG, persistence -- is just algebra on these two types.

By keeping messages as pure content blobs and contexts as freestanding immutable objects, composition is free:

- **Reuse**: Pull any message into any context. A summary from session A works in session B's context. The message doesn't "know" where it came from.
- **Compaction**: Summarize old messages into a new message. Create a new context with the summary replacing the originals.
- **Fan-out**: Send the same context to different models. Each produces a new message. Pick the best.
- **Branching**: Create two contexts with the same parent but different message lists.
- **Checkpointing**: After each agent turn, create a new context. On reload, jump straight to the head context's messages -- no replay needed.
- **Merging**: Create a context whose message list combines messages from different branches.

## The pool

The pool is the in-memory object store. Messages and contexts live here, referenced by ID.

```rust
pub struct Pool {
    messages: HashMap<MessageId, Message>,
    contexts: HashMap<ContextId, Context>,
}
```

Core operations:

- `get_message(id)` -- retrieve a message by ID
- `get_context(id)` -- retrieve a context by ID
- `resolve(ids)` -- resolve an ordered list of message IDs to their messages
- `resolve_context(ctx)` -- resolve a Context to its messages
- `put_message(msg)` -- add a message (crate-internal)
- `put_context(ctx)` -- add a context (crate-internal)

The pool doesn't know about sessions, files, or agents. It's a bag of objects with lookup by ID. The `Store` layer populates it from disk and writes new objects to session files.

## Filing: how objects are organized on disk

The pool is the logical model. Filing is the physical model -- how objects are stored in files on disk.

For ri, the filing strategy is one JSONL file per session:

```
~/.ri/sessions/
  2026-02-09_080000_fix-login.jsonl
  2026-02-09_090000_review.jsonl
```

Each file contains five line types:

1. **Session header** (first line): `{"session": "name", "ts": "...", "cwd": "..."}`
2. **Message lines**: `{"msg": "m1", "role": "user", "content": [...]}`
3. **Context lines**: `{"context": "c1", "messages": ["m1", "m2"], "parents": ["c0"]}`
4. **Head updates**: `{"head": "c1"}` -- the last one wins
5. **Title updates**: `{"title": "Fix login crash"}` -- the last one wins

The session's current state is fully determined by the last `{"head": ...}` line. On reload, follow head -> context -> messages. No replay of the full history needed.

### Loading

On startup, the Store scans all session files and loads every message and context into the pool. Sessions are registered from their headers and head pointers. This is fast -- a year of heavy use produces manageable JSONL volumes that load in under a second.

### Writing

New messages are appended as `{"msg": ...}` lines. New contexts are appended as `{"context": ...}` + `{"head": ...}` pairs. Append-only means crash safety: partial writes only affect the last line.

## Architecture layers

```
Layer 0: Foundation (ri)
  - model: Message, Context (the data model)
    - MessageId, ContextId, SessionId, Role, ContentBlock, Usage
  - store: Pool, Store, Session, SessionHeader (persistence)
  - provider: Model, ThinkingLevel, ModelCost
    - LlmProvider trait, RequestOptions, ApiError
    - Tool trait, ToolSchema, ToolOutput, ToolContext
  - stream: StreamEvent
  - accumulator: StreamAccumulator

Layer 1: I/O (ri-ai, ri-tools)
  - ri-ai: LLM provider implementations (Anthropic, Gemini, OpenAI Codex)
    - Streaming SSE, OAuth, API keys
    - Turn: call provider + accumulate response
    - Provider registry and model catalog
  - ri-tools: Building blocks for agent applications
    - Coding tools: bash, read, write, edit
    - Prompt template system (load, parse, expand)
    - Context file discovery (AGENTS.md, settings.json)
    - System prompt construction

Layer 2: Application (ri-cli, ri-web)
  - ri-cli: Terminal agent (interactive TUI, print mode, RPC mode)
  - ri-web: Web agent (axum server, SolidJS frontend, SSE streaming)
  Both compose the same Layer 0/1 primitives into agent loops.
```

Each layer depends only on the layers above it. There is no agent loop crate -- the loop is application-level composition of Turn, Tool, and Store. Different applications compose these primitives differently.

## Crate structure

```
ri/                          # Workspace root
  ri-core/                   # Git submodule: foundation crates
    crates/
      ri/                    # Layer 0: types, pool, store, traits
      ri-ai/                 # Layer 1: LLM providers, Turn, auth
      ri-tools/              # Layer 1: coding tools, prompts, resources
  ri-cli/                    # Layer 2: terminal agent
  ri-web/                    # Layer 2: web agent
```

### ri

The foundation crate. Everything the rest of the system depends on:

- `model` -- `Message`, `Context` (the core data model), `Role`, `ContentBlock`, `Usage`, `MessageId`, `ContextId`, `SessionId`, `gen_id()`
- `store` -- `Pool`, `Store`, `Session`, `SessionHeader` (persistence and filing)
- `stream` -- `StreamEvent` (normalized incremental events from any provider)
- `accumulator` -- `StreamAccumulator` (pure state machine: `StreamEvent` -> `ContentBlock` + `Usage`)
- `provider` -- `Model`, `ModelCost`, `ThinkingLevel`, `LlmProvider` trait, `RequestOptions`, `ApiError`, `Tool` trait, `ToolSchema`, `ToolOutput`, `ToolContext`, `AuthMethod`

Does NOT handle: LLM API calls, tool execution, agent loop logic.

### ri-ai

LLM provider implementations:

- `anthropic` -- Anthropic Messages API (API key + OAuth auth, thinking config)
- `gemini` -- Google Gemini (Cloud Code Assist CLI variant, Antigravity variant, API key variant)
- `openai_codex` -- OpenAI Codex via ChatGPT Responses API (OAuth, encrypted reasoning)
- `turn` -- `Turn`: call a provider and accumulate the streamed response. The fundamental "call the LLM once" building block.
- `registry` -- Provider factories, model resolution, auth checking
- `sse` -- Shared SSE parser used by all providers
- `creds` -- Credential persistence (~/.ri/auth.json)

Does NOT handle: message storage, tool execution, agent loop.

### ri-tools

Building blocks for agent applications:

- **Coding tools**: `bash`, `read`, `write`, `edit`. Each implements the `Tool` trait from ri. `all_tools()` returns the full set.
- **Prompt templates** (`prompts`): Load `.md` templates from config directories, parse `/command arg1 arg2` invocations, expand `$1`, `$@`, `${@:N}` placeholders.
- **Context and resources** (`resources`): Discover AGENTS.md/CLAUDE.md files by walking up from the working directory, load settings from `~/.config/agents/settings.json`, build environment info for system prompts, `{{include:path}}` directive expansion.

### ri-cli

Terminal agent. Wires everything together for interactive use:

- `agent` -- The agent loop: composes `Turn`, tool execution, and `Store` into the standard "call LLM, execute tools, repeat" cycle. Returns `impl Stream<Item = AgentEvent>`.
- `interactive` -- TUI with ratatui (inline viewport, scrollback, tui-textarea input, streaming preview)
- `print_mode` -- Single-shot: run one prompt, print output, exit
- `rpc_mode` -- JSON-over-stdio for embedding in other tools
- `meta_tools` -- `runAgent`, `readSession`, `readMessage` tools for sub-agent orchestration

### ri-web

Web agent. Same primitives, different composition:

- `agent` -- Agent loop adapted for broadcast (tokio::sync::broadcast instead of stream return). Background title generation.
- `api` -- axum REST + SSE routes (session CRUD, message streaming, model listing, auth management, log streaming)
- `state` -- `AppState`, `SessionState`, `RunHandle` (multi-session server state)
- `meta_tools` -- Same three meta-tools, adapted for shared server state (Weak<AppState>)
- `tracing_broadcast` -- Live tracing log forwarding to SSE clients

## The agent loop

The agent loop is application code, not infrastructure. It lives in ri-cli and ri-web, composing foundation primitives:

```
1. User provides input -> user message written to pool + session file
2. Assemble the context (currently: the session's full message list)
3. Start a Turn with the context resolved to messages
4. Stream events from the Turn (yielded to display/broadcast)
5. Turn finishes -> extract content blocks and usage
6. Write assistant message to pool + session file
7. If response contains tool calls:
   a. Execute each tool
   b. Write tool results as a user message
   c. Go to step 2
8. If no tool calls: checkpoint as a new context, done
```

ri-cli's loop returns `impl Stream<Item = AgentEvent>` (one consumer: the TUI/print/RPC handler). ri-web's loop broadcasts through `tokio::sync::broadcast` (multiple SSE consumers). Both are application code composing the same building blocks. The divergence is intentional -- different output shapes for different consumers.

## Context algebra examples

The model makes context manipulation first-class. The current strategy is simple (include all session messages in order), but the architecture supports:

### Compacting
Track approximate token usage. When approaching the budget, summarize old messages into a new message, create a new context with the summary replacing the originals.

### Cross-session
Pull messages from another session's pool into a new context. The globally unique IDs make this work -- no special cross-session machinery needed.

### Branching
Create two contexts with the same parent but different message lists. Explore different approaches, then merge by creating a context that combines insights from both.

## Error handling

**Crash during write**: Each line is appended independently. Partial writes only affect the last line. On next load: skip malformed lines. All previous objects are intact.

**LLM call failure**: If the call fails, an error content block is written to the assistant message. The context is still checkpointed so the session state is consistent.

**Tool execution failure**: Tool errors produce `tool_result` content blocks with `is_error: true`. This is a normal message -- the LLM sees the error and reacts.

## What's NOT in this design

- **Extensions / plugins**: Not in v1. The extension point is "write Rust code in your own ri project."
- **Multi-model orchestration**: Each Turn uses one model. Multi-model workflows are multiple messages with different provenance in their context metadata.
- **Full-text search**: Grep across session files is sufficient at this scale.
- **Encryption / access control**: Not needed. Single-user, personal machine.
