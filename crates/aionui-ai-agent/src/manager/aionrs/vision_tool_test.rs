use super::*;
use axum::{Json, Router, routing::post};
use std::sync::atomic::{AtomicUsize, Ordering};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\ncontrolled";

async fn fixture() -> (
    tempfile::TempDir,
    VisionTool,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let app = Router::new().route("/mcp", post(move |Json(body): Json<Value>| {
        let calls = counter.clone();
        async move {
            let result = match body["method"].as_str().unwrap_or("") {
                "initialize" => json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"2"}}),
                "tools/list" => json!({"tools":[]}),
                "tools/call" => {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let args = &body["params"]["arguments"];
                    if args["question"] == "fail" {
                        json!({"content":[{"type":"text","text":"image_upload_failed"}],"isError":true})
                    } else {
                        let data = STANDARD.decode(args["image_data_url"].as_str().unwrap().split_once(',').unwrap().1).unwrap();
                        let digest = format!("{:x}", Sha256::digest(data));
                        json!({"content":[{"type":"text","text":json!({"sha256":digest,"answer":args["question"]}).to_string()}],"isError":false})
                    }
                },
                _ => json!({}),
            };
            Json(json!({"jsonrpc":"2.0","id":body["id"],"result":result}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = super::super::vision::server_config(format!("http://{}/mcp", listener.local_addr().unwrap()));
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut manager = McpManager::connect_all(&HashMap::new()).await.unwrap();
    manager.connect_one("vision".into(), &config).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let scope = ImageScope {
        id: "trusted-scope".into(),
        ..Default::default()
    };
    (
        dir,
        VisionTool {
            name: "look_at_image".into(),
            manager: Arc::new(manager),
            scope: Arc::new(RwLock::new(scope)),
        },
        calls,
        task,
    )
}

#[tokio::test]
async fn exact_bytes_same_path_change_errors_and_history_binding() {
    let (dir, tool, calls, task) = fixture().await;
    let path = dir.path().join("image.png").to_string_lossy().to_string();
    std::fs::write(&path, PNG).unwrap();
    tool.scope.write().unwrap().register(std::slice::from_ref(&path));
    let first = tool.execute(json!({"file_path":path,"question":"first"})).await;
    assert!(!first.is_error, "{}", first.content);
    let first: Value = serde_json::from_str(&first.content).unwrap();
    let changed = [PNG, b"changed"].concat();
    std::fs::write(&path, &changed).unwrap();
    let next = tool.execute(json!({"file_path":path,"question":"second"})).await;
    let next: Value = serde_json::from_str(&next.content).unwrap();
    assert_ne!(first["sha256"], next["sha256"]);
    assert_eq!(next["sha256"], format!("{:x}", Sha256::digest(&changed)));
    assert_eq!(next["answer"], "second");
    assert!(!next.to_string().contains("data:image"));
    let error = tool.execute(json!({"file_path":path,"question":"fail"})).await;
    assert!(error.is_error);
    assert_eq!(error.content, "image_upload_failed");
    let serialized = serde_json::to_value(&tool.scope.read().unwrap().attachments).unwrap();
    let restored: HashMap<String, Attachment> = serde_json::from_value(serialized).unwrap();
    assert_eq!(restored[&path].canonical, Path::new(&path).canonicalize().unwrap());
    std::fs::remove_file(&path).unwrap();
    let error = tool.execute(json!({"file_path":path,"question":"missing"})).await;
    assert_eq!(error.content, "image_not_found");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    task.abort();
}

#[tokio::test]
async fn unregistered_cross_scope_relative_and_changed_target_never_call_mcp() {
    let (dir, tool, calls, task) = fixture().await;
    let path = dir.path().join("image.png").to_string_lossy().to_string();
    std::fs::write(&path, PNG).unwrap();
    for candidate in [&path, "image.png"] {
        let result = tool.execute(json!({"file_path":candidate,"question":"q"})).await;
        assert_eq!(result.content, "image_not_in_scope");
        assert!(result.is_error);
    }
    tool.scope.write().unwrap().register(std::slice::from_ref(&path));
    tool.scope
        .write()
        .unwrap()
        .attachments
        .get_mut(&path)
        .unwrap()
        .canonical = dir.path().join("other.png");
    assert_eq!(
        tool.execute(json!({"file_path":path,"question":"q"})).await.content,
        "image_not_in_scope"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
async fn oversized_invalid_and_question_only_inputs_fail_before_network() {
    let (dir, tool, calls, task) = fixture().await;
    let path = dir.path().join("image.png").to_string_lossy().to_string();
    std::fs::write(&path, b"not PNG").unwrap();
    tool.scope.write().unwrap().register(std::slice::from_ref(&path));
    assert_eq!(
        tool.execute(json!({"file_path":path,"question":"q"})).await.content,
        "image_invalid"
    );
    std::fs::File::create(&path)
        .unwrap()
        .set_len(20 * 1024 * 1024 + 1)
        .unwrap();
    assert_eq!(
        tool.execute(json!({"file_path":path,"question":"q"})).await.content,
        "image_too_large"
    );
    assert_eq!(
        tool.execute(json!({"question":"old"})).await.content,
        "image_argument_required"
    );
    assert!(tool.preflight(&json!({"question":"old"})).unwrap_err().is_error);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
#[ignore = "requires the controlled local real FastMCP service"]
async fn real_fastmcp_bytes_contract_and_remote_cancellation() {
    let config = super::super::vision::server_config(std::env::var("AIONUI_VISION_FIXTURE_URL").unwrap());
    let mut manager = McpManager::connect_all(&HashMap::new()).await.unwrap();
    manager.connect_one("vision".into(), &config).await.unwrap();
    let manager = Arc::new(manager);
    let dir = tempfile::tempdir().unwrap();
    let normal = dir.path().join("normal.png");
    let slow = dir.path().join("slow.png");
    std::fs::write(&normal, PNG).unwrap();
    std::fs::write(&slow, [PNG, b"slow"].concat()).unwrap();
    let scope = Arc::new(RwLock::new(ImageScope {
        id: "real-http-scope".into(),
        ..Default::default()
    }));
    scope
        .write()
        .unwrap()
        .register(&[normal.to_string_lossy().into(), slow.to_string_lossy().into()]);
    let tool = Arc::new(VisionTool {
        name: "look_at_image".into(),
        manager: manager.clone(),
        scope,
    });
    let result = tool.execute(json!({"file_path":normal,"question":"normal"})).await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result
            .content
            .contains(&format!("CONTROLLED:{:x}", Sha256::digest(PNG)))
    );
    let running = tool.clone();
    let task = tokio::spawn(async move {
        running
            .execute(json!({"file_path":slow,"question":"cancel upload"}))
            .await
    });
    for _ in 0..100 {
        let state = manager.call_tool("vision", "fixture_status", json!({})).await.unwrap();
        let state: Value = serde_json::from_str(&state).unwrap();
        if state["uploads"].as_array().unwrap().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let state = manager.call_tool("vision", "fixture_status", json!({})).await.unwrap();
    let state: Value = serde_json::from_str(&state).unwrap();
    assert_eq!(state["uploads"].as_array().unwrap().len(), 2);
    assert_eq!(
        state["recognitions"].as_array().unwrap().len(),
        1,
        "remote cancellation must prevent recognition after a cancelled upload"
    );
}
