use super::*;
use crate::init_database_memory;

async fn setup() -> (SqliteSubagentRepository, crate::Database) {
    let db = init_database_memory().await.unwrap();
    sqlx::query("INSERT INTO conversations(id,user_id,name,type,extra,created_at,updated_at) VALUES('c','system_default_user','test','aionrs','{}',1,1)")
        .execute(db.pool()).await.unwrap();
    (SqliteSubagentRepository::new(db.pool().clone()), db)
}

fn run(id: &str) -> SubagentRunRow {
    SubagentRunRow {
        child_run_id: id.into(),
        conversation_id: "c".into(),
        turn_id: "t".into(),
        parent_call_id: "aionrs-spawn".into(),
        task_index: 0,
        name: "same".into(),
        prompt: "task".into(),
        latest_activity: None,
        status: "running".into(),
        started_at: 1,
        ended_at: None,
        elapsed_ms: None,
        result: None,
        seq: 1,
        incomplete: false,
    }
}

#[tokio::test]
async fn versions_are_idempotent_and_terminal_cannot_regress() {
    let (repo, _) = setup().await;
    let mut r = run("a");
    repo.apply("system_default_user", &r, &[]).await.unwrap();
    r.status = "failed".into();
    r.seq = 2;
    r.result = Some("failed".into());
    repo.apply("system_default_user", &r, &[]).await.unwrap();
    repo.apply("system_default_user", &r, &[]).await.unwrap();
    r.status = "running".into();
    r.seq = 3;
    assert!(matches!(
        repo.apply("system_default_user", &r, &[]).await,
        Err(DbError::Conflict(_))
    ));
    assert_eq!(
        repo.list("system_default_user", "c", None, 100).await.unwrap()[0].status,
        "failed"
    );
}

#[tokio::test]
async fn ownership_and_child_identity_are_enforced() {
    let (repo, _) = setup().await;
    let r = run("a");
    assert!(matches!(repo.apply("other", &r, &[]).await, Err(DbError::NotFound(_))));
    repo.apply("system_default_user", &r, &[]).await.unwrap();
    assert!(repo.list("other", "c", None, 100).await.unwrap().is_empty());
    assert!(matches!(
        repo.entries("other", "c", "a", 0, 100).await,
        Err(DbError::NotFound(_))
    ));
}

#[tokio::test]
async fn entries_recover_paginate_and_delete_with_conversation() {
    let (repo, db) = setup().await;
    let r = run("a");
    let entries: Vec<_> = (1..=3)
        .map(|position| SubagentEntryRow {
            child_run_id: "a".into(),
            entry_id: position.to_string(),
            position,
            kind: "tool".into(),
            name: "Read".into(),
            content: String::new(),
            input: Some("input".into()),
            output: Some("output".into()),
            status: "completed".into(),
            started_at: 1,
            ended_at: Some(2),
            seq: 1,
        })
        .collect();
    repo.apply("system_default_user", &r, &entries).await.unwrap();
    assert_eq!(
        repo.entries("system_default_user", "c", "a", 1, 1).await.unwrap()[0].entry_id,
        "2"
    );
    assert_eq!(repo.recover_interrupted().await.unwrap(), 1);
    assert_eq!(
        repo.list("system_default_user", "c", None, 100).await.unwrap()[0].status,
        "unknown"
    );
    sqlx::query("DELETE FROM conversations WHERE id='c'")
        .execute(db.pool())
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM subagent_entries")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn older_migrator_rejects_new_schema_without_destroying_records() {
    let (repo, db) = setup().await;
    repo.apply("system_default_user", &run("a"), &[]).await.unwrap();
    let mut old = sqlx::migrate!();
    old.migrations = std::borrow::Cow::Owned(old.iter().filter(|migration| migration.version < 44).cloned().collect());
    let result = old.run(db.pool()).await;
    assert!(matches!(result, Err(sqlx::migrate::MigrateError::VersionMissing(44))));
    assert_eq!(repo.list("system_default_user", "c", None, 100).await.unwrap().len(), 1);
}
