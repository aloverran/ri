# ri Architecture

## What ri is

ri is a Rust tool for building applications on top of turn-based LLM APIs. Its first application is a coding agent (like Claude Code / pi), but the core data model is general-purpose.

## Foundational insight

An LLM API is a stateless pure function:

```
f(Vec<Message>) -> Response
```

Every call is independent. The API has no memory. All "memory" is in the caller's hands -- it's whatever the caller chooses to put in `Vec<Message>`.

This means the fundamental unit of interaction is not the *message* but the *turn* -- a single call to this function. A turn has:

- **Input**: the exact `Vec<Message>` sent
- **Output**: the response received

A *session* is a linked sequence of turns (each turn points to its predecessor). A session is NOT a conversation -- it's a history of LLM calls. A "conversation" is one *strategy* for constructing those calls' inputs.

## Why the turn, not the message

Most agent frameworks treat the conversation (a list of messages) as the primary data structure. The agent "accumulates" messages and sends them all each turn. This conflates two concerns:

1. **What happened** (raw events: user typed, tool returned, LLM responded)
2. **What the LLM sees** (the constructed input for each call)

These are different things. Between turns, the input can be *arbitrarily transformed*:

- Messages removed (stripping old tool results to save tokens)
- Messages replaced (compaction: summarize old messages into one)
- Messages rewritten (summarize each message individually)
- Messages injected (system reminders, context files)
- Messages reordered (for emphasis or API requirements)

"Compaction" is not a special operation. It's one of many possible input transformations between turns. The session records turns (what was actually sent/received). The *strategy* decides how to construct each turn's input. They're separate concerns.

## The turn store

All turns live in one unified store (`~/.ri/store.jsonl`). This is like git's object store -- one database of all objects, with named pointers (refs/heads) into it.

### Why a unified store (not per-session files)

We considered per-session files (one JSONL per session). Simpler in some ways, but loses:

- **Cross-session forking**: "Start a new session from where session A's codebase analysis ended" -- just point a new turn to session A's last turn as its parent. No copying.
- **Shared ancestry**: Two sessions that established the same context share those turns structurally.
- **Cross-session querying**: "Find every turn where the LLM called edit on this file" -- query one store.
- **Session composition**: Merge insights from session A into session B by referencing turns across sessions.

The downside (growing file, GC complexity) is manageable. A year of heavy use produces ~64MB. Loading takes under a second. If it ever gets too large, archive the file and start fresh.

### Why not a complex directory structure (like .git)

git's object store uses a directory of files organized by hash prefix, with packfiles for efficiency. This is complex. We don't need it because:

- Our objects are small and few compared to a git repo (hundreds/thousands of turns, not millions of blobs)
- We don't need content-addressable storage (we use random IDs, not hashes)
- Linear scanning of a <100MB JSONL file is fast enough
- One file is maximally portable, inspectable, and simple

## Sessions are just pointers

A "session" has no structural existence. It's a named pointer to a turn, like a git branch is a pointer to a commit. The pointer is called a "head."

Multiple heads can point into the same turn chain (branching within a session). Heads can point to turns that are children of turns from other sessions (cross-session linking).

"Creating a session" = creating a head pointing to a new root turn.
"Advancing a session" = moving the head to a new turn whose parent is the old head.
"Branching" = creating a new head pointing to an existing turn, then appending from there.
"Resuming from another session" = creating a turn whose parent is a turn from the other session's chain.

## The three entry types

The store has exactly three entry types:

### 1. Messages (`msg`)

A message is a content blob with a role. It's the unit of content -- what gets included in LLM call inputs and outputs. Messages are immutable once written.

Messages exist independently of turns. A message may be referenced by many turns' inputs (e.g., a system prompt reused across every turn). Or it may be referenced by only one turn.

Messages correspond to the LLM API's message format: a role (system, user, assistant) and a list of content blocks (text, images, tool use, tool results, thinking).

### 2. Turns (`turn`)

A turn records one LLM API call. It stores:

- The ordered list of message IDs that were sent as input
- The message ID of the output (the LLM's response)
- The parent turn ID (the previous turn in this chain, or null for a root)
- Metadata: model used, timestamp, token usage, cost, duration

A turn is a complete, self-describing record of an LLM interaction. Given the turn and the messages it references, you can reconstruct exactly what the LLM saw and what it said.

Turns form a DAG via parent pointers (in practice, usually a tree -- chains with occasional branches).

### 3. Heads (`head`)

A head is a named pointer to a turn. It represents a "session" or "branch" in human terms. Heads are updated by appending a new head entry (the latest entry for each name wins).

The head history (all entries for a given name) shows how a session evolved over time.

## The strategy layer (application-specific)

The store knows nothing about conversations, agents, tools, compaction, or context management. These are all handled by the *strategy* -- application-level code that:

1. Maintains application state (conversation events, tool results, config)
2. Constructs the next turn's input messages from that state
3. Processes the LLM's response (executes tool calls, updates state)
4. Writes turns and messages to the store

For the coding agent:

- Application state: conversation events (user typed, tool returned), system prompt, active tools
- Strategy: accumulate conversation messages, maybe compact old ones, maybe strip old tool results
- Tool execution: run bash, read/write files, etc.

For a pipeline:

- Application state: list of pipeline stages, current stage's output
- Strategy: each turn uses a different system prompt and the previous stage's output as input
- No tool execution

For prompt A/B testing:

- Application state: the test document, list of prompt variants
- Strategy: each turn sends the same document with a different prompt
- Compare outputs

The store handles all of these identically. It just records turns.

## Architecture layers

```
Layer 0: Store (store.jsonl)
  - Messages, turns, heads
  - Append-only JSONL
  - Knows nothing about LLMs, tools, or conversations

Layer 1: Provider (ri-ai)
  - LLM API abstraction
  - Streaming SSE
  - Provider-specific formatting (Anthropic, OpenAI, etc.)
  - OAuth, API keys
  - Knows about LLM APIs but not about agents or tools

Layer 2: Agent / Strategy (ri-core)
  - The agent loop (call LLM, execute tools, repeat)
  - Context construction (the "strategy" -- how to build the next turn's input)
  - Tool execution
  - Knows about conversations, tools, compaction
  - Reads from and writes to the store

Layer 3: Application (ri-cli)
  - CLI parsing, run modes (interactive, print, RPC)
  - TUI / display
  - User interaction (input, output, commands)
  - Config resolution (models.json, settings, auth)
  - Resource loading (context files, skills, prompts)
```

Each layer depends only on the layers below it. The store never imports provider code. The provider never imports agent code. The agent never imports CLI code.

## Crate structure

```
ri/
  crates/
    ri-store/       # Layer 0: Turn store, on-disk format, in-memory index
    ri-core/        # Layer 2: Agent loop, strategy, tool trait, event system
    ri-ai/          # Layer 1: LLM providers (Anthropic, etc.), streaming, auth
    ri-tools/       # Layer 2: Built-in tool implementations
  ri-cli/           # Layer 3: CLI entry point, modes, config, TUI
```

### ri-store

The turn store. Handles:
- Reading/writing store.jsonl
- In-memory index (messages, turns, heads)
- Turn chain traversal (walk parent pointers)
- Head management (create, update, list)

Does NOT handle:
- LLM API calls
- Message content interpretation
- Context construction
- Tool execution

This crate is tiny (~200-300 lines) and finished the day it's written. It has one job: persist and query turns.

### ri-core

Agent types and the agent loop. Handles:
- ContentBlock, Message, Role types (shared vocabulary)
- The Tool trait
- The agent loop (stream LLM -> execute tools -> loop)
- AgentEvent system (broadcast events to consumers)
- Context strategy trait/interface

Note: ri-core depends on ri-store (to read/write turns) and on ri-ai (to call LLMs). The agent loop is the orchestrator that ties the store and the provider together.

### ri-ai

LLM provider abstraction. Handles:
- LlmProvider trait (stream messages -> get response events)
- Anthropic provider (SSE parsing, request construction, OAuth)
- Model definitions, model registry
- API key resolution

Does NOT handle:
- Turn storage
- Tool execution
- Agent loop logic

### ri-tools

Built-in tool implementations: bash, read, write, edit, find, grep, ls. Each tool implements the Tool trait from ri-core.

### ri-cli

Application entry point. Wires everything together:
- CLI argument parsing (clap)
- Config resolution (models.json, settings.json, auth.json)
- Resource loading (context files, skills, prompts)
- System prompt construction
- Run modes: interactive (REPL), print (single-shot), RPC (JSON-RPC over stdio)
- Display / TUI

## Agent loop and the store

The agent loop's relationship to the store:

```
1. User provides input (or follow-up arrives)
2. Strategy constructs Vec<Message> for the next LLM call
   - Reads previous turns from store (for context)
   - Applies transforms (compaction, stripping, injection)
   - Produces the exact message list to send
3. Each new message is written to the store (msg entries)
4. LLM is called with the message list
5. Response is streamed back
6. Response is written to the store (msg entry)
7. Turn is written to the store (turn entry referencing input + output msgs)
8. Head is updated (head entry)
9. If response contains tool calls:
   a. Tools are executed
   b. Tool results become new messages (written to store)
   c. Go to step 2 (strategy constructs next input, including tool results)
10. If no tool calls: done, wait for user input
```

The strategy (step 2) is where all context management lives. It's a function:

```rust
fn build_context(&self, store: &Store, head: &str) -> Vec<Message>
```

It reads the store, decides what to include, and returns the message list. The agent loop doesn't know how context is managed -- it just calls the strategy and sends whatever it returns.

## Context strategy examples

### Naive (current ri behavior)
Accumulate all user messages, assistant responses, and tool results. Send everything.

### Windowed
Send the system prompt + last N turns worth of messages.

### Compacting
Track token count. When approaching the budget, ask the LLM to summarize old messages. The summary becomes a new message. Future turns' inputs include the summary instead of the old messages.

Note: "asking the LLM to summarize" is itself a turn! The compaction call is recorded in the store like any other turn. The summary message is a regular message. Nothing special about compaction at the store level.

### Custom
Any arbitrary logic. Strip thinking blocks from old turns. Inject context files. Summarize each message individually. Rewrite tool results as prose. Whatever the strategy decides.

## Error handling

The store is append-only, so the worst that can happen is a partial write (crash mid-append). Recovery: read the file, skip the last line if it's incomplete JSON. All previous entries are intact.

The agent loop can fail mid-turn (network error, API error, tool crash). In this case, no turn is written (the turn entry is only written after the LLM call completes). The store is in a consistent state: it contains all completed turns, and the head points to the last completed turn.

## What's NOT in this design

- **Extensions / plugins**: Not in v1. The strategy is just Rust code. If we want pluggable strategies later, we can add a trait.
- **Multi-model orchestration**: Not in v1. Each turn uses one model. Sub-agents can be implemented as separate turn chains.
- **Real-time collaboration**: Never. This is a single-user tool.
- **Full-text search / indexing**: Not in v1. Linear scan of JSONL is fast enough for our scale.
- **Encryption**: Not needed. Single-player indie games, personal machine.
