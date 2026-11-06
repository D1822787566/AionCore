use super::*;

#[tokio::test]
async fn snapshots_do_not_cross_read_same_named_skills_or_fall_back() {
    let temp = tempfile::tempdir().unwrap();
    let mut sources = Vec::new();
    for user in ["user-a", "user-b"] {
        let root = temp.path().join(user);
        tokio::fs::create_dir(&root).await.unwrap();
        tokio::fs::write(
            root.join("SKILL.md"),
            format!("---\nname: form\ndescription: form\n---\n{user}"),
        )
        .await
        .unwrap();
        sources.push(SessionSkillSource {
            enabled: BTreeSet::from(["form".into(), "missing".into()]),
            entries: BTreeMap::from([("form".into(), ("form".into(), root))]),
        });
    }
    assert_eq!(sources[0].read("form").await.unwrap().body.trim(), "user-a");
    assert_eq!(sources[1].read("form").await.unwrap().body.trim(), "user-b");
    assert!(matches!(
        sources[0].read("other").await,
        Err(HostSkillReadError::NotEnabled)
    ));
    assert!(matches!(
        sources[0].read("missing").await,
        Err(HostSkillReadError::Unavailable)
    ));
    let root = &sources[0].entries["form"].1;
    tokio::fs::remove_file(root.join("SKILL.md")).await.unwrap();
    assert!(matches!(
        sources[0].read("form").await,
        Err(HostSkillReadError::Unavailable)
    ));
}

#[tokio::test]
async fn host_resolution_uses_user_repository_and_selected_names() {
    use aionui_db::{ISkillRepository, SqliteSkillRepository, UpsertSkillParams, init_database_memory};
    use aionui_extension::resolve_skill_paths;
    let temp = tempfile::tempdir().unwrap();
    let db = init_database_memory().await.unwrap();
    sqlx::query("INSERT INTO users (id,user_type,external_user_id,username,email,password_hash,avatar_path,jwt_secret,status,session_generation,created_at,updated_at,last_login) VALUES ('host-user-b','aionpro','host-external-b','Host B',NULL,NULL,NULL,NULL,'active',0,1,1,NULL)")
        .execute(db.pool()).await.unwrap();
    let repo = Arc::new(SqliteSkillRepository::new(db.pool().clone()));
    for user in ["system_default_user", "host-user-b"] {
        let root = temp.path().join(user);
        tokio::fs::create_dir(&root).await.unwrap();
        tokio::fs::write(
            root.join("SKILL.md"),
            format!("---\nname: shared-probe-form\ndescription: shared form\n---\n{user}"),
        )
        .await
        .unwrap();
        repo.upsert_for_user(
            user,
            UpsertSkillParams {
                name: "shared-probe-form",
                description: Some("form"),
                path: &root.to_string_lossy(),
                source: "user",
                enabled: true,
            },
        )
        .await
        .unwrap();
    }
    let manager = AcpSkillManager::new_with_repo(
        Arc::new(resolve_skill_paths(temp.path(), &temp.path().join("data"))),
        repo,
    );
    for user in ["system_default_user", "host-user-b"] {
        let source = manager
            .host_skill_source_for_user(user, &["shared-probe-form".into()])
            .await;
        assert_eq!(source.available().len(), 1);
        assert_eq!(source.read("shared-probe-form").await.unwrap().body.trim(), user);
        assert!(matches!(
            source.read("unselected").await,
            Err(HostSkillReadError::NotEnabled)
        ));
    }
    let none = manager.host_skill_source_for_user("system_default_user", &[]).await;
    assert!(none.available().is_empty());
    assert!(matches!(
        none.read("shared-probe-form").await,
        Err(HostSkillReadError::NotEnabled)
    ));
}
