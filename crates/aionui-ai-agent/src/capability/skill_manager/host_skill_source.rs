use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use aion_agent::host_skills::{HostSkill, HostSkillContent, HostSkillReadError, HostSkillSource};
use aionui_extension::skill_service::{extract_skill_body, parse_frontmatter_fields};
use async_trait::async_trait;
use tracing::{info, warn};

use super::AcpSkillManager;

#[derive(Debug)]
struct SessionSkillSource {
    enabled: BTreeSet<String>,
    entries: BTreeMap<String, (String, PathBuf)>,
}

impl AcpSkillManager {
    pub(crate) async fn host_skill_source_for_user(&self, user_id: &str, names: &[String]) -> Arc<dyn HostSkillSource> {
        let mut source = SessionSkillSource {
            enabled: names.iter().cloned().collect(),
            entries: BTreeMap::new(),
        };
        // Never use the unscoped dev discovery fallback for a host skill reader.
        if self.skill_repo.is_some() {
            for spec in self.resolve_skill_dirs_for_user(user_id, names).await {
                let path = PathBuf::from(spec.path);
                let Ok(root) = tokio::fs::canonicalize(&path).await else {
                    continue;
                };
                let Ok(content) = tokio::fs::read_to_string(root.join("SKILL.md")).await else {
                    continue;
                };
                let Some((_, description)) = parse_frontmatter_fields(&content) else {
                    continue;
                };
                if source.enabled.contains(&spec.name) {
                    source.entries.insert(spec.name, (description, root));
                }
            }
        } else if !names.is_empty() {
            warn!("host skills unavailable: user-scoped repository is not configured");
        }
        info!(
            selected = source.enabled.len(),
            available = source.entries.len(),
            unavailable = source.enabled.len().saturating_sub(source.entries.len()),
            "conversation host skill source resolved"
        );
        Arc::new(source)
    }
}

#[async_trait]
impl HostSkillSource for SessionSkillSource {
    fn unavailable(&self) -> Vec<String> {
        self.enabled
            .iter()
            .filter(|name| !self.entries.contains_key(*name))
            .cloned()
            .collect()
    }
    fn available(&self) -> Vec<HostSkill> {
        self.entries
            .iter()
            .map(|(name, (description, _))| HostSkill {
                name: name.clone(),
                description: description.clone(),
            })
            .collect()
    }

    async fn read(&self, name: &str) -> Result<HostSkillContent, HostSkillReadError> {
        if !self.enabled.contains(name) {
            return Err(HostSkillReadError::NotEnabled);
        }
        let (_, root) = self.entries.get(name).ok_or(HostSkillReadError::Unavailable)?;
        let content = tokio::fs::read_to_string(root.join("SKILL.md"))
            .await
            .map_err(|_| HostSkillReadError::Unavailable)?;
        if parse_frontmatter_fields(&content).is_none() {
            return Err(HostSkillReadError::Unavailable);
        }
        Ok(HostSkillContent {
            root: root.clone(),
            body: extract_skill_body(&content),
        })
    }
}

#[cfg(test)]
#[path = "host_skill_source_test.rs"]
mod host_skill_source_test;
