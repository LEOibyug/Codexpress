use crate::model::{AppState, Source};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, sync::Arc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Codex,
    Omp,
    Chatbot,
    Images,
}

fn default_clients() -> Vec<ClientKind> {
    vec![ClientKind::Codex, ClientKind::Omp, ClientKind::Chatbot, ClientKind::Images]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPolicy {
    pub model: String,
    #[serde(default)]
    pub source_ids: Option<Vec<String>>,
    #[serde(default = "default_clients")]
    pub clients: Vec<ClientKind>,
}

pub fn validate(policies: &[ModelPolicy]) -> Result<()> {
    let mut models = HashSet::new();
    for policy in policies {
        if policy.model.is_empty()
            || policy.model.len() > 256
            || policy.model.chars().any(|c| c.is_whitespace() || c.is_control())
            || !models.insert(&policy.model)
        {
            bail!("请填写不含空白且不重复的模型 ID");
        }
        if let Some(ids) = &policy.source_ids {
            let mut sources = HashSet::new();
            for id in ids {
                if id.is_empty()
                    || id.len() > 128
                    || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                    || !sources.insert(id)
                {
                    bail!("模型来源标识无效或重复");
                }
            }
        }
        let mut clients = HashSet::new();
        if policy.clients.iter().any(|client| !clients.insert(*client)) {
            bail!("模型接入端不能重复");
        }
    }
    Ok(())
}

pub fn policy<'a>(policies: &'a [ModelPolicy], model: &str) -> Option<&'a ModelPolicy> {
    policies.iter().find(|policy| policy.model == model)
}

pub fn source_allowed(policies: &[ModelPolicy], model: &str, source_id: &str) -> bool {
    policy(policies, model)
        .and_then(|policy| policy.source_ids.as_ref())
        .is_none_or(|ids| ids.iter().any(|id| id == source_id))
}

pub fn client_allowed(policies: &[ModelPolicy], model: &str, client: ClientKind) -> bool {
    policy(policies, model).is_none_or(|policy| policy.clients.contains(&client))
}

pub fn has_custom_sources(policies: &[ModelPolicy], model: &str) -> bool {
    policy(policies, model).is_some_and(|policy| policy.source_ids.is_some())
}

pub fn sort_sources(policies: &[ModelPolicy], model: &str, sources: &mut [Source]) {
    if let Some(ids) = policy(policies, model).and_then(|policy| policy.source_ids.as_ref()) {
        sources.sort_by_key(|source| ids.iter().position(|id| id == &source.id).unwrap_or(usize::MAX));
    } else {
        sources.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.id.cmp(&b.id)));
    }
}

pub(crate) async fn codex_excluded_models(state: &Arc<AppState>) -> HashSet<String> {
    let _sync = state.routing_sync.lock().await;
    let policies = state.model_policies.read().await;
    let settings = state.settings.read().await;
    let sources = state.sources.read().await;
    policies.iter().filter(|policy| {
        !policy.clients.contains(&ClientKind::Codex)
            || policy.source_ids.as_ref().is_some_and(|ids| !sources.iter().any(|runtime| {
                let source = &runtime.source;
                source.enabled && ids.contains(&source.id)
                    && (settings.mode != "fixed" || settings.fixed_source_id.as_ref() == Some(&source.id))
                    && (source.models.is_empty() || source.models.contains(&policy.model))
            }))
    }).map(|policy| policy.model.clone()).collect()
}

pub(crate) fn filter_codex_catalog(catalog: &mut Value, excluded: &HashSet<String>) {
    catalog["models"].as_array_mut().expect("generated model catalog")
        .retain(|model| model["slug"].as_str().is_none_or(|id| !excluded.contains(id)));
}
