use crate::error::DbError;
use crate::models::{SubagentEntryRow, SubagentRunRow};
use async_trait::async_trait;

#[async_trait]
pub trait ISubagentRepository: Send + Sync {
    /// Commit the next run version and only its changed entries atomically.
    async fn apply(&self, user_id: &str, run: &SubagentRunRow, entries: &[SubagentEntryRow]) -> Result<(), DbError>;
    async fn list(
        &self,
        user_id: &str,
        conversation_id: &str,
        after: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SubagentRunRow>, DbError>;
    async fn entries(
        &self,
        user_id: &str,
        conversation_id: &str,
        child_run_id: &str,
        after: i64,
        limit: i64,
    ) -> Result<Vec<SubagentEntryRow>, DbError>;
    /// Called once during composition, before any new runtime starts.
    async fn recover_interrupted(&self) -> Result<u64, DbError>;
}
