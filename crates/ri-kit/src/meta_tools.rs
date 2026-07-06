//! Meta-tools for orchestrating ri from within an agent loop.
//!
//! Eight tools. Six are pure data operations on the three atoms of
//! `ri::model` -- they need only a [`StoreAccess`]. `updateRef` is data plus
//! one runtime seam: persisting a ref move must also keep a resident session's
//! cached head in step, so it routes through [`MetaExec`] too. `runAgent` and
//! `readAgent` are pure runtime, because an "agent" is not an atom: it is a ref
//! plus a live loop, and "is a loop running" lives in the harness, never in the
//! DAG.
//!
//! ```text
//!             produce         read          tier
//!   Message   createMessage   readMessage   data   (StoreAccess)
//!   Context   createContext   readContext   data   (StoreAccess)
//!   Ref       updateRef       readRef       data + one runtime seam
//!   Agent     runAgent        readAgent     runtime (MetaExec)
//! ```
//!
//! `context_id` is the universal plug: `createMessage`/`createContext` produce
//! the messages and contexts an execute tool consumes, and `updateRef` points a
//! ref at one. A `session_id` (a ref id) is what `runAgent` hands back and
//! `readAgent` reads.
//!
//! The tools are built within a capability grant (`crate::caps`): [`create`]
//! takes the calling loop's effective [`CapSet`] and constructs exactly the
//! meta-tools it names -- a loop cannot hold a meta-tool it was not granted,
//! by construction. The two tools that transfer authority (`runAgent`,
//! `updateRef`) also carry the grant, because everything they convey -- a
//! forked child's grant, a continued run's toolset, another ref's rewritten
//! grant -- is bounded by its transition (see `crate::caps`).
//!
//! The tools themselves are harness-independent: names, descriptions, schemas,
//! validation, store surgery, and output formatting all live here, once, shared
//! by every harness. What a harness genuinely owns is reached through two seams:
//!
//! - [`StoreAccess`]: how to obtain the current store view. ri-web hands out a
//!   clone of its long-lived shared handle; ri-cli mounts the sessions
//!   directory fresh per call.
//! - [`MetaExec`]: model resolution, how a spawned loop runs, and the runtime
//!   status of a ref. ri-web wires a child SessionState with SSE and SSH
//!   inheritance and reads its live `live_sessions`; ri-cli spawns a
//!   self-contained background task and has no live registry to read.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use ri::{
    ContentBlock, Context, ContextId, HasMeta, Message, MessageId, Ref, RefId, Role, Store,
    ThinkingLevel, Tool, ToolOutput,
};

use crate::caps::CapSet;

/// How the meta-tools obtain the store. The harness chooses freshness and
/// lifetime semantics; `Err` carries the harness's own human-readable reason
/// and is surfaced verbatim as the tool error.
pub trait StoreAccess: Send + Sync {
    fn store(&self) -> Result<Store, String>;
}

/// Where a spawned run begins. The one field that decides fork-vs-continue;
/// the harness realizes it (mint a ref at the context, or resume an existing
/// ref's loop). `resolve_exec_request` picks the variant by whether the id
/// names a context or a ref.
pub enum ExecTarget {
    /// Fork a new session whose history begins at this context. The context's
    /// own parents are preserved, so a context composed with a parent yields a
    /// connected sub-agent, and a parentless one yields an island.
    Fork(ContextId),
    /// Continue an existing session on its current head -- run another loop
    /// (or turn) without forking, so the session's chain grows in place.
    Continue(RefId),
}

/// A validated execution request assembled by `runAgent`: where to begin
/// (`target`), the caller's model and parameter choices, and the run's
/// capability grant. `thinking: None` means "harness default".
pub struct ExecRequest {
    pub target: ExecTarget,
    pub model_id: String,
    pub thinking: Option<ThinkingLevel>,
    pub max_tokens: Option<usize>,
    /// Display label for a forked session. Ignored when continuing (the ref
    /// already has a name).
    pub label: Option<String>,
    /// The grant the spawned run executes with, fully resolved and already
    /// attenuation-checked against the caller. Empty means no function tools
    /// -- a single turn with the model's native capabilities enabled. The
    /// harness assembles the loop's tools from exactly these names, and on a
    /// fork stamps this grant onto the new ref as its `caps` facet.
    pub caps: CapSet,
}

/// Runtime status of a ref's agent loop, as the harness sees it. A ref the
/// harness is not running -- finished, never started, or not loaded -- reads as
/// idle with no preview.
pub struct AgentStatus {
    pub running: bool,
    /// Assistant text accumulated so far in a turn that is generating right
    /// now, if any. `None` when idle or between turns.
    pub streaming_preview: Option<String>,
}

/// The outcome of an `updateRef` move through [`MetaExec::repoint_ref`],
/// reported so `UpdateRefTool` can phrase the result in its own voice.
pub enum Repoint {
    /// The ref was written; a resident idle session's cached head moved with it.
    Moved,
    /// Refused, nothing written: a loop currently owns the ref and is the
    /// authoritative writer of its head. The caller phrases the refusal.
    Running,
}

/// Harness seam for the runtime tier: which models exist, how a spawned loop
/// runs, the live status of a ref, and how a ref move reaches a resident
/// session. `spawn_agent` returns the new session's ref id immediately; the
/// loop proceeds in the background.
#[async_trait]
pub trait MetaExec: Send + Sync {
    /// Model ids advertised in `runAgent`'s schema.
    fn model_ids(&self) -> Vec<String>;
    /// Spawn a detached agent loop running with `request.caps`; an empty
    /// grant yields a single turn with native capabilities on.
    async fn spawn_agent(&self, request: ExecRequest) -> Result<RefId, String>;
    /// Runtime status of a ref's loop: whether one is alive and any partial
    /// output it is streaming. The liveness oracle behind `readAgent` and the
    /// `runAgent` continue guard.
    async fn agent_status(&self, ref_id: &RefId) -> AgentStatus;
    /// Persist a validated `updateRef` move, keeping any resident session's
    /// working head in step with it. The harness owns this because "is a loop
    /// running" and "is this ref resident in memory" live in the harness, not
    /// the DAG: a resident idle session caches the head in `head_messages`, and
    /// if the ref moved without it the session's next checkpoint would weld the
    /// stale list onto the new head. Writing the ref and the cached head
    /// together, under the session's own lock, is the fix. `Running` refuses a
    /// ref a loop owns; a ref no session holds is written to disk alone (a
    /// later load hydrates the new head).
    async fn repoint_ref(&self, new_ref: &Ref) -> Result<Repoint, String>;
}

/// Build the meta-tools over the two harness seams, within a capability grant.
///
/// `session_id` is the calling session: spawned runs are parented to it, and
/// composed contexts and forged messages land in its family file. `caps` is
/// the calling loop's effective grant: exactly the meta-tools it names are
/// constructed (an ungranted meta-tool cannot be held, by construction), and
/// the two authority-transferring tools (`runAgent`, `updateRef`) carry the
/// grant as the ceiling on everything they convey.
///
/// A running ref -- including the caller's own -- is never repointed by
/// `updateRef` (the loop owns its head); a live ref relocates its head only
/// through its own loop, via a `jump` envelope (`createContext` /
/// `crate::envelope`).
pub fn create(
    store: Arc<dyn StoreAccess>,
    exec: Arc<dyn MetaExec>,
    session_id: RefId,
    caps: &CapSet,
) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    if caps.contains("createMessage") {
        tools.push(Box::new(CreateMessageTool { store: store.clone(), session_id: session_id.clone() }));
    }
    if caps.contains("readMessage") {
        tools.push(Box::new(ReadMessageTool { store: store.clone() }));
    }
    if caps.contains("createContext") {
        tools.push(Box::new(CreateContextTool { store: store.clone(), session_id: session_id.clone() }));
    }
    if caps.contains("readContext") {
        tools.push(Box::new(ReadContextTool { store: store.clone() }));
    }
    if caps.contains("updateRef") {
        tools.push(Box::new(UpdateRefTool {
            store: store.clone(),
            exec: exec.clone(),
            session_id,
            caps: caps.clone(),
        }));
    }
    if caps.contains("readRef") {
        tools.push(Box::new(ReadRefTool { store: store.clone() }));
    }
    if caps.contains(RUN_AGENT) {
        tools.push(Box::new(RunAgentTool { store: store.clone(), exec: exec.clone(), caps: caps.clone() }));
    }
    if caps.contains("readAgent") {
        tools.push(Box::new(ReadAgentTool { store, exec }));
    }
    // `TOOL_NAMES` is a hand-maintained mirror used to advertise the meta-tool
    // inventory; assert the grant-filtered build matches it, loudly and in
    // debug, so a tool added here but forgotten there can't silently become
    // un-buildable.
    debug_assert!(
        tools.iter().map(|t| t.name())
            .eq(TOOL_NAMES.iter().copied().filter(|n| caps.contains(n))),
        "meta-tool set drifted from TOOL_NAMES"
    );
    tools
}

/// The name of the `runAgent` meta-tool. Called out as a constant because it
/// is the budgeted capability in practice: harnesses grant it with a transfer
/// budget (`crate::caps`), decremented at each hand-down, so a holder with a
/// spent budget cannot pass it on at all -- recursion is bounded by data, not
/// by construction-site special cases.
pub const RUN_AGENT: &str = "runAgent";

/// The names of the eight meta-tools, in the order [`create`] builds them. The
/// meta-tool inventory a harness combines with its base tool names when
/// minting a root grant. Must stay in sync with the set [`create`] can return.
pub const TOOL_NAMES: &[&str] = &[
    "createMessage",
    "readMessage",
    "createContext",
    "readContext",
    "updateRef",
    "readRef",
    RUN_AGENT,
    "readAgent",
];

// -- Message -----------------------------------------------------------------

/// Mint message atoms: the one place messages are created.
///
/// A message is either fresh text (`role` + `content`) or a role rewrite of an
/// existing message (`role` + `from`), which forges a copy of the source's
/// content under a new role with a provenance backlink. Composition is a
/// separate concern -- `createContext` references these ids, never mints.
struct CreateMessageTool {
    store: Arc<dyn StoreAccess>,
    /// The calling session. Forged messages are written into its family file so
    /// the family stays self-contained for deletion.
    session_id: RefId,
}

#[async_trait]
impl Tool for CreateMessageTool {
    fn name(&self) -> &str {
        "createMessage"
    }

    fn description(&self) -> &str {
        "Create one or more message atoms and return their ids. Each entry is \
         either a new text message ({\"role\": ..., \"content\": ...}) or a role \
         rewrite of an existing message ({\"role\": ..., \"from\": MSG_ID}), which \
         forges a copy of that message's content under the new role and records \
         the source for traceability (useful for replaying assistant output as \
         user input). A rewrite to the same role the message already has returns \
         the original id unchanged. Messages are the immutable content atoms; \
         compose them into a context with createContext."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "messages": {
                    "type": "array",
                    "description": "Ordered messages to create. Each entry has a \
                        'role' plus exactly one of: 'content' (text for a new \
                        message) or 'from' (an existing message id to copy under \
                        the new role).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "role": {
                                "type": "string",
                                "enum": ["user", "assistant", "system"],
                                "description": "Role of the created message."
                            },
                            "content": {
                                "type": "string",
                                "description": "Text content for a new message."
                            },
                            "from": {
                                "type": "string",
                                "description": "Existing message id whose content \
                                    is copied under 'role' (a role rewrite). \
                                    Records the source via meta."
                            }
                        },
                        "required": ["role"]
                    }
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
        let family_store = match crate::chat::family_store(&store, &self.session_id) {
            Ok(s) => s,
            Err(e) => return ToolOutput::error(&format!("resolve family store: {}", e)),
        };

        let entries = match input.get("messages").and_then(|v| v.as_array()) {
            Some(arr) if !arr.is_empty() => arr,
            _ => return ToolOutput::error(
                "'messages' is required and must be a non-empty array"
            ),
        };

        let mut created: Vec<String> = Vec::new();
        let mut all_ids: Vec<String> = Vec::new();

        for (i, entry) in entries.iter().enumerate() {
            let role = match entry.get("role").and_then(|v| v.as_str()).and_then(parse_role) {
                Some(r) => r,
                None => return ToolOutput::error(&format!(
                    "messages[{}]: 'role' is required (user, assistant, or system)", i
                )),
            };

            let has_content = entry.get("content").is_some();
            let has_from = entry.get("from").is_some();
            if has_content == has_from {
                return ToolOutput::error(&format!(
                    "messages[{}]: specify exactly one of 'content' (new text) or \
                     'from' (role rewrite of an existing message)", i
                ));
            }

            let id = if has_content {
                let content = match entry["content"].as_str() {
                    Some(s) => s,
                    None => return ToolOutput::error(&format!(
                        "messages[{}]: 'content' must be a string", i
                    )),
                };
                let msg = Message::new(role, vec![ContentBlock::text(content)], None);
                if let Err(e) = family_store.write_message(&msg) {
                    return ToolOutput::error(&format!(
                        "messages[{}]: failed to write message: {}", i, e
                    ));
                }
                created.push(msg.id.to_string());
                msg.id
            } else {
                let from = match entry["from"].as_str() {
                    Some(s) if !s.is_empty() => s,
                    _ => return ToolOutput::error(&format!(
                        "messages[{}]: 'from' must be a non-empty message id", i
                    )),
                };
                // Resolve the source now: a dangling id should surface here, not
                // later when the message lands in an LLM call missing content.
                let original = match store.get_message(&MessageId::from(from)) {
                    Some(m) => m,
                    None => return ToolOutput::error(&format!(
                        "messages[{}]: message [{}] not found", i, from
                    )),
                };
                // A genuine role change forges a new message recording its
                // source; a same-role rewrite is a no-op that reuses the original.
                if role == original.role {
                    original.id
                } else {
                    let meta = json!({ "source_message_id": original.id });
                    let forged = Message::new(role, original.content.clone(), Some(meta));
                    if let Err(e) = family_store.write_message(&forged) {
                        return ToolOutput::error(&format!(
                            "messages[{}]: failed to write rewritten message: {}", i, e
                        ));
                    }
                    created.push(forged.id.to_string());
                    forged.id
                }
            };
            all_ids.push(id.to_string());
        }

        let ids: Vec<_> = all_ids.iter().map(|id| format!("[{}]", id)).collect();
        ToolOutput::text(format!("Created messages {}", ids.join(", ")))
            .with_details(json!({ "message_ids": all_ids, "created_ids": created }))
    }
}

/// Read a single message by id with full content and provenance.
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

// -- Context -----------------------------------------------------------------

/// Compose a new context from existing atoms: reference messages by id, embed
/// whole contexts (registering them as DAG parents), and add lineage-only
/// parents. Pure composition -- to create new message content, use
/// `createMessage` first and reference the returned id.
struct CreateContextTool {
    store: Arc<dyn StoreAccess>,
    /// The calling session. Composed contexts are written into its family file
    /// so the family stays self-contained for deletion.
    session_id: RefId,
}

#[async_trait]
impl Tool for CreateContextTool {
    fn name(&self) -> &str {
        "createContext"
    }

    fn description(&self) -> &str {
        "Create a new context (an ordered list of messages plus parent links) \
         from existing atoms. Each entry either references a message \
         ({\"message_id\": MSG_ID}) or embeds a whole context ({\"context_id\": \
         CTX_ID}) -- a context id, or a ref id resolved to its current head -- \
         expanding its messages in place and registering it as a DAG parent. The \
         optional top-level 'exclude' then drops named message ids from the \
         assembled list, so you can take a long context minus a few messages \
         without relisting the rest. Returns the new context_id. Use this to \
         compose, merge, filter, fork, extend, or wrap contexts. This tool only \
         references existing atoms -- to author \
         new message text or rewrite a role, call createMessage first and \
         reference the id it returns. The optional 'merge_into' addresses the \
         context to another ref: if that ref's owner runs an agent loop, it \
         discovers and merges the context (messages and provenance) at its next \
         safe boundary -- the data-native way to hand messages to another \
         session without owning its ref. The optional 'jump' instead addresses \
         a relocation: at its next safe boundary the owner preserves its \
         current head as a live snapshot ref, then resumes from the target with \
         this context's messages appended -- this is how an agent rewinds or \
         branches itself. 'merge_into' and \
         'jump' are mutually exclusive (a context carries one instruction)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "messages": {
                    "type": "array",
                    "description": "Ordered entries, each matching exactly one of:\n\
                        - {\"message_id\": MSG_ID}: include one existing message. \
                        Ids are global across the whole ri database, so this can \
                        point into any session.\n\
                        - {\"context_id\": ID}: expand all messages of a context \
                        (a context id, or a ref id resolved to its current head) \
                        in place, and register the context as a DAG parent.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "message_id": {
                                "type": "string",
                                "description": "Reference an existing message by id."
                            },
                            "context_id": {
                                "type": "string",
                                "description": "Embed all messages from a context, registering it as a parent. Accepts a context id, or a ref id resolved to its current head context (snapshotted now)."
                            }
                        }
                    }
                },
                "exclude": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Message ids to drop from the final assembled \
                        list (after every entry expands). Removes all occurrences \
                        of each id; an id matching no assembled message is an \
                        error. The ergonomic way to take a long context minus a \
                        few messages without relisting the rest."
                },
                "parents": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Lineage-only parents for the new context, \
                        without embedding their messages. Each accepts a context \
                        id (used as-is) or a ref id (snapshotted to its current \
                        head context at author time); an unknown id is an error. \
                        Contexts embedded via context_id are already registered \
                        as parents."
                },
                "merge_into": {
                    "type": "string",
                    "description": "Address the new context to an existing ref: \
                        stamps the merge_into facet so the destination's owner \
                        (if it runs an agent loop) discovers this context at its \
                        next safe boundary and merges it -- messages woven into \
                        its head, the context recorded as a checkpoint parent. \
                        Nothing is sent and nothing wakes; an idle ref merges on \
                        its next run. Delivery is verbatim and unframed, so \
                        include a framing message if the recipient needs one. \
                        Pass parents for provenance (e.g. your own current \
                        context)."
                },
                "jump": {
                    "type": "object",
                    "description": "Address a head relocation to a ref. At its \
                        next safe boundary -- not when this call returns -- the \
                        owner preserves its current head as a live snapshot \
                        sub-session, then resumes on a fresh head: the target's \
                        messages, followed by any messages you composed into \
                        this context, parented on the old head, the target, and \
                        this context. Until that boundary the ref keeps its \
                        current head. This is the pull-based way an agent rewinds \
                        or branches itself; the loop owns its head, so it cannot \
                        be repointed from outside. Mutually exclusive with \
                        merge_into.",
                    "properties": {
                        "target": {
                            "type": "string",
                            "description": "Where the head should land: a \
                                context id (used as-is, immutable) or a ref id \
                                (resolved to its current head context now, at \
                                construction). The relocation lands at this \
                                fixed point even if applied later."
                        },
                        "to": {
                            "type": "string",
                            "description": "The ref whose head should jump. \
                                Defaults to the calling session -- a self-jump \
                                (rewind/branch yourself). Set it to address a \
                                jump to another ref's owner."
                        }
                    },
                    "required": ["target"]
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

        // Resolve the optional addressing (merge_into / jump) into an envelope
        // before any write, so a bad address has no side effects. A valid ref
        // that never runs an agent loop is still a legal destination (a topic
        // ref accumulates pending envelopes).
        let envelope = match resolve_envelope(&store, &self.session_id, &input) {
            Ok(e) => e,
            Err(msg) => return ToolOutput::error(&msg),
        };

        let mut all_messages: Vec<MessageId> = Vec::new();
        let mut parents: Vec<ContextId> = Vec::new();
        let mut seen_parents: HashSet<String> = HashSet::new();

        for (i, entry) in entries.iter().enumerate() {
            let has_ctx = entry.get("context_id").is_some();
            let has_msg = entry.get("message_id").is_some();
            if has_ctx == has_msg {
                return ToolOutput::error(&format!(
                    "messages[{}]: each entry must specify exactly one of \
                     {{\"message_id\"}} or {{\"context_id\"}}. To author new text \
                     or rewrite a role, use createMessage and reference its id.", i
                ));
            }

            if has_ctx {
                let cid = match entry["context_id"].as_str() {
                    Some(s) if !s.is_empty() => s,
                    _ => return ToolOutput::error(&format!(
                        "messages[{}]: context_id must be a non-empty string", i
                    )),
                };
                // ctx-or-ref: a known context is used as-is, a ref resolves to
                // its head. Register the resolved context -- never the ref id --
                // as the parent, so the DAG never holds a ref where a context id
                // belongs.
                let resolved = match resolve_to_context_id(&store, cid) {
                    Some(c) => c,
                    None => return ToolOutput::error(&format!(
                        "messages[{}]: [{}] is neither a known context nor a ref", i, cid
                    )),
                };
                let ctx = match store.get_context(&resolved) {
                    Some(c) => c,
                    // Only reachable when cid named a ref whose head context is
                    // missing; a cid that named a context already resolved by
                    // being fetched inside resolve_to_context_id.
                    None => return ToolOutput::error(&format!(
                        "messages[{}]: ref [{}] points at a missing head context [{}]",
                        i, cid, resolved
                    )),
                };
                all_messages.extend(ctx.messages);
                if seen_parents.insert(resolved.to_string()) {
                    parents.push(resolved);
                }
            } else {
                let mid = match entry["message_id"].as_str() {
                    Some(s) if !s.is_empty() => s,
                    _ => return ToolOutput::error(&format!(
                        "messages[{}]: message_id must be a non-empty string", i
                    )),
                };
                // Resolve to surface a dangling id here, not later in an LLM call.
                if store.get_message(&MessageId::from(mid)).is_none() {
                    return ToolOutput::error(&format!(
                        "messages[{}]: message [{}] not found", i, mid
                    ));
                }
                all_messages.push(MessageId::from(mid));
            }
        }

        // Explicit lineage-only parents, deduplicated against the embed-slot
        // parents above. ctx-or-ref: a known context is used as-is, a ref
        // resolves to its head -- register the resolved context, never the ref
        // id, so the DAG never holds a ref where a context id belongs.
        if let Some(extra) = input.get("parents").and_then(|v| v.as_array()) {
            for (i, v) in extra.iter().enumerate() {
                let id = match v.as_str() {
                    Some(s) if !s.is_empty() => s,
                    _ => return ToolOutput::error(&format!(
                        "parents[{}]: must be a non-empty string", i
                    )),
                };
                let resolved = match resolve_to_context_id(&store, id) {
                    Some(c) => c,
                    None => return ToolOutput::error(&format!(
                        "parents[{}]: [{}] is neither a known context nor a ref", i, id
                    )),
                };
                // Only reachable when id named a ref whose head context is
                // missing (e.g. its family was deleted); a context id already
                // proved present inside resolve_to_context_id.
                if store.get_context(&resolved).is_none() {
                    return ToolOutput::error(&format!(
                        "parents[{}]: ref [{}] points at a missing head context [{}]", i, id, resolved
                    ));
                }
                if seen_parents.insert(resolved.to_string()) {
                    parents.push(resolved);
                }
            }
        }

        // Top-level exclude: drop every occurrence of each id from the assembled
        // list, applied once after assembly so the result is independent of
        // entry order. An id that matches nothing is a canary (a stale id or the
        // wrong source context), surfaced before any write -- consistent with
        // the dangling-reference handling above.
        let mut excluded_count = 0usize;
        if let Some(raw) = input.get("exclude") {
            let arr = match raw.as_array() {
                Some(a) => a,
                None => return ToolOutput::error("'exclude' must be an array of message ids"),
            };
            let mut drop: HashSet<String> = HashSet::new();
            for (i, v) in arr.iter().enumerate() {
                match v.as_str() {
                    Some(s) if !s.is_empty() => { drop.insert(s.to_string()); }
                    _ => return ToolOutput::error(&format!(
                        "exclude[{}]: must be a non-empty message id string", i
                    )),
                }
            }
            if !drop.is_empty() {
                let present: HashSet<&str> = all_messages.iter().map(|m| m.as_str()).collect();
                let mut unmatched: Vec<&str> =
                    drop.iter().map(|s| s.as_str()).filter(|s| !present.contains(s)).collect();
                if !unmatched.is_empty() {
                    unmatched.sort();
                    let list = unmatched.iter().map(|s| format!("[{}]", s)).collect::<Vec<_>>().join(", ");
                    return ToolOutput::error(&format!(
                        "exclude: {} not present in the assembled {}-message list",
                        list, all_messages.len()
                    ));
                }
                let before = all_messages.len();
                all_messages.retain(|m| !drop.contains(m.as_str()));
                excluded_count = before - all_messages.len();
            }
        }

        let mut new_ctx = Context::new(all_messages, parents, None);
        if let Some(env) = &envelope {
            if let Err(e) = new_ctx.set_facet(env) {
                return ToolOutput::error(&format!("failed to stamp envelope facet: {}", e));
            }
        }
        if let Err(e) = family_store.write_context(&new_ctx) {
            return ToolOutput::error(&format!("failed to write context: {}", e));
        }

        let ctx_id = new_ctx.id.to_string();
        let addressed = match &envelope {
            Some(crate::envelope::Envelope { to, instruction: crate::envelope::Instruction::Merge }) => {
                format!(", merge_into [{}]", to)
            }
            Some(crate::envelope::Envelope { to, instruction: crate::envelope::Instruction::Jump { target } }) => {
                format!(", jump [{}] -> target [{}]", to, target)
            }
            None => String::new(),
        };
        let counts = if excluded_count > 0 {
            format!("{} messages, {} excluded", new_ctx.messages.len(), excluded_count)
        } else {
            format!("{} messages", new_ctx.messages.len())
        };
        ToolOutput::text(format!(
            "Created context [{}] ({}){}", ctx_id, counts, addressed
        )).with_details(json!({ "context_id": ctx_id }))
    }
}

/// Parse the optional addressing parameters (`merge_into`, `jump`) into an
/// [`crate::envelope::Envelope`], validating every ref and resolving a jump
/// target to an immutable context id -- all before any write, so a bad
/// address is surfaced with no side effects.
///
/// The two are mutually exclusive: a context carries one envelope with one
/// instruction. `merge_into` is the merge shorthand; `jump` defaults its
/// `to` to the calling session (a self-rewind) and resolves its `target`
/// now, so a deferred jump lands at a fixed point regardless of later head
/// movement.
fn resolve_envelope(
    store: &Store,
    session_id: &RefId,
    input: &Value,
) -> Result<Option<crate::envelope::Envelope>, String> {
    use crate::envelope::{Envelope, Instruction};

    let has_merge = input.get("merge_into").map_or(false, |v| !v.is_null());
    let has_jump = input.get("jump").map_or(false, |v| !v.is_null());
    if has_merge && has_jump {
        return Err(
            "specify at most one of 'merge_into' or 'jump' -- a context carries one instruction"
                .to_string(),
        );
    }

    if has_merge {
        let s = input["merge_into"].as_str().filter(|s| !s.is_empty())
            .ok_or("merge_into: must be a non-empty string")?;
        let dest = RefId::from(s);
        if store.get_ref(&dest).is_none() {
            return Err(format!("merge_into: ref [{}] not found", s));
        }
        return Ok(Some(Envelope { to: dest, instruction: Instruction::Merge }));
    }

    if has_jump {
        let obj = input["jump"].as_object()
            .ok_or("jump: must be an object { \"target\": ..., \"to\"?: ... }")?;
        // `to` defaults to the calling session -- the self-jump (rewind/branch
        // yourself) case the operator framed; set it to address another ref.
        let to = match obj.get("to").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            Some(s) => {
                let r = RefId::from(s);
                if store.get_ref(&r).is_none() {
                    return Err(format!("jump.to: ref [{}] not found", s));
                }
                r
            }
            None => session_id.clone(),
        };
        let target_str = obj.get("target").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
            .ok_or("jump.target: required (a context id, or a ref id resolved to its head)")?;
        let target = resolve_to_context_id(store, target_str)
            .ok_or_else(|| format!("jump.target: [{}] is neither a known context nor a ref", target_str))?;
        return Ok(Some(Envelope { to, instruction: Instruction::Jump { target } }));
    }

    Ok(None)
}

/// Resolve an id that may be a context id or a ref id into an immutable context
/// id: a known context is used as-is; a known ref resolves to its current head
/// context (snapshotted now). `None` when the id names neither -- the caller
/// phrases the error with its own context. The single "ctx-or-ref -> context"
/// resolver, shared by context embedding and `jump.target`, so author-time "use
/// ref R" sugar lands at a fixed point everywhere, never re-chasing a moving ref.
fn resolve_to_context_id(store: &Store, s: &str) -> Option<ContextId> {
    let cid = ContextId::from(s);
    if store.get_context(&cid).is_some() {
        return Some(cid);
    }
    store.get_ref(&RefId::from(s)).map(|r| r.head)
}

/// Default ancestor-walk depth for `readContext`.
const CONTEXT_DEPTH: usize = 10;

/// Read one context and how it sits in the DAG: its full message list, its
/// immediate parents and children, and its ancestor chain rendered as per-step
/// diffs. The focused replacement for the old bidirectional graph dump.
struct ReadContextTool {
    store: Arc<dyn StoreAccess>,
}

#[async_trait]
impl Tool for ReadContextTool {
    fn name(&self) -> &str {
        "readContext"
    }

    fn description(&self) -> &str {
        "Read a context and its place in the history DAG. Give a context_id, or \
         a session_id to read its current head. Shows the context's full message \
         list (with inline summaries), its immediate parents and children for \
         navigation, and its ancestor chain as a diff at each step (what that \
         context added or removed versus its parent). 'depth' bounds the \
         ancestor walk (default 10). Or pass 'diff_base' (a context id you \
         already read) to see only what's changed since then -- the new \
         messages, any removed ones noted, the full list and ancestor walk \
         omitted. Ideal for cheaply polling a session for what's new."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "context_id": {
                    "type": "string",
                    "description": "Context to read. Takes precedence over session_id."
                },
                "session_id": {
                    "type": "string",
                    "description": "Session (ref) to read -- resolves to its head context."
                },
                "depth": {
                    "type": "integer",
                    "description": "How far up the ancestor chain to walk. Default 10. Ignored when diff_base is set."
                },
                "diff_base": {
                    "type": "string",
                    "description": "Optional context id to diff this context against. \
                        When set, the reply shows only this context's messages that \
                        are not already in the base (what's new since you last saw \
                        it), with any removed messages noted; the full message list \
                        and ancestor walk are omitted. Must be a context id (the one \
                        you last read), not a session id."
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
            .unwrap_or(CONTEXT_DEPTH);

        // Resolve the focal context: context_id takes precedence, then session_id.
        let focal_id: ContextId = if let Some(cid) = input.get("context_id").and_then(|v| v.as_str()) {
            ContextId::from(cid)
        } else if let Some(sid) = input.get("session_id").and_then(|v| v.as_str()) {
            match store.get_ref(&RefId::from(sid)).map(|r| r.head) {
                Some(head) => head,
                None => return ToolOutput::error(&format!("session '{}' not found", sid)),
            }
        } else {
            return ToolOutput::error("either 'context_id' or 'session_id' is required");
        };

        let focal = match store.get_context(&focal_id) {
            Some(c) => c,
            None => return ToolOutput::error(&format!("context '{}' not found", focal_id)),
        };

        // Optional diff base. Resolved as a context id only -- never a ref:
        // a ref resolves to the live head (= focal here), which would silently
        // diff focal against itself. A present-but-malformed base (wrong type,
        // empty, or an unknown id) is surfaced loudly rather than ignored.
        let diff_base = if let Some(raw) = input.get("diff_base") {
            match raw.as_str() {
                Some(s) if !s.is_empty() => {
                    let base_id = ContextId::from(s);
                    match store.get_context(&base_id) {
                        Some(c) => Some(c),
                        None => return ToolOutput::error(&format!("diff_base context '{}' not found", base_id)),
                    }
                }
                _ => return ToolOutput::error("'diff_base' must be a context id string"),
            }
        } else {
            None
        };

        let mut out = format!("CONTEXT {}\n", focal_id);
        out.push_str(&format!("parents: {}\n", id_list(&focal.parents)));
        let children: Vec<ContextId> = store.children(&focal_id).into_iter().map(|c| c.id).collect();
        out.push_str(&format!("children: {}\n", id_list_capped(&children, 12)));

        // A diff base turns the read into a delta against it -- the messages new
        // to focal since the caller last saw that base. Without one, the full
        // message list plus the ancestry log: the two other reads a node affords.
        if let Some(base) = diff_base {
            append_base_diff(&mut out, &store, &base, &focal);
        } else {
            out.push_str(&format!("\nmessages ({}):\n", focal.messages.len()));
            append_message_list(&mut out, &store, &focal.messages);
            append_ancestors(&mut out, &store, &focal, depth);
        }

        ToolOutput::text(out)
    }
}

/// Render `focal` as a two-tree diff against `base`: the messages in focal not
/// already in base, and any base had that focal does not. A pure set difference
/// over message ids -- no chain walk, well-defined for any two contexts.
fn append_base_diff(out: &mut String, store: &Store, base: &Context, focal: &Context) {
    let (added, removed) = diff_message_lists(&base.messages, &focal.messages);
    if added.is_empty() && removed.is_empty() {
        out.push_str(&format!("\nno new messages since {}\n", base.id));
        return;
    }
    if removed.is_empty() {
        out.push_str(&format!("\n{} new since {}:\n", added.len(), base.id));
    } else {
        out.push_str(&format!("\n{} new, {} removed since {}:\n", added.len(), removed.len(), base.id));
    }
    append_diff(out, store, &added, &removed);
}

/// Walk the first-parent chain from `focal`, rendering each ancestor as a diff
/// against its own parent. First-parent is the checkpoint spine; merge parents
/// (e.g. addressed-context envelopes) still show in each context's `parents:`
/// line above, but aren't traversed here so the history reads linearly.
fn append_ancestors(out: &mut String, store: &Store, focal: &Context, depth: usize) {
    let mut current = focal.clone();
    let mut shown = 0usize;
    while let Some(parent_id) = current.parents.first().cloned() {
        if shown >= depth {
            out.push_str(&format!("\n... (ancestor walk stopped at depth {})\n", depth));
            return;
        }
        let Some(parent) = store.get_context(&parent_id) else {
            out.push_str(&format!("\n{} <- {} (not loaded)\n", current.id, parent_id));
            return;
        };
        if shown == 0 {
            out.push_str("\nancestors:\n");
        }
        out.push_str(&format!("\n{} <- {}\n", current.id, parent_id));
        // First-parent is the spine; a merge context has more, and its diff
        // here is only against that first parent -- say so rather than hide it.
        if current.parents.len() > 1 {
            out.push_str(&format!("  ({} parents; diff is vs the first)\n", current.parents.len()));
        }
        let (added, removed) = diff_message_lists(&parent.messages, &current.messages);
        append_diff(out, store, &added, &removed);
        current = parent;
        shown += 1;
    }
    if shown == 0 {
        out.push_str("\nancestors: none (root context)\n");
    }
}

// -- Ref ---------------------------------------------------------------------

/// Create or update a ref: point `ref_id` at `context_id`, creating the ref as
/// a bare pointer if it doesn't exist, and optionally (re)write its capability
/// grant. Enforces single ownership -- a ref with a running agent loop is
/// refused, because that loop is the authoritative writer of its head and
/// would silently overwrite the move at its next checkpoint; influence such a
/// ref via `createContext merge_into`, or move it once idle. The move goes
/// through the [`MetaExec::repoint_ref`] seam, which keeps a resident session's
/// cached head in step with the ref. A swap that drops the prior head out of
/// the new head's ancestry is surfaced, not blocked.
struct UpdateRefTool {
    store: Arc<dyn StoreAccess>,
    exec: Arc<dyn MetaExec>,
    /// The calling session, so the guard can phrase a self-targeted refusal:
    /// a session cannot repoint its own head from inside its running loop.
    session_id: RefId,
    /// The calling loop's grant: the ceiling on any `caps` it writes. Like
    /// every capability transfer, a written grant is bounded by this set's
    /// transition (`crate::caps`).
    caps: CapSet,
}

#[async_trait]
impl Tool for UpdateRefTool {
    fn name(&self) -> &str {
        "updateRef"
    }

    fn description(&self) -> &str {
        "Create or update a ref: a named, mutable pointer to a context (a \
         session is a ref). Points ref_id at context_id, creating the ref as a \
         bare pointer (no chat facet, so it won't appear in the session list) if \
         it doesn't already exist. Optionally sets the ref's capability grant \
         via 'caps' -- the toolset it runs with when continued -- bounded by \
         what this session can convey. A ref has exactly one owner: while an \
         agent loop is running on ref_id this is refused -- that loop owns its \
         head and would overwrite the move -- so deliver to a running ref with \
         createContext merge_into instead, or move it once idle. The move is a \
         raw pointer swap; if context_id does not descend from the ref's current \
         head, the response notes that the previous chain history was left \
         behind."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref_id": {
                    "type": "string",
                    "description": "The ref to create or move."
                },
                "context_id": {
                    "type": "string",
                    "description": "The context the ref should point at (its new head)."
                },
                "caps": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": format!(
                        "Optionally replace the ref's capability grant: the tool \
                         names it runs with when continued. Each name must be \
                         conveyable by this session; a budgeted capability \
                         (runAgent) is granted with its transfer budget reduced \
                         by one. Pass base names only -- 'runAgent', never \
                         'runAgent(1)'. You can convey: {}. Omit to leave the \
                         ref's grant unchanged.",
                        self.caps.transition().describe())
                }
            },
            "required": ["ref_id", "context_id"]
        })
    }

    async fn run(&self, input: Value, _cancel: CancellationToken) -> ToolOutput {
        let store = match self.store.store() {
            Ok(s) => s,
            Err(msg) => return ToolOutput::error(&msg),
        };

        let ref_id = match input.get("ref_id").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => RefId::from(s),
            _ => return ToolOutput::error("missing 'ref_id' parameter"),
        };
        let head = match input.get("context_id").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => ContextId::from(s),
            _ => return ToolOutput::error("missing 'context_id' parameter"),
        };

        // Resolve the requested grant before any write, so an unconveyable
        // name has no side effects. Bounded like every transfer: by the
        // transition of the caller's own grant.
        let new_caps = match input.get("caps") {
            None | Some(Value::Null) => None,
            Some(Value::Array(arr)) => {
                let mut names = Vec::with_capacity(arr.len());
                for v in arr {
                    match v.as_str() {
                        Some(s) if !s.is_empty() => names.push(s.to_string()),
                        _ => return ToolOutput::error("caps: each entry must be a tool name string"),
                    }
                }
                match self.caps.grant(Some(&names)) {
                    Ok(c) => Some(c),
                    Err(e) => return ToolOutput::error(&format!("caps: {}", e)),
                }
            }
            Some(_) => return ToolOutput::error("caps: must be an array of tool names"),
        };

        // Surface a dangling head now, not later when the ref resolves to a
        // missing context.
        if store.get_context(&head).is_none() {
            return ToolOutput::error(&format!("context '{}' not found", head));
        }

        let existing = store.get_ref(&ref_id);

        let (mut new_ref, created, prior_head) = match &existing {
            Some(r) => (r.clone().with_head(head.clone()), false, Some(r.head.clone())),
            None => (Ref::with_id(ref_id.clone(), head.clone()), true, None),
        };
        if let Some(c) = &new_caps {
            if let Err(e) = new_ref.set_facet(c) {
                return ToolOutput::error(&format!("failed to encode caps facet: {}", e));
            }
        }

        // Single ownership, enforced by the harness under one lock: a ref a
        // running loop owns is never repointed -- the loop is its authoritative
        // head-writer (this holds even when the loop is the caller, since a tool
        // call runs inside it). The seam performs the guarded move and reports
        // whether it happened.
        match self.exec.repoint_ref(&new_ref).await {
            Ok(Repoint::Moved) => {}
            Ok(Repoint::Running) => {
                let hint = if ref_id == self.session_id {
                    "your own loop owns your head while you run -- relocate it with a \
                     createContext jump envelope, which the loop applies at its next boundary"
                } else {
                    "address it with createContext merge_into (weave messages) or jump \
                     (relocate its head) instead, or move it once idle"
                };
                return ToolOutput::error(&format!(
                    "ref [{}] is owned by a running agent loop; {}", ref_id, hint
                ));
            }
            Err(e) => return ToolOutput::error(&e),
        }

        // Chain canary: a raw swap can drop the prior head out of the new
        // head's ancestry, which strands merge receipts (re-delivery) and
        // detaches history. Surfaced as a note; never blocks the move.
        let severed_note = prior_head.as_ref().and_then(|prior| {
            let reachable = crate::envelope::reachable_contexts(store.pool(), &head);
            if reachable.contains(prior) {
                None
            } else {
                Some(format!(
                    "\nnote: [{}] does not descend from the previous head [{}]; \
                     this ref's prior chain history was left behind",
                    head, prior
                ))
            }
        });

        let verb = if created { "Created" } else { "Updated" };
        let mut text = format!("{} ref [{}] -> [{}]", verb, ref_id, head);
        if let Some(c) = &new_caps {
            text.push_str(&format!("; caps set to {}", c.describe()));
        }
        if let Some(note) = severed_note {
            text.push_str(&note);
        }
        ToolOutput::text(text).with_details(json!({
            "ref_id": ref_id.to_string(),
            "head": head.to_string(),
            "created": created,
        }))
    }
}

/// Read a ref as data: its head, parent-chain size, facets, and its pending
/// envelope inbox (instructions addressed to it that its chain hasn't applied).
/// Pure data -- for whether a loop is alive on the ref, use `readAgent`.
struct ReadRefTool {
    store: Arc<dyn StoreAccess>,
}

#[async_trait]
impl Tool for ReadRefTool {
    fn name(&self) -> &str {
        "readRef"
    }

    fn description(&self) -> &str {
        "Read a ref (a named pointer to a context; a session is a ref) as data: \
         its head context, how many contexts its chain reaches, the facets \
         attached to it (chat title/parent/pinned, or 'raw' if none), and its \
         pending envelope inbox -- instructions (merge or jump) addressed to \
         this ref that its chain has not applied yet. This is pure data from \
         the pool; for live runtime status (is a loop running, what is it \
         streaming) use readAgent."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref_id": {
                    "type": "string",
                    "description": "The ref to read."
                }
            },
            "required": ["ref_id"]
        })
    }

    async fn run(&self, input: Value, _cancel: CancellationToken) -> ToolOutput {
        let store = match self.store.store() {
            Ok(s) => s,
            Err(msg) => return ToolOutput::error(&msg),
        };

        let ref_id = match input.get("ref_id").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => RefId::from(s),
            _ => return ToolOutput::error("missing 'ref_id' parameter"),
        };

        let r = match store.get_ref(&ref_id) {
            Some(r) => r,
            None => return ToolOutput::error(&format!("ref '{}' not found", ref_id)),
        };

        let reachable = crate::envelope::reachable_contexts(store.pool(), &r.head);
        let pending = crate::envelope::pending_envelopes(store.pool(), &ref_id, &reachable);

        let mut out = format!("REF {}\n", ref_id);
        out.push_str(&format!("head: {}\n", r.head));
        out.push_str(&format!("chain: {} contexts reachable\n", reachable.len()));
        out.push_str(&format!("facets: {}\n", describe_facets(&r)));
        let caps_line = match r.facet::<CapSet>() {
            Some(Ok(c)) => c.describe(),
            Some(Err(e)) => format!("(malformed: {})", e),
            None => "(no grant)".to_string(),
        };
        out.push_str(&format!("caps: {}\n", caps_line));

        out.push_str(&format!(
            "\ninbox ({} pending envelope{}):\n",
            pending.len(), if pending.len() == 1 { "" } else { "s" }
        ));
        if pending.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for env in &pending {
                let verb = match env.facet::<crate::envelope::Envelope>() {
                    Some(Ok(crate::envelope::Envelope {
                        instruction: crate::envelope::Instruction::Merge, ..
                    })) => "merge".to_string(),
                    Some(Ok(crate::envelope::Envelope {
                        instruction: crate::envelope::Instruction::Jump { target }, ..
                    })) => format!("jump -> {}", target),
                    _ => "?".to_string(),
                };
                out.push_str(&format!("  {} [{}] ({} messages)", env.id, verb, env.messages.len()));
                if let Some(first) = env.messages.first().and_then(|id| store.get_message(id)) {
                    out.push(' ');
                    out.push_str(&first.summarize());
                }
                out.push('\n');
            }
        }

        ToolOutput::text(out).with_details(json!({
            "ref_id": ref_id.to_string(),
            "head": r.head.to_string(),
            "pending_merge_count": pending.len(),
        }))
    }
}

/// Describe a ref's facets for `readRef`. The chat facet is the common one;
/// a malformed payload is surfaced rather than swallowed.
fn describe_facets(r: &Ref) -> String {
    match r.facet::<crate::chat::ChatFacet>() {
        Some(Ok(c)) => {
            let mut s = format!("chat \"{}\"", c.title);
            if let Some(parent) = &c.parent {
                s.push_str(&format!(", parent {}", parent));
            }
            if c.pinned {
                s.push_str(", pinned");
            }
            s
        }
        Some(Err(e)) => format!("chat facet malformed: {}", e),
        None => "raw (no chat facet)".to_string(),
    }
}

// -- Agent -------------------------------------------------------------------

/// Spawn a sub-agent loop asynchronously from a context or ref.
///
/// Validates the shared execute parameters, resolves the run's capability
/// grant against the caller's (every transfer is bounded by the caller's
/// transition -- see `crate::caps`), and hands the request to the harness
/// seam, which creates the child session (stamping the grant on its ref) and
/// starts the loop in the background. Returns immediately with the new
/// session_id.
struct RunAgentTool {
    store: Arc<dyn StoreAccess>,
    exec: Arc<dyn MetaExec>,
    /// The calling loop's grant: the ceiling on what any spawned run receives.
    caps: CapSet,
}

#[async_trait]
impl Tool for RunAgentTool {
    fn name(&self) -> &str {
        RUN_AGENT
    }

    fn description(&self) -> &str {
        "Run an agent asynchronously and return its session_id. The target is \
         either a context id -- forks a new session whose history begins at \
         that context, keeping the context's parents (so a context you composed \
         with a parent yields a connected sub-agent, a parentless one an island) \
         -- or a ref/session id -- continues that existing session on its \
         current head instead of forking, erroring if it is already running. \
         Capabilities attenuate: a run receives at most what you can convey \
         (your own grant, with runAgent's transfer budget reduced by one -- so \
         recursion is bounded), and continuing a session whose own grant \
         exceeds that is refused unless you pass 'tools' to run it narrowed. \
         With no tools \
         (tools: []) it is a single LLM turn -- the model's native capabilities \
         (e.g. Gemini search and code execution) turn on automatically. With \
         tools it loops -- LLM turn, tool calls, repeat -- until the model \
         stops calling them, writing every message back to the session. Check \
         on the result with readAgent."
    }

    fn parameters(&self) -> Value {
        let models = self.exec.model_ids().join(", ");
        json!({
            "type": "object",
            "properties": {
                "context_id": {
                    "type": "string",
                    "description": "A context id -- fork a new session whose \
                        history begins there, preserving the context's parents \
                        -- or a ref/session id -- continue that session on its \
                        current head (errors if it is already running)."
                },
                "model_id": {
                    "type": "string",
                    "description": format!("The model to run. Available: {}", models)
                },
                "model_params": {
                    "type": "object",
                    "description": "Parameters to pass to the model.",
                    "properties": {
                        "thinking": { "type": "string", "description": "Thinking level: off, low, medium, high, xhigh" },
                        "max_tokens": { "type": "integer", "description": "Maximum output tokens." }
                    }
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": format!(
                        "Capabilities to grant the run, by tool name. Omit to \
                         convey everything you can; pass [] for a single turn \
                         with no tools. On a continue, omitting runs the session \
                         with its own grant (refused if that exceeds yours). An \
                         unconveyable name is an error. Pass base names only -- \
                         'runAgent', never 'runAgent(1)'; the (n) shows a grant's \
                         remaining transfer budget, and a bare runAgent can spawn \
                         but not share. You can convey: {}",
                        self.caps.transition().describe())
                },
                "label": {
                    "type": "string",
                    "description": "Display label for a forked sub-session (ignored when continuing a ref). Defaults to the model name."
                }
            },
            "required": ["context_id", "model_id"]
        })
    }

    async fn run(&self, input: Value, _cancel: CancellationToken) -> ToolOutput {
        let request = match resolve_exec_request(self.store.as_ref(), &self.caps, &input) {
            Ok(r) => r,
            Err(msg) => return ToolOutput::error(&msg),
        };
        // Continuing a session that is already running would race its owning
        // loop -- the same single-ownership rule updateRef enforces. Surface it
        // before spawning; the loop is the authoritative writer of its head.
        if let ExecTarget::Continue(ref_id) = &request.target {
            if self.exec.agent_status(ref_id).await.running {
                return ToolOutput::error(&format!(
                    "session [{}] is already running; wait for it to idle, or \
                     deliver input as data with createContext merge_into", ref_id
                ));
            }
        }
        let single_turn = request.caps.is_empty();
        let continuing = matches!(&request.target, ExecTarget::Continue(_));
        let granted = request.caps.describe();

        match self.exec.spawn_agent(request).await {
            Ok(id) => {
                let action = if continuing {
                    "Continuing session"
                } else if single_turn {
                    "Single turn started on session"
                } else {
                    "Agent loop started on session"
                };
                ToolOutput::text(format!("{} '{}' (caps: {})", action, id, granted))
                    .with_details(json!({ "session_id": id }))
            }
            Err(msg) => ToolOutput::error(&msg),
        }
    }
}

/// Parse and validate `runAgent`'s input (context_id, model_id, model_params,
/// tools, label), resolve the target, and resolve the run's grant against the
/// caller's. The `context_id` slot names either a context (fork a new session
/// there) or a ref (continue that session); an id that is neither, or a grant
/// the caller cannot convey, is rejected here so the canary surfaces before
/// anything spawns.
///
/// Grant resolution is the one attenuation rule, applied per target:
/// - Fork: the request's `tools` (or everything conveyable when omitted),
///   resolved by `caps.grant` -- always within the caller's transition.
/// - Continue with `tools`: the same resolution; a per-run override that never
///   rewrites the target's own grant.
/// - Continue without `tools`: the target ref's own `caps` facet, refused
///   loudly if it exceeds what the caller can convey (or if the ref has no
///   grant at all), with both remedies named.
fn resolve_exec_request(
    store: &dyn StoreAccess,
    caps: &CapSet,
    input: &Value,
) -> Result<ExecRequest, String> {
    let store = store.store()?;

    let id = input.get("context_id").and_then(|v| v.as_str())
        .ok_or("missing 'context_id' parameter")?;
    let model_id = input.get("model_id").and_then(|v| v.as_str())
        .ok_or("missing 'model_id' parameter")?;

    // A context forks a new session that begins there; a ref continues that
    // session on its current head. Distinct id prefixes make this unambiguous,
    // and an id naming neither is surfaced now rather than at spawn.
    let target = if store.get_context(&ContextId::from(id)).is_some() {
        ExecTarget::Fork(ContextId::from(id))
    } else if store.get_ref(&RefId::from(id)).is_some() {
        ExecTarget::Continue(RefId::from(id))
    } else {
        return Err(format!("'{}' is neither a known context nor a ref", id));
    };

    let params = input.get("model_params");
    let thinking = match params.and_then(|p| p.get("thinking")).and_then(|v| v.as_str()) {
        Some(s) => Some(s.parse::<ThinkingLevel>()
            .map_err(|_| format!("invalid thinking level '{}'", s))?),
        None => None,
    };
    let max_tokens = match params.and_then(|p| p.get("max_tokens")) {
        Some(v) => {
            let n = v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()));
            Some(n.ok_or_else(|| format!("invalid max_tokens: {}", v))? as usize)
        }
        None => None,
    };

    let requested: Option<Vec<String>> = match input.get("tools") {
        None => None,
        Some(Value::Array(arr)) => {
            let mut names = Vec::with_capacity(arr.len());
            for v in arr {
                match v.as_str() {
                    Some(s) if !s.is_empty() => names.push(s.to_string()),
                    _ => return Err("tools: each entry must be a tool name string".to_string()),
                }
            }
            Some(names)
        }
        Some(_) => return Err("tools: must be an array of tool names".to_string()),
    };

    let run_caps = match (&target, &requested) {
        (_, Some(names)) => caps.grant(Some(names)).map_err(|e| format!("tools: {}", e))?,
        (ExecTarget::Fork(_), None) => caps.grant(None).map_err(|e| format!("tools: {}", e))?,
        (ExecTarget::Continue(ref_id), None) => {
            // The target already proved present during target resolution.
            let r = store.get_ref(ref_id)
                .ok_or_else(|| format!("ref [{}] disappeared during resolution", ref_id))?;
            match r.facet::<CapSet>() {
                None => return Err(format!(
                    "session [{}] has no capability grant; pass 'tools' to run it \
                     with an explicit set, or updateRef with 'caps' to give it one",
                    ref_id
                )),
                Some(Err(e)) => return Err(format!(
                    "session [{}] has a malformed caps facet: {}", ref_id, e
                )),
                Some(Ok(target_caps)) => {
                    let conveyable = caps.transition();
                    let violations = target_caps.violations(&conveyable);
                    if !violations.is_empty() {
                        // A name the caller holds but transition dropped (a
                        // spent transfer budget) would otherwise read as "not
                        // granted" -- say what actually happened.
                        let exhausted: Vec<String> = target_caps.names().into_iter()
                            .filter(|n| caps.contains(n) && !conveyable.contains(n))
                            .map(|n| format!("[{}]", n))
                            .collect();
                        let note = if exhausted.is_empty() {
                            String::new()
                        } else {
                            format!(
                                " (you hold {} but with no transfer budget left \
                                 to convey it)",
                                exhausted.join(", ")
                            )
                        };
                        return Err(format!(
                            "session [{}] runs with a grant exceeding what you can \
                             convey: {}{}. Pass 'tools' to run it narrowed this run, \
                             or updateRef with 'caps' to change its grant.",
                            ref_id, violations.join("; "), note
                        ));
                    }
                    target_caps
                }
            }
        }
    };

    let label = input.get("label").and_then(|v| v.as_str()).map(str::to_string);

    Ok(ExecRequest {
        target,
        model_id: model_id.to_string(),
        thinking,
        max_tokens,
        label,
        caps: run_caps,
    })
}

/// Inspect a spawned agent's runtime status: whether its loop is alive, what it
/// is streaming right now, and a preview of its latest output. The runtime
/// counterpart to `readRef` -- it reports only what the harness knows, not the
/// ref's data (head, facets), which `readRef` owns.
struct ReadAgentTool {
    store: Arc<dyn StoreAccess>,
    exec: Arc<dyn MetaExec>,
}

#[async_trait]
impl Tool for ReadAgentTool {
    fn name(&self) -> &str {
        "readAgent"
    }

    fn description(&self) -> &str {
        "Inspect a spawned agent's runtime status: whether its loop is still \
         running, the text it is streaming right now (if mid-turn), and a \
         preview of its latest persisted output or error. Use this to check on \
         a session started with runAgent. For the ref as data (head, facets, \
         merge_into inbox) use readRef."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The session (ref id) returned by runAgent."
                }
            },
            "required": ["session_id"]
        })
    }

    async fn run(&self, input: Value, _cancel: CancellationToken) -> ToolOutput {
        let store = match self.store.store() {
            Ok(s) => s,
            Err(msg) => return ToolOutput::error(&msg),
        };

        let session_id = match input.get("session_id").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => RefId::from(s),
            _ => return ToolOutput::error("missing 'session_id' parameter"),
        };

        if store.get_ref(&session_id).is_none() {
            return ToolOutput::error(&format!("session '{}' not found", session_id));
        }

        let status = self.exec.agent_status(&session_id).await;

        let mut out = format!(
            "AGENT {} [{}]\n", session_id,
            if status.running { "running" } else { "idle" }
        );

        let head_messages = store.head_context(&session_id)
            .map(|c| store.pool().resolve(&c.messages))
            .unwrap_or_default();

        if let Some(model) = latest_model(&head_messages) {
            out.push_str(&format!("model: {}\n", model));
        }

        // The toolset the most recent turn ran with -- the runtime counterpart
        // of `model`, read the same way off the result meta.
        let tools = latest_tools(&head_messages);
        if let Some(names) = &tools {
            out.push_str(&format!(
                "tools: {}\n",
                if names.is_empty() { "(none -- single turn)".to_string() } else { names.join(", ") }
            ));
        }

        if let Some(partial) = &status.streaming_preview {
            out.push_str(&format!("streaming: {}\n", truncate(partial, 280)));
        }

        out.push_str(&format!("latest: {}\n", latest_output_preview(&head_messages)));

        ToolOutput::text(out).with_details(json!({
            "session_id": session_id.to_string(),
            "running": status.running,
            "tools": tools,
        }))
    }
}

/// The model id of the most recent assistant message, from its `meta.model`.
fn latest_model(messages: &[Message]) -> Option<String> {
    messages.iter().rev()
        .find(|m| m.role == Role::Assistant)
        .and_then(|m| m.meta.as_ref())
        .and_then(|meta| meta.get("model"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// The tool names the session's most recent tool-bearing assistant turn ran
/// with, read from its `meta.tools`. The mirror of [`latest_model`]. This is
/// provenance, never authority: `readAgent` displays it, and a harness may
/// consult it once to derive a `caps` grant for a ref predating the facet --
/// a session's durable grant lives on its ref (`crate::caps`), not in its
/// content. An empty array is a real recorded value (a single-turn agent),
/// distinct from `None` (no turn has recorded a toolset yet); a trailing
/// error message records no tools and is skipped, so the last real toolset
/// still surfaces.
pub fn latest_tools(messages: &[Message]) -> Option<Vec<String>> {
    messages.iter().rev()
        .filter(|m| m.role == Role::Assistant)
        .find_map(|m| {
            m.meta.as_ref()
                .and_then(|meta| meta.get("tools"))
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        })
}

/// A short preview of an agent's latest output: the last assistant message's
/// text, or an error if it ended in one, or a note that it hasn't spoken yet.
fn latest_output_preview(messages: &[Message]) -> String {
    let Some(msg) = messages.iter().rev().find(|m| m.role == Role::Assistant) else {
        return "(no output yet)".to_string();
    };
    let mut text = String::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text { text: t, .. } => {
                if !text.is_empty() { text.push(' '); }
                text.push_str(t);
            }
            ContentBlock::Error { message, .. } => {
                return format!("error: {}", truncate(message, 280));
            }
            _ => {}
        }
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "(last turn produced no text)".to_string()
    } else {
        truncate(trimmed, 280)
    }
}

// -- Shared rendering helpers ------------------------------------------------

fn parse_role(s: &str) -> Option<Role> {
    match s {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        "system" => Some(Role::System),
        _ => None,
    }
}

/// Render a list of context ids inline, or "none" when empty.
fn id_list(ids: &[ContextId]) -> String {
    if ids.is_empty() {
        return "none".to_string();
    }
    ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(", ")
}

/// Like [`id_list`] but caps a long list (a branchy context can have many
/// children) with a "(+N more)" tail so the output stays bounded.
fn id_list_capped(ids: &[ContextId], cap: usize) -> String {
    if ids.is_empty() {
        return "none".to_string();
    }
    if ids.len() <= cap {
        return id_list(ids);
    }
    let shown = ids[..cap].iter().map(|id| id.to_string()).collect::<Vec<_>>().join(", ");
    format!("{}, (+{} more)", shown, ids.len() - cap)
}

/// Compute added/removed message ids between a parent and child context.
fn diff_message_lists(parent: &[MessageId], child: &[MessageId]) -> (Vec<MessageId>, Vec<MessageId>) {
    let parent_set: HashSet<&str> = parent.iter().map(|id| id.as_str()).collect();
    let child_set: HashSet<&str> = child.iter().map(|id| id.as_str()).collect();
    let added = child.iter().filter(|id| !parent_set.contains(id.as_str())).cloned().collect();
    let removed = parent.iter().filter(|id| !child_set.contains(id.as_str())).cloned().collect();
    (added, removed)
}

/// Write indented message lines with inline summaries, or "(no messages)".
fn append_message_list(out: &mut String, store: &Store, messages: &[MessageId]) {
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

/// Write a diff block: added (+) and removed (-) messages with summaries.
fn append_diff(out: &mut String, store: &Store, added: &[MessageId], removed: &[MessageId]) {
    if added.is_empty() && removed.is_empty() {
        out.push_str("  (no changes)\n");
        return;
    }
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

/// Truncate text to a character budget with an ellipsis, for previews.
fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{}...", cut.trim_end())
}
