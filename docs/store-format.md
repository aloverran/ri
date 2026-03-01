# ri Store Format

## Overview

The ri store is a pool of messages and steps persisted as JSONL files. Each file groups related objects by session. The pool is the logical model; files are the physical organization.

**Location**: `~/.ri/sessions/`

**Files**: `<YYYY-MM-DD>_<HHMMSS>_<name>.jsonl`

**Properties**:
- Append-only: lines are only added, never modified or deleted
- Self-describing: each line is a JSON object, parseable independently
- Globally unique IDs: message and step IDs are unique across all files
- Deterministic recovery: the session's current state is the last `{"head": ...}` line

## File structure

Each session file has five kinds of lines, distinguished by their JSON keys:

### 1. Session header (first line)

Session-level metadata. Has a `session` key but no `msg` or `step` key.

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
{"msg":"fx_m1","role":"user","content":[{"type":"text","text":"Fix the login crash."}]}
```

### 3. Step lines

A step captures a context snapshot and its position in the history DAG. Uses `step` as the ID key.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `step` | `string` | yes | Globally unique step ID |
| `context` | `string[]` | yes | Ordered list of message IDs (what the LLM sees) |
| `parents` | `string[]` | no | Parent step IDs (the DAG edges) |
| `meta` | `object` | no | Model info, usage, timestamps, etc. |

```json
{"step":"fx_s2","context":["fx_m1","fx_m2","fx_m3"],"parents":["fx_s1"],"meta":{"model":"claude-sonnet-4-20250514","ts":"2026-02-09T08:00:15Z","usage":{"input_tokens":534,"output_tokens":156}}}
```

### 4. Head updates

A head line moves the session's pointer to a new step. The last `{"head": ...}` line in the file wins.

| Field | Type | Description |
|-------|------|-------------|
| `head` | `string` | Step ID this session now points to |

```json
{"head":"fx_s2"}
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

IDs must be globally unique across all files. The implementation generates them from a session prefix plus a sequential counter:

- `fx_abc123_1`, `fx_abc123_2` -- prefixed with session slug + random suffix

The prefix is derived from the session name (first 6 alphanumeric chars) plus 6 random hex chars, making IDs human-readable while staying unique. The system never parses ID structure -- they are opaque strings.

## Complete example

A coding agent session: user asks to fix a bug, agent reads a file, makes an edit.

```jsonl
{"session":"fix-login-crash","ts":"2026-02-09T08:00:00Z","cwd":"/Users/john/Projects/myapp"}
{"step":"fx_s1","context":[],"parents":[],"meta":null}
{"head":"fx_s1"}
{"msg":"fx_m1","role":"system","content":[{"type":"text","text":"You are ri, a coding agent."}]}
{"step":"fx_s2","context":["fx_m1"],"parents":["fx_s1"]}
{"head":"fx_s2"}
{"msg":"fx_m2","role":"user","content":[{"type":"text","text":"There's a crash in the login handler. Fix it."}]}
{"msg":"fx_m3","role":"assistant","content":[{"type":"text","text":"I'll look at the login handler."},{"type":"tool_use","id":"tc_1","name":"read","input":{"path":"src/handlers/login.rs"}}],"meta":{"model":"claude-sonnet-4-20250514","ts":"2026-02-09T08:00:05Z","usage":{"input_tokens":245,"output_tokens":89}}}
{"msg":"fx_m4","role":"user","content":[{"type":"tool_result","toolUseId":"tc_1","content":[{"type":"text","text":"pub fn handle_login(req: Request) -> Response { ... }"}],"is_error":false}]}
{"msg":"fx_m5","role":"assistant","content":[{"type":"text","text":"Fixed. The crash was caused by an unhandled None from db.find_user."}],"meta":{"model":"claude-sonnet-4-20250514","ts":"2026-02-09T08:00:15Z","usage":{"input_tokens":534,"output_tokens":67}}}
{"step":"fx_s3","context":["fx_m1","fx_m2","fx_m3","fx_m4","fx_m5"],"parents":["fx_s2"],"meta":{"model":"claude-sonnet-4-20250514","ts":"2026-02-09T08:00:15Z"}}
{"head":"fx_s3"}
{"title":"Fix login null pointer crash"}
```

### Reading this file

The session's current state is always the last `{"head": ...}` line: `fx_s3`. Look up step `fx_s3`, which has context `["fx_m1", "fx_m2", "fx_m3", "fx_m4", "fx_m5"]`. That's the full conversation. No provenance chain-walking needed.

### Scrutability

```bash
# List all sessions
ls ~/.ri/sessions/

# See all lines in a session
cat ~/.ri/sessions/2026-02-09_080000_fix-login-crash.jsonl

# See only messages
grep '"msg"' ~/.ri/sessions/2026-02-09_080000_fix-login-crash.jsonl

# See only steps (context snapshots)
grep '"step"' ~/.ri/sessions/2026-02-09_080000_fix-login-crash.jsonl

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
    pub messages: Vec<MessageId>,
}

struct Step {
    pub id: StepId,
    pub context: Context,
    pub parents: Vec<StepId>,
    pub meta: Option<serde_json::Value>,
}

// -- store.rs: persistence --

struct Pool {
    messages: HashMap<MessageId, Message>,
    steps: HashMap<StepId, Step>,
}

struct Session {
    pub name: String,
    pub file_id: SessionId,
    pub head: StepId,
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

    // Create a new session file with header, root step, and head pointer.
    fn create_session(&mut self, name: &str, cwd: &str, parent: Option<&SessionId>) -> Result<SessionId>;

    // Write a message to a session file and add it to the pool.
    fn write_message(&mut self, session_id: &SessionId, role: Role, content: Vec<ContentBlock>, meta: Option<Value>) -> Result<Message>;

    // Snapshot the current context as a new step and update the session's head.
    fn checkpoint(&mut self, session_id: &SessionId, message_ids: &[MessageId], meta: Option<Value>) -> Result<Step>;

    // Get the current context for a session (from its head step).
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

### Why steps exist

A step records a context at a point in time, with parent links forming a history DAG. This gives:

- **History is explicit**: The step DAG shows exactly how the context evolved. No inference from provenance chains.
- **Checkpointing is cheap**: Writing a step is just listing message IDs. The messages themselves aren't copied.
- **Branching is natural**: Two steps can share the same parent but diverge in context.

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

File-local IDs prevent cross-file references. Since the pool enables composition across sessions, IDs must be globally unique. The prefixed format (`fx_abc123_1`) keeps them human-readable while staying unique.

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
