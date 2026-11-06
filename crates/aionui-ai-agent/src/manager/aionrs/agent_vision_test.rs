use super::agent_test::make_test_config;
use super::*;
use crate::types::BaitongVisionConfig;
use axum::{Json, Router, response::IntoResponse, routing::post};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

fn message(files: Vec<String>) -> SendMessageData {
    SendMessageData {
        content: "fixture text".into(),
        msg_id: "msg-vision".into(),
        turn_id: Some("turn-vision".into()),
        files,
        inject_skills: Vec::new(),
    }
}

#[tokio::test]
async fn unavailable_vision_blocks_images_but_text_reaches_model_without_view_image() {
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            let names: Vec<_> = body["tools"].as_array().unwrap().iter().filter_map(|tool| tool["function"]["name"].as_str()).collect();
            assert!(!names.contains(&"ViewImage"));
            assert!(!names.contains(&"look_at_image"));
            let data = json!({"id":"fixture","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"TEXT_OK"},"finish_reason":null}]});
            let stop = json!({"id":"fixture","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
            ([ ("content-type", "text/event-stream") ], format!("data: {data}\n\ndata: {stop}\n\ndata: [DONE]\n\n")).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = make_test_config();
    config.provider = "openai".into();
    config.model = "fixture-model".into();
    config.base_url = Some(format!("http://{}/v1", listener.local_addr().unwrap()));
    config.compat_overrides.image_input = Some(ImageInputCapability::Supported);
    config.vision = Some(BaitongVisionConfig {
        service: None,
        override_config: None,
    });
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    config.session_directory = dir.path().join("sessions");
    let agent = AionrsAgentManager::new(
        "vision-boundary".into(),
        dir.path().to_string_lossy().into(),
        config,
        None,
    )
    .await
    .unwrap();
    let mut rx = agent.subscribe();
    let error = agent
        .send_message(message(vec!["fixture.png".into()]))
        .await
        .unwrap_err();
    assert_eq!(error.stream_error().message, "图片识别未完成");
    assert!(error.stream_error().detail.as_ref().unwrap().contains("配置"));
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert_eq!(agent.status(), Some(ConversationStatus::Finished));
    assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|event| matches!(event, AgentStreamEvent::Error(_))));
    agent.send_message(message(Vec::new())).await.unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(agent.status(), Some(ConversationStatus::Finished));
    task.abort();
}

#[tokio::test]
async fn cancellation_during_preparation_ends_without_stale_signal() {
    let app = Router::new().route(
        "/mcp",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(20)).await;
            Json(json!({}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let mut config = make_test_config();
    config.session_directory = dir.path().join("sessions");
    config.vision = Some(BaitongVisionConfig {
        service: None,
        override_config: Some(super::super::vision::server_config(url)),
    });
    let agent = Arc::new(
        AionrsAgentManager::new(
            "vision-cancel".into(),
            dir.path().to_string_lossy().into(),
            config,
            None,
        )
        .await
        .unwrap(),
    );
    let sending = agent.clone();
    let send = tokio::spawn(async move { sending.send_message(message(vec!["fixture.png".into()])).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(agent.status(), Some(ConversationStatus::Running));
    agent.cancel().await.unwrap();
    timeout(Duration::from_secs(1), send).await.unwrap().unwrap().unwrap();
    assert_eq!(agent.status(), Some(ConversationStatus::Finished));
    assert_no_vision_stop_signal(&agent).await;
    task.abort();
}

async fn assert_no_vision_stop_signal(agent: &AionrsAgentManager) {
    assert!(
        timeout(Duration::from_millis(20), agent.cancel_notify.notified())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn image_tool_loop_and_recovery_keep_history_and_other_tools() {
    use aion_agent::session::SessionManager;
    use base64::{Engine, engine::general_purpose::STANDARD};
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("red.png");
    std::fs::write(
        &image,
        STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLttAAAAABJRU5ErkJggg==")
            .unwrap(),
    )
    .unwrap();
    let image_path = image.to_string_lossy().to_string();
    let image_seen = Arc::new(AtomicUsize::new(0));
    let turns = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = image_seen.clone();
    let model_turns = turns.clone();
    let mcp_seen = image_seen.clone();
    let mcp_calls = calls.clone();
    let app = Router::new()
        .route("/mcp", post(move |Json(body): Json<Value>| {
            let seen = mcp_seen.clone();
            let calls = mcp_calls.clone();
            async move {
                let result = match body["method"].as_str().unwrap_or("") {
                    "initialize" => json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}),
                    "tools/list" => json!({"tools":[{"name":"look_at_image","inputSchema":{"type":"object","properties":{"question":{"type":"string"}},"required":["question"]}}]}),
                    "tools/call" => {
                        assert_eq!(body["params"]["name"], "look_at_image");
                        assert_eq!(seen.load(Ordering::SeqCst), 1, "image bytes must have reached the adapter/model boundary first");
                        calls.fetch_add(1, Ordering::SeqCst);
                        json!({"content":[{"type":"text","text":"CONTROLLED_VISION_RESULT_RED_SQUARE"}],"isError":false})
                    }
                    _ => json!({}),
                };
                Json(json!({"jsonrpc":"2.0","id":body["id"],"result":result}))
            }
        }))
        .route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let seen = seen.clone();
            let turns = model_turns.clone();
            let path = image_path.clone();
            async move {
                let phase = turns.fetch_add(1, Ordering::SeqCst);
                let names: Vec<_> = body["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
                assert_eq!(names.iter().filter(|name| **name == "look_at_image").count(), 1);
                assert!(names.contains(&"ViewImage"));
                let (delta, finish) = match phase {
                    0 => (json!({"tool_calls":[{"index":0,"id":"view-1","type":"function","function":{"name":"ViewImage","arguments":json!({"file_path":path}).to_string()}}]}), "tool_calls"),
                    1 => {
                        assert!(body["messages"].to_string().contains("data:image/png;base64,"));
                        seen.store(1, Ordering::SeqCst);
                        (json!({"tool_calls":[{"index":0,"id":"vision-1","type":"function","function":{"name":"look_at_image","arguments":"{\"question\":\"what color?\"}"}}]}), "tool_calls")
                    }
                    _ => {
                        assert!(body["messages"].to_string().contains("CONTROLLED_VISION_RESULT_RED_SQUARE"));
                        (json!({"content":"CONTROLLED_ANSWER_RED_SQUARE"}), "stop")
                    }
                };
                let delta = json!({"id":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
                let end = json!({"id":"fixture","choices":[{"index":0,"delta":{},"finish_reason":finish}]});
                ([("content-type", "text/event-stream")], format!("data: {delta}\n\ndata: {end}\n\ndata: [DONE]\n\n"))
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let server_config = super::super::vision::server_config(format!("http://{address}/mcp"));
    let mut config = make_test_config();
    config.provider = "openai".into();
    config.model = "fixture-model".into();
    config.session_mode = Some("yolo".into());
    config.base_url = Some(format!("http://{address}/v1"));
    config.compat_overrides.image_input = Some(ImageInputCapability::Supported);
    config.session_directory = dir.path().join("sessions");
    config.vision = Some(BaitongVisionConfig {
        service: None,
        override_config: Some(server_config.clone()),
    });
    let agent = AionrsAgentManager::new("vision-loop".into(), dir.path().to_string_lossy().into(), config, None)
        .await
        .unwrap();
    agent
        .send_message(message(vec![image.to_string_lossy().into()]))
        .await
        .unwrap();
    assert_eq!(turns.load(Ordering::SeqCst), 3);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Rebuild only vision runtime, leaving the engine, approvals and history in place.
    *agent.vision.as_ref().unwrap().lock().await = VisionRuntime::new(
        BaitongVisionConfig {
            service: None,
            override_config: Some(server_config),
        },
        &[],
        &HashMap::new(),
    );
    agent.send_message(message(Vec::new())).await.unwrap();
    assert_eq!(turns.load(Ordering::SeqCst), 4);
    let session = SessionManager::new(dir.path().join("sessions"), 100)
        .load("vision-loop")
        .unwrap();
    let history = serde_json::to_string(&session.messages).unwrap();
    assert!(history.contains("CONTROLLED_VISION_RESULT_RED_SQUARE"));
    assert!(history.contains("CONTROLLED_ANSWER_RED_SQUARE"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "preparation must never replay an executed tool"
    );
    task.abort();
}
