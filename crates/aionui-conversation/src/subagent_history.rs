use super::ConversationService;
use crate::error::ConversationError;
use aionui_ai_agent::protocol::events::subagent::{entry_response, run_response};
use aionui_api_types::{SubagentEntriesQuery, SubagentEntriesResponse, SubagentListQuery, SubagentListResponse};
use aionui_db::ISubagentRepository;
use std::sync::Arc;

impl ConversationService {
    pub fn with_subagent_repo(&self, repo: Arc<dyn ISubagentRepository>) {
        *self.subagent_repo.write().unwrap_or_else(|e| e.into_inner()) = Some(repo);
    }

    async fn subagent_repository(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Arc<dyn ISubagentRepository>, ConversationError> {
        self.conversation_repo
            .get(user_id, conversation_id)
            .await?
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.into(),
            })?;
        self.subagent_repo
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| ConversationError::Internal {
                reason: "Subagent records unavailable".into(),
            })
    }

    pub async fn list_subagents(
        &self,
        user_id: &str,
        conversation_id: &str,
        query: SubagentListQuery,
    ) -> Result<SubagentListResponse, ConversationError> {
        let repo = self.subagent_repository(user_id, conversation_id).await?;
        let limit = page_limit(query.limit)?;
        let items = repo
            .list(user_id, conversation_id, query.after.as_deref(), limit)
            .await?;
        let next_cursor = (items.len() == limit as usize)
            .then(|| items.last().map(|r| r.child_run_id.clone()))
            .flatten();
        Ok(SubagentListResponse {
            items: items.into_iter().map(run_response).collect(),
            next_cursor,
        })
    }

    pub async fn subagent_entries(
        &self,
        user_id: &str,
        conversation_id: &str,
        child_run_id: &str,
        query: SubagentEntriesQuery,
    ) -> Result<SubagentEntriesResponse, ConversationError> {
        let repo = self.subagent_repository(user_id, conversation_id).await?;
        let limit = page_limit(query.limit)?;
        let after = query.after.unwrap_or(0);
        if after < 0 {
            return Err(ConversationError::BadRequest {
                reason: "Subagent cursor must be nonnegative".into(),
            });
        }
        let items = repo
            .entries(user_id, conversation_id, child_run_id, after, limit)
            .await?;
        let next_cursor = (items.len() == limit as usize)
            .then(|| items.last().map(|e| e.position))
            .flatten();
        Ok(SubagentEntriesResponse {
            items: items.into_iter().map(entry_response).collect(),
            next_cursor,
        })
    }
}

fn page_limit(limit: Option<i64>) -> Result<i64, ConversationError> {
    let limit = limit.unwrap_or(100);
    if !(1..=100).contains(&limit) {
        return Err(ConversationError::BadRequest {
            reason: "Subagent page limit must be between 1 and 100".into(),
        });
    }
    Ok(limit)
}
