use super::*;
use aion_types::subagent::SubagentIdentity;
use aionui_db::{DbError, SqliteSubagentRepository, init_database_memory};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};

async fn setup(
    capacity: usize,
) -> (
    Arc<SubagentRecorder>,
    Arc<SqliteSubagentRepository>,
    broadcast::Receiver<AgentStreamEvent>,
    aionui_db::Database,
) {
    let db = init_database_memory().await.unwrap();
    sqlx::query("INSERT INTO conversations(id,user_id,name,type,extra,created_at,updated_at) VALUES('c','system_default_user','test','aionrs','{}',1,1)")
        .execute(db.pool()).await.unwrap();
    let repo = Arc::new(SqliteSubagentRepository::new(db.pool().clone()));
    let (tx, rx) = broadcast::channel(capacity);
    let recorder = SubagentRecorder::new(repo.clone(), "system_default_user".into(), "c".into(), tx);
    recorder.begin_turn("turn");
    (recorder, repo, rx, db)
}
fn event(change: SubagentChange) -> SubagentEvent {
    SubagentEvent {
        run: SubagentIdentity {
            child_run_id: "child".into(),
            parent_call_id: "spawn".into(),
            task_index: 0,
            name: "same".into(),
            prompt: "task".into(),
            started_at: 1,
        },
        occurred_at: 2,
        change,
    }
}
fn text(value: &str) -> SubagentEvent {
    event(SubagentChange::Text {
        entry_id: "text-1".into(),
        text: value.into(),
        thinking: false,
    })
}

#[tokio::test]
async fn history_records_without_any_ui_subscriber_and_commits_before_broadcast() {
    let (recorder, repo, rx, _db) = setup(1).await;
    drop(rx);
    recorder.emit(event(SubagentChange::Started));
    recorder.emit(text("first"));
    recorder.emit(text(" second"));
    recorder.emit(event(SubagentChange::MessageEnd { msg_id: "1".into() }));
    recorder.emit(event(SubagentChange::ToolCall {
        tool_call_id: "t".into(),
        name: "Read".into(),
        input: "input".into(),
    }));
    recorder.emit(event(SubagentChange::ToolResult {
        tool_call_id: "t".into(),
        name: "Read".into(),
        output: "output".into(),
        is_error: false,
    }));
    recorder.emit(event(SubagentChange::Finished {
        status: SubagentStatus::Completed,
        text: "result".into(),
        elapsed_ms: 5,
    }));
    recorder.flush().await;
    let runs = repo.list("system_default_user", "c", None, 100).await.unwrap();
    assert_eq!(
        (
            runs[0].parent_call_id.as_str(),
            runs[0].turn_id.as_str(),
            runs[0].status.as_str()
        ),
        ("aionrs-spawn", "turn", "completed")
    );
    let entries = repo.entries("system_default_user", "c", "child", 0, 100).await.unwrap();
    assert_eq!(entries[0].content, "first second");
    assert_eq!(entries[1].output.as_deref(), Some("output"));
}

#[tokio::test]
async fn lagged_broadcast_does_not_lose_authoritative_text() {
    let (recorder, repo, mut rx, _db) = setup(1).await;
    recorder.emit(event(SubagentChange::Started));
    for _ in 0..5 {
        recorder.emit(text("x"));
        tokio::time::sleep(Duration::from_millis(90)).await;
    }
    recorder.emit(event(SubagentChange::Finished {
        status: SubagentStatus::Completed,
        text: "done".into(),
        elapsed_ms: 9,
    }));
    recorder.flush().await;
    assert!(matches!(rx.recv().await, Err(broadcast::error::RecvError::Lagged(_))));
    let AgentStreamEvent::Subagent(update) = rx.recv().await.unwrap() else {
        panic!("subagent event")
    };
    let runs = repo.list("system_default_user", "c", None, 100).await.unwrap();
    assert_eq!(runs[0].seq, update.run.seq);
    assert_eq!(
        repo.entries("system_default_user", "c", "child", 0, 100).await.unwrap()[0].content,
        "xxxxx"
    );
}

#[tokio::test]
async fn overflow_preserves_terminal_and_marks_record_incomplete() {
    let (recorder, repo, _rx, _db) = setup(1).await;
    recorder.emit(event(SubagentChange::Started));
    for _ in 0..2000 {
        recorder.emit(text("x"));
    }
    recorder.emit(event(SubagentChange::Finished {
        status: SubagentStatus::Cancelled,
        text: String::new(),
        elapsed_ms: 8,
    }));
    recorder.flush().await;
    let run = repo
        .list("system_default_user", "c", None, 100)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(run.status, "cancelled");
    assert!(run.incomplete);
}

struct FailingRepository {
    inner: Arc<SqliteSubagentRepository>,
    fail: AtomicBool,
}
#[async_trait]
impl ISubagentRepository for FailingRepository {
    async fn apply(&self, user: &str, run: &SubagentRunRow, entries: &[SubagentEntryRow]) -> Result<(), DbError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(DbError::Init("injected failure".into()));
        }
        self.inner.apply(user, run, entries).await
    }
    async fn list(
        &self,
        user: &str,
        conv: &str,
        after: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SubagentRunRow>, DbError> {
        self.inner.list(user, conv, after, limit).await
    }
    async fn entries(
        &self,
        user: &str,
        conv: &str,
        child: &str,
        after: i64,
        limit: i64,
    ) -> Result<Vec<SubagentEntryRow>, DbError> {
        self.inner.entries(user, conv, child, after, limit).await
    }
    async fn recover_interrupted(&self) -> Result<u64, DbError> {
        self.inner.recover_interrupted().await
    }
}

#[tokio::test]
async fn storage_failure_is_visible_and_retry_keeps_continuous_committed_versions() {
    let (_unused, repo, _, _db) = setup(1).await;
    let failing = Arc::new(FailingRepository {
        inner: repo.clone(),
        fail: AtomicBool::new(true),
    });
    let (tx, mut rx) = broadcast::channel(8);
    let recorder = SubagentRecorder::new(failing.clone(), "system_default_user".into(), "c".into(), tx);
    recorder.begin_turn("turn");
    recorder.emit(text("preserved"));
    let AgentStreamEvent::Subagent(failed) = rx.recv().await.unwrap() else {
        panic!("subagent event")
    };
    assert!(!failed.durable && failed.run.seq == 0 && failed.run.incomplete);
    failing.fail.store(false, Ordering::SeqCst);
    recorder.emit(event(SubagentChange::Finished {
        status: SubagentStatus::Failed,
        text: "reason".into(),
        elapsed_ms: 8,
    }));
    recorder.flush().await;
    let run = repo
        .list("system_default_user", "c", None, 100)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(run.seq, 1);
    assert!(run.incomplete && run.status == "failed");
}
