# ri Store Format

## Overview

The ri store is a pool of messages and contexts persisted as JSONL files. Each file groups related objects by session. The pool is the logical model; files are the physical organization.

**Location**: `~/.ri/sessions/`

**Files**: `<YYYY-MM-DD>_<HHMMSS>_<name>.jsonl`

**Properties**:
- Append-only: lines are only added, never modified or deleted
- Self-describing: each line is a JSON object, parseable independently
- Globally unique IDs: message and context IDs are unique across all files
- Deterministic recovery: the session's current state is the last `{"head": ...}` line

## File structure

Each session file has five kinds of lines, distinguished by their JSON keys:

### 1. Session header (first line)

Session-level metadata. Has a `session` key but no `msg` or `context` key.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session` | `string` | yes | Human-readable session name |
| `ts` | `string` | yes | ISO 8601 creation timestamp |
| `cwd` | `string` | no | Working directory for this session |
| `parent` | `string` | no | File-stem ID of the parent session (sub-agent spawning) |

```json
{"session":"fix-login-crash","ts":"2026-02-09T08:00:00Z","cwd":"/Users/john/Projects/myapp"}
```

### 2. Message lines

Each message is a content blob. Uses `msg` as the ID key (not `id`) to distinguish from other line types.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `msg` | `string` | yes | Globally unique message ID |
| `role` | `"system" \| "user" \| "assistant"` | yes | Message role |
| `content` | `ContentBlock[]` | yes | Array of typed content blocks |
| `meta` | `object` | no | Application-defined metadata |

```json
{"msg":"msg_2602_4a1b3c5d7e9f","role":"user","content":[{"type":"text","text":"Fix the login crash."}]}
```

### 3. Context lines

A context is an immutable object with an ordered message list and its position in the history DAG. Uses `context` as the ID key.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `context` | `string` | yes | Globally unique context ID |
| `messages` | `string[]` | yes | Ordered list of message IDs (what the LLM sees) |
| `parents` | `string[]` | no | Parent context IDs (the DAG edges) |
| `meta` | `object` | no | Model info, usage, timestamps, etc. |

```json
{"context":"ctx_2602_2b4d6f8a0c1e","messages":["msg_2602_4a1b3c5d7e9f","msg_2602_5b2c4d6e8fa0","msg_2602_6c3d5e7fa0b1"],"parents":["ctx_2602_1a3b5c7d9e0f"],"meta":{"model":"claude-sonnet-4-20250514","ts":"2026-02-09T08:00:15Z","usage":{"input_tokens":534,"output_tokens":156}}}
```

**Backward compatibility**: older files use `{"step": ..., "context": [...]}` instead of `{"context": ..., "messages": [...]}`. The loader accepts both formats.

### 4. Head updates

A head line moves the session's pointer to a new context. The last `{"head": ...}` line in the file wins.

| Field | Type | Description |
|-------|------|-------------|
| `head` | `string` | Context ID this session now points to |

```json
{"head":"ctx_2602_2b4d6f8a0c1e"}
```

### 5. Title updates

A title line updates the session's display name. Written by background title generation so names can evolve without rewriting the header.

| Field | Type | Description |
|-------|------|-------------|
| `title` | `string` | New session title |

```json
{"title":"Fix login null pointer crash"}
```

## Content blocks

The `content` array on a message contains typed content blocks. Each has a `type` field.

| Block type | Required fields | Description |
|------------|----------------|-------------|
| `text` | `text: string` | Plain text content |
| `thinking` | `thinking: string` | LLM reasoning content. Optional `sig` for provider replay. |
| `image` | `mediaType: string, data: string` | Base64-encoded image |
| `tool_use` | `id: string, name: string, input: object` | Tool call request from the LLM |
| `tool_result` | `toolUseId: string, content: ContentBlock[], is_error: bool` | Result of executing a tool. Optional `details` for UI rendering. |
| `error` | `message: string` | Error content (API failures, stream errors) |

Content blocks may carry additional fields. Unknown block types are preserved via an `Unknown` catch-all variant.

## Metadata

The `meta` field on messages and steps is an open object for application-defined data. The store preserves it on round-trip but does not interpret it.

For ri (the coding agent), common metadata on assistant messages:

| Field | Type | Description |
|-------|------|-------------|
| `model` | `string` | Model ID used for this turn |
| `ts` | `string` | ISO 8601 timestamp of the LLM call |
| `usage` | `Usage` | Token counts (input, output, cache read, cache write) |
| `thinking` | `string` | Thinking level used |

Step metadata mirrors this when meaningful (model, usage, timestamps).

## Message IDs

Messages and contexts live in a global pool. IDs must be globally unique so any context can reference any message across sessions. Generated IDs carry a type prefix so they're distinguishable at a glance:

- `msg_<YYMM>_<12 hex>` -- e.g. `msg_2604_b31b6d48c79e`
- `ctx_<YYMM>_<12 hex>` -- e.g. `ctx_2604_ef8acc68fb72`

The `YYMM` block is a two-digit year + month stamp for at-a-glance temporal context; the 12 hex characters (48 bits of randomness) are collision-safe well past 200k objects per month. Session IDs remain `YYYY-MM-DD_HHMMSS_<slug>` -- they're more like git branch names and double as filenames.

The prefix is a convention applied at generation time. IDs are opaque strings everywhere else, so older unprefixed IDs (pre-prefix sessions) resolve normally alongside new ones.

## Complete example

A coding agent session: user asks to fix a bug, agent reads a file, makes an edit.

```jsonl
{"session":"fix-login-crash","ts":"2026-02-09T08:00:00Z","cwd":"/Users/john/Projects/myapp"}
{"context":"ctx_2602_1a3b5c7d9e0f","messages":[],"parents":[],"meta":null}
{"head":"ctx_2602_1a3b5c7d9e0f"}
{"msg":"msg_2602_4a1b3c5d7e9f","role":"system","content":[{"type":"text","text":"You are ri, a coding agent."}]}
{"context":"ctx_2602_2b4d6f8a0c1e","messages":["msg_2602_4a1b3c5d7e9f"],"parents":["ctx_2602_1a3b5c7d9e0f"]}
{"head":"ctx_2602_2b4d6f8a0c1e"}
{"msg":"msg_2602_5b2c4d6e8fa0","role":"user","content":[{"type":"text","text":"There's a crash in the login handler. Fix it."}]}
{"msg":"msg_2602_6c3d5e7fa0b1","role":"assistant","content":[{"type":"text","text":"I'll look at the login handler."},{"type":"tool_use","id":"tc_1","name":"read","input":{"path":"src/handlers/login.rs"}}],"meta":{"model":"claude-sonnet-4-20250514","ts":"2026-02-09T08:00:05Z","usage":{"input_tokens":245,"output_tokens":89}}}
{"msg":"msg_2602_7d4e6f80b1c2","role":"user","content":[{"type":"tool_result","toolUseId":"tc_1","content":[{"type":"text","text":"pub fn handle_login(req: Request) -> Response { ... }"}],"is_error":false}]}
{"msg":"msg_2602_8e5f70a1c2d3","role":"assistant","content":[{"type":"text","text":"Fixed. The crash was caused by an unhandled None from db.find_user."}],"meta":{"model":"claude-sonnet-4-20250514","ts":"2026-02-09T08:00:15Z","usage":{"input_tokens":534,"output_tokens":67}}}
{"context":"ctx_2602_3c5e7f9b1d2f","messages":["msg_2602_4a1b3c5d7e9f","msg_2602_5b2c4d6e8fa0","msg_2602_6c3d5e7fa0b1","msg_2602_7d4e6f80b1c2","msg_2602_8e5f70a1c2d3"],"parents":["ctx_2602_2b4d6f8a0c1e"],"meta":{"model":"claude-sonnet-4-20250514","ts":"2026-02-09T08:00:15Z"}}
{"head":"ctx_2602_3c5e7f9b1d2f"}
{"title":"Fix login null pointer crash"}
```

### Reading this file

The session's current state is always the last `{"head": ...}` line: `ctx_2602_3c5e7f9b1d2f`. Look up context `ctx_2602_3c5e7f9b1d2f`, which has messages `["msg_2602_4a1b3c5d7e9f", "msg_2602_5b2c4d6e8fa0", "msg_2602_6c3d5e7fa0b1", "msg_2602_7d4e6f80b1c2", "msg_2602_8e5f70a1c2d3"]`. That's the full conversation. No chain-walking needed.

### Scrutability

```bash
# List all sessions
ls ~/.ri/sessions/

# See all lines in a session
cat ~/.ri/sessions/2026-02-09_080000_fix-login-crash.jsonl

# See only messages
grep '"msg"' ~/.ri/sessions/2026-02-09_080000_fix-login-crash.jsonl

# See only contexts
grep '"context"' ~/.ri/sessions/2026-02-09_080000_fix-login-crash.jsonl

# Current head
grep '"head"' ~/.ri/sessions/2026-02-09_080000_fix-login-crash.jsonl | tail -1

# Session name (last title or header)
grep '"title"' ~/.ri/sessions/2026-02-09_080000_fix-login-crash.jsonl | tail -1
```

## In-memory representation

```rust
// -- model.rs: the core data model --

struct Message {
    pub id: MessageId,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub meta: Option<serde_json::Value>,
}

struct Context {
    pub id: ContextId,
    pub messages: Vec<MessageId>,
    pub parents: Vec<ContextId>,
    pub meta: Option<serde_json::Value>,
}

// -- store.rs: persistence --

struct Pool {
    messages: HashMap<MessageId, Message>,
    contexts: HashMap<ContextId, Context>,
}

struct Session {
    pub name: String,
    pub file_id: SessionId,
    pub head: ContextId,
    pub cwd: Option<String>,
    pub parent: Option<SessionId>,
    pub ts: String,
}
```

### Store operations

```rust
impl Store {
    // Load all .jsonl session files into the pool and session map.
    fn load_all(&mut self) -> Result<()>;

    // Create a new session file with header, root context, and head pointer.
    fn create_session(&mut self, name: &str, cwd: &str, parent: Option<&SessionId>) -> Result<SessionId>;

    // Write a message to a session file and add it to the pool.
    fn write_message(&mut self, session_id: &SessionId, role: Role, content: Vec<ContentBlock>, meta: Option<Value>) -> Result<Message>;

    // Create a new context from the current message list and update the session's head.
    fn checkpoint(&mut self, session_id: &SessionId, message_ids: &[MessageId], meta: Option<Value>) -> Result<Context>;

    // Get the current context for a session (from its head).
    fn head_context(&self, session_id: &str) -> Option<&Context>;

    // Persist a generated title to the session file.
    fn write_title(&mut self, session_id: &SessionId, title: &str) -> Result<()>;
}
```

## Design decisions and rationale

### Why messages and contexts are the primitives

Messages are content. Contexts are selections of content. Together they're the two things you need to call an LLM: `f(Context) -> Message`. Everything else -- steps, sessions, the DAG -- is built on top.

By keeping them separate:

- **Messages are reusable**: The same message can appear in multiple contexts. A summary message works in any context that needs it.
- **Contexts are composable**: Pull messages from anywhere -- different sessions, different agents, different time periods.

### Why contexts combine messages and history

A context is a single immutable object: a message list, parent links, and metadata. There's no separate "tree" type that gets embedded -- the context IS the thing. This keeps the model to two types (Message and Context) plus a pointer (Session).

- **History is explicit**: The context DAG shows exactly how the conversation evolved.
- **Checkpointing is cheap**: Creating a context is just listing message IDs + parents. The messages themselves aren't copied.
- **Branching is natural**: Two contexts can share the same parent but diverge in their message lists.

### Why per-session files

One big file with all objects has scrutability problems:
- "Find all messages in a session" requires understanding the DAG
- The file grows indefinitely
- No natural way to archive old sessions

Per-session files give:
- `ls sessions/` shows all sessions at a glance
- `cat session.jsonl` shows one complete session
- Files can be individually archived, deleted, or shared

The tradeoff: cross-session references require loading multiple files. The global pool handles this by loading all files at startup.

### Why globally unique IDs

File-local IDs prevent cross-file references. Since the pool enables composition across sessions, IDs must be globally unique. The type-prefixed format (`msg_2604_b31b6d48c79e`, `ctx_2604_ef8acc68fb72`) keeps them distinguishable at a glance while staying unique.

### Why append-only files

- **Crash safety**: partial writes only affect the last line
- **No corruption**: existing objects are never modified
- **Simplicity**: no seeking, no in-place updates, no locking

### Why the last-head-wins pattern

Head and title lines can appear multiple times. The last one wins. This lets the session advance without rewriting previous lines, and title generation can update the name asynchronously.

## Crash recovery

On load, each file is read line by line:
1. First line: parse as session header
2. Subsequent lines: dispatch by key (`msg`, `step`, `head`, `title`)
3. Malformed lines are skipped with a warning (should only happen on the last line from an interrupted write)

All successfully parsed objects enter the pool. The file is otherwise unmodified.

## Forward compatibility

- Unknown fields on any line type are preserved (serde round-trip)
- Unknown content block types are preserved via an `Unknown` catch-all variant
- New optional fields can be added without breaking older readers
- New line types can be added -- older readers skip lines with unrecognized keys
