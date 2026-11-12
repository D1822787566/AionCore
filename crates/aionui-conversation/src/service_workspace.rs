use super::*;
use aionui_api_types::SwitchConversationWorkspaceRequest;

impl ConversationService {
    /// Save the directory for future turns without interrupting the active runtime.
    pub async fn switch_workspace(
        &self,
        user_id: &str,
        id: &str,
        req: SwitchConversationWorkspaceRequest,
        _task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<ConversationResponse, ConversationError> {
        let _guard = self.runtime_state.begin_directory_change(id)?;
        let row = self
            .conversation_repo
            .get(user_id, id)
            .await?
            .ok_or_else(|| ConversationError::NotFound { id: id.into() })?;
        if row.r#type != "aionrs" || team_id_from_extra(&row.extra).is_some() {
            return Err(ConversationError::bad_request(
                "WORKSPACE_SWITCH_UNSUPPORTED: only standalone aionrs conversations can switch directories",
            ));
        }
        let mut extra: serde_json::Value =
            serde_json::from_str(&row.extra).map_err(|_| ConversationError::internal("invalid conversation extra"))?;
        let old_workspace = extra.get("workspace").and_then(|v| v.as_str()).unwrap_or("").to_owned();
        if req.expected_workspace != old_workspace {
            return Err(ConversationError::Busy {
                reason: "WORKSPACE_SWITCH_STALE: reload the conversation before switching".into(),
            });
        }
        if usize::from(req.project_id.is_some())
            + usize::from(req.workspace.is_some())
            + usize::from(req.restore_default)
            != 1
        {
            return Err(ConversationError::bad_request(
                "WORKSPACE_SWITCH_INVALID: choose exactly one project, directory, or restore-default action",
            ));
        }
        let project = self
            .project_service
            .read()
            .ok()
            .and_then(|s| s.clone())
            .ok_or_else(|| ConversationError::internal("project service unavailable"))?;
        // Legacy default conversations did not persist this field; capture it before leaving.
        if extra.get("default_workspace").is_none() && is_temp_session_workspace(Path::new(&old_workspace)) {
            extra["default_workspace"] = old_workspace.clone().into();
        }
        let target = if req.restore_default {
            if let Some(path) = extra.get("default_workspace").and_then(|v| v.as_str()) {
                path.to_owned()
            } else {
                // A conversation created in a project has no earlier private directory.
                let path = auto_workspace_parent(&self.workspace_root, user_id).join(format!("aionrs-temp-{id}"));
                std::fs::create_dir_all(&path)
                    .map_err(|_| ConversationError::bad_request("WORKSPACE_SWITCH_DIRECTORY_UNAVAILABLE"))?;
                let path = path.to_string_lossy().into_owned();
                extra["default_workspace"] = path.clone().into();
                path
            }
        } else if let Some(project_id) = req.project_id.as_deref() {
            let detail = project
                .get_project(user_id, project_id)
                .await
                .map_err(|_| ConversationError::not_found_reason("WORKSPACE_SWITCH_PROJECT_NOT_FOUND"))?;
            let entry = detail
                .explorer
                .entries
                .iter()
                .find(|entry| entry.role == "workspace")
                .ok_or_else(|| ConversationError::bad_request("WORKSPACE_SWITCH_DIRECTORY_UNAVAILABLE"))?;
            canonical::uri_to_path(&entry.folder.resource_uri)
                .map_err(|_| ConversationError::bad_request("WORKSPACE_SWITCH_DIRECTORY_UNAVAILABLE"))?
                .to_string_lossy()
                .into_owned()
        } else {
            req.workspace.clone().unwrap_or_default()
        };
        let target = normalize_workspace_path(&target)?;
        if !Path::new(&target).is_dir() || std::fs::read_dir(&target).is_err() {
            return Err(ConversationError::bad_request("WORKSPACE_SWITCH_DIRECTORY_UNAVAILABLE"));
        }
        let binding = if req.restore_default {
            None
        } else {
            let uri = canonical::to_file_uri(Path::new(&target))
                .map_err(|_| ConversationError::bad_request("WORKSPACE_SWITCH_DIRECTORY_UNAVAILABLE"))?;
            Some(
                project
                    .create_standard(user_id, uri)
                    .await
                    .map_err(|_| ConversationError::bad_request("WORKSPACE_SWITCH_DIRECTORY_UNAVAILABLE"))?,
            )
        };
        // Existing turns retain their runtime and cwd. The next turn reconciles
        // its build options with the cached runtime before sending any prompt.
        extra["workspace"] = target.into();
        extra["using_default_workspace"] = req.restore_default.into();
        let updates = ConversationRowUpdate {
            extra: Some(extra.to_string()),
            project_id: binding.as_ref().map(|b| b.project.project_id.clone()),
            folder_id: binding.as_ref().map(|b| b.folder.folder_id.clone()),
            clear_project_binding: req.restore_default,
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        self.conversation_repo.update(user_id, id, &updates).await?;
        let updated = self
            .conversation_repo
            .get(user_id, id)
            .await?
            .ok_or_else(|| ConversationError::NotFound { id: id.into() })?;
        drop(_guard);
        info!(
            conversation_id = id,
            restored_default = req.restore_default,
            active_turn = self.runtime_state.is_claimed(id),
            "Conversation directory saved for future turns; active runtime preserved"
        );
        self.broadcast_list_changed(
            user_id,
            id,
            "updated",
            row_to_response(updated.clone(), &self.workspace_root)?.source.as_ref(),
        );
        let mut response = row_to_response(updated, &self.workspace_root)?;
        response.runtime = Some(self.runtime_summary_for(id).await);
        Ok(response)
    }

    /// Reconcile cwd only at a turn boundary, never from warmup/config reads.
    pub(crate) async fn build_turn_runtime(
        &self,
        id: &str,
        options: BuildTaskOptions,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<AgentInstance, AgentError> {
        let agent = task_manager.get_or_build_task(id, options.clone()).await?;
        if agent.agent_type() != AgentType::Aionrs
            || options.context.conversation.agent_type != AgentType::Aionrs
            || options.context.team.is_some()
            || options.context.workspace.stored_path.is_empty()
            || agent.workspace() == options.context.workspace.path
        {
            return Ok(agent);
        }
        // A background task may outlive the foreground turn. Preserve it too;
        // a new turn can be retried once those tasks finish.
        if agent.live_background_tasks() > 0 {
            return Err(AgentError::bad_request(
                "WORKSPACE_SWITCH_BACKGROUND_ACTIVE: previous-directory background tasks are still running; retry after they finish",
            ));
        }
        info!(
            conversation_id = id,
            "Rebuilding idle runtime for the next turn's saved directory"
        );
        task_manager
            .kill_and_wait(id, Some(AgentKillReason::RuntimeRestart))
            .await;
        task_manager.get_or_build_task(id, options).await
    }
}
