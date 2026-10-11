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
    assert_eq!(last["tools"].as_array().unwrap().len(), 7);
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
