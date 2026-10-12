//! Drives the full agent loop against a mock OpenAI-compatible server.

use hermit::agent::{Agent, Outcome};
use hermit::config::Settings;
use hermit::llm::Role;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse_tool_call(id: &str, name: &str, args: serde_json::Value) -> String {
    let chunk = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":id,"type":"function",
        "function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]});
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

fn sse_text(text: &str) -> String {
    let chunk = json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":10,"completion_tokens":3}});
    format!(": OPENROUTER PROCESSING\n\ndata: {chunk}\n\ndata: [DONE]\n\n")
}

fn sse(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(body)
}

#[tokio::test]
async fn edits_then_fixes_until_verification_passes() {
    let server = MockServer::start().await;
    // Scripted model: each response is served once, in registration order.
    let script = vec![
        ResponseTemplate::new(429).insert_header("retry-after", "0"),
        sse(sse_tool_call("c1", "read_file", json!({"path":"calc.txt"}))),
        sse(sse_tool_call("c2", "edit_file", json!({"path":"calc.txt","old":"2+2=5","new":"2+2=6"}))),
        sse(sse_text("Fixed the sum.")),
        // Verification fails (expects 4) → model is asked to fix.
        sse(sse_tool_call("c3", "edit_file", json!({"path":"calc.txt","old":"2+2=6","new":"2+2=4"}))),
        sse(sse_text("Now correct.")),
    ];
    for r in script {
        Mock::given(method("POST")).and(path("/chat/completions")).respond_with(r).up_to_n_times(1).mount(&server).await;
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("calc.txt"), "2+2=5\n").unwrap();

    let settings = Settings {
        root: root.clone(),
        base_url: server.uri(),
        api_key: Some("test".into()),
        model: "mock-model".into(),
        fallback_models: vec![],
        verify_cmd: Some("grep -q '2+2=4' calc.txt".into()),
        verify: true,
        allow_commands: vec![],
        max_steps: 10,
        max_fix_attempts: 2,
        context_budget_tokens: 60_000,
        command_timeout_secs: 10,
        temperature: None,
        max_tokens: None,
        tool_mode: hermit::config::ToolMode::Auto,
        yolo: true,
    };
    let mut agent = Agent::new(settings).unwrap();
    let outcome = agent.run_turn("make the sum correct").await.unwrap();

    assert_eq!(outcome, Outcome::Verified);
    assert_eq!(std::fs::read_to_string(root.join("calc.txt")).unwrap(), "2+2=4\n");
    assert_eq!(agent.prompt_tokens, 20);

    // Conversation shape: tool results are paired with their calls, and the failure was fed back.
    let tool_ids: Vec<_> = agent.messages.iter().filter(|m| m.role == Role::Tool).filter_map(|m| m.tool_call_id.clone()).collect();
    assert_eq!(tool_ids, ["c1", "c2", "c3"]);
    assert!(agent.messages.iter().any(|m| m.role == Role::User
        && m.content.as_deref().is_some_and(|c| c.starts_with("Automatic verification"))));

    // The request body carried tools and the full history.
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 6);
    let last: serde_json::Value = serde_json::from_slice(&reqs[5].body).unwrap();
    assert_eq!(last["model"], "mock-model");
    assert_eq!(last["tools"].as_array().unwrap().len(), 9);
    assert!(last["stream"].as_bool().unwrap());

    // /undo reverts the whole turn.
    agent.tools.undo.undo().unwrap();
    assert_eq!(std::fs::read_to_string(root.join("calc.txt")).unwrap(), "2+2=5\n");
}

#[tokio::test]
async fn falls_back_to_next_model_when_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::body_partial_json(json!({"model":"busy"})))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::body_partial_json(json!({"model":"backup"})))
        .respond_with(sse(sse_text("hi from backup")))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let mut s = Settings::load(&dir.path().canonicalize().unwrap()).unwrap();
    s.base_url = server.uri();
    s.model = "busy".into();
    s.fallback_models = vec!["backup".into()];
    let mut agent = Agent::new(s).unwrap();
    agent.client.retries = 1;
    let outcome = agent.run_turn("hello").await.unwrap();
    assert_eq!(outcome, Outcome::Done);
    assert_eq!(agent.messages.last().unwrap().content.as_deref(), Some("hi from backup"));
}

fn settings_for(server: &MockServer, root: &std::path::Path) -> Settings {
    let mut s = Settings::load(root).unwrap();
    s.base_url = server.uri();
    s.api_key = Some("test".into());
    s.model = "mock-model".into();
    s.fallback_models = vec![];
    s.tool_mode = hermit::config::ToolMode::Auto;
    s.verify_cmd = None;
    s.yolo = true;
    s
}

/// Stream `text` as several content deltas, splitting it at awkward places.
fn sse_text_chunks(text: &str, parts: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let size = chars.len().div_ceil(parts);
    let mut out = String::new();
    for chunk in chars.chunks(size) {
        let piece: String = chunk.iter().collect();
        out.push_str(&format!("data: {}\n\n", json!({"choices":[{"delta":{"content":piece}}]})));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

#[tokio::test]
async fn switches_to_text_tools_when_model_rejects_native_tools() {
    let server = MockServer::start().await;
    // Any request that carries native tool definitions is rejected like OpenRouter does.
    Mock::given(method("POST"))
        .and(wiremock::matchers::body_string_contains("\"tools\":["))
        .respond_with(ResponseTemplate::new(404).set_body_string(
            r#"{"error":{"message":"No endpoints found that support tool use.","code":404}}"#,
        ))
        .mount(&server)
        .await;
    let call = "I'll fix it.\n<tool_call>\n{\"name\": \"edit_file\", \"arguments\": {\"path\": \"calc.txt\", \"old\": \"2+2=5\", \"new\": \"2+2=4\"}}\n</tool_call>";
    for r in [sse(sse_text_chunks(call, 7)), sse(sse_text("Done."))] {
        Mock::given(method("POST")).and(path("/chat/completions")).respond_with(r).up_to_n_times(1).mount(&server).await;
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("calc.txt"), "2+2=5\n").unwrap();
    let mut s = settings_for(&server, &root);
    s.verify_cmd = Some("grep -q '2+2=4' calc.txt".into());
    let mut agent = Agent::new(s).unwrap();

    let outcome = agent.run_turn("fix the sum").await.unwrap();
    assert_eq!(outcome, Outcome::Verified);
    assert!(agent.text_mode);
    assert_eq!(std::fs::read_to_string(root.join("calc.txt")).unwrap(), "2+2=4\n");

    // History stays in canonical (native) shape…
    let assistant = agent.messages.iter().find(|m| m.role == Role::Assistant).unwrap();
    assert_eq!(assistant.content.as_deref(), Some("I'll fix it."));
    assert_eq!(assistant.tool_calls[0].function.name, "edit_file");
    assert!(agent.messages.iter().any(|m| m.role == Role::Tool));

    // …but text-mode requests carry no tool roles, and the result comes back as <tool_result>.
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 3);
    let last: serde_json::Value = serde_json::from_slice(&reqs[2].body).unwrap();
    assert!(last.get("tools").is_none());
    let msgs = last["messages"].as_array().unwrap();
    assert!(msgs.iter().all(|m| m["role"] != "tool"));
    assert!(msgs[0]["content"].as_str().unwrap().contains("<tool_call>"));
    assert!(msgs.last().unwrap()["content"].as_str().unwrap().starts_with("<tool_result name=\"edit_file\">"));
}

#[tokio::test]
async fn compaction_summarizes_old_turns_and_keeps_recent_ones() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::body_string_contains("You compress the earlier part"))
        .respond_with(sse(sse_text("Goal: fix parser. Done: edited src/parse.rs.")))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = settings_for(&server, &root);
    s.context_budget_tokens = 2_000;
    let mut agent = Agent::new(s).unwrap();
    for i in 0..10 {
        agent.messages.push(hermit::llm::Message::user(format!("request {i}: {}", "x".repeat(1200))));
        agent.messages.push(hermit::llm::Message::tool(format!("c{i}"), "y".repeat(1200)));
    }
    agent.messages.push(hermit::llm::Message::user("latest request"));
    let before = agent.messages.len();

    agent.compact(false).await;

    assert!(agent.messages.len() < before);
    assert_eq!(agent.messages[0].role, Role::System);
    let summary = agent.messages[1].content.as_deref().unwrap();
    assert!(summary.starts_with("[Summary of the earlier conversation"));
    assert!(summary.contains("edited src/parse.rs"));
    assert_eq!(agent.messages.last().unwrap().content.as_deref(), Some("latest request"));
    assert!(agent.messages[2].role != Role::Tool, "kept part must not start with an orphan tool result");
    let reqs = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&reqs[0].body);
    assert!(body.contains("request 0"));
    assert!(!body.contains("\"tools\""));
}

#[tokio::test]
async fn sessions_save_and_resume() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(sse_tool_call("c1", "todo_write", json!({"todos":[{"content":"step one","status":"in_progress"}]}))))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST")).respond_with(sse(sse_text("Planned."))).mount(&server).await;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let sessions = dir.path().join("sessions");
    let mut agent = Agent::new(settings_for(&server, &root)).unwrap();
    agent.sessions_dir = Some(sessions.clone());
    agent.run_turn("plan the refactor").await.unwrap();
    agent.save_session();
    let id = agent.session_id.clone();

    let mut fresh = Agent::new(settings_for(&server, &root)).unwrap();
    fresh.sessions_dir = Some(sessions.clone());
    assert_ne!(fresh.session_id, id);
    let resolved = hermit::session::resolve(&sessions, &root, "").unwrap();
    assert_eq!(resolved, id);
    fresh.resume(hermit::session::Session::load(&sessions, &resolved).unwrap());

    assert_eq!(fresh.session_id, id);
    assert_eq!(fresh.messages.len(), agent.messages.len());
    assert_eq!(fresh.messages[0].role, Role::System);
    assert_eq!(fresh.messages[1].content.as_deref(), Some("plan the refactor"));
    assert_eq!(fresh.tools.todos.len(), 1);
    assert_eq!(hermit::session::list(&sessions, &root)[0].title, "plan the refactor");
}
