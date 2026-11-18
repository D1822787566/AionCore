use aionui_ai_agent::{ActiveLeaseRegistry, AgentError, AgentInstance, IWorkerTaskManager};
use aionui_api_types::WebSocketMessage;
use aionui_auth::{AuthIdentityMode, AuthState, JwtService, auth_middleware};
use aionui_common::{AgentKillReason, TimestampMs};
use aionui_conversation::skill_resolver::{ResolvedAgentSkill, SkillResolver};
use aionui_conversation::{ConversationRouterState, ConversationService, conversation_routes};
use aionui_db::models::SubagentRunRow;
use aionui_db::{
    ISubagentRepository, IUserRepository, SqliteSubagentRepository, SqliteUserRepository, init_database_memory,
};
use aionui_realtime::EventBroadcaster;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    middleware,
};
use std::sync::Arc;
use tower::ServiceExt;

struct Noop;
impl EventBroadcaster for Noop {
    fn broadcast(&self, _: WebSocketMessage<serde_json::Value>) {}
}
#[async_trait::async_trait]
impl SkillResolver for Noop {
    async fn auto_inject_names(&self) -> Vec<String> {
        vec![]
    }
    async fn resolve_skills(&self, _: &[String]) -> Vec<ResolvedAgentSkill> {
        vec![]
    }
}
#[async_trait::async_trait]
impl IWorkerTaskManager for Noop {
    fn get_task(&self, _: &str) -> Option<AgentInstance> {
        None
    }
    async fn get_or_build_task(
        &self,
        _: &str,
        _: aionui_ai_agent::types::BuildTaskOptions,
    ) -> Result<AgentInstance, AgentError> {
        Err(AgentError::internal("unused"))
    }
    fn kill(&self, _: &str, _: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
    fn kill_and_wait(
        &self,
        _: &str,
        _: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }
    async fn clear(&self) {}
    fn active_count(&self) -> usize {
        0
    }
    fn collect_idle(&self, _: TimestampMs) -> Vec<String> {
        vec![]
    }
}
async fn setup() -> (Router, String, String) {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool().clone();
    let users = Arc::new(SqliteUserRepository::new(pool.clone()));
    let other = users.create_user("other", "test-hash").await.unwrap();
    for id in ["c", "other-conversation"] {
        sqlx::query("INSERT INTO conversations(id,user_id,name,type,extra,created_at,updated_at) VALUES(?,'system_default_user','test','aionrs','{}',1,1)")
            .bind(id).execute(&pool).await.unwrap();
    }
    let records = Arc::new(SqliteSubagentRepository::new(pool.clone()));
    for index in 0..3 {
        records
            .apply(
                "system_default_user",
                &SubagentRunRow {
                    child_run_id: format!("child-{index}"),
                    conversation_id: "c".into(),
                    turn_id: "turn".into(),
                    parent_call_id: "aionrs-spawn".into(),
                    task_index: index,
                    name: "same".into(),
                    prompt: "task".into(),
                    latest_activity: None,
                    status: "completed".into(),
                    started_at: index,
                    ended_at: Some(index + 1),
                    elapsed_ms: Some(1),
                    result: Some("done".into()),
                    seq: 1,
                    incomplete: false,
                },
                &[],
            )
            .await
            .unwrap();
    }
    let manager: Arc<dyn IWorkerTaskManager> = Arc::new(Noop);
    let service = ConversationService::new(
        std::env::temp_dir(),
        Arc::new(Noop),
        Arc::new(Noop),
        manager.clone(),
        Arc::new(aionui_db::SqliteConversationRepository::new(pool.clone())),
        Arc::new(aionui_db::SqliteAgentMetadataRepository::new(pool.clone())),
        Arc::new(aionui_db::SqliteAcpSessionRepository::new(pool)),
    );
    service.with_subagent_repo(records);
    let jwt = Arc::new(JwtService::new("test-subagent-history-only-secret".into()));
    let owner_token = jwt.sign("system_default_user", "admin").unwrap();
    let other_token = jwt.sign(&other.id, "other").unwrap();
    let auth = AuthState {
        jwt_service: jwt,
        user_repo: users,
        identity_mode: AuthIdentityMode::UserSession,
        runtime_token_verifier: None,
    };
    let router = conversation_routes(ConversationRouterState {
        service,
        task_manager: manager,
        active_leases: Arc::new(ActiveLeaseRegistry::new()),
    })
    .layer(middleware::from_fn_with_state(auth, auth_middleware));
    (router, owner_token, other_token)
}
async fn get(router: &Router, path: &str, token: Option<&str>) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().uri(path);
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1_000_000).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}
#[tokio::test]
async fn authenticated_history_pages_with_no_runtime_or_observer() {
    let (app, token, _) = setup().await;
    let (status, first) = get(&app, "/api/conversations/c/subagents?limit=2", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["data"]["items"].as_array().unwrap().len(), 2);
    let (_, second) = get(
        &app,
        "/api/conversations/c/subagents?limit=2&after=child-1",
        Some(&token),
    )
    .await;
    assert_eq!(second["data"]["items"][0]["child_run_id"], "child-2");
}
#[tokio::test]
async fn history_requires_auth_and_rejects_other_user_and_wrong_conversation() {
    let (app, token, other) = setup().await;
    assert_eq!(
        get(&app, "/api/conversations/c/subagents", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&app, "/api/conversations/c/subagents", Some(&other)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(
            &app,
            "/api/conversations/other-conversation/subagents/child-0/entries",
            Some(&token)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}
#[tokio::test]
async fn history_validates_pagination_and_missing_children() {
    let (app, token, _) = setup().await;
    for path in [
        "/api/conversations/c/subagents?limit=0",
        "/api/conversations/c/subagents?limit=101",
        "/api/conversations/c/subagents/child-0/entries?after=-1",
    ] {
        assert_eq!(get(&app, path, Some(&token)).await.0, StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        get(&app, "/api/conversations/c/subagents/missing/entries", Some(&token))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}
