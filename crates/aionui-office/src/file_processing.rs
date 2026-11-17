//! Local file lifecycle shared by the four fixed helper operations.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zip::ZipArchive;

use super::file_processing_runtime::{close_officecli, failure, hssf, verify_officecli};
use super::file_processing_types::{
    FileProcessingContext, FileProcessingError, FinishInput, InspectInput, PrepareInput, ProcessingRecord, WriteInput,
};

const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const PREFIX: &str = ".aion-file-";

pub async fn execute_file_operation(
    context: &FileProcessingContext,
    operation: &str,
    input: Value,
) -> Result<Value, FileProcessingError> {
    if context.user_id.is_empty() || context.conversation_id.is_empty() || !context.task_directory.is_absolute() {
        return Err(failure(
            "file_context_missing",
            "host identity and absolute frozen task directory are required",
        ));
    }
    match operation {
        "file.prepare" => prepare(context, parse(input)?).await,
        "file.inspect" => inspect(context, parse(input)?).await,
        "file.xls.write" => write_xls(context, parse(input)?).await,
        "file.finish" => finish(context, parse(input)?).await,
        _ => Err(failure("file_operation_unknown", "unknown file operation")),
    }
}

fn parse<T: DeserializeOwned>(input: Value) -> Result<T, FileProcessingError> {
    serde_json::from_value(input).map_err(|error| failure("file_input_invalid", error.to_string()))
}

fn io(error: std::io::Error) -> FileProcessingError {
    failure("file_io_failed", error.to_string())
}

fn root(context: &FileProcessingContext) -> Result<PathBuf, FileProcessingError> {
    let root = context.task_directory.canonicalize().map_err(io)?;
    if !root.is_dir() {
        return Err(failure("file_context_invalid", "task directory is not a directory"));
    }
    Ok(root)
}

fn no_link(path: &Path) -> Result<(), FileProcessingError> {
    let metadata = fs::symlink_metadata(path).map_err(io)?;
    let linked = metadata.file_type().is_symlink();
    #[cfg(windows)]
    let linked = {
        use std::os::windows::fs::MetadataExt;
        linked || metadata.file_attributes() & 0x400 != 0
    };
    if linked {
        return Err(failure(
            "file_path_invalid",
            "processing paths must not be symbolic links or reparse points",
        ));
    }
    Ok(())
}

fn regular_file(path: &Path) -> Result<(), FileProcessingError> {
    no_link(path)?;
    let metadata = path.metadata().map_err(io)?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err(failure(
            "file_size_invalid",
            "input must be a regular file of at most 256 MiB",
        ));
    }
    Ok(())
}

fn hash(path: &Path) -> Result<String, FileProcessingError> {
    regular_file(path)?;
    let mut file = File::open(path).map_err(io)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer).map_err(io)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn copy_new(source: &Path, target: &Path) -> Result<(), FileProcessingError> {
    let mut source = File::open(source).map_err(io)?;
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(io)?;
    std::io::copy(&mut Read::by_ref(&mut source).take(MAX_FILE_BYTES + 1), &mut target).map_err(io)?;
    if target.metadata().map_err(io)?.len() > MAX_FILE_BYTES {
        return Err(failure("file_size_invalid", "input grew beyond 256 MiB"));
    }
    target.sync_all().map_err(io)
}

fn format(path: &Path) -> Result<&'static str, FileProcessingError> {
    let mut file = File::open(path).map_err(io)?;
    let mut header = [0u8; 8];
    let count = file.read(&mut header).map_err(io)?;
    if count == 8 && header == [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1] {
        return Ok("xls");
    }
    let mut archive = ZipArchive::new(File::open(path).map_err(io)?).map_err(|_| {
        failure(
            "file_format_unsupported",
            "file is not readable OLE2 or Office Open XML; verify the local preparation trigger",
        )
    })?;
    for (entry, name) in [
        ("xl/workbook.xml", "xlsx"),
        ("word/document.xml", "docx"),
        ("ppt/presentation.xml", "pptx"),
    ] {
        if archive.by_name(entry).is_ok() {
            return Ok(name);
        }
    }
    Err(failure("file_format_unsupported", "unsupported Office format"))
}

fn working(dir: &Path, record: &ProcessingRecord) -> PathBuf {
    dir.join(format!("working.{}", record.format))
}

fn save_record(dir: &Path, record: &ProcessingRecord) -> Result<(), FileProcessingError> {
    let next = dir.join("record.next");
    if next.exists() {
        no_link(&next)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&next)
        .map_err(io)?;
    file.write_all(&serde_json::to_vec_pretty(record).map_err(|e| failure("file_record_invalid", e.to_string()))?)
        .map_err(io)?;
    file.sync_all().map_err(io)?;
    drop(file);
    fs::rename(next, dir.join("record.json")).map_err(io)
}

fn load(context: &FileProcessingContext, id: &str) -> Result<(PathBuf, ProcessingRecord, File), FileProcessingError> {
    let suffix = id
        .strip_prefix(PREFIX)
        .ok_or_else(|| failure("file_processing_id_invalid", "invalid processing id"))?;
    Uuid::parse_str(suffix).map_err(|_| failure("file_processing_id_invalid", "invalid processing id"))?;
    if id.contains(['/', '\\']) {
        return Err(failure(
            "file_processing_id_invalid",
            "processing id must be one directory name",
        ));
    }
    let task_root = root(context)?;
    let dir = task_root.join(id);
    no_link(&dir)?;
    let lock_path = dir.join("operation.lock");
    if lock_path.exists() {
        no_link(&lock_path)?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(io)?;
    lock.try_lock().map_err(|_| {
        failure(
            "file_processing_busy",
            "another operation is active for this working file",
        )
    })?;
    let record_path = dir.join("record.json");
    regular_file(&record_path)?;
    let record: ProcessingRecord = serde_json::from_slice(&fs::read(record_path).map_err(io)?)
        .map_err(|_| failure("file_record_invalid", "processing record is invalid"))?;
    if record.version != 1
        || record.user_id != context.user_id
        || record.conversation_id != context.conversation_id
        || record.task_directory != task_root
        || !["xls", "xlsx", "docx", "pptx"].contains(&record.format.as_str())
    {
        return Err(failure(
            "file_context_mismatch",
            "processing record does not belong to this conversation and task directory",
        ));
    }
    if let Some(final_path) = &record.final_path
        && (final_path.parent() != Some(task_root.as_path()) || final_path == &record.source_path)
    {
        return Err(failure(
            "file_record_invalid",
            "final path is outside this task's delivery directory",
        ));
    }
    Ok((dir, record, lock))
}

async fn verify(
    context: &FileProcessingContext,
    path: &Path,
    record: &ProcessingRecord,
    checks: &[super::file_processing_types::OfficeCheck],
) -> Result<Value, FileProcessingError> {
    if record.format == "xls" {
        hssf(
            context.hssf_home.as_deref(),
            json!({"action":"verify","path":path,"changes":record.changes}),
        )
        .await
    } else {
        verify_officecli(path, checks).await
    }
}

async fn prepare(context: &FileProcessingContext, input: PrepareInput) -> Result<Value, FileProcessingError> {
    if !input.source_path.is_absolute() {
        return Err(failure("file_source_invalid", "source_path must be absolute"));
    }
    let source = input.source_path.canonicalize().map_err(io)?;
    regular_file(&source)?;
    let extension = source
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !["xls", "xlsx", "docx", "pptx"].contains(&extension.as_str()) {
        return Err(failure(
            "file_format_unsupported",
            "this edit workflow supports XLS, XLSX, DOCX and PPTX; existing readers remain separate",
        ));
    }
    let task_directory = root(context)?;
    let id = format!("{PREFIX}{}", Uuid::now_v7());
    let dir = task_directory.join(&id);
    fs::create_dir(&dir).map_err(io)?;
    let path = dir.join(format!("working.{extension}"));
    let mut record = ProcessingRecord {
        version: 1,
        user_id: context.user_id.clone(),
        conversation_id: context.conversation_id.clone(),
        task_directory,
        source_path: source.clone(),
        source_sha256: hash(&source)?,
        working_sha256: String::new(),
        format: extension.clone(),
        recalculation: "not_run".into(),
        changes: Vec::new(),
        final_path: None,
        final_sha256: None,
        accepted_uncalculated: false,
    };
    // This ports the existing MCP's local read/write step. The intranet trigger is verified separately.
    copy_new(&source, &path)?;
    record.working_sha256 = hash(&path)?;
    save_record(&dir, &record)?;
    let result = async {
        let actual = format(&path)?;
        if actual != extension {
            return Err(failure(
                "file_format_mismatch",
                format!("extension {extension} does not match the detected {actual} container"),
            ));
        }
        verify(context, &path, &record, &[]).await
    }
    .await;
    let metadata = result.map_err(|mut error| {
        error.execution = "succeeded";
        error.data = Box::new(
            json!({"processing_id":id,"working_path":path,"working_sha256":record.working_sha256,
            "prepare":"saved","readback":"failed","cleanup":"not_run","intranet_trigger":"pending_verification"}),
        );
        error
    })?;
    tracing::info!(processing_id = %id, format = %extension, "File working copy prepared");
    Ok(
        json!({"processing_id":id,"working_path":path,"working_sha256":record.working_sha256,
        "source_sha256":record.source_sha256,"format":extension,"engine":if extension=="xls" {"hssf"} else {"officecli"},
        "prepare":"passed","readback":"passed","cleanup":"not_run","metadata":metadata,
        "preparation_method":"local_read_write","intranet_trigger":"pending_verification"}),
    )
}

async fn inspect(context: &FileProcessingContext, input: InspectInput) -> Result<Value, FileProcessingError> {
    if input.sheet.is_some() != input.range.is_some() {
        return Err(failure(
            "file_input_invalid",
            "sheet and range must be supplied together",
        ));
    }
    let (dir, record, _lock) = load(context, &input.processing_id)?;
    let path = working(&dir, &record);
    let actual_hash = if path.exists() { Some(hash(&path)?) } else { None };
    let mut data = json!({"processing_id":input.processing_id,"working_path":path,"working_sha256":actual_hash,
        "recorded_sha256":record.working_sha256,"format":record.format,"recalculation":record.recalculation,
        "final_path":record.final_path,"final_sha256":record.final_sha256,
        "requires_review":actual_hash.as_deref()!=Some(record.working_sha256.as_str()),"cleanup":"not_run"});
    if record.format == "xls" && path.exists() {
        let mut request = json!({"action":"inspect","path":path});
        if let (Some(sheet), Some(range)) = (input.sheet, input.range) {
            request["sheet"] = json!(sheet);
            request["range"] = json!(range);
        }
        data["metadata"] = hssf(context.hssf_home.as_deref(), request).await?;
    } else if input.sheet.is_some() {
        return Err(failure(
            "file_engine_mismatch",
            "read modern formats with OfficeCLI through ExecCommand",
        ));
    }
    Ok(data)
}

fn expected_hash(path: &Path, expected: &str) -> Result<String, FileProcessingError> {
    let actual = hash(path)?;
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(failure(
            "file_hash_conflict",
            "working file changed; inspect before writing or finishing",
        ));
    }
    Ok(actual)
}

async fn write_xls(context: &FileProcessingContext, input: WriteInput) -> Result<Value, FileProcessingError> {
    let (dir, mut record, _lock) = load(context, &input.processing_id)?;
    if record.format != "xls" || record.final_path.is_some() {
        return Err(failure(
            "file_state_invalid",
            "XLS write requires an active XLS working file",
        ));
    }
    if input.changes.is_empty() || input.changes.len() > 10000 {
        return Err(failure("file_input_invalid", "changes must contain 1..10000 cells"));
    }
    let path = working(&dir, &record);
    let current = expected_hash(&path, &input.expected_sha256)?;
    if current != record.working_sha256 {
        return Err(failure(
            "file_state_unknown",
            "unrecorded external modification requires review; do not overwrite it",
        ));
    }
    let candidate = dir.join("candidate.xls");
    if candidate.exists() {
        return Err(failure(
            "file_candidate_exists",
            "a previous candidate remains; inspect before continuing",
        ));
    }
    let data = hssf(
        context.hssf_home.as_deref(),
        json!({"action":"write","path":path,"output":candidate,"changes":input.changes}),
    )
    .await?;
    expected_hash(&path, &current)?;
    let candidate_hash = hash(&candidate)?;
    fs::rename(&candidate, &path).map_err(io)?;
    record.working_sha256 = candidate_hash;
    record.recalculation = data["recalculation"].as_str().unwrap_or("unknown").into();
    // The latest requested value wins when a later batch edits the same address.
    for change in input.changes {
        record.changes.retain(|old| {
            !(old["sheet"] == change["sheet"]
                && old["cell"].as_str().map(str::to_ascii_uppercase)
                    == change["cell"].as_str().map(str::to_ascii_uppercase))
        });
        record.changes.push(change);
    }
    save_record(&dir, &record).map_err(|mut error| {
        error.execution = "succeeded";
        error.data = Box::new(
            json!({"processing_id":input.processing_id,"working_path":path,"working_sha256":record.working_sha256,
            "save":"passed","readback":"passed","record":"failed","cleanup":"not_run"}),
        );
        error
    })?;
    tracing::info!(processing_id = %input.processing_id, recalculation = %record.recalculation, "XLS working file saved and reopened");
    Ok(
        json!({"processing_id":input.processing_id,"working_path":path,"working_sha256":record.working_sha256,
        "save":data["save"],"readback":data["readback"],"recalculation":record.recalculation,
        "diagnostics":data["diagnostics"],"changed_cells":data["changed_cells"],"cleanup":"not_run",
        "delivery":if record.recalculation=="passed" {"ready"} else {"awaiting_user_choice"}}),
    )
}

fn output_path(root: &Path, name: &str, format: &str) -> Result<PathBuf, FileProcessingError> {
    let path = Path::new(name);
    if path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
        || name.contains(['/', '\\', ':'])
        || name.ends_with([' ', '.'])
        || path
            .extension()
            .and_then(|v| v.to_str())
            .is_none_or(|v| !v.eq_ignore_ascii_case(format))
    {
        return Err(failure(
            "file_output_name_invalid",
            "output_name must be a filename with the original format extension",
        ));
    }
    let stem = path.file_stem().and_then(|v| v.to_str()).unwrap_or("result");
    for index in 0..1000 {
        let candidate = root.join(if index == 0 {
            name.to_owned()
        } else {
            format!("{stem} ({index}).{format}")
        });
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(failure("file_output_name_conflict", "no unused output filename"))
}

async fn finish(context: &FileProcessingContext, input: FinishInput) -> Result<Value, FileProcessingError> {
    let (dir, mut record, _lock) = load(context, &input.processing_id)?;
    let path = working(&dir, &record);
    if input.checks.len() > 100
        || input
            .checks
            .iter()
            .any(|c| !c.selector.starts_with('/') || c.expected.as_object().is_none_or(|v| v.is_empty()))
    {
        return Err(failure(
            "file_checks_invalid",
            "checks must contain at most 100 DOM selectors with nonempty expected objects",
        ));
    }
    let final_path = if let Some(final_path) = &record.final_path {
        expected_hash(final_path, record.final_sha256.as_deref().unwrap_or(""))?;
        final_path.clone()
    } else {
        if record.format != "xls" {
            if input.checks.is_empty() {
                return Err(failure(
                    "file_checks_required",
                    "modern-format delivery requires expected JSON checks from OfficeCLI get",
                ));
            }
            close_officecli(&path).await?;
        }
        let current = expected_hash(&path, &input.expected_sha256)?;
        if record.format == "xls" && current != record.working_sha256 {
            return Err(failure(
                "file_state_unknown",
                "XLS working file has an unrecorded modification",
            ));
        }
        if record.format == "xls"
            && record.recalculation != "passed"
            && record.recalculation != "not_run"
            && !input.accept_uncalculated
        {
            return Err(failure(
                "file_recalculation_pending",
                "formula recalculation is incomplete; retain the working file until the user chooses",
            ));
        }
        verify(context, &path, &record, &input.checks).await?;
        let final_path = output_path(&record.task_directory, &input.output_name, &record.format)?;
        copy_new(&path, &final_path)?;
        let verification = verify(context, &final_path, &record, &input.checks).await;
        if let Err(mut error) = verification {
            error.execution = "succeeded";
            error.data = Box::new(json!({"processing_id":input.processing_id,"working_path":path,
                "candidate_final_path":final_path,"delivery":"failed","readback":"failed","cleanup":"not_run"}));
            return Err(error);
        }
        let final_hash = hash(&final_path)?;
        if final_hash != current {
            return Err(failure(
                "file_hash_conflict",
                "delivery copy differs from the verified working file",
            ));
        }
        record.final_path = Some(final_path.clone());
        record.final_sha256 = Some(final_hash);
        record.accepted_uncalculated =
            input.accept_uncalculated && record.recalculation != "passed" && record.recalculation != "not_run";
        save_record(&dir, &record).map_err(|mut error| {
            error.execution = "succeeded";
            error.data = Box::new(
                json!({"processing_id":input.processing_id,"final_path":final_path,"readback":"passed",
                "delivery":"passed","record":"failed","cleanup":"not_run"}),
            );
            error
        })?;
        final_path
    };
    verify(context, &final_path, &record, &input.checks).await?;
    let cleanup = cleanup(&dir, &record);
    tracing::info!(processing_id = %input.processing_id, cleanup_passed = cleanup.is_ok(), "File final delivery verified");
    Ok(
        json!({"processing_id":input.processing_id,"final_path":final_path,"final_sha256":record.final_sha256,
        "save":"passed","readback":"passed","delivery":"passed","recalculation":record.recalculation,
        "accepted_uncalculated":record.accepted_uncalculated,"cleanup":if cleanup.is_ok() {"passed"} else {"failed"},
        "cleanup_diagnostic":cleanup.err().map(|e|e.message)}),
    )
}

fn cleanup(dir: &Path, record: &ProcessingRecord) -> Result<(), FileProcessingError> {
    no_link(dir)?;
    let work = format!("working.{}", record.format);
    let names = [
        work.as_str(),
        "candidate.xls",
        "record.next",
        "record.json",
        "operation.lock",
    ];
    // Unknown entries are retained rather than broadening cleanup to the whole tree.
    for entry in fs::read_dir(dir).map_err(io)? {
        let entry = entry.map_err(io)?;
        if !names.iter().any(|n| entry.file_name() == *n) {
            return Err(failure(
                "file_cleanup_incomplete",
                "unexpected temporary entry retained",
            ));
        }
        no_link(&entry.path())?;
        if !entry.file_type().map_err(io)?.is_file() {
            return Err(failure("file_cleanup_incomplete", "unexpected directory retained"));
        }
    }
    for name in [
        work.as_str(),
        "candidate.xls",
        "record.next",
        "operation.lock",
        "record.json",
    ] {
        let path = dir.join(name);
        if path.exists() {
            fs::remove_file(path).map_err(io)?;
        }
    }
    fs::remove_dir(dir).map_err(io)
}

#[cfg(test)]
#[path = "file_processing_test.rs"]
mod tests;
