# ri-web Architecture

## What ri-web is

ri-web is a web interface for ri. It is a Layer 2 application (like ri-cli) that composes the same foundation crates -- `ri`, `ri-ai`, `ri-kit` -- to provide a browser-based interface for LLM sessions. It replaces the terminal TUI for daily use and is accessible over a tailnet from a phone.

ri-web is not a wrapper around ri-cli. It composes the same primitives directly: `Turn` for LLM calls, `Store` for persistence, `Pool` for message/step management, and the standard tool implementations. The agent loop in ri-web is its own application code, shaped for a multi-session web context.

## Why a separate binary

ri-cli is built around a terminal: ratatui viewport, crossterm events, inline scrollback. A web interface needs HTTP, SSE, and a browser frontend. Sharing a binary would mean both pulling in dependencies the other doesn't need.

More importantly, ri-web manages multiple concurrent sessions. ri-cli is one session per process. The server state model is fundamentally different, even though the per-session logic is the same composition of `Turn` + tools + `Store`.

## Architecture

```
Browser (SolidJS)
  |
  |  HTTP REST (session CRUD, auth, settings, models)
  |  SSE (agent events, tracing logs)
  |
ri-web (axum)
  |
  +-- ri       (Pool, Store, types)
  +-- ri-ai    (Turn, providers, registry)
  +-- ri-kit  (bash, read, write, edit, prompts, resources)
```

The browser talks only to the axum server. All LLM calls, tool execution, and message persistence happen server-side.

## Crate structure

```
ri-web/
  Cargo.toml
  src/
    main.rs               # Entry point, CLI flags, server startup
    state.rs              # AppState, SessionState, RunHandle, LoginInProgress
    api.rs                # axum route handlers (REST + SSE + auth)
    agent.rs              # Agent loop + background title generation
    meta_tools.rs         # runAgent, readContextGraph, readMessage, appendMessage, createContext
    tracing_broadcast.rs  # Live tracing log forwarding to SSE clients
  frontend/               # SolidJS + Vite project
```

ri-web is a workspace member alongside ri-cli. Both are Layer 2 applications that compose the same Layer 0 and Layer 1 crates.

## Server state

### AppState

The top-level state shared across all axum handlers:

```rust
struct AppState {
    /// All tools (base coding tools + meta-tools).
    tools: Vec<Arc<dyn Tool>>,
    /// Base tools only (bash, read, write, edit) -- given to sub-agents.
    base_tools: Vec<Arc<dyn Tool>>,
    /// Global defaults from CLI flags / settings.json.
    default_model: String,
    default_thinking: ThinkingLevel,
    sessions_dir: PathBuf,
    /// Active sessions keyed by file_id string.
    sessions: RwLock<HashMap<String, Arc<Mutex<SessionState>>>>,
    /// In-progress OAuth login flows, keyed by provider id.
    logins: RwLock<HashMap<String, LoginInProgress>>,
    /// Broadcast channel for live tracing log entries.
    log_tx: broadcast::Sender<LogEntry>,
    /// Ring buffer of all log entries since boot (50k cap).
    log_buffer: Arc<LogBuffer>,
}
```

Meta-tools are constructed with a `Weak<AppState>` (via `Arc::new_cyclic`) so `runAgent` can access the shared sessions map and tools without creating a reference cycle.

### SessionState

Per-session state:

```rust
struct SessionState {
    store: Store,
    message_ids: Vec<MessageId>,
    cwd: PathBuf,
    name: String,
    ts: String,
    file_id: SessionId,
    parent: Option<SessionId>,
    /// Broadcast channel for SSE clients to subscribe to agent events.
    events_tx: broadcast::Sender<AgentEvent>,
    /// Active agent run handle. None when idle.
    current_run: Option<RunHandle>,
    /// Monotonic counter for background title generation (stale-write prevention).
    title_gen_seq: u64,
}

struct RunHandle {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}
```

**Locking discipline**: The `Mutex<SessionState>` is held only for brief synchronous operations (reading message_ids, writing a message, checking the running flag). It is never held across async await points -- particularly not during LLM calls or tool execution. The agent task acquires the lock, reads/writes what it needs, releases it, then performs the long async operation.

### Session lifecycle

1. **Create**: `POST /api/sessions` creates a `Store`, writes the JSONL header + system prompt + root step, initializes `SessionState`, inserts it into `AppState.sessions`.

2. **Load on demand**: When a client opens a session not yet in memory, `get_or_load_session` loads it from disk (Store.load_all, resolve head context, register in the sessions map).

3. **Send message**: `POST /api/sessions/:id/messages` spawns a tokio task running the agent loop. Returns 202 immediately. Events flow through the broadcast channel.

4. **Stream**: `GET /api/sessions/:id/events` subscribes to the broadcast channel and forwards events as SSE frames.

5. **Cancel**: `POST /api/sessions/:id/cancel` triggers the CancellationToken. The agent loop stops, clears `current_run`.

## API

All routes are prefixed with `/api`.

### Sessions

```
GET    /api/sessions              List all sessions (from disk, enriched with in-memory names)
POST   /api/sessions              Create a new session { cwd }
GET    /api/sessions/:id          Get session detail with all messages, status
DELETE /api/sessions/:id          Stop agent loop, remove from memory (file kept)
POST   /api/sessions/:id/messages Send user message, start agent loop { text, model?, thinking? }
GET    /api/sessions/:id/events   SSE stream of agent events
POST   /api/sessions/:id/cancel   Cancel active agent loop
```

### Models and settings

```
GET    /api/models                List all models from authenticated providers
GET    /api/settings              Server defaults (model, thinking level)
```

### Auth

```
GET    /api/auth/status                  Provider auth status (authenticated, can_logout, account)
POST   /api/auth/login                   Begin OAuth flow { provider_id } -> { method, url }
POST   /api/auth/complete                Complete paste-code flow { provider_id, code }
POST   /api/auth/logout                  Remove stored credentials { provider_id }
GET    /api/auth/login-status/:provider  Poll local-callback flow status
```

### Logs

```
GET    /api/logs                  SSE stream of tracing log entries (global, not per-session)
```

### SSE event format

Agent events:

```
event: text_start          data: {}
event: text_delta          data: {"delta":"Hello"}
event: text_end            data: {}
event: thinking_start      data: {}
event: thinking_delta      data: {"delta":"Let me think..."}
event: thinking_end        data: {}
event: tool_call_start     data: {"id":"tc_1","name":"bash"}
event: tool_call_delta     data: {"id":"tc_1","delta":"{\"command\":\"ls\"}"}
event: tool_call_end       data: {}
event: tool_start          data: {"id":"tc_1","name":"bash"}
event: tool_end            data: {"id":"tc_1","output":"...","is_error":false,"details":{...}}
event: usage               data: {"input_tokens":1234,"output_tokens":567,...}
event: message_complete    data: {full serialized Message}
event: title_update        data: {"title":"Fix login crash"}
event: agent_error         data: {"message":"Rate limited"}
event: done                data: {}
event: resync              data: {}  (sent when SSE client lags behind broadcast)
```

Log events:

```
event: log                 data: {"ts":"12:34:56.789","level":"INFO","target":"ri_web","message":"..."}
```

## The agent loop

ri-web's agent loop broadcasts events through a `tokio::sync::broadcast` channel instead of returning a stream. Multiple SSE clients observe the same run simultaneously.

The loop also triggers **background title generation**: after the first user message and after assistant messages with text content, a background task calls a cheap model (Haiku) to generate a short title for the session. A monotonic sequence counter prevents stale writes from overwriting newer titles.

### Code reuse

The agent loop logic is similar between ri-cli and ri-web but not identical:
- ri-cli: returns `impl Stream<Item = AgentEvent>` (one consumer)
- ri-web: broadcasts through a channel (multiple consumers)
- ri-web: spawns as a tokio task, includes title generation, AGENTS.md discovery via meta tags

Both are ~100-300 lines of application code composing the same primitives. Extracting a shared crate would over-abstract. Two applications writing their own loops from the same building blocks is the intended pattern.

## Frontend

- **SolidJS**: Fine-grained reactivity, the operator's preferred framework.
- **Vite**: Fast dev server with HMR.
- **TypeScript**: Type safety for the API contract.

## Dev and production modes

### Dev mode (two processes)

```
Terminal 1:  cd ri-web && cargo run -- --dev --port 3001
Terminal 2:  cd ri-web/frontend && npm run dev
```

`--dev` enables permissive CORS and skips static file serving. Vite proxies `/api` to axum.

### Production mode (one process)

```
cd ri-web/frontend && npm run build
cd ri-web && cargo run
```

axum serves `/api/*` routes and falls back to `frontend/dist/` for static files (SPA routing via `index.html` fallback).

## Tailnet access

Bind to `0.0.0.0` for tailnet access. Tailscale provides encrypted transport and access control. No TLS configuration needed in the Rust code.

## Error handling

- **LLM API errors**: Broadcast as `agent_error` SSE events. Written as error content blocks in the persisted assistant message.
- **Tool errors**: Produce `tool_end` events with `is_error: true`. The LLM sees the error and reacts.
- **SSE disconnection**: Client reconnects, GETs the session to sync state. Missed streaming events are acceptable -- persisted messages are the source of truth.
- **Broadcast lag**: Receivers that fall behind get a `resync` event, prompting the client to GET and rebuild state.
- **Server crash**: JSONL is append-only. On restart, all sessions recover from disk.
- **Concurrent modification**: One agent loop per session. `POST /messages` returns 409 if already running.

## What's NOT in this design

- **Authentication beyond tailscale**: Not needed. Single user.
- **Database**: JSONL files are the store. Pool loads into memory.
- **WebSockets**: SSE is sufficient for streaming. POST for input.
- **Multi-user**: One user. No accounts, no permissions.
