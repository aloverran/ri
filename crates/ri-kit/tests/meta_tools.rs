//! Integration test for the shared meta-tools: drives the real tool boundary
//! (JSON input -> ToolOutput + on-disk store state) over a temporary mount,
//! with a stub execution seam standing in for a harness.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use ri::{ContentBlock, ContextId, HasMeta, MessageId, RefId, Role, Store, Tool, ToolOutput};
use ri_kit::meta_tools::{AgentStatus, ExecRequest, ExecTarget, MetaExec, StoreAccess};

/// A fresh, empty sessions dir for one test.
fn temp_sessions_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ri-kit-meta-test-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn mount(dir: &PathBuf) -> Store {
    ri::Pool::new().mount(dir).unwrap()
}

/// Stub harness seam: store access mounts fresh per call (CLI-style),
/// `spawn_agent` records the request, and `agent_status` reports a ref running
/// iff it has been marked so (driving the ownership guard).
struct StubSeam {
    dir: PathBuf,
    spawned: Mutex<Vec<ExecRequest>>,
    running: Mutex<HashSet<String>>,
}

impl StubSeam {
    fn mark_running(&self, id: &RefId) {
        self.running.lock().unwrap().insert(id.to_string());
    }
}

impl StoreAccess for StubSeam {
    fn store(&self) -> Result<Store, String> {
        ri::Pool::new()
            .mount(&self.dir)
            .map_err(|e| format!("failed to load store: {}", e))
    }
}

#[async_trait]
impl MetaExec for StubSeam {
    fn model_ids(&self) -> Vec<String> {
        vec!["stub-model".to_string()]
    }

    async fn spawn_agent(&self, request: ExecRequest) -> Result<RefId, String> {
        self.spawned.lock().unwrap().push(request);
        Ok(RefId::from("ref_stub_agent"))
    }

    async fn agent_status(&self, ref_id: &RefId) -> AgentStatus {
        AgentStatus {
            running: self.running.lock().unwrap().contains(ref_id.as_str()),
            streaming_preview: None,
        }
    }
}

/// The grant the fixture session runs with: a web-root-shaped set (four base
/// names plus every meta tool), `runAgent` leveled at 2.
fn root_caps() -> ri_kit::caps::CapSet {
    ri_kit::caps::CapSet::unit(
        ["bash", "read", "write", "edit"].into_iter()
            .chain(ri_kit::meta_tools::TOOL_NAMES.iter().copied()),
    )
    .with_leveled(ri_kit::meta_tools::RUN_AGENT, 2)
}

/// One assembled test fixture: a session ref in a temp store carrying the
/// root grant, the meta-tools built over the stub seam within it.
struct Fixture {
    dir: PathBuf,
    session_id: RefId,
    seam: Arc<StubSeam>,
    tools: Vec<Box<dyn Tool>>,
}

fn fixture(tag: &str) -> Fixture {
    let dir = temp_sessions_dir(tag);
    let store = mount(&dir);
    let session = ri_kit::chat::create(&store, ri_kit::chat::ChatFacet {
        title: "test session".to_string(),
        cwd: "/tmp".to_string(),
        host: None,
        parent: None,
        pinned: false,
    })
    .unwrap();
    let caps = root_caps();
    let stamped = session.clone().with_facet(&caps).unwrap();
    store.write_ref(&stamped).unwrap();
    let seam = Arc::new(StubSeam {
        dir: dir.clone(),
        spawned: Mutex::new(Vec::new()),
        running: Mutex::new(HashSet::new()),
    });
    let tools = ri_kit::meta_tools::create(seam.clone(), seam.clone(), session.id.clone(), &caps);
    Fixture { dir, session_id: session.id, seam, tools }
}

impl Fixture {
    fn tool(&self, name: &str) -> &dyn Tool {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|b| b.as_ref())
            .unwrap_or_else(|| panic!("tool '{}' not assembled", name))
    }

    async fn run(&self, name: &str, input: serde_json::Value) -> ToolOutput {
        self.tool(name).run(input, CancellationToken::new()).await
    }

    /// createMessage one text message, return its id.
    async fn one_message(&self, role: &str, content: &str) -> String {
        let out = self.run("createMessage", json!({
            "messages": [ { "role": role, "content": content } ]
        })).await;
        assert!(!out.is_error, "createMessage failed: {}", text(&out));
        ids(&out, "message_ids")[0].clone()
    }

    /// createContext referencing existing message ids, return the context id.
    async fn context_of(&self, message_ids: &[&str]) -> String {
        let entries: Vec<_> = message_ids.iter().map(|m| json!({ "message_id": m })).collect();
        let out = self.run("createContext", json!({ "messages": entries })).await;
        assert!(!out.is_error, "createContext failed: {}", text(&out));
        detail_str(&out, "context_id")
    }
}

/// Concatenated text blocks of a tool output.
fn text(out: &ToolOutput) -> String {
    out.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn detail_str(out: &ToolOutput, key: &str) -> String {
    out.details
        .as_ref()
        .and_then(|d| d.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn ids(out: &ToolOutput, key: &str) -> Vec<String> {
    out.details.as_ref().unwrap()[key]
        .as_array().unwrap().iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn create_message_text_forge_and_noop() {
    let f = fixture("create-message");

    // New text message lands on disk with the right role.
    let out = f.run("createMessage", json!({
        "messages": [ { "role": "user", "content": "hello world" } ]
    })).await;
    assert!(!out.is_error, "unexpected error: {}", text(&out));
    let m1 = ids(&out, "message_ids")[0].clone();
    let store = mount(&f.dir);
    let msg = store.get_message(&MessageId::from(m1.as_str())).expect("m1 on disk");
    assert_eq!(msg.role, Role::User);

    // Role rewrite of m1 forges a new message recording its source.
    let out = f.run("createMessage", json!({
        "messages": [ { "role": "assistant", "from": m1.as_str() } ]
    })).await;
    assert!(!out.is_error, "unexpected error: {}", text(&out));
    let forged = ids(&out, "message_ids")[0].clone();
    assert_ne!(forged, m1, "role change forges a new id");
    let store = mount(&f.dir);
    let forged_msg = store.get_message(&MessageId::from(forged.as_str())).unwrap();
    assert_eq!(forged_msg.role, Role::Assistant);
    let forged_json = serde_json::to_value(&forged_msg).unwrap();
    assert_eq!(
        forged_json["meta"]["source_message_id"].as_str(), Some(m1.as_str()),
        "forged message records provenance"
    );

    // Same-role rewrite is a no-op: returns the original id, forges nothing.
    let out = f.run("createMessage", json!({
        "messages": [ { "role": "user", "from": m1.as_str() } ]
    })).await;
    assert!(!out.is_error);
    assert_eq!(ids(&out, "message_ids")[0], m1, "same-role rewrite reuses original");
    assert!(ids(&out, "created_ids").is_empty(), "same-role rewrite forges nothing");

    // Validation: empty array, missing role, both content+from, dangling from.
    let out = f.run("createMessage", json!({ "messages": [] })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("non-empty array"), "got: {}", text(&out));

    let out = f.run("createMessage", json!({ "messages": [ { "content": "x" } ] })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("'role' is required"), "got: {}", text(&out));

    let out = f.run("createMessage", json!({
        "messages": [ { "role": "user", "content": "x", "from": "msg_y" } ]
    })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("exactly one of"), "got: {}", text(&out));

    let out = f.run("createMessage", json!({
        "messages": [ { "role": "user", "from": "msg_missing" } ]
    })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("message [msg_missing] not found"), "got: {}", text(&out));
}

#[tokio::test]
async fn create_context_is_pure_composition() {
    let f = fixture("create-context");

    let m1 = f.one_message("user", "first").await;
    let m2 = f.one_message("assistant", "second").await;

    // Reference two messages -> a context holding exactly them, no parents.
    let ctx1 = f.context_of(&[m1.as_str(), m2.as_str()]).await;
    let store = mount(&f.dir);
    let c1 = store.get_context(&ContextId::from(ctx1.as_str())).unwrap();
    assert_eq!(c1.messages.len(), 2);
    assert_eq!(c1.messages[0].as_str(), m1);

    // Embed ctx1 + reference a third message -> messages expand in place and
    // ctx1 registers as a parent.
    let m3 = f.one_message("user", "third").await;
    let out = f.run("createContext", json!({
        "messages": [ { "context_id": ctx1 }, { "message_id": m3.as_str() } ]
    })).await;
    assert!(!out.is_error, "unexpected error: {}", text(&out));
    let ctx2 = detail_str(&out, "context_id");
    let store = mount(&f.dir);
    let c2 = store.get_context(&ContextId::from(ctx2.as_str())).unwrap();
    assert_eq!(c2.messages.len(), 3);
    assert_eq!(c2.parents.len(), 1);
    assert_eq!(c2.parents[0].as_str(), ctx1);

    // The removed minting shapes are rejected with a pointer to createMessage.
    let out = f.run("createContext", json!({
        "messages": [ { "role": "user", "content": "inline" } ]
    })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("exactly one of") && text(&out).contains("createMessage"),
        "inline content rejected: {}", text(&out));

    let out = f.run("createContext", json!({
        "messages": [ { "message_id": m1.as_str(), "context_id": ctx1 } ]
    })).await;
    assert!(out.is_error, "both keys must be rejected");

    let out = f.run("createContext", json!({
        "messages": [ { "message_id": "msg_missing" } ]
    })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("message [msg_missing] not found"), "got: {}", text(&out));

    // merge_into stamps an Envelope carrying the Merge instruction and validates the destination.
    let out = f.run("createContext", json!({
        "messages": [ { "message_id": m1.as_str() } ],
        "merge_into": f.session_id.as_str()
    })).await;
    assert!(!out.is_error, "unexpected error: {}", text(&out));
    assert!(text(&out).contains(", merge_into ["), "got: {}", text(&out));
    let env_id = detail_str(&out, "context_id");
    let store = mount(&f.dir);
    let env = store.get_context(&ContextId::from(env_id.as_str())).unwrap();
    let envelope = env.facet::<ri_kit::envelope::Envelope>().unwrap().unwrap();
    assert_eq!(envelope.to.to_string(), f.session_id.to_string());
    assert_eq!(envelope.instruction, ri_kit::envelope::Instruction::Merge);

    let out = f.run("createContext", json!({
        "messages": [ { "message_id": m1.as_str() } ], "merge_into": "ref_nope"
    })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "merge_into: ref [ref_nope] not found");
}

#[tokio::test]
async fn create_context_embed_ref_and_exclude() {
    let f = fixture("create-context-embed-exclude");

    let m1 = f.one_message("user", "one").await;
    let m2 = f.one_message("assistant", "two").await;
    let ctx_a = f.context_of(&[m1.as_str(), m2.as_str()]).await;

    // A ref stands in for its head context in the embed slot (wrap/extend): its
    // messages expand in place, surrounded by the bare-message entries. The
    // registered parent must be the RESOLVED context, never the ref id.
    f.run("updateRef", json!({ "ref_id": "ref_topic", "context_id": ctx_a })).await;
    let sys = f.one_message("system", "prelude").await;
    let tail = f.one_message("user", "postlude").await;
    let out = f.run("createContext", json!({
        "messages": [
            { "message_id": sys.as_str() },
            { "context_id": "ref_topic" },
            { "message_id": tail.as_str() }
        ]
    })).await;
    assert!(!out.is_error, "ref embed failed: {}", text(&out));
    let store = mount(&f.dir);
    let w = store.get_context(&ContextId::from(detail_str(&out, "context_id").as_str())).unwrap();
    assert_eq!(
        w.messages.iter().map(|m| m.to_string()).collect::<Vec<_>>(),
        vec![sys.clone(), m1.clone(), m2.clone(), tail.clone()],
        "ref head expands in place, wrapped by the surrounding messages"
    );
    assert_eq!(w.parents.len(), 1);
    assert_eq!(w.parents[0].as_str(), ctx_a, "parent is the resolved context, not the ref id");

    // exclude drops every occurrence from the assembled list. Embedding ctx_a and
    // also m1 by id puts m1 in twice; exclude:[m1] removes both (count > list len).
    let out = f.run("createContext", json!({
        "messages": [ { "context_id": ctx_a }, { "message_id": m1.as_str() } ],
        "exclude": [ m1.as_str() ]
    })).await;
    assert!(!out.is_error, "exclude failed: {}", text(&out));
    assert!(text(&out).contains("2 excluded"), "all occurrences removed + reported: {}", text(&out));
    let store = mount(&f.dir);
    let c = store.get_context(&ContextId::from(detail_str(&out, "context_id").as_str())).unwrap();
    assert_eq!(
        c.messages.iter().map(|m| m.to_string()).collect::<Vec<_>>(),
        vec![m2.clone()],
        "both m1 occurrences gone, m2 remains"
    );
    assert_eq!(c.parents[0].as_str(), ctx_a, "embedded context stays a parent even after exclusion");

    // An exclude id matching nothing in the assembled list is a canary, surfaced
    // before any write.
    let out = f.run("createContext", json!({
        "messages": [ { "context_id": ctx_a } ], "exclude": [ "msg_not_here" ]
    })).await;
    assert!(out.is_error);
    assert!(
        text(&out).contains("[msg_not_here]") && text(&out).contains("not present"),
        "unmatched exclude errors: {}", text(&out)
    );

    // exclude must be an array of ids, and reject empty-string members.
    let out = f.run("createContext", json!({
        "messages": [ { "context_id": ctx_a } ], "exclude": "msg_x"
    })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("must be an array"), "got: {}", text(&out));

    let out = f.run("createContext", json!({
        "messages": [ { "context_id": ctx_a } ], "exclude": [ "" ]
    })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("exclude[0]"), "got: {}", text(&out));
}

#[tokio::test]
async fn create_context_parents_resolve_and_validate() {
    let f = fixture("create-context-parents");

    let m1 = f.one_message("user", "seed").await;
    let ctx_a = f.context_of(&[m1.as_str()]).await;
    let body = f.one_message("user", "body").await;

    // A context id in `parents` is recorded as lineage only -- no message
    // embedding, unlike the context_id embed slot.
    let out = f.run("createContext", json!({
        "messages": [ { "message_id": body.as_str() } ],
        "parents": [ ctx_a.as_str() ]
    })).await;
    assert!(!out.is_error, "context-id parent failed: {}", text(&out));
    let store = mount(&f.dir);
    let c = store.get_context(&ContextId::from(detail_str(&out, "context_id").as_str())).unwrap();
    assert_eq!(
        c.messages.iter().map(|m| m.to_string()).collect::<Vec<_>>(), vec![body.clone()],
        "a parent adds lineage only, never messages"
    );
    assert_eq!(c.parents.iter().map(|p| p.to_string()).collect::<Vec<_>>(), vec![ctx_a.clone()]);

    // A ref id in `parents` snapshots to its head context: the stored parent is
    // the resolved context, never the ref id.
    f.run("updateRef", json!({ "ref_id": "ref_topic", "context_id": ctx_a })).await;
    let out = f.run("createContext", json!({
        "messages": [ { "message_id": body.as_str() } ],
        "parents": [ "ref_topic" ]
    })).await;
    assert!(!out.is_error, "ref parent failed: {}", text(&out));
    let store = mount(&f.dir);
    let c = store.get_context(&ContextId::from(detail_str(&out, "context_id").as_str())).unwrap();
    assert_eq!(
        c.parents.iter().map(|p| p.to_string()).collect::<Vec<_>>(), vec![ctx_a.clone()],
        "ref parent resolved to its head context, not stored as the ref id"
    );

    // A ref embedded in `messages` and also named in `parents` collapses to one
    // parent: dedup keys on the resolved context id across both slots.
    let out = f.run("createContext", json!({
        "messages": [ { "context_id": "ref_topic" } ],
        "parents": [ "ref_topic" ]
    })).await;
    assert!(!out.is_error, "dedup case failed: {}", text(&out));
    let store = mount(&f.dir);
    let c = store.get_context(&ContextId::from(detail_str(&out, "context_id").as_str())).unwrap();
    assert_eq!(
        c.parents.iter().map(|p| p.to_string()).collect::<Vec<_>>(), vec![ctx_a.clone()],
        "embed + parents naming the same resolved context dedup to one parent"
    );

    // An unknown id -- neither context nor ref -- surfaces before any write.
    let out = f.run("createContext", json!({
        "messages": [ { "message_id": body.as_str() } ],
        "parents": [ "ctx_nope" ]
    })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("neither a known context nor a ref"), "got: {}", text(&out));
}

#[tokio::test]
async fn create_context_jump_envelope() {
    use ri_kit::envelope::{Envelope, Instruction};
    let f = fixture("create-context-jump");
    let m1 = f.one_message("user", "a").await;
    let ctx_a = f.context_of(&[m1.as_str()]).await;

    // jump with a context target, self-addressed (to defaults to the caller).
    let out = f.run("createContext", json!({
        "messages": [], "jump": { "target": ctx_a }
    })).await;
    assert!(!out.is_error, "unexpected error: {}", text(&out));
    assert!(
        text(&out).contains(&format!("jump [{}] -> target [{}]", f.session_id, ctx_a)),
        "got: {}", text(&out)
    );
    let env_id = detail_str(&out, "context_id");
    let env = mount(&f.dir).get_context(&ContextId::from(env_id.as_str())).unwrap();
    let envelope = env.facet::<Envelope>().unwrap().unwrap();
    assert_eq!(envelope.to.to_string(), f.session_id.to_string(), "self-addressed by default");
    assert_eq!(
        envelope.instruction,
        Instruction::Jump { target: ContextId::from(ctx_a.as_str()) },
        "context target stamped as-is"
    );

    // A ref target resolves to that ref's current head context at author time.
    f.run("updateRef", json!({ "ref_id": "ref_topic", "context_id": ctx_a })).await;
    let out = f.run("createContext", json!({
        "messages": [], "jump": { "target": "ref_topic" }
    })).await;
    assert!(!out.is_error, "ref target resolves: {}", text(&out));
    let env = mount(&f.dir).get_context(&ContextId::from(detail_str(&out, "context_id").as_str())).unwrap();
    assert_eq!(
        env.facet::<Envelope>().unwrap().unwrap().instruction,
        Instruction::Jump { target: ContextId::from(ctx_a.as_str()) },
        "ref target resolved to its head context"
    );

    // An explicit `to` addresses another ref's owner.
    let out = f.run("createContext", json!({
        "messages": [], "jump": { "to": "ref_topic", "target": ctx_a }
    })).await;
    assert!(!out.is_error, "explicit to: {}", text(&out));
    let env = mount(&f.dir).get_context(&ContextId::from(detail_str(&out, "context_id").as_str())).unwrap();
    assert_eq!(env.facet::<Envelope>().unwrap().unwrap().to.to_string(), "ref_topic");

    // merge_into and jump are mutually exclusive.
    let out = f.run("createContext", json!({
        "messages": [], "merge_into": f.session_id.as_str(), "jump": { "target": ctx_a }
    })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("at most one of 'merge_into' or 'jump'"), "got: {}", text(&out));

    // Missing target, dangling target, dangling to -- each surfaces before any write.
    let out = f.run("createContext", json!({ "messages": [], "jump": {} })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("jump.target: required"), "got: {}", text(&out));

    let out = f.run("createContext", json!({ "messages": [], "jump": { "target": "ctx_nope" } })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("neither a known context nor a ref"), "got: {}", text(&out));

    let out = f.run("createContext", json!({
        "messages": [], "jump": { "to": "ref_nope", "target": ctx_a }
    })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "jump.to: ref [ref_nope] not found");
}

#[tokio::test]
async fn read_message_and_context() {
    let f = fixture("read");

    let m1 = f.one_message("user", "needle in the pool").await;
    let out = f.run("readMessage", json!({ "message_id": m1.as_str() })).await;
    assert!(!out.is_error);
    assert!(text(&out).contains("needle in the pool"), "got: {}", text(&out));

    let out = f.run("readMessage", json!({ "message_id": "msg_nope" })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "message 'msg_nope' not found");

    // ctx1 (root) <- ctx2 (embeds ctx1 + adds a message). readContext on ctx2
    // shows its messages, ctx1 as a parent, and the ancestor diff.
    let ctx1 = f.context_of(&[m1.as_str()]).await;
    let m2 = f.one_message("user", "more").await;
    let out = f.run("createContext", json!({
        "messages": [ { "context_id": ctx1 }, { "message_id": m2.as_str() } ]
    })).await;
    let ctx2 = detail_str(&out, "context_id");

    let out = f.run("readContext", json!({ "context_id": ctx2 })).await;
    assert!(!out.is_error);
    let view = text(&out);
    assert!(view.starts_with(&format!("CONTEXT {}", ctx2)), "got: {}", view);
    assert!(view.contains(&format!("parents: {}", ctx1)), "parent shown: {}", view);
    assert!(view.contains("ancestors:") && view.contains(&format!("<- {}", ctx1)), "ancestor diff: {}", view);

    // ctx1 is the focal context's parent, so it should list ctx2 as a child.
    let out = f.run("readContext", json!({ "context_id": ctx1 })).await;
    assert!(text(&out).contains(&format!("children: {}", ctx2)), "child link: {}", text(&out));

    // Session entry point resolves through the ref head.
    let store = mount(&f.dir);
    ri_kit::chat::set_head(&store, &f.session_id, ContextId::from(ctx2.as_str())).unwrap();
    let out = f.run("readContext", json!({ "session_id": f.session_id.as_str() })).await;
    assert!(!out.is_error);
    assert!(text(&out).starts_with(&format!("CONTEXT {}", ctx2)), "got: {}", text(&out));

    let out = f.run("readContext", json!({ "session_id": "ref_nope" })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "session 'ref_nope' not found");

    let out = f.run("readContext", json!({})).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "either 'context_id' or 'session_id' is required");
}

#[tokio::test]
async fn update_ref_create_update_guard_and_canary() {
    let f = fixture("update-ref");

    let m1 = f.one_message("user", "a").await;
    let ctx_a = f.context_of(&[m1.as_str()]).await;

    // Create a raw ref at ctx_a.
    let out = f.run("updateRef", json!({ "ref_id": "ref_topic", "context_id": ctx_a })).await;
    assert!(!out.is_error, "unexpected error: {}", text(&out));
    assert!(text(&out).starts_with("Created ref [ref_topic]"), "got: {}", text(&out));
    let store = mount(&f.dir);
    assert_eq!(store.get_ref(&RefId::from("ref_topic")).unwrap().head.as_str(), ctx_a);

    // Move it to a descendant (ctx_a stays reachable) -> no severance note.
    let m2 = f.one_message("user", "b").await;
    let out = f.run("createContext", json!({
        "messages": [ { "context_id": ctx_a }, { "message_id": m2.as_str() } ]
    })).await;
    let ctx_b = detail_str(&out, "context_id");
    let out = f.run("updateRef", json!({ "ref_id": "ref_topic", "context_id": ctx_b })).await;
    assert!(!out.is_error);
    assert!(text(&out).starts_with("Updated ref [ref_topic]"), "got: {}", text(&out));
    assert!(!text(&out).contains("does not descend"), "no severance expected: {}", text(&out));

    // Move it to an unrelated root (prior head ctx_b not reachable) -> note.
    let m3 = f.one_message("user", "c").await;
    let ctx_c = f.context_of(&[m3.as_str()]).await;
    let out = f.run("updateRef", json!({ "ref_id": "ref_topic", "context_id": ctx_c })).await;
    assert!(!out.is_error);
    assert!(text(&out).contains("does not descend from the previous head"), "severance note: {}", text(&out));

    // Dangling context is refused before any write.
    let out = f.run("updateRef", json!({ "ref_id": "ref_topic", "context_id": "ctx_nope" })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "context 'ctx_nope' not found");

    // A caps write mints the grant on the ref, bounded by what the caller can
    // convey: runAgent lands one level lower, and an unconveyable name is
    // refused before any write.
    let out = f.run("updateRef", json!({
        "ref_id": "ref_topic", "context_id": ctx_c, "caps": ["read", "runAgent"]
    })).await;
    assert!(!out.is_error, "caps write failed: {}", text(&out));
    assert!(text(&out).contains("caps set to read, runAgent(1)"), "got: {}", text(&out));
    let store = mount(&f.dir);
    let granted = store.get_ref(&RefId::from("ref_topic")).unwrap()
        .facet::<ri_kit::caps::CapSet>().unwrap().unwrap();
    assert_eq!(granted.describe(), "read, runAgent(1)");

    let head_before = store.get_ref(&RefId::from("ref_topic")).unwrap().head;
    let out = f.run("updateRef", json!({
        "ref_id": "ref_topic", "context_id": ctx_c, "caps": ["frobnicate"]
    })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("cannot convey [frobnicate]"), "got: {}", text(&out));
    let store = mount(&f.dir);
    let after = store.get_ref(&RefId::from("ref_topic")).unwrap();
    assert_eq!(after.head, head_before, "a refused caps write moves nothing");
    let kept = after.facet::<ri_kit::caps::CapSet>().unwrap().unwrap();
    assert_eq!(kept.describe(), "read, runAgent(1)", "a refused caps write grants nothing");

    // Ownership guard: a different ref a running agent owns is refused.
    let store = mount(&f.dir);
    let other = ri_kit::chat::create(&store, ri_kit::chat::ChatFacet {
        title: "other".into(), cwd: String::new(),
        host: None, parent: None, pinned: false,
    }).unwrap();
    f.seam.mark_running(&other.id);
    let out = f.run("updateRef", json!({ "ref_id": other.id.as_str(), "context_id": ctx_a })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("owned by a running agent") && text(&out).contains("merge_into"),
        "guard error teaches the pull: {}", text(&out));

    // No self-exception: a running ref -- including the caller's own session --
    // is refused, because the loop (not a tool it calls) owns the head.
    f.seam.mark_running(&f.session_id);
    let out = f.run("updateRef", json!({ "ref_id": f.session_id.as_str(), "context_id": ctx_a })).await;
    assert!(out.is_error, "self-move refused while running: {}", text(&out));
    assert!(text(&out).contains("owned by a running agent loop"), "self refusal: {}", text(&out));
}

#[tokio::test]
async fn read_ref_head_facets_and_inbox() {
    let f = fixture("read-ref");

    let m1 = f.one_message("user", "x").await;
    let ctx_a = f.context_of(&[m1.as_str()]).await;

    // The chat session ref reports its facet and its grant.
    let out = f.run("readRef", json!({ "ref_id": f.session_id.as_str() })).await;
    assert!(!out.is_error, "unexpected error: {}", text(&out));
    assert!(text(&out).contains("chat \"test session\""), "facet shown: {}", text(&out));
    assert!(text(&out).contains("runAgent(2)"), "grant shown: {}", text(&out));

    // A raw ref (created by updateRef) reports "raw" and no grant.
    f.run("updateRef", json!({ "ref_id": "ref_raw", "context_id": ctx_a })).await;
    let out = f.run("readRef", json!({ "ref_id": "ref_raw" })).await;
    assert!(text(&out).contains("raw"), "raw ref: {}", text(&out));
    assert!(text(&out).contains("caps: (no grant)"), "capless shown: {}", text(&out));

    // An envelope addressed to ref_raw shows up as one pending inbox item.
    f.run("createContext", json!({
        "messages": [ { "message_id": m1.as_str() } ], "merge_into": "ref_raw"
    })).await;
    let out = f.run("readRef", json!({ "ref_id": "ref_raw" })).await;
    assert!(!out.is_error);
    assert!(text(&out).contains("inbox (1 pending envelope)"), "inbox count: {}", text(&out));
    assert!(text(&out).contains("[merge]"), "verb label shown: {}", text(&out));

    let out = f.run("readRef", json!({ "ref_id": "ref_nope" })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "ref 'ref_nope' not found");
}

#[tokio::test]
async fn run_agent_tool_selection_and_validation() {
    let f = fixture("run-agent");
    let m1 = f.one_message("user", "seed").await;
    let ctx1 = f.context_of(&[m1.as_str()]).await;

    // Schema advertises the seam's models and the conveyable grant: the
    // caller's set transitioned, so runAgent shows one level lower.
    let params = f.tool("runAgent").parameters().to_string();
    assert!(params.contains("stub-model"), "models advertised: {}", params);
    assert!(params.contains("bash") && params.contains("edit"), "tools advertised: {}", params);
    assert!(params.contains("runAgent(1)"), "conveyable runAgent level shown: {}", params);

    // Default (no tools key) -> everything conveyable, loop.
    let out = f.run("runAgent", json!({
        "context_id": ctx1, "model_id": "stub-model", "label": "worker",
        "model_params": { "thinking": "high", "max_tokens": 1000 }
    })).await;
    assert!(!out.is_error, "unexpected error: {}", text(&out));
    assert!(text(&out).starts_with("Agent loop started on session 'ref_stub_agent'"),
        "got: {}", text(&out));
    assert!(text(&out).contains("runAgent(1)"), "granted set reported: {}", text(&out));

    // Empty tools -> single turn.
    let out = f.run("runAgent", json!({
        "context_id": ctx1, "model_id": "stub-model", "tools": []
    })).await;
    assert!(!out.is_error);
    assert!(text(&out).starts_with("Single turn started on session 'ref_stub_agent'"),
        "got: {}", text(&out));

    // Subset -> loop with that subset.
    let out = f.run("runAgent", json!({
        "context_id": ctx1, "model_id": "stub-model", "tools": ["read"]
    })).await;
    assert!(!out.is_error);

    // A name outside the caller's grant is a surfaced error.
    let out = f.run("runAgent", json!({
        "context_id": ctx1, "model_id": "stub-model", "tools": ["frobnicate"]
    })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("cannot convey [frobnicate]") && text(&out).contains("does not include"),
        "got: {}", text(&out));

    // The requests that reached the seam carry the resolved grants.
    {
        let spawned = f.seam.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 3, "three successful spawns");
        assert_eq!(spawned[0].caps, root_caps().transition(), "default = everything conveyable");
        assert_eq!(spawned[0].max_tokens, Some(1000));
        assert_eq!(spawned[0].thinking.as_ref().map(|t| t.to_string()), Some("high".to_string()));
        assert!(
            matches!(&spawned[0].target, ExecTarget::Fork(c) if c.to_string() == ctx1),
            "a context target forks a new session beginning there"
        );
        assert!(spawned[1].caps.is_empty(), "empty = single turn");
        assert_eq!(spawned[2].caps.names(), vec!["read".to_string()]);
    }

    // Validation surface.
    let out = f.run("runAgent", json!({ "model_id": "stub-model" })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "missing 'context_id' parameter");

    let out = f.run("runAgent", json!({ "context_id": "ctx_nope", "model_id": "stub-model" })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "'ctx_nope' is neither a known context nor a ref");

    let out = f.run("runAgent", json!({
        "context_id": ctx1, "model_id": "stub-model", "model_params": { "thinking": "bananas" }
    })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "invalid thinking level 'bananas'");
}

#[tokio::test]
async fn run_agent_attenuates_run_agent_level() {
    let f = fixture("run-agent-attenuation");
    let m1 = f.one_message("user", "seed").await;
    let ctx1 = f.context_of(&[m1.as_str()]).await;

    // The root fixture (runAgent level 2) grants runAgent at level 1.
    let out = f.run("runAgent", json!({
        "context_id": ctx1, "model_id": "stub-model", "tools": ["read", "runAgent"]
    })).await;
    assert!(!out.is_error, "level-2 caller conveys runAgent: {}", text(&out));
    {
        let spawned = f.seam.spawned.lock().unwrap();
        let caps = &spawned.last().unwrap().caps;
        assert_eq!(caps.describe(), "read, runAgent(1)", "runAgent granted one level lower");
    }

    // A level-1 loop (a child's grant) cannot convey runAgent at all: build
    // its tools directly, as a harness would for the spawned child.
    let child_caps = root_caps().transition();
    let child_tools = ri_kit::meta_tools::create(
        f.seam.clone(), f.seam.clone(), f.session_id.clone(), &child_caps,
    );
    let child_run = child_tools.iter().find(|t| t.name() == "runAgent").unwrap();
    let out = child_run.run(json!({
        "context_id": ctx1, "model_id": "stub-model", "tools": ["runAgent"]
    }), CancellationToken::new()).await;
    assert!(out.is_error);
    assert!(text(&out).contains("holds it at level 1") && text(&out).contains("does not extend"),
        "exhausted level teaches: {}", text(&out));

    // The grandchild grant (transition of the child's) has no runAgent, so
    // the tool is never even constructed for it.
    let grandchild_caps = child_caps.transition();
    assert!(!grandchild_caps.contains("runAgent"));
    let grandchild_tools = ri_kit::meta_tools::create(
        f.seam.clone(), f.seam.clone(), f.session_id.clone(), &grandchild_caps,
    );
    assert!(
        grandchild_tools.iter().all(|t| t.name() != "runAgent"),
        "an ungranted meta-tool is not built"
    );

    // A level-1 caller refusing a runAgent-holding continue names the real
    // cause -- held, but exhausted -- beside the violation list.
    let strong = ri::Ref::with_id(RefId::from("ref_exhausted"), ContextId::from(ctx1.as_str()))
        .with_facet(&ri_kit::caps::CapSet::none().with_leveled("runAgent", 1))
        .unwrap();
    mount(&f.dir).write_ref(&strong).unwrap();
    let out = child_run.run(json!({
        "context_id": "ref_exhausted", "model_id": "stub-model"
    }), CancellationToken::new()).await;
    assert!(out.is_error);
    assert!(text(&out).contains("does not extend to runs you start"),
        "exhausted note shown: {}", text(&out));
}

#[tokio::test]
async fn run_agent_continue_reads_the_target_grant() {
    let f = fixture("run-agent-continue-caps");
    let m1 = f.one_message("user", "seed").await;
    let ctx1 = f.context_of(&[m1.as_str()]).await;
    let store = mount(&f.dir);

    // A capless ref refuses a default continue, teaching both remedies.
    f.run("updateRef", json!({ "ref_id": "ref_bare", "context_id": ctx1 })).await;
    let out = f.run("runAgent", json!({
        "context_id": "ref_bare", "model_id": "stub-model"
    })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("no capability grant") && text(&out).contains("updateRef"),
        "capless continue teaches: {}", text(&out));

    // A ref granted within the caller's conveyable set continues quietly,
    // running with its own grant.
    let out = f.run("updateRef", json!({
        "ref_id": "ref_worker", "context_id": ctx1, "caps": ["read", "bash"]
    })).await;
    assert!(!out.is_error, "grant write failed: {}", text(&out));
    let out = f.run("runAgent", json!({
        "context_id": "ref_worker", "model_id": "stub-model"
    })).await;
    assert!(!out.is_error, "within-grant continue: {}", text(&out));
    {
        let spawned = f.seam.spawned.lock().unwrap();
        assert_eq!(spawned.last().unwrap().caps.describe(), "bash, read",
            "a default continue runs the target's own grant");
    }

    // A ref whose grant exceeds the caller's conveyable set is refused with
    // the violations and both remedies -- here, runAgent at the caller's own
    // level, which a spawn cannot convey.
    let strong = ri::Ref::with_id(RefId::from("ref_strong"), ContextId::from(ctx1.as_str()))
        .with_facet(&ri_kit::caps::CapSet::unit(["read"]).with_leveled("runAgent", 2))
        .unwrap();
    store.write_ref(&strong).unwrap();
    let out = f.run("runAgent", json!({
        "context_id": "ref_strong", "model_id": "stub-model"
    })).await;
    assert!(out.is_error);
    assert!(
        text(&out).contains("exceeding what you can convey")
            && text(&out).contains("[runAgent] at level 2 exceeds level 1"),
        "exceeding continue refused with violations: {}", text(&out)
    );

    // An explicit tools override runs the target narrowed, without touching
    // its durable grant.
    let out = f.run("runAgent", json!({
        "context_id": "ref_strong", "model_id": "stub-model", "tools": ["read"]
    })).await;
    assert!(!out.is_error, "explicit downgrade: {}", text(&out));
    {
        let spawned = f.seam.spawned.lock().unwrap();
        assert_eq!(spawned.last().unwrap().caps.describe(), "read");
    }
    let store = mount(&f.dir);
    let still = store.get_ref(&RefId::from("ref_strong")).unwrap()
        .facet::<ri_kit::caps::CapSet>().unwrap().unwrap();
    assert_eq!(still.describe(), "read, runAgent(2)", "a per-run override never rewrites the facet");
}

#[tokio::test]
async fn run_agent_continue_and_running_guard() {
    let f = fixture("run-agent-continue");
    let m1 = f.one_message("user", "seed").await;
    let ctx1 = f.context_of(&[m1.as_str()]).await;

    // A ref id targets continue-mode: the run resumes that ref on its head
    // rather than forking a new session.
    f.run("updateRef", json!({ "ref_id": "ref_cont", "context_id": ctx1 })).await;
    let out = f.run("runAgent", json!({
        "context_id": "ref_cont", "model_id": "stub-model", "tools": []
    })).await;
    assert!(!out.is_error, "continue on an idle ref: {}", text(&out));
    assert!(text(&out).starts_with("Continuing session"), "got: {}", text(&out));
    {
        let spawned = f.seam.spawned.lock().unwrap();
        let last = spawned.last().expect("a spawn was recorded");
        assert!(
            matches!(&last.target, ExecTarget::Continue(r) if r.as_str() == "ref_cont"),
            "a ref target continues that ref"
        );
    }

    // Single ownership: a ref a running loop owns is refused before any spawn.
    f.seam.mark_running(&RefId::from("ref_cont"));
    let before = f.seam.spawned.lock().unwrap().len();
    let out = f.run("runAgent", json!({
        "context_id": "ref_cont", "model_id": "stub-model", "tools": []
    })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("already running"), "got: {}", text(&out));
    assert_eq!(
        f.seam.spawned.lock().unwrap().len(), before,
        "a running ref never reaches spawn_agent"
    );
}

#[tokio::test]
async fn read_agent_running_and_idle() {
    let f = fixture("read-agent");

    // Idle session with no assistant output yet.
    let out = f.run("readAgent", json!({ "session_id": f.session_id.as_str() })).await;
    assert!(!out.is_error, "unexpected error: {}", text(&out));
    assert!(text(&out).contains("[idle]"), "idle status: {}", text(&out));
    assert!(text(&out).contains("(no output yet)"), "no-output preview: {}", text(&out));

    // Marked running -> reported running.
    f.seam.mark_running(&f.session_id);
    let out = f.run("readAgent", json!({ "session_id": f.session_id.as_str() })).await;
    assert!(text(&out).contains("[running]"), "running status: {}", text(&out));

    let out = f.run("readAgent", json!({ "session_id": "ref_nope" })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "session 'ref_nope' not found");
}
