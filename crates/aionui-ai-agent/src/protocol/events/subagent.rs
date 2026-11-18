use aionui_api_types::{SubagentEntry, SubagentRun};
use aionui_db::models::{SubagentEntryRow, SubagentRunRow};

/// One mapping shared by live observations and authorized history queries.
pub fn run_response(row: SubagentRunRow) -> SubagentRun {
    SubagentRun {
        child_run_id: row.child_run_id,
        conversation_id: row.conversation_id,
        turn_id: row.turn_id,
        parent_call_id: row.parent_call_id,
        task_index: row.task_index,
        name: row.name,
        prompt: row.prompt,
        latest_activity: row.latest_activity,
        status: row.status,
        started_at: row.started_at,
        ended_at: row.ended_at,
        elapsed_ms: row.elapsed_ms,
        result: row.result,
        seq: row.seq,
        incomplete: row.incomplete,
    }
}

pub fn entry_response(row: SubagentEntryRow) -> SubagentEntry {
    SubagentEntry {
        child_run_id: row.child_run_id,
        entry_id: row.entry_id,
        position: row.position,
        kind: row.kind,
        name: row.name,
        content: row.content,
        input: row.input,
        output: row.output,
        status: row.status,
        started_at: row.started_at,
        ended_at: row.ended_at,
        seq: row.seq,
    }
}
