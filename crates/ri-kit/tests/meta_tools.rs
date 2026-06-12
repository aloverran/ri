//! Integration test for the shared meta-tools: drives the real tool
//! boundary (JSON input -> ToolOutput + on-disk store state) over a
//! temporary mount, with a stub execution seam standing in for a harness.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use ri::{ContentBlock, ContextId, HasMeta, MessageId, RefId, Role, Store, Tool, ToolOutput};
use ri_kit::meta_tools::{ExecRequest, MetaExec, StoreAccess};

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
/// execution records the request and returns a fixed session id.
struct StubSeam {
    dir: PathBuf,
    spawned: Mutex<Vec<(&'static str, ExecRequest)>>,
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
        self.spawned.lock().unwrap().push(("agent", request));
        Ok(RefId::from("ref_stub_agent"))
    }

    async fn spawn_turn(&self, request: ExecRequest) -> Result<RefId, String> {
        self.spawned.lock().unwrap().push(("turn", request));
        Ok(RefId::from("ref_stub_turn"))
    }
}

/// One assembled test fixture: a session ref in a temp store, the five
/// tools built over the stub seam.
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
        created_at: chrono::Utc::now().to_rfc3339(),
        cwd: "/tmp".to_string(),
        host: None,
        parent: None,
        pinned: false,
    })
    .unwrap();
    let seam = Arc::new(StubSeam { dir: dir.clone(), spawned: Mutex::new(Vec::new()) });
    let tools = ri_kit::meta_tools::create(seam.clone(), seam.clone(), session.id.clone());
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

#[tokio::test]
async fn create_context_composes_embeds_references_and_forges() {
    let f = fixture("compose");

    // Inline message -> new message + context on disk.
    let out = f.run("createContext", json!({
        "messages": [ { "role": "user", "content": "hello world" } ]
    })).await;
    assert!(!out.is_error, "unexpected error: {}", text(&out));
    assert!(text(&out).starts_with("Created messages ["), "got: {}", text(&out));
    let ctx1 = detail_str(&out, "context_id");
    let store = mount(&f.dir);
    let ctx = store.get_context(&ContextId::from(ctx1.as_str())).expect("ctx1 on disk");
    assert_eq!(ctx.messages.len(), 1);
    let m1 = ctx.messages[0].clone();
    let msg = store.get_message(&m1).expect("m1 on disk");
    assert_eq!(msg.role, Role::User);

    // Embed ctx1 + reference m1 with a role rewrite -> forged copy with
    // provenance, ctx1 registered as parent.
    let out = f.run("createContext", json!({
        "messages": [
            { "context_id": ctx1 },
            { "message_id": m1.as_str(), "role": "assistant" }
        ]
    })).await;
    assert!(!out.is_error, "unexpected error: {}", text(&out));
    let ctx2_id = detail_str(&out, "context_id");
    let forged: Vec<String> = out.details.as_ref().unwrap()["message_ids"]
        .as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    assert_eq!(forged.len(), 1, "role rewrite forges exactly one message");

    let store = mount(&f.dir);
    let ctx2 = store.get_context(&ContextId::from(ctx2_id.as_str())).expect("ctx2 on disk");
    assert_eq!(ctx2.messages.len(), 2);
    assert_eq!(ctx2.messages[0], m1);
    assert_eq!(ctx2.messages[1].as_str(), forged[0]);
    assert_eq!(ctx2.parents.len(), 1);
    assert_eq!(ctx2.parents[0].as_str(), ctx1);

    let forged_msg = store.get_message(&MessageId::from(forged[0].as_str())).unwrap();
    assert_eq!(forged_msg.role, Role::Assistant);
    let forged_json = serde_json::to_value(&forged_msg).unwrap();
    assert_eq!(
        forged_json["meta"]["source_message_id"].as_str(),
        Some(m1.as_str()),
        "forged message records provenance"
    );

    // Same-role reference does not forge.
    let out = f.run("createContext", json!({
        "messages": [ { "message_id": m1.as_str(), "role": "user" } ]
    })).await;
    assert!(!out.is_error);
    assert!(text(&out).starts_with("Created context ["), "no forge expected: {}", text(&out));
}

#[tokio::test]
async fn create_context_merge_into_stamps_facet_and_validates() {
    let f = fixture("merge");

    let out = f.run("createContext", json!({
        "messages": [ { "role": "user", "content": "envelope" } ],
        "merge_into": f.session_id.as_str()
    })).await;
    assert!(!out.is_error, "unexpected error: {}", text(&out));
    assert!(text(&out).contains(", addressed to ["), "got: {}", text(&out));
    let ctx_id = detail_str(&out, "context_id");

    let store = mount(&f.dir);
    let ctx = store.get_context(&ContextId::from(ctx_id.as_str())).unwrap();
    let dest = ctx.facet::<ri_kit::merge::MergeInto>()
        .expect("merge_into facet present")
        .expect("merge_into facet well-formed");
    assert_eq!(dest.0.to_string(), f.session_id.to_string());

    // Unknown destination is rejected before any write.
    let out = f.run("createContext", json!({
        "messages": [ { "role": "user", "content": "x" } ],
        "merge_into": "ref_nope"
    })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "merge_into: ref [ref_nope] not found");

    // Entry shape validation.
    let out = f.run("createContext", json!({
        "messages": [ { "message_id": "msg_x", "content": "both" } ]
    })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("exactly one of"), "got: {}", text(&out));

    let out = f.run("createContext", json!({
        "messages": [ { "context_id": "ctx_x", "role": "user" } ]
    })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("'role' cannot be combined"), "got: {}", text(&out));

    let out = f.run("createContext", json!({
        "messages": [ { "message_id": "msg_missing" } ]
    })).await;
    assert!(out.is_error);
    assert!(text(&out).contains("message [msg_missing] not found"), "got: {}", text(&out));

    let out = f.run("createContext", json!({})).await;
    assert!(out.is_error);
    assert!(text(&out).contains("'messages' is required"), "got: {}", text(&out));
}

#[tokio::test]
async fn read_message_and_context_graph() {
    let f = fixture("read");

    let out = f.run("createContext", json!({
        "messages": [ { "role": "user", "content": "needle in the pool" } ]
    })).await;
    let ctx1 = detail_str(&out, "context_id");
    let store = mount(&f.dir);
    let m1 = store.get_context(&ContextId::from(ctx1.as_str())).unwrap().messages[0].clone();

    let out = f.run("readMessage", json!({ "message_id": m1.as_str() })).await;
    assert!(!out.is_error);
    assert!(text(&out).contains("needle in the pool"), "got: {}", text(&out));

    let out = f.run("readMessage", json!({ "message_id": "msg_nope" })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "message 'msg_nope' not found");

    // A child context referencing ctx1 -> graph shows entry + parent.
    let out = f.run("createContext", json!({
        "messages": [ { "context_id": ctx1 }, { "role": "user", "content": "more" } ]
    })).await;
    let ctx2 = detail_str(&out, "context_id");

    let out = f.run("readContextGraph", json!({ "context_id": ctx2 })).await;
    assert!(!out.is_error);
    let graph = text(&out);
    assert!(graph.starts_with(&format!("CONTEXT GRAPH entry={} count=", ctx2)), "got: {}", graph);
    assert!(graph.contains(&format!("{} (entry) <- {}", ctx2, ctx1)), "got: {}", graph);
    assert!(graph.contains(&ctx1), "parent reachable: {}", graph);

    // Session entry point resolves through the ref head.
    let store = mount(&f.dir);
    ri_kit::chat::set_head(&store, &f.session_id, ContextId::from(ctx2.as_str())).unwrap();
    let out = f.run("readContextGraph", json!({ "session_id": f.session_id.as_str() })).await;
    assert!(!out.is_error);
    assert!(text(&out).contains(&format!("entry={}", ctx2)), "got: {}", text(&out));

    let out = f.run("readContextGraph", json!({ "session_id": "ref_nope" })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "session 'ref_nope' not found or has no head");

    let out = f.run("readContextGraph", json!({})).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "either 'session_id' or 'context_id' is required");
}

#[tokio::test]
async fn execute_tools_validate_and_hand_off_to_the_seam() {
    let f = fixture("exec");

    let out = f.run("createContext", json!({
        "messages": [ { "role": "user", "content": "seed" } ]
    })).await;
    let ctx1 = detail_str(&out, "context_id");

    // Schemas advertise the seam's models.
    let params = f.tool("runAgent").parameters().to_string();
    assert!(params.contains("stub-model"), "got: {}", params);

    let out = f.run("runAgent", json!({
        "context_id": ctx1,
        "model_id": "stub-model",
        "label": "worker",
        "model_params": { "thinking": "high", "max_tokens": 1000 }
    })).await;
    assert!(!out.is_error, "unexpected error: {}", text(&out));
    assert_eq!(text(&out), "Agent loop started on session 'ref_stub_agent'");
    assert_eq!(detail_str(&out, "session_id"), "ref_stub_agent");

    let out = f.run("runTurn", json!({ "context_id": ctx1, "model_id": "stub-model" })).await;
    assert!(!out.is_error, "unexpected error: {}", text(&out));
    assert_eq!(text(&out), "Single turn started on session 'ref_stub_turn'");

    {
        let spawned = f.seam.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 2);
        let (kind, req) = &spawned[0];
        assert_eq!(*kind, "agent");
        assert_eq!(req.model_id, "stub-model");
        assert_eq!(req.label.as_deref(), Some("worker"));
        assert_eq!(req.max_tokens, Some(1000));
        assert_eq!(req.thinking.as_ref().map(|t| t.to_string()), Some("high".to_string()));
        assert_eq!(req.messages.len(), 1, "seed context resolved to its messages");
        let (kind, req) = &spawned[1];
        assert_eq!(*kind, "turn");
        assert_eq!(req.thinking.as_ref().map(|t| t.to_string()), None, "absent thinking defers to harness");
    }

    // Validation surface.
    let out = f.run("runAgent", json!({ "model_id": "stub-model" })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "missing 'context_id' parameter");

    let out = f.run("runAgent", json!({ "context_id": ctx1 })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "missing 'model_id' parameter");

    let out = f.run("runAgent", json!({ "context_id": "ctx_nope", "model_id": "stub-model" })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "context 'ctx_nope' not found");

    let out = f.run("runAgent", json!({
        "context_id": ctx1, "model_id": "stub-model",
        "model_params": { "thinking": "bananas" }
    })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "invalid thinking level 'bananas'");

    let out = f.run("runAgent", json!({
        "context_id": ctx1, "model_id": "stub-model",
        "model_params": { "max_tokens": true }
    })).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "invalid max_tokens: true");
}
