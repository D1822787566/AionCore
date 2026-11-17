use super::*;
use tempfile::TempDir;

fn fixture() -> (TempDir, FileProcessingContext, String, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let context = FileProcessingContext {
        user_id: "user-a".into(),
        conversation_id: "conversation-a".into(),
        task_directory: temp.path().to_path_buf(),
        hssf_home: None,
    };
    let id = format!("{PREFIX}{}", Uuid::now_v7());
    let dir = temp.path().join(&id);
    fs::create_dir(&dir).unwrap();
    let source = temp.path().join("original.xls");
    fs::write(&source, b"original").unwrap();
    let record = ProcessingRecord {
        version: 1,
        user_id: context.user_id.clone(),
        conversation_id: context.conversation_id.clone(),
        task_directory: root(&context).unwrap(),
        source_path: source.clone(),
        source_sha256: hash(&source).unwrap(),
        working_sha256: hash(&source).unwrap(),
        format: "xls".into(),
        recalculation: "not_run".into(),
        changes: vec![],
        final_path: None,
        final_sha256: None,
        accepted_uncalculated: false,
    };
    fs::write(dir.join("working.xls"), b"original").unwrap();
    save_record(&dir, &record).unwrap();
    (temp, context, id, dir)
}

#[test]
fn processing_record_rejects_another_user_and_conversation() {
    let (_temp, mut context, id, _dir) = fixture();
    context.user_id = "user-b".into();
    assert_eq!(load(&context, &id).unwrap_err().code, "file_context_mismatch");
    context.user_id = "user-a".into();
    context.conversation_id = "conversation-b".into();
    assert_eq!(load(&context, &id).unwrap_err().code, "file_context_mismatch");
}

#[test]
fn processing_id_cannot_escape_frozen_task_directory() {
    let (_temp, context, _id, _dir) = fixture();
    for value in ["../another", "C:\\another", ".aion-file-../x", ".aion-file-not-a-uuid"] {
        assert_eq!(load(&context, value).unwrap_err().code, "file_processing_id_invalid");
    }
}

#[test]
fn operations_on_one_record_are_serialized_by_an_os_lock() {
    let (_temp, context, id, _dir) = fixture();
    let (_dir, _record, lock) = load(&context, &id).unwrap();
    assert_eq!(load(&context, &id).unwrap_err().code, "file_processing_busy");
    drop(lock);
    assert!(load(&context, &id).is_ok());
}

#[test]
fn output_names_do_not_overwrite_original_or_other_files() {
    let (temp, _context, _id, _dir) = fixture();
    let output = output_path(temp.path(), "original.xls", "xls").unwrap();
    assert_eq!(output.file_name().unwrap(), "original (1).xls");
    for name in [
        "../result.xls",
        "a\\result.xls",
        "result.xlsx",
        "a:result.xls",
        "result.xls.",
    ] {
        assert_eq!(
            output_path(temp.path(), name, "xls").unwrap_err().code,
            "file_output_name_invalid"
        );
    }
}

#[test]
fn cleanup_preserves_unknown_entries_and_can_be_retried() {
    let (temp, context, id, dir) = fixture();
    let (_, record, lock) = load(&context, &id).unwrap();
    fs::write(dir.join("unrecorded.txt"), b"retain").unwrap();
    assert_eq!(cleanup(&dir, &record).unwrap_err().code, "file_cleanup_incomplete");
    assert!(dir.join("working.xls").exists());
    fs::remove_file(dir.join("unrecorded.txt")).unwrap();
    cleanup(&dir, &record).unwrap();
    drop(lock);
    assert!(!dir.exists());
    assert_eq!(fs::read(temp.path().join("original.xls")).unwrap(), b"original");
}

#[test]
fn changed_file_hash_is_not_silently_accepted() {
    let (_temp, _context, _id, dir) = fixture();
    let path = dir.join("working.xls");
    let baseline = hash(&path).unwrap();
    fs::write(&path, b"external edit").unwrap();
    assert_eq!(expected_hash(&path, &baseline).unwrap_err().code, "file_hash_conflict");
}

#[tokio::test]
async fn untrusted_context_fields_are_rejected_before_file_creation() {
    let (_temp, context, _id, _dir) = fixture();
    let error = execute_file_operation(
        &context,
        "file.prepare",
        json!({"source_path":"example.xls","user_id":"other-user"}),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "file_input_invalid");
    assert_eq!(error.execution, "not_executed");
}
