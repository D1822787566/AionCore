//! Real AionOperation -> helper -> bundled HSSF -> final file.
use aion_tools::{Tool, aion_operation::AionOperationTool, aion_operation_contract::OperationContract};
use aionui_api_types::aion_operation_contract;
use serde_json::{Value, json};

fn tool(env: &[(String, String)]) -> AionOperationTool {
    AionOperationTool::from_runtime_env(env)
        .unwrap()
        .with_contract(Some(OperationContract::new(aion_operation_contract())))
}

async fn call(tool: &AionOperationTool, operation: &str, input: Value) -> Value {
    let result = tool
        .execute(json!({"operation":operation,"input":input,"confirm":true}))
        .await;
    assert!(!result.is_error, "{operation}: {}", result.content);
    serde_json::from_str(&result.content).unwrap()
}

#[tokio::test]
#[ignore = "set AIONUI_TEST_HSSF_HOME to a verified bundled runtime and run with --ignored"]
async fn xls_lifecycle_through_the_production_sdk_preserves_original_and_checks_final_file() {
    let home = std::env::var("AIONUI_TEST_HSSF_HOME").expect("verified HSSF bundle required");
    let task = tempfile::tempdir().unwrap();
    let source = task.path().join("input.xls");
    std::fs::write(&source, include_bytes!("fixtures/file-processing/normal.xls")).unwrap();
    let before = std::fs::read(&source).unwrap();
    let env = vec![
        ("AIONUI_HELPER_BIN".into(), env!("CARGO_BIN_EXE_aioncore").into()),
        ("AIONUI_USER_ID".into(), "sdk-user".into()),
        ("AIONUI_CONVERSATION_ID".into(), "sdk-conversation".into()),
        (
            "AIONUI_TASK_WORK_DIR".into(),
            task.path().to_string_lossy().into_owned(),
        ),
        ("AIONUI_HSSF_HOME".into(), home),
    ];
    let sdk = tool(&env);
    let prepared = call(&sdk, "file.prepare", json!({"source_path":source})).await;
    let id = &prepared["data"]["processing_id"];
    let working = prepared["data"]["working_path"].as_str().unwrap();
    let inspected = call(
        &sdk,
        "file.inspect",
        json!({"processing_id":id,"sheet":"报表","range":"B1:C1"}),
    )
    .await;
    assert_eq!(inspected["data"]["metadata"]["cells"][0]["value"], 12.5);
    let written = call(
        &sdk,
        "file.xls.write",
        json!({
            "processing_id":id,"expected_sha256":prepared["data"]["working_sha256"],
            "changes":[{"sheet":"报表","cell":"B1","type":"number","value":100}]
        }),
    )
    .await;
    assert_eq!(written["data"]["recalculation"], "passed");
    let inspected = call(
        &sdk,
        "file.inspect",
        json!({"processing_id":id,"sheet":"报表","range":"B1:C1"}),
    )
    .await;
    assert_eq!(inspected["data"]["metadata"]["cells"][1]["cached_value"], 200.0);
    let finished = call(
        &sdk,
        "file.finish",
        json!({
            "processing_id":id,"expected_sha256":written["data"]["working_sha256"],"output_name":"final.xls"
        }),
    )
    .await;
    assert_eq!(finished["data"]["delivery"], "passed");
    assert_eq!(finished["data"]["cleanup"], "passed");
    assert!(!std::path::Path::new(working).exists());
    assert!(std::path::Path::new(finished["data"]["final_path"].as_str().unwrap()).is_file());
    assert_eq!(std::fs::read(source).unwrap(), before);
}
