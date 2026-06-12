//! Meta-tools for orchestrating ri from within an agent loop.
//!
//! Five tools organized by function:
//!
//! Observe:
//! - `readContextGraph`: DAG neighborhood explorer
//! - `readMessage`: inspect a single message with provenance
//!
//! Build:
//! - `createContext`: the unified context algebra primitive -- extend, compose,
//!    merge, and create messages in one operation
//!
//! Execute:
//! - `runTurn`: single LLM call (no tools, native capabilities enabled)
//! - `runAgent`: spawn a sub-agent loop asynchronously (LLM + tools, repeats)
//!
//! Design: context_id is the universal "plug" type. Build tools produce them,
//! execute tools consume them. session_id is output-only from execute tools
//! (an async handle to check back on). Observation tools accept either.
//!
//! The tools themselves are harness-independent: everything the LLM sees
//! (names, descriptions, schemas, validation, output formatting) and all
//! store surgery lives here, once, shared by every harness. What a harness
//! genuinely owns is reached through two seams:
//!
//! - [`StoreAccess`]: how to obtain the current store view. ri-web hands
//!   out a clone of its long-lived shared handle; ri-cli mounts the
//!   sessions directory fresh per call.
//! - [`MetaExec`]: how spawned work actually runs -- model resolution,
//!   session creation, and the loop itself. ri-web wires a child
//!   SessionState with SSE and SSH inheritance; ri-cli spawns a
//!   self-contained background task.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use ri::{
    ContentBlock, ContextId, HasMeta, MessageId, RefId, Role, Store, ThinkingLevel, Tool,
    ToolOutput,
};

/// How the meta-tools obtain the store. The harness chooses freshness and
/// lifetime semantics; `Err` carries the harness's own human-readable
/// reason and is surfaced verbatim as the tool error.
pub trait StoreAccess: Send + Sync {
    fn store(&self) -> Result<Store, String>;
}

/// A validated execution request assembled by the execute tools
/// (`runAgent`/`runTurn`): the resolved seed messages plus the caller's
/// model and parameter choices. `thinking: None` means "harness default".
pub struct ExecRequest {
    pub messages: Vec<MessageId>,
    pub model_id: String,
    pub thinking: Option<ThinkingLevel>,
    pub max_tokens: Option<usize>,
    pub label: Option<String>,
}

/// Harness seam for the execute tools: which models exist, and how a
/// spawned run executes (session creation, the loop, persistence).
/// Implementations return the new session's ref id immediately; the run
/// itself proceeds in the background.
#[async_trait]
pub trait MetaExec: Send + Sync {
    /// Model ids advertised in the tools' schemas.
    fn model_ids(&self) -> Vec<String>;
    /// Spawn a detached agent loop (LLM + tools, repeats until done).
    async fn spawn_agent(&self, request: ExecRequest) -> Result<RefId, String>;
    /// Spawn a detached single LLM turn (no function-calling tools).
    async fn spawn_turn(&self, request: ExecRequest) -> Result<RefId, String>;
}

/// Build the five meta-tools over the two harness seams. `session_id` is
/// the calling session: spawned runs are parented to it, and composed
/// contexts / forged messages land in its family file.
pub fn create(
    store: Arc<dyn StoreAccess>,
    exec: Arc<dyn MetaExec>,
    session_id: RefId,
) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(RunAgentTool { store: store.clone(), exec: exec.clone() }),
        Box::new(RunTurnTool { store: store.clone(), exec }),
        Box::new(ReadContextGraphTool { store: store.clone() }),
        Box::new(ReadMessageTool { store: store.clone() }),
        Box::new(CreateContextTool { store, session_id }),
    ]
}

/// Spawns a sub-agent loop asynchronously from a context.
///
/// Validates the shared execute parameters, resolves the context to its
/// message list, and hands the request to the harness seam, which creates
/// the child session and starts the loop in the background. Returns
/// immediately with the new session_id.
struct RunAgentTool {
    store: Arc<dyn StoreAccess>,
    exec: Arc<dyn MetaExec>,
}

#[async_trait]
impl Tool for RunAgentTool {
    fn name(&self) -> &str {
        "runAgent"
    }

    fn description(&self) -> &str {
        "Starts a full agent loop (LLM turn, tool calls, repeat until done), async. \
         Writes resulting messages back to the store and updates the session head. \
         Use context_id to specify the context snapshot to begin with. Returns the session_id \
         of the session that was created."
    }

    fn parameters(&self) -> Value {
        let models = self.exec.model_ids().join(", ");
        json!({
            "type": "object",
            "properties": {
                "context_id": {
                    "type": "string",
                    "description": "The context to use as the prompt history."
                },
                "model_id": {
                    "type": "string",
                    "description": format!("The model identifier to use for the turn. Available models: {}", models)
                },
                "model_params": {
                    "type": "object",
                    "description": "Parameters to pass to the model.",
                    "properties": {
                        "thinking": { "type": "string", "description": "Thinking level: off, low, medium, high, xhigh" },
                        "max_tokens": { "type": "integer", "description": "Maximum output tokens." }
                    }
                },
                "label": {
                    "type": "string",
                    "description": "Display label for the sub-session. Defaults to the model name."
                }
            },
            "required": ["context_id", "model_id"]
        })
    }

    async fn run(&self, input: Value, _cancel: CancellationToken) -> ToolOutput {
        let request = match resolve_exec_request(self.store.as_ref(), &input) {
            Ok(r) => r,
            Err(msg) => return ToolOutput::error(&msg),
        };

        match self.exec.spawn_agent(request).await {
            Ok(id) => ToolOutput::text(format!("Agent loop started on session '{}'", id))
                .with_details(json!({ "session_id": id })),
            Err(msg) => ToolOutput::error(&msg),
        }
    }
}

/// A single LLM call: send a context, get a response, persist it.
///
/// Unlike `runAgent` (which loops: LLM -> tools -> LLM -> ... ), `runTurn`
/// makes exactly one LLM call with no function-calling tools. The model's
/// native capabilities are enabled automatically -- for Gemini this means
/// search grounding and sandboxed code execution.
///
/// Use this for getting a single model response: research queries, second
/// opinions, advisor calls, or any situation where you want a model's
/// answer without an agent loop.
struct RunTurnTool {
    store: Arc<dyn StoreAccess>,
    exec: Arc<dyn MetaExec>,
}

#[async_trait]
impl Tool for RunTurnTool {
    fn name(&self) -> &str {
        "runTurn"
    }

    fn description(&self) -> &str {
        "Invoke an LLM for a single response (no tool calls, no agent loop). \
         The model's native capabilities are enabled automatically -- Gemini \
         models get search grounding and code execution. Writes the response \
         to a session asynchronously. Returns a session_id handle of the resulting session. "
    }

    fn parameters(&self) -> Value {
        let models = self.exec.model_ids().join(", ");
        json!({
            "type": "object",
            "properties": {
                "context_id": {
                    "type": "string",
                    "description": "The context to use as the prompt history."
                },
                "model_id": {
                    "type": "string",
                    "description": format!("The model to call. Available: {}", models)
                },
                "model_params": {
                    "type": "object",
                    "description": "Parameters for the model call.",
                    "properties": {
                        "thinking": { "type": "string", "description": "Thinking level: off, low, medium, high, xhigh" },
                        "max_tokens": { "type": "integer", "description": "Maximum output tokens." }
                    }
                },
                "label": {
                    "type": "string",
                    "description": "Display label for the sub-session. Defaults to the model name."
                }
            },
            "required": ["context_id", "model_id"]
        })
    }

    async fn run(&self, input: Value, _cancel: CancellationToken) -> ToolOutput {
        let request = match resolve_exec_request(self.store.as_ref(), &input) {
            Ok(r) => r,
            Err(msg) => return ToolOutput::error(&msg),
        };

        match self.exec.spawn_turn(request).await {
            Ok(id) => ToolOutput::text(format!("Single turn started on session '{}'", id))
                .with_details(json!({ "session_id": id })),
            Err(msg) => ToolOutput::error(&msg),
        }
    }
}

/// Parse and validate the input shared by the execute tools (context_id,
/// model_id, model_params, label) and resolve the context to its message
/// list. Returns the assembled request, or the error message to surface.
fn resolve_exec_request(store: &dyn StoreAccess, input: &Value) -> Result<ExecRequest, String> {
    let store = store.store()?;

    let context_id = input.get("context_id").and_then(|v| v.as_str())
        .ok_or("missing 'context_id' parameter")?;

    let model_id = input.get("model_id").and_then(|v| v.as_str())
        .ok_or("missing 'model_id' parameter")?;

    let messages = store.get_context(&ContextId::from(context_id))
        .map(|ctx| ctx.messages)
        .ok_or_else(|| format!("context '{}' not found", context_id))?;

    let params = input.get("model_params");
    let thinking = match params.and_then(|p| p.get("thinking")).and_then(|v| v.as_str()) {
        Some(s) => Some(s.parse::<ThinkingLevel>()
            .map_err(|_| format!("invalid thinking level '{}'", s))?),
        None => None,
    };
    let max_tokens = match params.and_then(|p| p.get("max_tokens")) {
        Some(v) => {
            let n = v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()));
            Some(n.ok_or_else(|| format!("invalid max_tokens: {}", v))? as usize)
        }
        None => None,
    };

    let label = input.get("label").and_then(|v| v.as_str()).map(str::to_string);

    Ok(ExecRequest {
        messages,
        model_id: model_id.to_string(),
        thinking,
        max_tokens,
        label,
    })
}

/// DAG neighborhood explorer. Shows all contexts reachable from an entry
/// point (session head or explicit context), with diffs showing how each
/// context's message list changed from its parent.
struct ReadContextGraphTool {
    store: Arc<dyn StoreAccess>,
}

/// Default depth limit for ancestor/descendant traversal.
const GRAPH_DEPTH: usize = 10;

#[async_trait]
impl Tool for ReadContextGraphTool {
    fn name(&self) -> &str {
        "readContextGraph"
    }

    fn description(&self) -> &str {
        "Summarizes the history of a session, exploring the context nodes reachable from a given \
         session or context. Requires \
         either session_id or context_id as an entry point. \
         Returns a flat list of context nodes with parent references \
         and diffs showing which messages were added/removed at each step. \
         The entry context shows its full message list. Message IDs include \
         inline summaries so you can understand the conversation without \
         follow-up readMessage calls."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Session to explore (resolves to its head context)."
                },
                "context_id": {
                    "type": "string",
                    "description": "Context to explore directly. Takes precedence over session_id."
                },
                "depth": {
                    "type": "integer",
                    "description": "Max traversal depth in each direction. Default 10."
                }
            }
        })
    }

    async fn run(&self, input: Value, _cancel: CancellationToken) -> ToolOutput {
        let store = match self.store.store() {
            Ok(s) => s,
            Err(msg) => return ToolOutput::error(&msg),
        };

        let depth = input.get("depth")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .map(|n| n as usize)
            .unwrap_or(GRAPH_DEPTH);

        // Resolve entry point: context_id takes precedence, then session_id.
        let context_id_str = input.get("context_id").and_then(|v| v.as_str());
        let session_id_str = input.get("session_id").and_then(|v| v.as_str());

        let entry_id: ContextId = if let Some(cid) = context_id_str {
            ContextId::from(cid)
        } else if let Some(sid) = session_id_str {
            match store.get_ref(&RefId::from(sid)).map(|s| s.head) {
                Some(head) => head,
                None => return ToolOutput::error(&format!("session '{}' not found or has no head", sid)),
            }
        } else {
            return ToolOutput::error("either 'session_id' or 'context_id' is required");
        };

        let out = (|| {
            let entry = match store.get_context(&entry_id) {
                Some(ctx) => ctx,
                None => return Err(format!("context '{}' not found", entry_id)),
            };

            // Collect all reachable contexts via BFS in both directions.
            let mut visited: HashSet<ContextId> = HashSet::new();
            let mut reachable: Vec<ContextId> = Vec::new();
            visited.insert(entry_id.clone());

            // Backward: walk parents.
            {
                let mut back_queue: VecDeque<(ContextId, usize)> = VecDeque::new();
                for pid in &entry.parents {
                    back_queue.push_back((pid.clone(), 1));
                }
                while let Some((id, d)) = back_queue.pop_front() {
                    if d > depth || !visited.insert(id.clone()) { continue; }
                    reachable.push(id.clone());
                    if let Some(ctx) = store.get_context(&id) {
                        for pid in &ctx.parents {
                            back_queue.push_back((pid.clone(), d + 1));
                        }
                    }
                }
            }

            // Forward: walk children.
            {
                let mut fwd_queue: VecDeque<(ContextId, usize)> = VecDeque::new();
                for child in store.children(&entry_id) {
                    fwd_queue.push_back((child.id.clone(), 1));
                }
                while let Some((id, d)) = fwd_queue.pop_front() {
                    if d > depth || !visited.insert(id.clone()) { continue; }
                    reachable.push(id.clone());
                    for child in store.children(&id) {
                        fwd_queue.push_back((child.id.clone(), d + 1));
                    }
                }
            }

            // Format as text: compact representation optimized for LLM consumption.
            let total = 1 + reachable.len();
            let mut out = format!("CONTEXT GRAPH entry={} count={}\n", entry_id, total);

            // Entry context (full message list).
            out.push('\n');
            format_context_header(&mut out, entry_id.as_str(), &entry.parents, true);
            format_message_list(&mut out, &store, &entry.messages);

            // Remaining reachable contexts.
            for id in &reachable {
                let ctx = match store.get_context(id) {
                    Some(c) => c,
                    None => {
                        out.push('\n');
                        format_context_header(&mut out, id.as_str(), &[], false);
                        out.push_str("  (not loaded)\n");
                        continue;
                    }
                };

                out.push('\n');
                format_context_header(&mut out, id.as_str(), &ctx.parents, false);

                if let Some(parent_id) = ctx.parents.first() {
                    if let Some(parent) = store.get_context(parent_id) {
                        let (added, removed) = diff_message_lists(&parent.messages, &ctx.messages);
                        format_diff(&mut out, &store, parent_id, &added, &removed);
                    } else {
                        format_message_list(&mut out, &store, &ctx.messages);
                    }
                } else {
                    format_message_list(&mut out, &store, &ctx.messages);
                }
            }

            Ok(out)
        })();

        match out {
            Ok(text) => ToolOutput::text(text),
            Err(msg) => ToolOutput::error(&msg),
        }
    }
}

/// Compute added/removed message IDs between a parent and child context.
fn diff_message_lists(parent: &[MessageId], child: &[MessageId]) -> (Vec<MessageId>, Vec<MessageId>) {
    let parent_set: HashSet<&str> = parent.iter().map(|id| id.as_str()).collect();
    let child_set: HashSet<&str> = child.iter().map(|id| id.as_str()).collect();

    let added: Vec<MessageId> = child.iter()
        .filter(|id| !parent_set.contains(id.as_str()))
        .cloned()
        .collect();
    let removed: Vec<MessageId> = parent.iter()
        .filter(|id| !child_set.contains(id.as_str()))
        .cloned()
        .collect();

    (added, removed)
}

/// Write a context header line to the output buffer.
///
/// Format: `<id> (entry) <- <parent1>, <parent2>`
/// Root contexts (no parents) omit the `<-`. Non-entry contexts omit `(entry)`.
fn format_context_header(out: &mut String, id: &str, parents: &[ContextId], is_entry: bool) {
    out.push_str(id);
    if is_entry {
        out.push_str(" (entry)");
    }
    if !parents.is_empty() {
        out.push_str(" <- ");
        for (i, p) in parents.iter().enumerate() {
            if i > 0 { out.push_str(", "); }
            out.push_str(p.as_str());
        }
    }
    out.push('\n');
}

/// Write indented message lines with inline summaries, or "(no messages)" for empty contexts.
fn format_message_list(out: &mut String, store: &Store, messages: &[MessageId]) {
    if messages.is_empty() {
        out.push_str("  (no messages)\n");
        return;
    }
    for id in messages {
        out.push_str("  ");
        out.push_str(id.as_str());
        if let Some(msg) = store.get_message(id) {
            out.push(' ');
            out.push_str(&msg.summarize());
        }
        out.push('\n');
    }
}

/// Write a diff block showing added (+) and removed (-) messages with summaries.
fn format_diff(out: &mut String, store: &Store, vs_id: &ContextId, added: &[MessageId], removed: &[MessageId]) {
    if added.is_empty() && removed.is_empty() {
        out.push_str("  (no changes)\n");
        return;
    }
    out.push_str("  diff vs ");
    out.push_str(vs_id.as_str());
    out.push('\n');
    for id in added {
        out.push_str("  + ");
        out.push_str(id.as_str());
        if let Some(msg) = store.get_message(id) {
            out.push(' ');
            out.push_str(&msg.summarize());
        }
        out.push('\n');
    }
    for id in removed {
        out.push_str("  - ");
        out.push_str(id.as_str());
        if let Some(msg) = store.get_message(id) {
            out.push(' ');
            out.push_str(&msg.summarize());
        }
        out.push('\n');
    }
}

/// Read a single message by ID with full content and provenance.
struct ReadMessageTool {
    store: Arc<dyn StoreAccess>,
}

#[async_trait]
impl Tool for ReadMessageTool {
    fn name(&self) -> &str {
        "readMessage"
    }

    fn description(&self) -> &str {
        "Returns the full text of a single message, and the provenance & \
         metadata associated with its creation. Useful for precise reading \
         of message data when you want to inspect a message id."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message_id": {
                    "type": "string",
                    "description": "The message ID to read."
                }
            },
            "required": ["message_id"]
        })
    }

    async fn run(&self, input: Value, _cancel: CancellationToken) -> ToolOutput {
        let store = match self.store.store() {
            Ok(s) => s,
            Err(msg) => return ToolOutput::error(&msg),
        };

        let message_id = match input.get("message_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return ToolOutput::error("missing 'message_id' parameter"),
        };

        match store.get_message(&MessageId::from(message_id)) {
            Some(msg) => ToolOutput::text(msg.display()),
            None => ToolOutput::error(&format!("message '{}' not found", message_id)),
        }
    }
}

/// The unified context algebra primitive.
///
/// Composes a new context from an ordered sequence of message entries,
/// each of which embeds a context, references an existing message, or
/// creates a new inline message. Context embeds automatically register
/// as DAG parents. Additional lineage-only parents can be specified
/// separately.
struct CreateContextTool {
    store: Arc<dyn StoreAccess>,
    /// The session this tool runs in. Composed contexts and forged
    /// messages are written into that session's family file so the family
    /// stays self-contained for deletion.
    session_id: RefId,
}

#[async_trait]
impl Tool for CreateContextTool {
    fn name(&self) -> &str {
        "createContext"
    }

    fn description(&self) -> &str {
        "Create a new context from an ordered sequence of message entries. \
         Each entry embeds a context ({\"context_id\": ...}), references an \
         existing message ({\"message_id\": ..., optional \"role\": ...}), or \
         creates a new inline message ({\"role\": ..., \"content\": ...}). \
         Returns the new context_id. Use this to compose, merge, filter, or \
         fork contexts. Prefer message_id references over inline content when \
         the data already exists in the pool. Never read a message just to \
         copy its text into an inline message -- reference it directly. \
         Pairing 'role' with 'message_id' rewrites the role: if it differs \
         from the original, a fresh message is forged with the same content \
         and a meta link back to the source (useful for replaying assistant \
         output as user input, or other context surgery). The optional \
         'merge_into' addresses the new context to another ref: if the \
         destination runs an agent loop, it discovers and merges it \
         (messages and provenance) at its next safe boundary -- the \
         data-native way to hand messages to another session."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "messages": {
                    "type": "array",
                    "description": "Ordered list of message entries. Each entry is an object \
                        matching exactly one of three shapes, identified by its keys:\n\n\
                        - Context embed: {\"context_id\": \"CTX_ID\"} expands all messages from \
                        that context in-place and registers it as a DAG parent.\n\
                        - Message reference: {\"message_id\": \"MSG_ID\"} includes a single \
                        existing message by its ID. A message id is global across the entire ri \
                        database so this can point into any session. May optionally include \
                        \"role\" to rewrite the role -- if the role differs from the original, \
                        a new message is created with the same content and meta linking back to \
                        the source. If the role matches (or is omitted), the original message is \
                        referenced unchanged.\n\
                        - Inline message: {\"role\": \"user\", \"content\": \"text\"} creates a \
                        new message. Only for genuinely new content (framing, labels, \
                        questions). If the content already exists as a message, use \
                        message_id instead.\n\n\
                        Discriminated by which of context_id / message_id / content is present \
                        (exactly one). 'role' is required with content, optional with message_id, \
                        and forbidden with context_id.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "context_id": {
                                "type": "string",
                                "description": "Embed all messages from this context into the new one."
                            },
                            "message_id": {
                                "type": "string",
                                "description": "Reference an existing message by ID from another context into this one."
                            },
                            "content": {
                                "type": "string",
                                "description": "Text content for a new inline message. \
                                    Only for genuinely new text -- if the content exists \
                                    as a message already, use message_id to reference it."
                            },
                            "role": {
                                "type": "string",
                                "enum": ["user", "assistant", "system"],
                                "description": "Required for inline messages. Optional with \
                                    message_id to rewrite the role (creates a copy when it differs \
                                    from the original)."
                            }
                        }
                    }
                },
                "parents": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Parent context IDs for DAG lineage without embedding \
                        their messages. Contexts referenced via context_id in the messages \
                        array are automatically included as parents."
                },
                "merge_into": {
                    "type": "string",
                    "description": "Address the new context to an existing ref: stamps the \
                        merge_into facet so the destination's owner (if it runs an agent \
                        loop) discovers this context at its next safe boundary and merges \
                        it -- messages woven into its head, the context recorded as a \
                        checkpoint parent. Nothing is sent and nothing wakes; an idle ref \
                        merges on its next run, and a ref that never runs simply \
                        accumulates pending envelopes. Delivery is verbatim and unframed, \
                        so include a framing message in the envelope if the recipient \
                        needs one. Pass parents for provenance (e.g. your own current \
                        context)."
                }
            },
            "required": ["messages"]
        })
    }

    async fn run(&self, input: Value, _cancel: CancellationToken) -> ToolOutput {
        let store = match self.store.store() {
            Ok(s) => s,
            Err(msg) => return ToolOutput::error(&msg),
        };

        // Composed contexts and forged messages are written into the
        // calling session's family file, keeping the family self-contained
        // for deletion. Reads (get_context/get_message) stay global.
        let family_store = match crate::chat::family_store(&store, &self.session_id) {
            Ok(s) => s,
            Err(e) => return ToolOutput::error(&format!("resolve family store: {}", e)),
        };

        let entries = match input.get("messages").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => return ToolOutput::error(
                "'messages' is required and must be an array"
            ),
        };

        let mut all_messages: Vec<MessageId> = Vec::new();
        let mut new_msg_ids: Vec<String> = Vec::new();
        let mut parents: Vec<ContextId> = Vec::new();
        let mut seen_parents: HashSet<String> = HashSet::new();

        // Addressed context: validate the destination before any message
        // is written, so a bad input has no side effects. The existence
        // check catches typo'd ids; a valid ref that never runs an agent
        // loop is still a legal destination per the design (a topic ref
        // simply accumulates pending envelopes until something merges
        // them).
        let merge_into = match input.get("merge_into") {
            None => None,
            Some(v) => match v.as_str() {
                Some(s) if !s.is_empty() => {
                    let dest = RefId::from(s);
                    if store.get_ref(&dest).is_none() {
                        return ToolOutput::error(&format!("merge_into: ref [{}] not found", s));
                    }
                    Some(dest)
                }
                _ => return ToolOutput::error("merge_into: must be a non-empty string"),
            },
        };

        for (i, entry) in entries.iter().enumerate() {
            // Discriminate by structural keys: exactly one of context_id,
            // message_id, or content must be present. 'role' is contextual --
            // required with content, optional with message_id (acting as a
            // role rewrite), and forbidden with context_id.
            let has_ctx = entry.get("context_id").is_some();
            let has_msg = entry.get("message_id").is_some();
            let has_content = entry.get("content").is_some();

            let variant_count = has_ctx as u8 + has_msg as u8 + has_content as u8;
            if variant_count != 1 {
                return ToolOutput::error(&format!(
                    "messages[{}]: each entry must specify exactly one of: \
                     {{\"context_id\"}}, {{\"message_id\", optional \"role\"}}, or \
                     {{\"role\", \"content\"}}", i
                ));
            }
            if has_ctx && entry.get("role").is_some() {
                return ToolOutput::error(&format!(
                    "messages[{}]: 'role' cannot be combined with 'context_id'", i
                ));
            }

            if has_ctx {
                let cid = match entry["context_id"].as_str() {
                    Some(s) if !s.is_empty() => s,
                    _ => return ToolOutput::error(&format!(
                        "messages[{}]: context_id must be a non-empty string", i
                    )),
                };
                match store.get_context(&ContextId::from(cid)) {
                    Some(ctx) => {
                        all_messages.extend(ctx.messages);
                        if seen_parents.insert(cid.to_string()) {
                            parents.push(ContextId::from(cid));
                        }
                    }
                    None => return ToolOutput::error(&format!(
                        "messages[{}]: context [{}] not found", i, cid
                    )),
                }
            } else if has_msg {
                let mid = match entry["message_id"].as_str() {
                    Some(s) if !s.is_empty() => s,
                    _ => return ToolOutput::error(&format!(
                        "messages[{}]: message_id must be a non-empty string", i
                    )),
                };

                // Unconditionally resolve the message: a dangling id should
                // surface here, not later when the context lands in an LLM
                // call missing a message it expected.
                let original = match store.get_message(&MessageId::from(mid)) {
                    Some(m) => m,
                    None => return ToolOutput::error(&format!(
                        "messages[{}]: message [{}] not found", i, mid
                    )),
                };

                let role_override = match entry.get("role") {
                    Some(v) => match v.as_str().and_then(parse_role) {
                        Some(r) => Some(r),
                        None => return ToolOutput::error(&format!(
                            "messages[{}]: invalid 'role' (expected user, assistant, or system)", i
                        )),
                    },
                    None => None,
                };

                // A genuine role change forges a new message that records its
                // source via meta for downstream traceability. None or a
                // matching override falls through to reusing the original.
                let resolved_id = match role_override {
                    Some(new_role) if new_role != original.role => {
                        let meta = json!({ "source_message_id": original.id });
                        let new_msg = ri::Message::new(new_role, original.content.clone(), Some(meta));
                        if let Err(e) = family_store.write_message(&new_msg) {
                            return ToolOutput::error(&format!(
                                "messages[{}]: failed to write rewritten message: {}", i, e
                            ));
                        }
                        new_msg_ids.push(new_msg.id.to_string());
                        new_msg.id
                    }
                    _ => original.id,
                };
                all_messages.push(resolved_id);
            } else {
                let role = match entry.get("role")
                    .and_then(|v| v.as_str())
                    .and_then(parse_role)
                {
                    Some(r) => r,
                    None => return ToolOutput::error(&format!(
                        "messages[{}]: inline message requires 'role' \
                         (user, assistant, or system)", i
                    )),
                };
                let content = match entry.get("content").and_then(|v| v.as_str()) {
                    Some(s) => s,
                    None => return ToolOutput::error(&format!(
                        "messages[{}]: inline message requires 'content'", i
                    )),
                };

                let msg = ri::Message::new(role, vec![ContentBlock::text(content)], None);
                if let Err(e) = family_store.write_message(&msg) {
                    return ToolOutput::error(&format!(
                        "messages[{}]: failed to write message: {}", i, e
                    ));
                }

                new_msg_ids.push(msg.id.to_string());
                all_messages.push(msg.id);
            }
        }

        // Explicit parents for lineage-only (deduplicated with auto-parents).
        if let Some(extra) = input.get("parents").and_then(|v| v.as_array()) {
            for (i, v) in extra.iter().enumerate() {
                let id = match v.as_str() {
                    Some(s) if !s.is_empty() => s,
                    _ => return ToolOutput::error(&format!(
                        "parents[{}]: must be a non-empty string", i
                    )),
                };
                if seen_parents.insert(id.to_string()) {
                    parents.push(ContextId::from(id));
                }
            }
        }

        let mut new_ctx = ri::Context::new(all_messages, parents, None);
        if let Some(dest) = &merge_into {
            if let Err(e) = new_ctx.set_facet(&crate::merge::MergeInto(dest.clone())) {
                return ToolOutput::error(&format!("merge_into: failed to stamp facet: {}", e));
            }
        }
        if let Err(e) = family_store.write_context(&new_ctx) {
            return ToolOutput::error(&format!("failed to write context: {}", e));
        }

        let ctx_id = new_ctx.id.to_string();
        let addressed = merge_into.as_ref()
            .map(|d| format!(", addressed to [{}]", d))
            .unwrap_or_default();

        let text = if !new_msg_ids.is_empty() {
            let ids: Vec<_> = new_msg_ids.iter().map(|id| format!("[{}]", id)).collect();
            format!("Created messages {}, context [{}] ({} messages total){}",
                ids.join(", "), ctx_id, new_ctx.messages.len(), addressed)
        } else {
            format!("Created context [{}] ({} messages){}", ctx_id, new_ctx.messages.len(), addressed)
        };

        ToolOutput::text(text).with_details(json!({
                "context_id": ctx_id,
                "message_ids": new_msg_ids,
            }))
    }
}

fn parse_role(s: &str) -> Option<Role> {
    match s {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        "system" => Some(Role::System),
        _ => None,
    }
}
