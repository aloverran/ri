# ri Store Format

## Overview

The ri store is a pool of messages persisted as JSONL files. Each file groups related messages (typically one file per "session"). The pool is the logical model; files are the physical organization.

**Location**: `~/.ri/sessions/`

**Files**: `<YYYY-MM-DD>_<HHMMSS>_<name>.jsonl`

**Properties**:
- Append-only: lines are only added, never modified or deleted
- Forward-referencing within a file: messages are defined before referenced by provenance in the same file
- Self-describing: each line is a JSON object, parseable independently
- Globally unique IDs: a message ID is unique across all files, enabling cross-file references

## File structure

Each session file has two kinds of lines:

1. **File header** (first line): session-level metadata
2. **Messages** (all subsequent lines): the actual content

### File header

The first line of each file is a metadata object. It has NO `id` or `role` field, which distinguishes it from messages.

**Fields**:
| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session` | `string` | yes | Human-readable session name |
| `ts` | `string` | yes | ISO 8601 creation timestamp |
| `cwd` | `string` | no | Working directory associated with this session |

**Example**:
```json
{"session":"fix-login-crash","ts":"2026-02-09T08:00:00Z","cwd":"/Users/john/Projects/myapp"}
```

### Messages

Every subsequent line is a message -- the single entity type in the pool.

**Required fields**:
| Field | Type | Description |
|-------|------|-------------|
| `id` | `string` | Globally unique identifier |
| `role` | `"system" \| "user" \| "assistant"` | Message role per LLM API convention |
| `content` | `ContentBlock[]` | Array of content blocks |

**Optional fields**:
| Field | Type | Description |
|-------|------|-------------|
| `provenance` | `Provenance` | Present on messages produced by an LLM call (see below) |
| `meta` | `object` | Application-defined metadata (see below) |

Messages WITHOUT provenance are "authored" -- written by humans, tools, or code.
Messages WITH provenance are "derived" -- produced by an LLM API call.

## Provenance

When a message was produced by an LLM call, it carries a `provenance` object recording the call details.

**Provenance fields**:
| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `input` | `string[]` | yes | Ordered array of message IDs that were sent as the LLM call's input |
| `model` | `string` | yes | Model identifier (e.g., `"claude-sonnet-4-20250514"`) |
| `ts` | `string` | yes | ISO 8601 timestamp of when the call completed |
| `usage` | `Usage` | no | Token usage (see below). Omitted when provider doesn't report it. |

These fields are the stable core of provenance -- they capture what went in, what produced it, when, and optionally the cost in tokens. They are unlikely to change.

**Usage fields**:
| Field | Type | Description |
|-------|------|-------------|
| `input_tokens` | `number` | Input tokens consumed |
| `output_tokens` | `number` | Output tokens generated |
| `cache_read_tokens` | `number` | Tokens read from cache (0 if not applicable) |
| `cache_write_tokens` | `number` | Tokens written to cache (0 if not applicable) |

## Metadata

The `meta` field on a message is an open object for application-defined data. The store preserves it on round-trip but does not interpret it. Applications use it for domain-specific information that may evolve over time.

For ri (the coding agent), common metadata fields include:

| Field | Type | Description |
|-------|------|-------------|
| `ts` | `string` | ISO 8601 timestamp of when the message was created |
| `provider` | `string` | Provider name (e.g., `"anthropic"`) |
| `duration_ms` | `number` | Wall-clock duration of the LLM call |
| `cost` | `number` | Estimated cost in USD |
| `thinking` | `string` | Thinking level used (e.g., `"medium"`) |

Other applications may store different metadata entirely. The format does not constrain this.

## Content blocks

The `content` array on a message contains typed content blocks. Each has a `type` field.

| Block type | Required fields | Description |
|------------|----------------|-------------|
| `text` | `text: string` | Plain text content |
| `image` | `mediaType: string, data: string` | Base64-encoded image |
| `tool_use` | `id: string, name: string, input: object` | Tool call request from the LLM |
| `tool_result` | `toolUseId: string, content: ContentBlock[], is_error: bool` | Result of executing a tool call |
| `thinking` | `thinking: string` | LLM reasoning/thinking content |

Content blocks may carry additional fields (e.g., `text_signature`, `thinking_signature` for provider-specific features). Unknown fields should be preserved on round-trip.

## Message IDs

IDs must be globally unique across all files. The recommended format is a short prefix (indicating the session) plus a sequential or random suffix:

- `fx_m1`, `fx_m2`, `fx_m3` -- prefixed with session shorthand
- `a3f7b2c1` -- random 8-character hex

The choice of ID format is up to the implementation. The only requirement is global uniqueness. IDs are opaque strings -- the system never parses them.

When a message's provenance references messages from other session files (cross-session composition), those IDs are resolved from the global pool at load time.

## Complete example

A coding agent session: user asks to fix a bug, agent reads a file, makes an edit, reports the fix.

```jsonl
{"session":"fix-login-crash","ts":"2026-02-09T08:00:00Z","cwd":"/Users/john/Projects/myapp"}
{"id":"fx_s1","role":"system","content":[{"type":"text","text":"You are ri, a coding agent. You have tools for reading and editing code."}]}
{"id":"fx_u1","role":"user","content":[{"type":"text","text":"There's a crash in the login handler. Fix it."}]}
{"id":"fx_a1","role":"assistant","content":[{"type":"text","text":"I'll look at the login handler."},{"type":"tool_use","id":"tc_1","name":"read","input":{"path":"src/handlers/login.rs"}}],"provenance":{"input":["fx_s1","fx_u1"],"model":"claude-sonnet-4-20250514","ts":"2026-02-09T08:00:05Z","usage":{"input_tokens":245,"output_tokens":89,"cache_read_tokens":0,"cache_write_tokens":0}},"meta":{"provider":"anthropic","duration_ms":2100}}
{"id":"fx_tr1","role":"user","content":[{"type":"tool_result","toolUseId":"tc_1","content":[{"type":"text","text":"     1\tpub fn handle_login(req: Request) -> Response {\n     2\t    let user = db.find_user(req.username);\n     3\t    if user.verify(req.password) {\n     4\t        session.create(user.id)\n     5\t    }\n     6\t}"}],"is_error":false}]}
{"id":"fx_a2","role":"assistant","content":[{"type":"text","text":"The function doesn't handle the case where the user is not found. `db.find_user` could return None. Let me fix it."},{"type":"tool_use","id":"tc_2","name":"edit","input":{"path":"src/handlers/login.rs","old_text":"let user = db.find_user(req.username);\n    if user.verify(req.password) {","new_text":"let user = match db.find_user(req.username) {\n        Some(u) => u,\n        None => return Response::not_found(),\n    };\n    if user.verify(req.password) {"}}],"provenance":{"input":["fx_s1","fx_u1","fx_a1","fx_tr1"],"model":"claude-sonnet-4-20250514","ts":"2026-02-09T08:00:15Z","usage":{"input_tokens":534,"output_tokens":156,"cache_read_tokens":0,"cache_write_tokens":0}},"meta":{"provider":"anthropic","duration_ms":3400}}
{"id":"fx_tr2","role":"user","content":[{"type":"tool_result","toolUseId":"tc_2","content":[{"type":"text","text":"Edited src/handlers/login.rs"}],"is_error":false}]}
{"id":"fx_a3","role":"assistant","content":[{"type":"text","text":"Fixed. The crash was caused by `db.find_user` returning None when the username doesn't exist. The function now returns a 404 response in that case."}],"provenance":{"input":["fx_s1","fx_u1","fx_a1","fx_tr1","fx_a2","fx_tr2"],"model":"claude-sonnet-4-20250514","ts":"2026-02-09T08:00:25Z","usage":{"input_tokens":823,"output_tokens":67,"cache_read_tokens":0,"cache_write_tokens":0}},"meta":{"provider":"anthropic","duration_ms":1800}}
```

### Reading this file

Top-to-bottom, you can follow the conversation:
1. System prompt (`fx_s1`): sets up the agent
2. User message (`fx_u1`): asks to fix a crash
3. Assistant response (`fx_a1`): decides to read the file. Provenance shows it saw [fx_s1, fx_u1].
4. Tool result (`fx_tr1`): the file contents
5. Assistant response (`fx_a2`): identifies the bug, makes an edit. Provenance shows it saw [fx_s1, fx_u1, fx_a1, fx_tr1].
6. Tool result (`fx_tr2`): edit confirmation
7. Assistant response (`fx_a3`): summarizes the fix. Provenance shows it saw all previous messages.

### Scrutability

```bash
# List all sessions
ls ~/.ri/sessions/

# See all messages in a session
cat ~/.ri/sessions/2026-02-09_080000_fix-login-crash.jsonl

# See only LLM-derived messages (the "turns")
grep '"provenance"' ~/.ri/sessions/2026-02-09_080000_fix-login-crash.jsonl

# See only user messages
grep '"role":"user"' ~/.ri/sessions/2026-02-09_080000_fix-login-crash.jsonl

# See what model was used
grep '"model"' ~/.ri/sessions/2026-02-09_080000_fix-login-crash.jsonl

# Session metadata
head -1 ~/.ri/sessions/2026-02-09_080000_fix-login-crash.jsonl
```

## Cross-session composition

A review session references a message from the fix session:

```jsonl
{"session":"review-login-fix","ts":"2026-02-09T09:00:00Z","cwd":"/Users/john/Projects/myapp"}
{"id":"rv_s1","role":"system","content":[{"type":"text","text":"You are a thorough code reviewer."}]}
{"id":"rv_u1","role":"user","content":[{"type":"text","text":"Review this fix and its rationale:"}]}
{"id":"rv_a1","role":"assistant","content":[{"type":"text","text":"The fix correctly handles the None case, but you should also consider..."}],"provenance":{"input":["rv_s1","rv_u1","fx_a2","fx_a3"],"model":"claude-sonnet-4-20250514","ts":"2026-02-09T09:00:10Z","usage":{"input_tokens":612,"output_tokens":98,"cache_read_tokens":0,"cache_write_tokens":0}}}
```

Message `rv_a1`'s provenance shows it was produced from inputs that include `fx_a2` and `fx_a3` -- messages from the `fix-login-crash` session. The system resolves these IDs from the global pool (populated by loading both session files).

The review session file is readable on its own (you can see what was sent and received), but to see the actual content of `fx_a2` and `fx_a3`, you'd load the referenced session file.

## Context manipulation

Compaction (summarizing old messages) produces a new message via an LLM call:

```jsonl
{"id":"fx_sum1","role":"system","content":[{"type":"text","text":"Previous context: We fixed a crash in src/handlers/login.rs where db.find_user could return None. Added a match statement returning 404 for unknown users."}],"provenance":{"input":["fx_s1","fx_u1","fx_a1","fx_tr1","fx_a2","fx_tr2","fx_a3"],"model":"claude-sonnet-4-20250514","ts":"2026-02-09T10:00:00Z","usage":{"input_tokens":823,"output_tokens":45,"cache_read_tokens":0,"cache_write_tokens":0}},"meta":{"purpose":"compaction"}}
```

This summary message (`fx_sum1`) was produced by asking the LLM to summarize the conversation. Its provenance records what it was derived from. Future LLM calls can use `fx_sum1` instead of the seven messages it summarizes, saving context space. The original messages remain in the pool.

Note: there is nothing special about compaction at the format level. It's just another derived message. The strategy decides when to summarize and what to include. The pool records the result.

## Design decisions and rationale

### Why one entity type (not separate messages + turns)

A "turn" (LLM call) produces a message. The turn metadata (what inputs, which model, when) is provenance on that message. Separating turns and messages creates two entity types that are really one: a message that may or may not have been LLM-derived. Merging them into one type with optional provenance is simpler and more general.

This also means every object in the pool composes uniformly. You can use any message as input to any LLM call, regardless of whether it was authored or derived. No type distinction needed.

### Why per-session files (not one big file)

One big file with all messages has scrutability problems:
- "Find all messages in a session" requires walking provenance chains, not simple grep
- The file grows indefinitely, eventually becoming unwieldy
- No natural way to archive or delete old sessions

Per-session files give:
- `ls sessions/` shows all sessions at a glance
- `cat session.jsonl` shows one complete session
- `grep` works within a file to find turns, user messages, etc.
- Files can be individually archived, deleted, or shared

The tradeoff: cross-session references require loading multiple files. This is acceptable -- the global pool loads all files at startup (fast for the expected data volumes).

### Why globally unique IDs (not file-local)

File-local IDs (m1, m2, ...) are more readable but prevent cross-file references. Since the pool model enables composition across sessions, IDs must be globally unique.

Short prefixed IDs (like `fx_m1`, `rv_a1`) offer a compromise: globally unique, indicate which session they originate from, and remain human-readable. The prefix is not parsed by the system -- it's a naming convention for human benefit.

### Why append-only files

Append-only gives:
- **Crash safety**: partial writes only affect the last line. All previous messages are intact.
- **No corruption**: existing messages are never modified.
- **Simplicity**: no seeking, no in-place updates, no locking.

The cost: "deleted" data stays in the file. For a personal tool, this is negligible. A rewrite/compaction tool can be built later if needed.

### Why provenance stores the full input list (not deltas)

Provenance records the exact message IDs sent to the LLM, in order. This is the complete record of what the LLM saw. An alternative (storing only "new messages since last turn") would be more compact but:

- Can't express context manipulation (compaction removes messages, reordering changes order)
- Requires chain-walking to reconstruct any single call's input
- Loses the key insight: each LLM call's input is CONSTRUCTED, not accumulated

The full input list makes each derived message self-describing: "I was produced from exactly these inputs." No reconstruction needed.

### Why the file header is not a message

The session metadata (name, cwd, timestamp) is file-level, not pool-level. It describes the file, not a message. Making it a separate non-message line keeps the pool clean: everything in the pool is a message. The header is filing metadata.

## In-memory representation

```rust
struct MessagePool {
    messages: HashMap<String, Message>,
}

struct Message {
    pub id: String,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub provenance: Option<Provenance>,
    pub meta: Option<serde_json::Value>,  // application-defined, open object
}

struct Provenance {
    pub input: Vec<String>,
    pub model: String,
    pub ts: String,
    pub usage: Option<Usage>,
}

struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}
```

### Core pool operations

```rust
impl MessagePool {
    // Add a message to the pool (does not write to disk -- filing layer handles that)
    fn put(&mut self, msg: Message);

    // Retrieve a message by ID
    fn get(&self, id: &str) -> Option<&Message>;

    // All messages whose provenance.input contains the given ID
    fn derived_from(&self, id: &str) -> Vec<&Message>;

    // Walk provenance.input recursively to find all ancestor messages
    fn ancestors(&self, id: &str) -> Vec<&Message>;

    // All derived messages (messages with provenance)
    fn derived(&self) -> Vec<&Message>;

    // All authored messages (messages without provenance)
    fn authored(&self) -> Vec<&Message>;
}
```

### Filing layer operations

```rust
struct SessionStore {
    pool: MessagePool,
    sessions_dir: PathBuf,
    active_session: Option<ActiveSession>,
}

struct ActiveSession {
    file: File,             // open for append
    path: PathBuf,
    name: String,
}

impl SessionStore {
    // Load all session files into the pool
    fn load_all(&mut self) -> Result<()>;

    // Create a new session file and set it as active
    fn new_session(&mut self, name: &str, cwd: &str) -> Result<()>;

    // Write a message to the pool AND append to the active session file
    fn write_message(&mut self, msg: Message) -> Result<String>;

    // List all sessions (from filenames + headers)
    fn list_sessions(&self) -> Result<Vec<SessionInfo>>;
}
```

## Crash recovery

On load, each file is read line by line:
1. First line: parse as session header. If malformed, skip (use filename for metadata).
2. Subsequent lines: parse as messages. If a line is malformed JSON, skip it and log a warning. This should only happen for the very last line (interrupted write).

All successfully parsed messages enter the pool. The file is otherwise unmodified.

## Forward compatibility

- Unknown fields on messages should be preserved (round-tripped through serialization) but can be ignored by code that doesn't understand them.
- Unknown content block types should be preserved but can be treated as opaque.
- New optional fields can be added to messages or provenance without breaking older readers.
- If a breaking format change is ever needed: use a different directory name or a version field in the file header. But the format is simple enough that this should rarely (if ever) be necessary.
