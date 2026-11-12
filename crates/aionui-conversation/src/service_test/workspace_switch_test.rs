use super::*;
use aionui_ai_agent::WorkerTaskManagerImpl;
use aionui_api_types::SwitchConversationWorkspaceRequest;
use aionui_common::now_ms;
use aionui_db::{SqliteConversationRepository, SqliteProjectStore};

struct WorkspaceProbe {
    id: String,
    workspace: String,
    status: Mutex<ConversationStatus>,
    events: broadcast::Sender<AgentStreamEvent>,
    release: Arc<Notify>,
    started: Arc<Notify>,
    kills: Arc<AtomicUsize>,
    backgrounds: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl IAgentTask for WorkspaceProbe {
    fn agent_type(&self) -> AgentType {
        AgentType::Aionrs
    }
    fn conversation_id(&self) -> &str {
        &self.id
    }
    fn workspace(&self) -> &str {
        &self.workspace
    }
    fn status(&self) -> Option<ConversationStatus> {
        Some(*self.status.lock().unwrap())
    }
    fn last_activity_at(&self) -> TimestampMs {
        now_ms()
    }
    fn live_background_tasks(&self) -> usize {
        self.backgrounds.load(Ordering::SeqCst)
    }
    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.events.subscribe()
    }
    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        *self.status.lock().unwrap() = ConversationStatus::Running;
        self.started.notify_one();
        if data.content == "first" {
            self.release.notified().await;
        }
        std::fs::write(Path::new(&self.workspace).join(format!("{}.txt", data.content)), "kept").unwrap();
        *self.status.lock().unwrap() = ConversationStatus::Finished;
        let _ = self.events.send(AgentStreamEvent::Finish(FinishEventData::default()));
        Ok(())
    }
    async fn cancel(&self) -> Result<(), AgentError> {
        panic!("moving must not cancel the active turn")
    }
    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        self.kills.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
impl IMockAgent for WorkspaceProbe {}

struct Fixture {
    _db: aionui_db::Database,
    root: tempfile::TempDir,
    service: ConversationService,
    manager: Arc<dyn IWorkerTaskManager>,
    release: Arc<Notify>,
    started: Arc<Notify>,
    kills: Arc<AtomicUsize>,
    backgrounds: Arc<AtomicUsize>,
}

impl Fixture {
    async fn new() -> Self {
        let db = init_database_memory().await.unwrap();
        seed_test_user(db.pool(), "user_1").await;
        let root = tempfile::tempdir().unwrap();
        let release = Arc::new(Notify::new());
        let started = Arc::new(Notify::new());
        let kills = Arc::new(AtomicUsize::new(0));
        let backgrounds = Arc::new(AtomicUsize::new(0));
        let (r, s, k, b) = (release.clone(), started.clone(), kills.clone(), backgrounds.clone());
        let manager: Arc<dyn IWorkerTaskManager> = Arc::new(WorkerTaskManagerImpl::new(Arc::new(move |options| {
            let (release, started, kills, backgrounds) = (r.clone(), s.clone(), k.clone(), b.clone());
            Box::pin(async move {
                Ok(AgentInstance::Mock(Arc::new(WorkspaceProbe {
                    id: options.context.conversation.conversation_id,
                    workspace: options.context.workspace.path,
                    status: Mutex::new(ConversationStatus::Pending),
                    events: broadcast::channel(32).0,
                    release,
                    started,
                    kills,
                    backgrounds,
                })))
            })
        })));
        let service = ConversationService::new(
            root.path().to_path_buf(),
            Arc::new(MockBroadcaster::new()),
            Arc::new(FixedSkillResolver { names: vec![] }),
            manager.clone(),
            Arc::new(SqliteConversationRepository::new(db.pool().clone())),
            Arc::new(StubAgentMetadataRepo),
            Arc::new(StubAcpSessionRepo::default()),
        );
        service.with_project_service(Arc::new(aionui_project::ProjectService::new(
            Arc::new(SqliteProjectStore::new(db.pool().clone())),
            root.path().join("conversations"),
        )));
        Self {
            _db: db,
            root,
            service,
            manager,
            release,
            started,
            kills,
            backgrounds,
        }
    }

    async fn conversation(&self) -> ConversationResponse {
        self.service
            .create(
                "user_1",
                serde_json::from_value(json!({
                    "type": "aionrs", "extra": {},
                    "model": { "provider_id": "fixture", "model": "fixture" }
                }))
                .unwrap(),
            )
            .await
            .unwrap()
    }

    fn request(&self, old: &str) -> SwitchConversationWorkspaceRequest {
        let target = self.root.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        serde_json::from_value(json!({ "workspace": target, "expected_workspace": old })).unwrap()
    }

    async fn send(&self, id: &str, text: &str) {
        self.service
            .send_message(
                "user_1",
                id,
                serde_json::from_value(json!({ "content": text })).unwrap(),
                &self.manager,
            )
            .await
            .unwrap();
    }

    async fn wait_idle(&self, id: &str) {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.service.runtime_state().wait_until_unclaimed(id),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn moving_preserves_the_active_turn_and_the_next_turn_writes_to_the_new_directory() {
    let f = Fixture::new().await;
    let conv = f.conversation().await;
    let old = conv.extra["workspace"].as_str().unwrap();
    f.send(&conv.id, "first").await;
    tokio::time::timeout(Duration::from_secs(5), f.started.notified())
        .await
        .unwrap();
    let turn = f.service.runtime_state().active_turn_id_for(&conv.id);
    let moved = f
        .service
        .switch_workspace("user_1", &conv.id, f.request(old), &f.manager)
        .await
        .unwrap();
    assert!(moved.project_id.is_some());
    assert_eq!(f.service.runtime_state().active_turn_id_for(&conv.id), turn);
    assert_eq!(f.kills.load(Ordering::SeqCst), 0);
    f.release.notify_one();
    f.wait_idle(&conv.id).await;
    assert_eq!(
        std::fs::read_to_string(Path::new(old).join("first.txt")).unwrap(),
        "kept"
    );
    let target = Path::new(moved.extra["workspace"].as_str().unwrap());
    assert!(!target.join("first.txt").exists());
    f.send(&conv.id, "next").await;
    f.wait_idle(&conv.id).await;
    assert_eq!(std::fs::read_to_string(target.join("next.txt")).unwrap(), "kept");
    assert!(!Path::new(old).join("next.txt").exists());
    assert_eq!(f.kills.load(Ordering::SeqCst), 1);
    let history = f
        .service
        .list_messages("user_1", &conv.id, aionui_api_types::ListMessagesQuery::default())
        .await
        .unwrap();
    assert!(
        history
            .items
            .iter()
            .any(|message| message.content.to_string().contains("first"))
    );
    assert!(
        history
            .items
            .iter()
            .any(|message| message.content.to_string().contains("next"))
    );
}

#[tokio::test]
async fn prewarmed_pending_runtime_can_move_and_restore_default_without_being_killed() {
    let f = Fixture::new().await;
    let conv = f.conversation().await;
    let old = conv.extra["workspace"].as_str().unwrap();
    f.service.ensure_runtime("user_1", &conv.id, &f.manager).await.unwrap();
    let moved = f
        .service
        .switch_workspace("user_1", &conv.id, f.request(old), &f.manager)
        .await
        .unwrap();
    let restored = f
        .service
        .switch_workspace(
            "user_1",
            &conv.id,
            serde_json::from_value(json!({
                "restore_default": true, "expected_workspace": moved.extra["workspace"]
            }))
            .unwrap(),
            &f.manager,
        )
        .await
        .unwrap();
    assert_eq!(restored.extra["workspace"], old);
    assert!(restored.project_id.is_none());
    assert_eq!(f.kills.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn moving_rejects_other_users_stale_directories_and_missing_targets_without_stopping_work() {
    let f = Fixture::new().await;
    let conv = f.conversation().await;
    let old = conv.extra["workspace"].as_str().unwrap();
    let err = f
        .service
        .switch_workspace("other", &conv.id, f.request(old), &f.manager)
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
    let err = f
        .service
        .switch_workspace("user_1", &conv.id, f.request("stale"), &f.manager)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("WORKSPACE_SWITCH_STALE"));
    let mut req = f.request(old);
    req.workspace = Some(f.root.path().join("missing").to_string_lossy().into_owned());
    let err = f
        .service
        .switch_workspace("user_1", &conv.id, req, &f.manager)
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::WorkspacePathUnavailable { path } if path.ends_with("missing")));
    assert_eq!(f.kills.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn late_runtime_workspace_resolution_cannot_undo_a_saved_project_move() {
    let f = Fixture::new().await;
    let conv = f.conversation().await;
    let old = conv.extra["workspace"].as_str().unwrap();
    let moved = f
        .service
        .switch_workspace("user_1", &conv.id, f.request(old), &f.manager)
        .await
        .unwrap();
    f.service
        .maybe_persist_workspace("user_1", &conv.id, old, "/old-runtime-resolution")
        .await
        .unwrap();
    assert_eq!(
        f.service.get("user_1", &conv.id).await.unwrap().extra["workspace"],
        moved.extra["workspace"]
    );
}

#[tokio::test]
async fn an_old_background_task_is_not_killed_to_start_a_turn_in_the_new_directory() {
    let f = Fixture::new().await;
    let conv = f.conversation().await;
    f.service.ensure_runtime("user_1", &conv.id, &f.manager).await.unwrap();
    f.backgrounds.store(1, Ordering::SeqCst);
    let moved = f
        .service
        .switch_workspace(
            "user_1",
            &conv.id,
            f.request(conv.extra["workspace"].as_str().unwrap()),
            &f.manager,
        )
        .await
        .unwrap();
    f.send(&conv.id, "next").await;
    f.wait_idle(&conv.id).await;
    assert_eq!(f.kills.load(Ordering::SeqCst), 0);
    assert!(
        !Path::new(moved.extra["workspace"].as_str().unwrap())
            .join("next.txt")
            .exists()
    );
    assert!(
        f.service
            .latest_conversation_error_message("user_1", &conv.id)
            .await
            .unwrap()
            .unwrap()
            .contains("WORKSPACE_SWITCH_BACKGROUND_ACTIVE")
    );
    f.backgrounds.store(0, Ordering::SeqCst);
    f.send(&conv.id, "next").await;
    f.wait_idle(&conv.id).await;
    assert!(
        Path::new(moved.extra["workspace"].as_str().unwrap())
            .join("next.txt")
            .exists()
    );
}
