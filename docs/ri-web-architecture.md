# ri-web Architecture

## What ri-web is

ri-web is a web interface for ri. It is a Layer 2 application (like ri-cli) that composes the same foundation crates -- `ri`, `ri-ai`, `ri-tools` -- to provide a browser-based interface for LLM sessions. It replaces the terminal TUI for daily use and is accessible over a tailnet from a phone.

ri-web is not a wrapper around ri-cli. It composes the same primitives directly: `Turn` for LLM calls, `SessionStore` for persistence, `MessagePool` for message management, and the standard tool implementations. The agent loop in ri-web is its own application code, shaped for a multi-session web context.

## Why a separate binary

ri-cli is built around a terminal: ratatui viewport, crossterm events, inline scrollback. These are deeply terminal-specific. A web interface needs HTTP, SSE, and a browser frontend. Sharing a binary would mean both pulling in terminal dependencies when you only want the web, and web dependencies when you only want the terminal.

More importantly, ri-web manages multiple concurrent sessions. ri-cli is one session per process. The server state model is fundamentally different, even though the per-session logic is the same composition of `Turn` + tools + `SessionStore`.

## Architecture

```
Browser (SolidJS)
  |
  |  HTTP REST (session CRUD)
  |  SSE (streaming agent events)
  |
ri-web (axum)
  |
  +-- ri       (MessagePool, SessionStore, types)
  +-- ri-ai    (Turn, providers, registry)
  +-- ri-tools (bash, read, write, edit)
```

The browser talks only to the axum server. All LLM calls, tool execution, and message persistence happen server-side. The browser never contacts LLM APIs directly -- it may not even have public internet access (e.g., phone on a tailnet).

## Crate structure

```
ri/
  crates/
    ri/             # Layer 0: Foundation
    ri-ai/          # Layer 1: LLM providers
    ri-tools/       # Layer 1: Tool implementations
  ri-cli/           # Layer 2: Terminal application
  ri-web/           # Layer 2: Web application  <-- new
    Cargo.toml
    src/
      main.rs       # Entry point, CLI flags, server startup
      state.rs      # AppState, SessionState, shared server state
      api.rs        # axum route handlers (REST + SSE)
      agent.rs      # Agent loop (adapted for web context)
      static.rs     # Static file serving for production mode
    frontend/       # SolidJS + Vite project
      package.json
      vite.config.ts
      src/
        index.tsx
        App.tsx
        ...
```

ri-web is a workspace member alongside ri-cli. Both are Layer 2 applications that compose the same Layer 0 and Layer 1 crates.

### Dependencies

```toml
[dependencies]
ri.workspace = true
ri-ai.workspace = true
ri-tools.workspace = true
axum.workspace = true
tokio.workspace = true
serde.workspace = true
serde_json.workspace = true
futures.workspace = true
eyre.workspace = true
color-eyre.workspace = true
clap.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
chrono.workspace = true
tokio-util.workspace = true
tokio-stream.workspace = true
tower-http = { version = "0.6", features = ["cors", "fs"] }
```

No ratatui, no crossterm, no tui-textarea, no tui-markdown. Those are ri-cli concerns.

## Server state

The server manages multiple concurrent sessions. Each session can have an active agent loop streaming responses.

### AppState

The top-level state shared across all axum handlers:

```rust
struct AppState {
    /// LLM provider (shared across sessions, thread-safe via &self on stream())
    provider: Arc<dyn LlmProvider>,
    model: Model,
    tools: Vec<Arc<dyn Tool>>,
    system_prompt: String,
    thinking: ThinkingLevel,
    sessions_dir: PathBuf,
    /// Active sessions keyed by session filename (without .jsonl extension)
    sessions: RwLock<HashMap<String, Arc<Mutex<SessionState>>>>,
}
```

`LlmProvider` is `Send + Sync` and its `stream()` method takes `&self`, so a single `Arc<dyn LlmProvider>` can serve concurrent sessions. Each call to `provider.stream()` creates an independent HTTP connection to the LLM API.

### SessionState

Per-session state:

```rust
struct SessionState {
    store: SessionStore,
    message_ids: Vec<String>,
    /// Broadcast channel for agent events. Multiple SSE clients can subscribe.
    events_tx: broadcast::Sender<AgentEvent>,
    /// Active agent run, if any. None when idle.
    current_run: Option<RunHandle>,
}

/// Handle to a running agent loop. Created per-run, not reused.
/// When the agent task finishes, it clears this from SessionState.
struct RunHandle {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}
```

The `broadcast::Sender` allows multiple SSE connections to the same session. When you have the session open on desktop and phone simultaneously, both receive the same events. The broadcast channel has a buffer (e.g., 256 events) so slow consumers don't block the agent loop.

**Locking discipline**: The `Mutex<SessionState>` is held only for brief synchronous operations (reading message_ids, writing a message to the store, checking/setting the `running` flag). It is never held across async await points -- particularly not during LLM calls or tool execution. The agent task acquires the lock, reads/writes what it needs, releases it, then performs the long async operation. This ensures `GET` requests and SSE subscriptions are never blocked by a running agent.

### Session lifecycle

1. **Create**: `POST /api/sessions` creates a new `SessionStore` (which creates a new JSONL file), initializes `SessionState`, and inserts it into `AppState.sessions`.

2. **Load**: On server startup, session file headers are scanned from `~/.ri/sessions/` for listing (name, timestamp, cwd). When a client opens a session, a `SessionStore` is created and the session's messages are loaded into its pool. This is consistent with the core architecture: the pool is fast to load (~60-100MB/year loads in under a second).

3. **Send message**: `POST /api/sessions/:id/messages` writes the user message to the store and spawns a tokio task running the agent loop. The loop streams `AgentEvent`s through the broadcast channel. The handler returns immediately (202 Accepted).

4. **Stream**: `GET /api/sessions/:id/events` returns an SSE stream. The handler subscribes to the session's broadcast channel and forwards events as SSE frames. Multiple clients can subscribe.

5. **Cancel**: `POST /api/sessions/:id/cancel` triggers the current run's `CancellationToken` (from the `RunHandle`), which propagates through the agent loop to stop the current Turn and any running tools. When the task exits, it clears `current_run` to `None`.

### Concurrency model

The agent loop runs as a spawned tokio task, not inline in the request handler. This is important because:

- The agent loop can run for minutes (tool execution, multiple LLM turns).
- The HTTP request that triggered it should return immediately.
- Multiple SSE clients observe the same running loop.
- Cancellation comes from a separate request.

```
POST /messages  -->  spawns agent loop task  -->  returns 202
                            |
                            v
                     Turn::start() --> stream events --> broadcast
                            |
                            v
                     tool execution --> broadcast
                            |
                            v
                     next turn (if tool calls) ...
                            |
GET /events     -->  subscribes to broadcast  -->  SSE stream to client
POST /cancel    -->  cancel token              -->  stops loop
```

## API

All routes are prefixed with `/api`.

### Sessions

```
GET    /api/sessions
  Response: [{ id, name, ts, cwd, message_count }]
  Lists all sessions from the sessions directory. Reads headers from JSONL files.

POST   /api/sessions
  Body: { name: string, cwd: string }
  Response: { id, name, ts, cwd }
  Creates a new session. Writes the JSONL header and system message.

GET    /api/sessions/:id
  Response: { id, name, ts, cwd, status: "idle" | "running", messages: Message[] }
  Returns session metadata, agent status, and all messages. Messages include
  full content blocks, provenance, and metadata -- the browser has the
  complete picture. The `status` field is critical for UI sync: on initial
  load or SSE reconnect, the frontend checks this to know whether the agent
  is mid-run (and should show a "running" state) or idle (awaiting input).

DELETE /api/sessions/:id
  Stops any active agent loop and removes the session from memory.
  Does not delete the JSONL file (append-only philosophy).
```

### Messages

```
POST   /api/sessions/:id/messages
  Body: { text: string }
  Response: 202 Accepted
  Writes the user message to the session and starts the agent loop.
  Events are streamed via the SSE endpoint. Returns immediately.
  If an agent loop is already running, returns 409 Conflict.

GET    /api/sessions/:id/events
  Response: SSE stream
  Subscribes to the session's event broadcast. Events are JSON objects
  with the same schema as ri-cli's RPC mode output.
  Reconnection: the client can reconnect at any time. Missed events
  during disconnection are gone (the client can GET the session to
  see the current state). This is fine -- SSE reconnection is for
  network hiccups, not long disconnections.

POST   /api/sessions/:id/cancel
  Response: 200 OK
  Cancels the active agent loop. The CancellationToken propagates to
  the Turn and any running tools.
```

### SSE event format

Each SSE frame has an `event:` field matching the event type and a `data:` field with a JSON payload. The JSON format follows the same schema as ri-cli's RPC mode. ri-web defines its own event-to-JSON conversion (same shape, separate implementation -- each Layer 2 app owns its serialization).

```
event: text_start
data: {}

event: text_delta
data: {"delta":"Hello, world!"}

event: text_end
data: {}

event: thinking_start
data: {}

event: thinking_delta
data: {"delta":"Let me think about this..."}

event: thinking_end
data: {}

event: tool_start
data: {"id":"tc_1","name":"bash"}

event: tool_end
data: {"id":"tc_1","output":"Exit code: 0\n...","is_error":false}

event: usage
data: {"input_tokens":1234,"output_tokens":567,"cache_read_tokens":0,"cache_write_tokens":0}

event: message_complete
data: {"id":"s_1","role":"assistant","content":[...],"provenance":{...}}

event: done
data: {}

event: error
data: {"message":"Rate limited, retry after 5000ms"}
```

The `event:` field lets the browser's `EventSource` dispatch to specific listeners:
```javascript
source.addEventListener("text_delta", (e) => { ... });
source.addEventListener("tool_start", (e) => { ... });
```

On the server side, axum 0.8's `Sse` response type constructs these frames:
```rust
Event::default().event("text_delta").data(r#"{"delta":"Hello"}"#)
```

The SSE stream includes `KeepAlive` to prevent idle timeouts, especially important for tailnet connections:
```rust
Sse::new(stream).keep_alive(KeepAlive::default())
```

## The agent loop

ri-web's agent loop is the same composition as ri-cli's: assemble messages from the pool, call Turn, execute tools, persist messages, repeat. The difference is that instead of yielding to a Stream consumer, it broadcasts events through a channel.

```
1. User message written to store
2. Select messages from pool (message_ids)
3. Build RequestOptions, start Turn
4. Poll Turn::next() in a loop:
   - Each StreamEvent is broadcast to SSE clients
5. Turn finishes -> build assistant Message with provenance
6. Write assistant message to store
7. If tool calls present:
   a. Broadcast ToolStart for each
   b. Execute tool (tool.run())
   c. Broadcast ToolEnd
   d. Write tool results message to store
   e. Go to step 2
8. No tool calls -> broadcast Done, loop ends
```

This is ~80 lines of async code, just like ri-cli's agent loop. The difference is the output destination (broadcast channel vs stream return).

### Code reuse

The agent loop logic is similar between ri-cli and ri-web but not identical. ri-cli returns `impl Stream<Item = AgentEvent>`. ri-web broadcasts through `tokio::sync::broadcast`. The composition shape differs:

- ri-cli: synchronous iteration by the TUI event loop (one consumer)
- ri-web: async broadcast to N SSE connections (multiple consumers)

These are different enough that extracting a shared crate would over-abstract. Both are ~80 lines of application code composing the same primitives. The architecture doc explicitly says the agent loop is application code, not infrastructure. Two applications writing their own loops from the same building blocks is the intended pattern.

If the loops drift apart significantly in the future (ri-web doing fan-out, context composition, etc.), the divergence is a feature, not duplication.

## Frontend

### Technology

- **SolidJS**: Fine-grained reactivity, tiny runtime, the operator's preferred framework.
- **Vite**: Fast dev server with HMR. Minimal configuration.
- **TypeScript**: Type safety for the API contract.
- **CSS**: Tailwind or plain CSS. The choice is deferred to implementation.

### Structure

```
frontend/
  package.json        # solid-js, vite, vite-plugin-solid
  vite.config.ts      # proxy /api to axum in dev mode
  tsconfig.json
  index.html
  src/
    index.tsx         # Mount point
    App.tsx           # Router: session list vs session view
    api.ts            # fetch wrappers + SSE connection
    types.ts          # Shared types (Session, Message, AgentEvent, etc.)
    components/
      SessionList.tsx # List/create sessions
      ChatView.tsx    # Active session: messages + streaming + input
      Message.tsx     # Render a message (text, thinking, tool call, tool result)
      Input.tsx       # Text input area
      StatusBar.tsx   # Model, tokens, phase
```

### Data flow

```
User types message
  |
  v
POST /api/sessions/:id/messages
  |
  v
EventSource /api/sessions/:id/events
  |
  v
SolidJS signals update reactively
  |
  +-- streaming text signal (appended on text_delta)
  +-- thinking signal (appended on thinking_delta)
  +-- messages signal (appended on message_complete)
  +-- tool status signal (updated on tool_start/tool_end)
  +-- usage signal (updated on usage events)
```

Streaming content is accumulated in signals for live preview. When a `message_complete` event arrives, it carries the **full serialized Message** (content blocks, provenance, metadata). The frontend replaces any accumulated streaming state with this authoritative message. This means the frontend doesn't need to perfectly reconstruct messages from deltas -- the server is the source of truth. Deltas are for real-time preview; `message_complete` is for correctness.

### Markdown rendering

Messages contain markdown text. The frontend renders it using a client-side markdown library (e.g., `marked` or `markdown-it`). This is a rendering concern that belongs entirely in the frontend.

### Vite configuration

```typescript
// vite.config.ts
import { defineConfig } from "vite";
import solidPlugin from "vite-plugin-solid";

export default defineConfig({
  plugins: [solidPlugin()],
  server: {
    port: 3000,
    proxy: {
      "/api": {
        target: "http://localhost:3001",
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: "dist",
  },
});
```

In dev, Vite serves the frontend on `:3000` and proxies `/api` to the axum server on `:3001`. In production, axum serves both.

## Dev and production modes

### Dev mode

Two processes:

```
Terminal 1:  cd ri-web && cargo run -- --dev --port 3001
Terminal 2:  cd ri-web/frontend && npm run dev
```

`--dev` tells the axum server to skip static file serving (since Vite handles it) and to enable permissive CORS headers via `tower_http::cors::CorsLayer::permissive()` so the browser allows requests from the Vite origin (`:3000`) to the API origin (`:3001`). In production mode, both are served from the same origin, so CORS is not needed.

Open `http://localhost:3000` in the browser. Vite serves the SolidJS app with HMR. API calls are proxied to axum.

### Production mode

One process:

```
cd ri-web/frontend && npm run build   # produces dist/
cd ri-web && cargo run                # serves everything
```

Without `--dev`, the axum server:
1. Serves `/api/*` routes as normal.
2. Serves static files from `frontend/dist/` for all other paths.
3. Falls back to `index.html` for unmatched paths (SPA routing).

Open `http://localhost:3001` directly.

### Static file serving

In production mode, axum serves the built frontend using `tower_http::services::ServeDir` with a fallback to `index.html`:

```rust
let serve = ServeDir::new("frontend/dist")
    .fallback(ServeFile::new("frontend/dist/index.html"));

let app = Router::new()
    .nest("/api", api_routes)
    .fallback_service(serve);
```

This is simpler than `rust-embed` (no compile-time embedding, no build order dependency) and works because the binary is always run from the repo checkout.

## Tailnet access

For local use, bind to `127.0.0.1`. For tailnet access (phone, other machines):

```
cargo run -- --host 0.0.0.0 --port 3001
```

Tailscale provides:
- **Encrypted transport**: All traffic over the tailnet is WireGuard-encrypted.
- **Access control**: Only devices on your tailnet can reach the server.
- **DNS**: Access via `<machine-name>:3001` from any device on the tailnet.

No TLS configuration needed in the Rust code. Tailscale handles it at the network layer.

For HTTPS (needed for some browser features and to avoid mixed-content warnings), run `tailscale cert <hostname>` and point axum at the cert files, or put a reverse proxy (caddy, nginx) in front. This is a deployment concern, not an architecture concern.

## What the frontend sees

The frontend interacts with a simple REST+SSE API. It does not import any Rust types. The TypeScript types mirror the JSON shapes:

```typescript
interface Session {
  id: string;
  name: string;
  ts: string;
  cwd: string;
  message_count: number;
}

interface Message {
  id: string;
  role: "system" | "user" | "assistant";
  content: ContentBlock[];
  provenance?: Provenance;
  meta?: Record<string, unknown>;
}

type ContentBlock =
  | { type: "text"; text: string }
  | { type: "thinking"; thinking: string }
  | { type: "tool_use"; id: string; name: string; input: unknown }
  | { type: "tool_result"; toolUseId: string; content: ContentBlock[]; is_error: boolean }
  | { type: "image"; mediaType: string; data: string };

interface Provenance {
  input: string[];
  model: string;
  ts: string;
  usage?: Usage;
}

interface Usage {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
}
```

These types are derived from the JSONL format. They are the same shapes that `ri` serializes. The frontend gets the real data, not a simplified projection.

## Future directions

This architecture is designed to grow toward the operator's stated long-term goals.

### Context composition

The message pool model makes arbitrary context composition trivial at the API level. Future endpoints could allow:

```
POST /api/turns
Body: { message_ids: [...], model: "...", thinking: "high" }
```

This is a raw "assemble these messages, call the LLM" endpoint -- no agent loop, no tool execution. The caller composes the context. The response streams via SSE. The output message is saved to the pool with provenance recording exactly which input messages were used.

This is the "leaving the agent idea entirely to think of directly composing and fanning out various LLM requests" goal. The pool + Turn primitives already support it. The API just needs to expose it.

### Fan-out

Multiple LLM calls with the same input but different models/parameters:

```
POST /api/fan-out
Body: { message_ids: [...], variants: [{ model, thinking }, ...] }
```

Runs N concurrent Turns, each producing a new message in the pool. The frontend can display them side-by-side for comparison.

### Message DAG visualization

`GET /api/sessions/:id` already returns messages with full provenance (which input messages produced each derived message). The frontend can render this as a graph. No backend changes needed -- the data is already there.

### Cross-session composition

Pull messages from one session's pool into another session's context. The pool supports this by design (globally unique message IDs, cross-file references). The API would expose it as a "reference message from another session" operation.

### Connecting to ri-cli

A future mode where ri-web manages ri-cli processes rather than running its own agent loop. The web server spawns `ri --mode rpc` child processes and proxies their JSON events to the SSE stream. This gives tool execution in the CLI's working directory context. The API surface is identical from the frontend's perspective.

## What's NOT in this design

- **Authentication beyond tailscale**: Not needed. Single user, personal machine, tailnet access control.
- **Database**: No database. JSONL files are the store. The pool loads into memory on startup.
- **WebSockets**: SSE is sufficient. User input goes via POST. Streaming goes via SSE. If bidirectional real-time is needed later (e.g., terminal-in-browser), WebSocket can be added for that specific feature.
- **Multi-user**: One user. No user accounts, no permissions, no tenancy.
- **Offline/PWA**: The server must be running. No service worker, no offline cache.

## Error handling

- **LLM API errors**: Streamed as `error` SSE events. The frontend displays them. The agent loop stops.
- **Tool errors**: Produce `tool_end` events with `is_error: true`. The LLM sees the error and reacts. Normal flow.
- **SSE disconnection**: The client reconnects automatically (EventSource built-in behavior). On reconnect, the frontend must `GET /api/sessions/:id` to sync state -- this returns the current `status` (idle/running) and all messages. Missed streaming events during disconnection are acceptable because the persisted messages are the source of truth. The SSE stream is for real-time observation, not for building state.
- **Server crash**: JSONL is append-only. On restart, all sessions are recoverable from disk. The last line of a file may be incomplete; it's skipped on load (same as ri-cli).
- **Concurrent modification**: One agent loop per session at a time. `POST /messages` returns 409 if `current_run` is `Some`. Each run creates a fresh `CancellationToken` (tokens stay cancelled permanently, so they must not be reused across runs). When the agent task finishes (success, error, or cancel), it clears `current_run` to `None`. The `RunHandle` pattern prevents both double-starts and stuck "running forever" states.
- **Broadcast lag**: `tokio::sync::broadcast` receivers that fall behind get a `Lagged` error. The SSE handler should catch this and send an `event: resync` frame, prompting the client to `GET` the session and rebuild state from the authoritative message list.

## Summary

ri-web is a thin web layer over ri's foundation primitives. The axum server manages sessions and runs agent loops. The SolidJS frontend is a reactive view over the SSE event stream. The architecture is deliberately simple:

- One server process (production) or two (dev).
- REST for CRUD, SSE for streaming. No WebSocket, no GraphQL, no RPC framework.
- The agent loop is ~80 lines of composition code, same as ri-cli.
- The frontend is a standard SolidJS + Vite project.
- Static files served by axum in production, by Vite in dev.
- The message pool and JSONL format are unchanged. ri-web reads and writes the same files as ri-cli.

The design is extensible toward direct context composition, fan-out, and message DAG visualization because those capabilities are already present in the pool model. The API just needs to expose them.
