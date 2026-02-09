# ri Store Format

## Overview

The ri store is a single append-only JSONL file that records all LLM interactions. It is the system's single source of truth.

**Location**: `~/.ri/store.jsonl`

**Properties**:
- Append-only: lines are only ever added, never modified or deleted
- Forward-referencing: every ID referenced by an entry was defined in an earlier line
- Self-describing: each line is a complete JSON object with a `type` field
- One file: no companion files, no directory structure, no indices

## Entry types

Every line in the file is a JSON object. Every object has a `type` field. There are exactly three entry types.

### `msg` -- Message

A message is an immutable content blob. It represents one element in an LLM API's message array.

**Required fields**:
| Field | Type | Description |
|-------|------|-------------|
| `type` | `"msg"` | Entry type discriminator |
| `id` | `string` | Unique identifier (UUID v4) |
| `role` | `"system" \| "user" \| "assistant"` | Message role per LLM API convention |
| `content` | `ContentBlock[]` | Array of content blocks (see below) |

**Example -- user text message**:
```json
{"type":"msg","id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","role":"user","content":[{"type":"text","text":"fix the bug in auth.rs"}]}
```

**Example -- assistant response with tool call**:
```json
{"type":"msg","id":"7c9e6679-7425-40de-944b-e07fc1f90ae7","role":"assistant","content":[{"type":"text","text":"I'll read that file."},{"type":"tool_use","id":"tc_001","name":"read","input":{"path":"src/auth.rs"}}]}
```

**Example -- tool result**:
```json
{"type":"msg","id":"550e8400-e29b-41d4-a716-446655440000","role":"user","content":[{"type":"tool_result","toolUseId":"tc_001","content":[{"type":"text","text":"fn verify() { ... }"}],"is_error":false}]}
```

**Example -- system prompt**:
```json
{"type":"msg","id":"6ba7b810-9dad-11d1-80b4-00c04fd430c8","role":"system","content":[{"type":"text","text":"You are ri, a coding agent."}]}
```

#### Content blocks

The `content` array contains typed content blocks. Each block has a `type` field.

| Block type | Fields | Description |
|------------|--------|-------------|
| `text` | `text: string` | Plain text |
| `image` | `mediaType: string, data: string` | Base64-encoded image |
| `tool_use` | `id: string, name: string, input: object` | Tool call request from the LLM |
| `tool_result` | `toolUseId: string, content: ContentBlock[], is_error: bool` | Result of executing a tool call |
| `thinking` | `thinking: string` | LLM reasoning/thinking content |

Content blocks may carry additional provider-specific fields (e.g., `text_signature`, `thinking_signature` for Gemini). Unknown fields should be preserved on round-trip but can be ignored.

### `turn` -- LLM API call

A turn records one call to an LLM API. It is the fundamental unit of interaction.

**Required fields**:
| Field | Type | Description |
|-------|------|-------------|
| `type` | `"turn"` | Entry type discriminator |
| `id` | `string` | Unique identifier (UUID v4) |
| `parent` | `string \| null` | ID of the previous turn in this chain, or null for a root turn |
| `input` | `string[]` | Ordered array of message IDs -- the exact messages sent to the LLM |
| `output` | `string` | Message ID of the LLM's response |
| `model` | `string` | Model identifier used for this call (e.g., `"claude-sonnet-4-20250514"`) |
| `ts` | `string` | ISO 8601 timestamp of when the call completed |

**Optional fields**:
| Field | Type | Description |
|-------|------|-------------|
| `provider` | `string` | Provider name (e.g., `"anthropic"`) |
| `usage` | `object` | Token usage: `{input_tokens, output_tokens, cache_read_tokens, cache_write_tokens}` |
| `cost` | `number` | Estimated cost in USD |
| `duration_ms` | `number` | Wall-clock duration of the LLM call in milliseconds |
| `thinking` | `string` | Thinking level used (e.g., `"medium"`, `"high"`) |
| `error` | `string` | If the call failed, the error message. Output may still be present (partial response). |

**Example**:
```json
{"type":"turn","id":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","parent":null,"input":["6ba7b810-9dad-11d1-80b4-00c04fd430c8","f47ac10b-58cc-4372-a567-0e02b2c3d479"],"output":"7c9e6679-7425-40de-944b-e07fc1f90ae7","model":"claude-sonnet-4-20250514","ts":"2026-02-09T08:15:00Z","provider":"anthropic","usage":{"input_tokens":1523,"output_tokens":342},"duration_ms":2847}
```

This turn sent two messages (a system prompt and a user message) and received an assistant response. It has no parent (it's the first turn in its chain).

### `head` -- Named pointer

A head is a named pointer to a turn. It represents a "session" or "branch" in human terms.

**Required fields**:
| Field | Type | Description |
|-------|------|-------------|
| `type` | `"head"` | Entry type discriminator |
| `name` | `string` | Human-readable name for this session/branch |
| `turn` | `string` | Turn ID this head points to |
| `ts` | `string` | ISO 8601 timestamp of when the head was set |

**Optional fields**:
| Field | Type | Description |
|-------|------|-------------|
| `cwd` | `string` | Working directory associated with this session |

**Resolution rule**: When multiple head entries exist for the same `name`, the LAST one in the file is the current value. This allows append-only updates.

**Example**:
```json
{"type":"head","name":"fix-auth-bug","turn":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","ts":"2026-02-09T08:15:01Z","cwd":"/Users/john/Projects/myapp"}
```

## File ordering invariants

1. Every message ID referenced by a `turn.input` or `turn.output` MUST appear as a `msg` entry earlier in the file.
2. Every turn ID referenced by a `turn.parent` MUST appear as a `turn` entry earlier in the file.
3. Every turn ID referenced by a `head.turn` MUST appear as a `turn` entry earlier in the file.
4. Entries are ordered chronologically (appended in the order they were created).

These invariants mean the file can be read in a single forward pass. No backtracking or multi-pass resolution is needed.

## Complete example session

A coding agent session where the user asks to fix a bug, the agent reads a file, then makes an edit.

```jsonl
{"type":"msg","id":"sys-1","role":"system","content":[{"type":"text","text":"You are ri, a coding agent. You have tools for reading and editing code."}]}
{"type":"msg","id":"usr-1","role":"user","content":[{"type":"text","text":"There's a crash in the login handler. Fix it."}]}
{"type":"msg","id":"ast-1","role":"assistant","content":[{"type":"text","text":"I'll look at the login handler."},{"type":"tool_use","id":"tc-1","name":"read","input":{"path":"src/handlers/login.rs"}}]}
{"type":"turn","id":"turn-1","parent":null,"input":["sys-1","usr-1"],"output":"ast-1","model":"claude-sonnet-4-20250514","ts":"2026-02-09T08:00:00Z","usage":{"input_tokens":245,"output_tokens":89}}
{"type":"head","name":"fix-login-crash","turn":"turn-1","ts":"2026-02-09T08:00:00Z","cwd":"/Users/john/Projects/myapp"}
{"type":"msg","id":"tr-1","role":"user","content":[{"type":"tool_result","toolUseId":"tc-1","content":[{"type":"text","text":"     1\tpub fn handle_login(req: Request) -> Response {\n     2\t    let user = db.find_user(req.username);\n     3\t    if user.verify(req.password) {\n     4\t        session.create(user.id)\n     5\t    }\n     6\t}"}],"is_error":false}]}
{"type":"msg","id":"ast-2","role":"assistant","content":[{"type":"text","text":"The function doesn't handle the case where the user is not found. `db.find_user` could return None, causing a crash. Let me fix it."},{"type":"tool_use","id":"tc-2","name":"edit","input":{"path":"src/handlers/login.rs","old_text":"let user = db.find_user(req.username);\n    if user.verify(req.password) {","new_text":"let user = match db.find_user(req.username) {\n        Some(u) => u,\n        None => return Response::not_found(),\n    };\n    if user.verify(req.password) {"}}]}
{"type":"turn","id":"turn-2","parent":"turn-1","input":["sys-1","usr-1","ast-1","tr-1"],"output":"ast-2","model":"claude-sonnet-4-20250514","ts":"2026-02-09T08:00:15Z","usage":{"input_tokens":534,"output_tokens":156}}
{"type":"head","name":"fix-login-crash","turn":"turn-2","ts":"2026-02-09T08:00:15Z"}
{"type":"msg","id":"tr-2","role":"user","content":[{"type":"tool_result","toolUseId":"tc-2","content":[{"type":"text","text":"Edited src/handlers/login.rs"}],"is_error":false}]}
{"type":"msg","id":"ast-3","role":"assistant","content":[{"type":"text","text":"Fixed. The crash was caused by `db.find_user` returning None when the username doesn't exist. The function now returns a 404 response in that case."}]}
{"type":"turn","id":"turn-3","parent":"turn-2","input":["sys-1","usr-1","ast-1","tr-1","ast-2","tr-2"],"output":"ast-3","model":"claude-sonnet-4-20250514","ts":"2026-02-09T08:00:25Z","usage":{"input_tokens":823,"output_tokens":67}}
{"type":"head","name":"fix-login-crash","turn":"turn-3","ts":"2026-02-09T08:00:25Z"}
```

### Reading this session

To reconstruct the session `fix-login-crash`:

1. Find the latest `head` entry with `name == "fix-login-crash"` -> points to `turn-3`
2. Walk the parent chain: `turn-3` -> `turn-2` -> `turn-1` -> null
3. For each turn, resolve `input` and `output` message IDs
4. The conversation is the sequence of output messages: `ast-1`, `ast-2`, `ast-3`
5. To see what the LLM saw on any specific turn, resolve that turn's `input` array

### Cross-session example

A new session that continues from the fix:

```jsonl
{"type":"msg","id":"usr-2","role":"user","content":[{"type":"text","text":"Write a test for the login handler fix."}]}
{"type":"msg","id":"ast-4","role":"assistant","content":[{"type":"text","text":"Here's a test for the None case..."},{"type":"tool_use","id":"tc-3","name":"write","input":{"path":"tests/login_test.rs","content":"..."}}]}
{"type":"turn","id":"turn-4","parent":"turn-3","input":["sys-1","usr-1","ast-1","tr-1","ast-2","tr-2","ast-3","usr-2"],"output":"ast-4","model":"claude-sonnet-4-20250514","ts":"2026-02-09T09:00:00Z"}
{"type":"head","name":"test-login-fix","turn":"turn-4","ts":"2026-02-09T09:00:00Z","cwd":"/Users/john/Projects/myapp"}
```

Turn `turn-4` belongs to session `test-login-fix` but its parent is `turn-3` from session `fix-login-crash`. The sessions share ancestry. No data was copied.

### Context manipulation example

Later, the context gets long. The strategy compacts old messages:

```jsonl
{"type":"msg","id":"summary-1","role":"system","content":[{"type":"text","text":"Previous context: We fixed a crash in src/handlers/login.rs where db.find_user could return None. The fix adds a match statement that returns 404 for unknown users. A test was written in tests/login_test.rs."}]}
{"type":"msg","id":"usr-3","role":"user","content":[{"type":"text","text":"Now add rate limiting to the login handler."}]}
{"type":"msg","id":"ast-5","role":"assistant","content":[{"type":"text","text":"I'll add rate limiting..."}]}
{"type":"turn","id":"turn-5","parent":"turn-4","input":["sys-1","summary-1","usr-3"],"output":"ast-5","model":"claude-sonnet-4-20250514","ts":"2026-02-09T10:00:00Z"}
{"type":"head","name":"test-login-fix","turn":"turn-5","ts":"2026-02-09T10:00:00Z"}
```

Turn `turn-5`'s input is `[sys-1, summary-1, usr-3]` -- the system prompt, a summary of the previous conversation, and the new request. All the intermediate messages (usr-1, ast-1, tr-1, ast-2, tr-2, ast-3, usr-2, ast-4) were replaced by `summary-1`. The store doesn't know "compaction" happened. It just sees messages and a turn.

## Design decisions and rationale

### Why messages are separate entries (not inlined in turns)

A turn's input is the full `Vec<Message>` sent to the LLM. In a 100-turn conversation, turn 100's input contains ~100 messages. If we inlined messages in turns, the file would grow quadratically: `sum(1..N) * avg_msg_size`. For a 100-turn session with 1KB average messages, that's ~5MB of duplicated data per session.

By storing messages as separate entries and referencing them by ID, each message is stored exactly once. Turn entries are small (just lists of IDs). The file grows linearly.

The tradeoff: turns are not self-contained (you need to look up message IDs to see the content). But the file is still scrutable -- messages are defined before they're referenced, so reading top-to-bottom shows you everything in order.

### Why one file (not per-session files)

Per-session files are simpler for basic use but make cross-session references impossible without cross-file ID resolution. A single file makes all IDs global and all references local.

The size concern is manageable: ~64MB/year for heavy use. Loading is fast (single sequential read). If size becomes a problem after years of use, the file can be archived and a new one started.

### Why append-only (not mutable)

Append-only gives us:
- **Crash safety**: a partial write only affects the last line. All previous entries are intact. Recovery: truncate the last incomplete line.
- **No corruption**: existing data is never modified, so it can't be corrupted by bugs.
- **Full history**: head updates are logged, so you can see how sessions evolved.
- **Simplicity**: no file seeking, no in-place updates, no locking.

The cost: "deleted" data (orphaned messages, old head entries) stays in the file. This is acceptable -- the overhead is negligible, and a compaction/rewrite tool can be built later if needed.

### Why heads are in the same file (not a separate file)

We considered a separate `refs.json` file for heads. But:
- Two files means synchronization concerns (what if one is written and not the other?)
- Separate files break the "one file" simplicity
- Head entries are tiny (~100 bytes each) and infrequent

Having heads in the same JSONL file means a head update is atomic with the turns it follows (both are just appended lines). No sync issues.

### Why UUIDs (not content hashes)

git uses content-addressable storage (SHA hashes as IDs). We use random UUIDs because:
- We don't need deduplication (messages are rarely identical)
- We don't need integrity checking (single-user, local file)
- UUIDs are simpler (no hashing step)
- UUIDs avoid the complexity of handling hash collisions

### Why the turn stores both input and output (not just a delta from parent)

A turn could store "input = parent's input + these new messages." This would be more compact but:
- Requires walking the full chain to reconstruct any turn's input
- Breaks self-description (a turn alone doesn't tell you what was sent)
- Makes context manipulation invisible (if the input was transformed, the delta model can't express "I removed messages 3-7 and added a summary")

By storing the complete input list (as message IDs), each turn explicitly records exactly what the LLM saw. Transformations (compaction, stripping) are visible: the input list differs from what you'd expect by naive accumulation.

## In-memory representation

On load, the file is scanned into three maps:

```rust
struct Store {
    messages: HashMap<String, Message>,
    turns: HashMap<String, Turn>,
    heads: HashMap<String, HeadEntry>,
}
```

Where `HeadEntry` contains the turn ID and metadata from the latest `head` entry for each name.

### Core operations

```rust
impl Store {
    // Write operations (append to file + update in-memory maps)
    fn put_message(&mut self, msg: Message) -> String;           // returns msg ID
    fn put_turn(&mut self, turn: Turn) -> String;                // returns turn ID
    fn set_head(&mut self, name: &str, turn_id: &str);          // update a head

    // Read operations (from in-memory maps)
    fn get_message(&self, id: &str) -> Option<&Message>;
    fn get_turn(&self, id: &str) -> Option<&Turn>;
    fn get_head(&self, name: &str) -> Option<&str>;             // returns turn ID
    fn list_heads(&self) -> Vec<(&str, &str)>;                  // (name, turn_id) pairs

    // Traversal
    fn turn_chain(&self, turn_id: &str) -> Vec<&Turn>;          // walk parent chain, root-to-leaf order
    fn resolve_input(&self, turn: &Turn) -> Vec<&Message>;      // resolve input message IDs

    // Loading
    fn open(path: &Path) -> Result<Self>;                       // load from file
    fn create(path: &Path) -> Result<Self>;                     // create new empty file
}
```

## File lifecycle

### Creation
When ri starts and no store file exists, create an empty file.

### Normal operation
Append entries as the agent runs. Each completed turn produces:
1. 0-N new `msg` entries (new user messages, tool results -- messages already in the store are reused by ID)
2. 1 `msg` entry for the LLM's response
3. 1 `turn` entry
4. 1 `head` entry (to advance the session pointer)

### Crash recovery
If the process crashes mid-write, the last line may be incomplete JSON. On next load:
1. Read all lines
2. Skip any line that fails JSON parsing (should only be the last line, if any)
3. Log a warning if a line was skipped

All entries before the incomplete line are valid and intact.

### Archival (future, not v1)
If the file grows too large:
1. Copy the file to `store-archive-YYYY-MM-DD.jsonl`
2. Create a new `store.jsonl` containing only:
   - All messages referenced by the latest turn for each current head
   - Those turns
   - The current head entries
3. Old turns and orphaned messages live in the archive

This is a manual/explicit operation, not automatic.

## Compatibility

### Forward compatibility
Unknown fields on any entry type should be preserved (round-tripped) but can be ignored. This allows future versions to add optional fields without breaking older readers.

### Unknown entry types
Lines with an unrecognized `type` value should be preserved in the file but can be ignored for in-memory indexing. This allows future versions to add new entry types.

### Versioning
No explicit version field. The format is simple enough that changes can be handled by field presence/absence. If a breaking change is ever needed, use a different filename or a magic first line.
