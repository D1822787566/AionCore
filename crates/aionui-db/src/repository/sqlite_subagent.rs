use async_trait::async_trait;
use sqlx::SqlitePool;

use super::subagent::ISubagentRepository;
use crate::error::DbError;
use crate::models::{SubagentEntryRow, SubagentRunRow};

pub struct SqliteSubagentRepository {
    pool: SqlitePool,
}

impl SqliteSubagentRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ISubagentRepository for SqliteSubagentRepository {
    async fn apply(&self, user_id: &str, run: &SubagentRunRow, entries: &[SubagentEntryRow]) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        let owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversations WHERE id=? AND user_id=?)")
            .bind(&run.conversation_id)
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
        if !owned {
            return Err(DbError::NotFound("Conversation not found".into()));
        }
        let prior: Option<SubagentRunRow> = sqlx::query_as("SELECT * FROM subagent_runs WHERE child_run_id=?")
            .bind(&run.child_run_id)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(prior) = prior {
            if prior.conversation_id != run.conversation_id
                || prior.parent_call_id != run.parent_call_id
                || prior.turn_id != run.turn_id
            {
                return Err(DbError::Conflict("Subagent identity mismatch".into()));
            }
            if run.seq <= prior.seq {
                return Ok(());
            }
            if run.seq != prior.seq + 1 || (prior.status != "running" && prior.status != run.status) {
                return Err(DbError::Conflict(
                    "Invalid subagent version or terminal transition".into(),
                ));
            }
        } else if run.seq != 1 {
            return Err(DbError::Conflict("Initial subagent version must be 1".into()));
        }
        sqlx::query("INSERT INTO subagent_runs(child_run_id,conversation_id,turn_id,parent_call_id,task_index,name,prompt,latest_activity,status,started_at,ended_at,elapsed_ms,result,seq,incomplete) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(child_run_id) DO UPDATE SET latest_activity=excluded.latest_activity,status=excluded.status,ended_at=excluded.ended_at,elapsed_ms=excluded.elapsed_ms,result=excluded.result,seq=excluded.seq,incomplete=excluded.incomplete")
            .bind(&run.child_run_id).bind(&run.conversation_id).bind(&run.turn_id).bind(&run.parent_call_id)
            .bind(run.task_index).bind(&run.name).bind(&run.prompt).bind(&run.latest_activity).bind(&run.status).bind(run.started_at)
            .bind(run.ended_at).bind(run.elapsed_ms).bind(&run.result).bind(run.seq).bind(run.incomplete)
            .execute(&mut *tx).await?;
        for entry in entries {
            if entry.child_run_id != run.child_run_id {
                return Err(DbError::Conflict("Subagent entry identity mismatch".into()));
            }
            sqlx::query("INSERT INTO subagent_entries(child_run_id,entry_id,position,kind,name,content,input,output,status,started_at,ended_at,seq) VALUES(?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(child_run_id,entry_id) DO UPDATE SET content=excluded.content,input=COALESCE(excluded.input,subagent_entries.input),output=COALESCE(excluded.output,subagent_entries.output),status=excluded.status,ended_at=excluded.ended_at,seq=excluded.seq")
                .bind(&entry.child_run_id).bind(&entry.entry_id).bind(entry.position).bind(&entry.kind)
                .bind(&entry.name).bind(&entry.content).bind(&entry.input).bind(&entry.output).bind(&entry.status)
                .bind(entry.started_at).bind(entry.ended_at).bind(run.seq).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn list(
        &self,
        user_id: &str,
        conversation_id: &str,
        after: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SubagentRunRow>, DbError> {
        Ok(sqlx::query_as("SELECT r.* FROM subagent_runs r JOIN conversations c ON c.id=r.conversation_id WHERE c.user_id=? AND c.id=? AND (? IS NULL OR (r.started_at,r.child_run_id) > (SELECT started_at,child_run_id FROM subagent_runs WHERE child_run_id=? AND conversation_id=?)) ORDER BY r.started_at,r.child_run_id LIMIT ?")
            .bind(user_id).bind(conversation_id).bind(after).bind(after).bind(conversation_id)
            .bind(limit.clamp(1,100)).fetch_all(&self.pool).await?)
    }

    async fn entries(
        &self,
        user_id: &str,
        conversation_id: &str,
        child_run_id: &str,
        after: i64,
        limit: i64,
    ) -> Result<Vec<SubagentEntryRow>, DbError> {
        let owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM subagent_runs r JOIN conversations c ON c.id=r.conversation_id WHERE c.user_id=? AND c.id=? AND r.child_run_id=?)")
            .bind(user_id).bind(conversation_id).bind(child_run_id).fetch_one(&self.pool).await?;
        if !owned {
            return Err(DbError::NotFound("Subagent not found".into()));
        }
        Ok(sqlx::query_as(
            "SELECT * FROM subagent_entries WHERE child_run_id=? AND position>? ORDER BY position LIMIT ?",
        )
        .bind(child_run_id)
        .bind(after.max(0))
        .bind(limit.clamp(1, 100))
        .fetch_all(&self.pool)
        .await?)
    }

    async fn recover_interrupted(&self) -> Result<u64, DbError> {
        Ok(
            sqlx::query("UPDATE subagent_runs SET status='unknown',incomplete=1,seq=seq+1 WHERE status='running'")
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
}

#[cfg(test)]
#[path = "sqlite_subagent_test.rs"]
mod tests;
