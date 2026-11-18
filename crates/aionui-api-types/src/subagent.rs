use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentRun {
    pub child_run_id: String,
    pub conversation_id: String,
    pub turn_id: String,
    pub parent_call_id: String,
    pub task_index: i64,
    pub name: String,
    pub prompt: String,
    pub latest_activity: Option<String>,
    pub status: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub elapsed_ms: Option<i64>,
    pub result: Option<String>,
    pub seq: i64,
    pub incomplete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentEntry {
    pub child_run_id: String,
    pub entry_id: String,
    pub position: i64,
    pub kind: String,
    pub name: String,
    pub content: String,
    pub input: Option<String>,
    pub output: Option<String>,
    pub status: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub seq: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentUpdate {
    pub run: SubagentRun,
    pub entries: Vec<SubagentEntry>,
    /// False only for an observation/storage failure; seq remains last committed.
    pub durable: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct SubagentListQuery {
    pub after: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SubagentEntriesQuery {
    pub after: Option<i64>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SubagentListResponse {
    pub items: Vec<SubagentRun>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SubagentEntriesResponse {
    pub items: Vec<SubagentEntry>,
    pub next_cursor: Option<i64>,
}
