// Throwaway test for ri-store: pool + filing + serialization round-trip.

fn main() {
    use ri_store::types::*;
    use ri_store::pool::Pool;
    use ri_store::filing::SessionFiling;
    use std::collections::HashMap;

    // --- Test 1: Pool basic operations ---
    println!("Test 1: Pool basics");
    let mut pool = Pool::new();

    let m1 = Message::new("s1_m1".into(), Role::System, vec![ContentBlock::text("You are ri.")]);
    let m2 = Message::user("Fix the bug");
    let m2 = Message { id: "s1_m2".into(), ..m2 };
    let m3 = Message {
        id: "s1_a1".into(),
        role: Role::Assistant,
        content: vec![ContentBlock::text("I'll look at it.")],
        provenance: Some(Provenance {
            input: vec!["s1_m1".into(), "s1_m2".into()],
            model: "claude-sonnet-4".into(),
            ts: "2026-02-09T08:00:00Z".into(),
            usage: Some(Usage { input_tokens: 100, output_tokens: 50, cache_read_tokens: 0, cache_write_tokens: 0 }),
        }),
        meta: Some(serde_json::json!({"provider": "anthropic", "duration_ms": 2100})),
        extra: HashMap::new(),
    };

    pool.put(m1);
    pool.put(m2);
    pool.put(m3);

    assert_eq!(pool.len(), 3);
    assert!(pool.get("s1_m1").is_some());
    assert!(pool.get("s1_m2").is_some());
    assert!(pool.get("s1_a1").is_some());
    assert!(pool.get("nonexistent").is_none());

    // Derived check
    let derived = pool.derived();
    assert_eq!(derived.len(), 1);
    assert_eq!(derived[0].id, "s1_a1");

    // Derived-from check
    let from_m1 = pool.derived_from("s1_m1");
    assert_eq!(from_m1.len(), 1);
    assert_eq!(from_m1[0].id, "s1_a1");

    println!("  PASS: pool operations work");

    // --- Test 2: Serialization round-trip ---
    println!("Test 2: Serialization round-trip");
    let msg = pool.get("s1_a1").unwrap();
    let json = serde_json::to_string(msg).unwrap();
    let parsed: Message = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.id, "s1_a1");
    assert_eq!(parsed.role, Role::Assistant);
    assert!(parsed.provenance.is_some());
    let prov = parsed.provenance.unwrap();
    assert_eq!(prov.input, vec!["s1_m1", "s1_m2"]);
    assert_eq!(prov.model, "claude-sonnet-4");
    println!("  PASS: serialization round-trip");

    // --- Test 3: ContentBlock unknown variant preservation ---
    println!("Test 3: Unknown content block");
    let unknown_json = r#"{"type":"magic_spell","incantation":"lumos","power":9000}"#;
    let block: ContentBlock = serde_json::from_str(unknown_json).unwrap();
    if let ContentBlock::Unknown(v) = &block {
        assert_eq!(v["incantation"], "lumos");
    } else {
        panic!("Expected Unknown variant");
    }
    let re_serialized = serde_json::to_string(&block).unwrap();
    assert!(re_serialized.contains("lumos"));
    println!("  PASS: unknown content blocks preserved");

    // --- Test 4: Tool result with nested content ---
    println!("Test 4: Tool result serialization");
    let tr = ContentBlock::tool_result_text("tc_1", "file contents here", false);
    let json = serde_json::to_string(&tr).unwrap();
    let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
    if let ContentBlock::ToolResult { tool_use_id, content, is_error, .. } = &parsed {
        assert_eq!(tool_use_id, "tc_1");
        assert!(!is_error);
        assert_eq!(content.len(), 1);
    } else {
        panic!("Expected ToolResult");
    }
    println!("  PASS: tool result round-trip");

    // --- Test 5: Filing (write + load) ---
    println!("Test 5: Filing write + load");
    let tmp_dir = std::env::temp_dir().join("ri-store-test");
    let _ = std::fs::remove_dir_all(&tmp_dir);

    let mut filing = SessionFiling::new(tmp_dir.clone());
    let path = filing.new_session("test-session", "/tmp/project").unwrap();
    println!("  Created session at: {}", path.display());

    // Write messages
    let id1 = filing.next_id();
    let msg1 = Message::new(id1.clone(), Role::System, vec![ContentBlock::text("System prompt")]);
    filing.write_message(msg1).unwrap();

    let id2 = filing.next_id();
    let msg2 = Message::new(id2.clone(), Role::User, vec![ContentBlock::text("Hello")]);
    filing.write_message(msg2).unwrap();

    let id3 = filing.next_id();
    let msg3 = Message {
        id: id3.clone(),
        role: Role::Assistant,
        content: vec![ContentBlock::text("Hi!")],
        provenance: Some(Provenance {
            input: vec![id1.clone(), id2.clone()],
            model: "test-model".into(),
            ts: "2026-02-09T08:00:00Z".into(),
            usage: None,
        }),
        meta: None,
        extra: HashMap::new(),
    };
    filing.write_message(msg3).unwrap();

    assert_eq!(filing.pool.len(), 3);

    // Now load a fresh filing from the same directory
    let mut filing2 = SessionFiling::new(tmp_dir.clone());
    filing2.load_all().unwrap();
    assert_eq!(filing2.pool.len(), 3);
    assert!(filing2.pool.get(&id1).is_some());
    assert!(filing2.pool.get(&id2).is_some());
    assert!(filing2.pool.get(&id3).is_some());

    // Check provenance survived round-trip
    let loaded = filing2.pool.get(&id3).unwrap();
    assert!(loaded.provenance.is_some());
    assert_eq!(loaded.provenance.as_ref().unwrap().input, vec![id1.clone(), id2.clone()]);

    // Check session listing
    let sessions = filing2.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].name, "test-session");

    // Print file contents for inspection before cleanup.
    println!("\n  --- Session file contents ---");
    let content = std::fs::read_to_string(&path).unwrap();
    for line in content.lines() {
        // Pretty-print each JSON line
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            println!("  {}", serde_json::to_string_pretty(&v).unwrap().replace('\n', "\n  "));
        }
    }
    println!("  --- end ---\n");

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp_dir);
    println!("  PASS: filing write + load round-trip");

    // --- Test 6: Session header detection ---
    println!("Test 6: Header vs message detection");
    let header_json = r#"{"session":"test","ts":"2026-01-01T00:00:00Z"}"#;
    let header: SessionHeader = serde_json::from_str(header_json).unwrap();
    assert_eq!(header.session, "test");
    // A header line has no "id" field -- the filing code uses this to distinguish.
    assert!(!header_json.contains("\"id\""));
    println!("  PASS: header detection");

    // --- Test 7: Extra fields preservation ---
    println!("Test 7: Extra fields on message");
    let msg_json = r#"{"id":"x1","role":"user","content":[{"type":"text","text":"hi"}],"future_field":"preserved"}"#;
    let msg: Message = serde_json::from_str(msg_json).unwrap();
    assert_eq!(msg.id, "x1");
    assert!(msg.extra.contains_key("future_field"));
    let re = serde_json::to_string(&msg).unwrap();
    assert!(re.contains("future_field"));
    assert!(re.contains("preserved"));
    println!("  PASS: extra fields preserved");

    // --- Test 8: Empty ID rejection ---
    println!("Test 8: Empty ID rejection");
    let mut pool3 = Pool::new();
    pool3.put(Message::user("no id"));
    assert_eq!(pool3.len(), 0, "Pool should reject empty ID");
    println!("  PASS: empty ID rejected by pool");

    // Test filing rejects empty ID too.
    let tmp_dir2 = std::env::temp_dir().join("ri-store-test-empty");
    let _ = std::fs::remove_dir_all(&tmp_dir2);
    let mut filing3 = SessionFiling::new(tmp_dir2.clone());
    filing3.new_session("test", "/tmp").unwrap();
    let result = filing3.write_message(Message::user("no id"));
    assert!(result.is_err(), "Filing should reject empty ID");
    let _ = std::fs::remove_dir_all(&tmp_dir2);
    println!("  PASS: empty ID rejected by filing");

    println!("\nAll tests passed.");
}
