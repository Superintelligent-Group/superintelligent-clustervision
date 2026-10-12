use super::*;
use serde_json::json;

#[test]
fn chatgpt_walks_mapping_active_branch() {
    let conv = json!({
        "id": "c1", "title": "Soy Sauce", "default_model_slug": "gpt-4o",
        "create_time": 1_700_000_000.0, "update_time": 1_700_000_100.0,
        "current_node": "n2",
        "mapping": {
            "root": { "id": "root", "parent": null, "children": ["n1"], "message": null },
            "n1": { "id": "n1", "parent": "root", "children": ["n2"],
                "message": { "author": { "role": "user" }, "content": { "content_type": "text", "parts": ["how to brew?"] }, "create_time": 1_700_000_001.0, "recipient": "all" } },
            "n2": { "id": "n2", "parent": "n1", "children": [],
                "message": { "author": { "role": "assistant" }, "content": { "content_type": "text", "parts": ["soak the beans"] }, "create_time": 1_700_000_002.0, "recipient": "all" } },
            // an off-branch sibling that current_node does NOT reach — must be excluded
            "n9": { "id": "n9", "parent": "n1", "children": [],
                "message": { "author": { "role": "assistant" }, "content": { "content_type": "text", "parts": ["DEAD BRANCH"] }, "recipient": "all" } }
        }
    });
    let msgs = chatgpt_messages(&conv);
    assert_eq!(msgs.len(), 2, "only the root->current_node branch");
    assert_eq!(msgs[0].role, Role::User);
    assert_eq!(msgs[1].role, Role::Assistant);
    assert!(msgs[0].text().unwrap().contains("how to brew"));
    assert!(msgs[1].text().unwrap().contains("soak the beans"));
    assert!(!msgs
        .iter()
        .any(|m| m.text().unwrap_or_default().contains("DEAD BRANCH")));

    let r = conv_ref(Kind::ChatGpt, std::path::Path::new("/x/conversations.json"), &conv).unwrap();
    assert_eq!(r.id, "c1");
    assert_eq!(r.harness, Harness::ChatGptExport);
    assert_eq!(r.title.as_deref(), Some("Soy Sauce"));
    assert_eq!(r.message_count, 2);
    assert!(r.created_at.is_some());
}

#[test]
fn chatgpt_tool_call_and_image() {
    let conv = json!({
        "id": "c2", "current_node": "t1",
        "mapping": {
            "t1": { "id": "t1", "parent": null, "children": [],
                "message": { "author": { "role": "assistant" }, "recipient": "python",
                    "content": { "content_type": "code", "text": "print(1)" } } },
        }
    });
    let m = &chatgpt_messages(&conv)[0];
    assert!(matches!(&m.content[0], Block::ToolUse { name, .. } if name == "python"));

    let conv2 = json!({ "id": "c3", "current_node": "i1", "mapping": {
        "i1": { "id": "i1", "parent": null, "children": [],
            "message": { "author": { "role": "assistant" },
                "content": { "content_type": "multimodal_text", "parts": [ {"content_type":"image_asset_pointer","asset_pointer":"file-service://abc"}, "see image" ] } } }
    }});
    let m2 = &chatgpt_messages(&conv2)[0];
    assert!(m2
        .content
        .iter()
        .any(|b| matches!(b, Block::Image { data_ref: Some(p), .. } if p == "file-service://abc")));
    assert!(m2
        .content
        .iter()
        .any(|b| matches!(b, Block::Text { text } if text.to_string().contains("see image"))));
}

#[test]
fn chatgpt_tool_call_links_to_result() {
    // assistant(recipient=browser, code) → tool(author.name=browser, result), linked by name+order.
    let conv = json!({ "id": "c4", "current_node": "r1", "mapping": {
        "call": { "id": "call", "parent": null, "children": ["r1"],
            "message": { "author": { "role": "assistant" }, "recipient": "browser",
                "content": { "content_type": "code", "text": "search(\"x\")" } } },
        "r1": { "id": "r1", "parent": "call", "children": [],
            "message": { "author": { "role": "tool", "name": "browser" }, "recipient": "all",
                "content": { "content_type": "tether_quote", "text": "the answer is 42" } } }
    }});
    let msgs = chatgpt_messages(&conv);
    assert_eq!(msgs.len(), 2);
    // the call
    let Block::ToolUse { id, name, input, .. } = &msgs[0].content[0] else {
        panic!("expected ToolUse")
    };
    assert_eq!(id, "call");
    assert_eq!(name, "browser");
    assert!(serde_json::to_string(input).unwrap().contains("search")); // raw content kept as input
                                                                       // the result, wired back to the call id by tool name
    let Block::ToolResult {
        tool_use_id,
        content,
        tool_name,
        ..
    } = &msgs[1].content[0]
    else {
        panic!("expected ToolResult")
    };
    assert_eq!(tool_use_id, "call", "result linked to the matching call");
    assert_eq!(tool_name.as_deref(), Some("browser"));
    assert!(content.to_string().contains("the answer is 42"));
}

#[test]
fn kind_and_origin_chatgpt_and_claude() {
    // ChatGPT: user prompt, assistant reply, a recipient!=all tool call (still a reply), tool result.
    let conv = json!({ "id": "k", "current_node": "res", "mapping": {
        "u": { "id": "u", "parent": null, "children": ["call"],
            "message": { "author": { "role": "user" }, "content": { "content_type": "text", "parts": ["hi"] }, "recipient": "all" } },
        "call": { "id": "call", "parent": "u", "children": ["res"],
            "message": { "author": { "role": "assistant" }, "recipient": "python", "content": { "content_type": "code", "text": "print(1)" } } },
        "res": { "id": "res", "parent": "call", "children": [],
            "message": { "author": { "role": "tool", "name": "python" }, "recipient": "all", "content": { "content_type": "execution_output", "text": "1" } } }
    }});
    let msgs = chatgpt_messages(&conv);
    assert_eq!((msgs[0].kind, msgs[0].origin), (MessageKind::Prompt, Origin::Human));
    assert_eq!((msgs[1].kind, msgs[1].origin), (MessageKind::Reply, Origin::Model));
    assert_eq!(
        (msgs[2].kind, msgs[2].origin),
        (MessageKind::ToolResult, Origin::Harness)
    );
    for m in &msgs {
        for k in m.extra.keys() {
            assert!(k == "chatgpt-export" || k == "_record", "unexpected extra key {k}");
        }
    }

    // Claude.ai export: human prompt, assistant reply.
    let cc = json!({ "uuid": "c", "chat_messages": [
        { "uuid": "m1", "sender": "human", "content": [{ "type": "text", "text": "q" }] },
        { "uuid": "m2", "sender": "assistant", "content": [{ "type": "text", "text": "a" }] }
    ]});
    let cm = claude_messages(&cc);
    assert_eq!((cm[0].kind, cm[0].origin), (MessageKind::Prompt, Origin::Human));
    assert_eq!((cm[1].kind, cm[1].origin), (MessageKind::Reply, Origin::Model));
}

#[test]
fn claude_export_blocks() {
    let conv = json!({
        "uuid": "u1", "name": "Refactor chat", "created_at": "2026-01-01T00:00:00Z",
        "chat_messages": [
            { "uuid": "m1", "sender": "human", "parent_message_uuid": null, "text": "fix it",
              "content": [{ "type": "text", "text": "fix it" }] },
            { "uuid": "m2", "sender": "assistant", "parent_message_uuid": "m1", "text": "",
              "content": [
                  { "type": "thinking", "thinking": "let me think" },
                  { "type": "text", "text": "here's the fix" },
                  { "type": "tool_use", "id": "tu1", "name": "str_replace", "input": {"path":"a.rs"} }
              ] }
        ]
    });
    let msgs = claude_messages(&conv);
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, Role::User);
    assert_eq!(msgs[1].role, Role::Assistant);
    let a = &msgs[1];
    assert!(matches!(&a.content[0], Block::Thinking { .. }));
    assert!(matches!(&a.content[1], Block::Text { .. }));
    assert!(matches!(&a.content[2], Block::ToolUse { name, .. } if name == "str_replace"));

    let r = conv_ref(Kind::Claude, std::path::Path::new("/x/conversations.json"), &conv).unwrap();
    assert_eq!(r.id, "u1");
    assert_eq!(r.harness, Harness::ClaudeExport);
    assert_eq!(r.message_count, 2);
}

#[test]
fn parse_ref_finds_conversation_in_file_and_sniffs_kind() {
    let dir = std::env::temp_dir().join(format!("cv-export-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("conversations.json");
    let arr = json!([
        { "uuid": "a", "name": "first", "chat_messages": [{ "uuid":"m","sender":"human","content":[{"type":"text","text":"hi"}] }] },
        { "uuid": "b", "name": "second", "chat_messages": [{ "uuid":"m2","sender":"assistant","content":[{"type":"text","text":"yo"}] }] }
    ]);
    std::fs::write(&path, arr.to_string()).unwrap();

    // sniff signal: Claude convos have chat_messages, not a ChatGPT mapping
    let (_t, arr) = read_array(&path).unwrap();
    let first: serde_json::Value = serde_json::from_str(arr[0].get()).unwrap();
    assert!(first.get("chat_messages").is_some() && first.get("mapping").is_none());

    let r = SessionRef {
        title: Some("second".into()),
        message_count: 1,
        ..SessionRef::new("b", Harness::ClaudeExport, path.clone())
    };
    let s = parse_ref(Kind::Claude, &r).unwrap();
    assert_eq!(s.id, "b");
    assert_eq!(s.messages.len(), 1);
    assert_eq!(s.messages[0].role, Role::Assistant);
    assert!(s.messages[0].text().unwrap().contains("yo"));
    std::fs::remove_dir_all(&dir).ok();
}

/// Real exports in ~/Downloads (or $CV_EXPORTS) — ignored by default (machine-specific).
#[test]
#[ignore]
fn real_downloads_exports_parse() {
    // discovery is opt-in; point it at Downloads for this machine-specific probe.
    std::env::set_var("CV_EXPORTS", dirs::home_dir().unwrap().join("Downloads"));
    for kind in [Kind::ChatGpt, Kind::Claude] {
        let refs = discover_kind(kind);
        eprintln!("{:?}: {} conversation(s)", kind, refs.len());
        if let Some(r) = refs.iter().find(|r| r.message_count > 0) {
            let s = parse_ref(kind, r).unwrap();
            let tools = s
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .filter(|b| matches!(b, Block::ToolUse { .. } | Block::ToolResult { .. }))
                .count();
            eprintln!(
                "  parsed {} -> {} msgs ({} tool blocks), model {:?}",
                &r.id[..8.min(r.id.len())],
                s.messages.len(),
                tools,
                s.model
            );
            assert!(!s.messages.is_empty());
        }
    }
    std::env::remove_var("CV_EXPORTS");
}

#[test]
fn native_archive_matches_owned_codec_across_modes_and_invalidation() {
    use crate::stream::{CollectSink, Flow, MessageSink, ParseOptions};
    struct Stop;
    impl MessageSink for Stop {
        fn message(&mut self, _: Message) -> Flow {
            Flow::Stop
        }
    }
    let dir = std::env::temp_dir().join(format!("cv-export-native-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("conversations.json");
    let body = "a\n\"é😀".repeat(8192);
    let conversations = [
        (
            Kind::Claude,
            json!({"uuid":"claude", "name":"", "chat_messages":[
                {"uuid":"u","sender":"human","content":[{"type":"text","text":body}]},
                {"uuid":"a","parent_message_uuid":"u","sender":"assistant","content":[
                    {"type":"thinking","thinking":"","text":body},
                    {"type":"tool_use","id":"t","name":"read","input":{"a":1,"payload":body}},
                    {"type":"tool_result","tool_use_id":"t","text":body},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":body}},
                    {"type":"document","title":"a","source":{"type":"file","file_id":"file1"}}
                ]}, {"sender":"human","text":"fallback"}, {"sender":"other","text":"system"}
            ]}),
        ),
        (
            Kind::ChatGpt,
            json!({"conversation_id":"chatgpt", "mapping":{
                "0":{"id":"u","parent":null,"message":{"author":{"role":"user"},"content":{"parts":[body]}}},
                "1":{"id":"t","parent":"0","message":{"author":{"role":"assistant"},"recipient":"python","content":{"text":body}}},
                "2":{"id":"r","parent":"1","message":{"author":{"role":"tool","name":"python"},"content":{"text":body}}},
                "3":{"id":"a","parent":"2","message":{"author":{"role":"assistant"},"content":{"parts":["","two",{"asset_pointer":"file-service://a"}]}}},
                "4":{"id":"s","parent":"3","message":{"author":{"role":"system"},"content":{"text":"context"}}}
            }}),
        ),
    ];
    for (kind, conv) in conversations {
        let prefix = match kind {
            Kind::Claude => json!({"uuid":"unrelated","chat_messages":[],"unknown":body}),
            Kind::ChatGpt => json!({"id":"unrelated","mapping":{},"unknown":body}),
        };
        std::fs::write(&path, serde_json::to_vec(&json!([prefix, conv])).unwrap()).unwrap();
        let expected_ref = conv_ref(kind, &path, &conv).unwrap();
        let refs = discover_kind_in(kind, vec![path.clone()]);
        assert_eq!(
            serde_json::to_value(refs.iter().find(|r| r.id == expected_ref.id).unwrap()).unwrap(),
            serde_json::to_value(&expected_ref).unwrap()
        );
        let expected = parse_ref(kind, &expected_ref).unwrap();
        for opts in [ParseOptions::lazy(), ParseOptions::full(), ParseOptions::complete()] {
            let mut sink = CollectSink::default();
            let mut actual = native::stream(kind, &expected_ref, &opts, &mut sink).unwrap();
            actual.messages = sink.messages;
            assert_eq!(actual.messages.iter().flat_map(|m| &m.content).any(|block| {
                matches!(block, Block::ToolUse { input: crate::json::Json::Source(_), .. })
            }), opts.spans);
            actual.materialize().unwrap();
            assert_eq!(
                serde_json::to_string(&actual).unwrap(),
                serde_json::to_string(&expected).unwrap()
            );
            assert_eq!(
                serde_json::to_value(&actual).unwrap(),
                serde_json::to_value(&expected).unwrap()
            );
        }
        assert_eq!(
            native::stream(kind, &expected_ref, &ParseOptions::lazy(), &mut Stop)
                .unwrap()
                .title,
            expected.title
        );
        // A same-size replacement with a new inode must invalidate descriptors and metadata.
        let changed = serde_json::to_string(&json!([conv]))
            .unwrap()
            .replace(&expected_ref.id, "changed");
        let replacement = dir.join("replacement.json");
        std::fs::write(&replacement, changed).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let fresh = discover_kind_in(kind, vec![path.clone()]);
        assert_eq!(fresh[0].id, "changed");
        assert!(native::stream(kind, &expected_ref, &ParseOptions::lazy(), &mut Stop).is_err());
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn native_archive_matches_raw_value_acceptance_and_duplicate_precedence() {
    use crate::stream::{CollectSink, ParseOptions};
    let dir = std::env::temp_dir().join(format!("cv-export-validation-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("conversations.json");
    let valid = r#"{"uuid":"same","chat_messages":[{"sender":"human","text":"ok"}]}"#;
    for invalid in ["1e400", "\"\\ud800\""] {
        let bad = format!("{{\"uuid\":\"same\",\"chat_messages\":[],\"unknown\":{invalid},\"unknown\":0}}");
        std::fs::write(&path, format!("[{bad},{valid}]")).unwrap();
        // Invalid first conversation prevents sniffed discovery; explicit reads skip it.
        assert!(discover_kind_in(Kind::Claude, vec![path.clone()]).is_empty());
        let r = SessionRef::new("same", Harness::ClaudeExport, path.clone());
        let expected = parse_ref(Kind::Claude, &r).unwrap();
        let mut sink = CollectSink::default();
        let mut actual = native::stream(Kind::Claude, &r, &ParseOptions::lazy(), &mut sink).unwrap();
        actual.messages = sink.messages;
        actual.materialize().unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
    let duplicate = r#"[{"conversation_id":false,"uuid":"hidden","chat_messages":[]},{"uuid":"old","\u0075uid":"new","chat_messages":[]}]"#;
    std::fs::write(&path, duplicate).unwrap();
    let refs = discover_kind_in(Kind::Claude, vec![path.clone()]);
    assert_eq!(refs.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["new"]);
    std::fs::write(&path, "[]").unwrap();
    assert!(discover_kind_in(Kind::Claude, vec![path.clone()]).is_empty());
    std::fs::write(&path, "").unwrap();
    assert!(discover_kind_in(Kind::Claude, vec![path.clone()]).is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cached_export_discovery_preserves_the_callers_path_spelling() {
    let relative_dir = PathBuf::from(format!("cv-export-path-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&relative_dir).unwrap();
    let relative = relative_dir.join("conversations.json");
    let absolute = std::env::current_dir().unwrap().join(&relative);
    std::fs::write(&relative, r#"[{"uuid":"same","chat_messages":[]}]"#).unwrap();
    let first = discover_kind_in(Kind::Claude, vec![relative.clone()]);
    let second = discover_kind_in(Kind::Claude, vec![absolute.clone()]);
    assert_eq!(first[0].path, relative);
    assert_eq!(second[0].path, absolute);
    std::fs::remove_dir_all(relative_dir).unwrap();
}

#[test]
fn native_chatgpt_fallback_keeps_insertion_order_and_final_duplicate_value() {
    use crate::stream::{CollectSink, ParseOptions};
    let dir = std::env::temp_dir().join(format!("cv-export-order-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("conversations.json");
    let archive = r#"[{"id":"ordered","mapping":{
        "z":{"id":"discarded","message":{"author":{"role":"user"},"content":{"parts":["old"]}}},
        "a":{"id":"second","message":{"author":{"role":"assistant"},"recipient":"python","content":{"z":1,"a":2,"text":"run"}}},
        "\u007a":{"id":"first","message":{"author":{"role":"user"},"content":{"parts":["replacement"]}}}
    }}]"#;
    std::fs::write(&path, archive).unwrap();
    let r = SessionRef::new("ordered", Harness::ChatGptExport, path.clone());
    let expected = parse_ref(Kind::ChatGpt, &r).unwrap();
    assert_eq!(
        expected.messages.iter().map(|m| m.id.as_deref()).collect::<Vec<_>>(),
        [Some("first"), Some("second")]
    );
    for opts in [ParseOptions::lazy(), ParseOptions::full(), ParseOptions::complete()] {
        let mut sink = CollectSink::default();
        let mut actual = native::stream(Kind::ChatGpt, &r, &opts, &mut sink).unwrap();
        actual.messages = sink.messages;
        actual.materialize().unwrap();
        assert_eq!(
            serde_json::to_string(&actual).unwrap(),
            serde_json::to_string(&expected).unwrap()
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}
