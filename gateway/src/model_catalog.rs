use crate::{
    model::AppState,
    compaction::ModelCompactionRule,
    model_policy::{self, ClientKind},
};
use axum::{http::{request::Parts, StatusCode}, response::{IntoResponse, Response}, Json};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{collections::BTreeSet, future::Future, sync::Arc};

pub(crate) fn client(parts: &Parts) -> ClientKind {
    if parts.extensions.get::<crate::omp_ingress::OmpIngress>().is_some() {
        ClientKind::Omp
    } else if parts.extensions.get::<crate::chatbot::ChatbotIngress>().is_some() {
        ClientKind::Chatbot
    } else {
        ClientKind::Codex
    }
}

#[derive(Clone)]
pub(crate) struct ModelClientRejection;

pub(crate) async fn check_client(state: &Arc<AppState>, model: &str, client: ClientKind) -> Option<Response> {
    let policies = state.model_policies.read().await;
    (!model_policy::client_allowed(&policies, model, client)).then(|| {
        let mut response = (StatusCode::BAD_REQUEST, Json(json!({"error":{
            "type":"invalid_request_error", "code":"model_not_enabled_for_client", "param":"model",
            "message":format!("模型 {model} 未向此接入端开放，请在模型设置中启用")
        }}))).into_response();
        response.extensions_mut().insert(ModelClientRejection);
        response
    })
}

// Release the configuration boundary after the HTTP future has been submitted,
// not after upstream headers arrive; saving settings must not wait on a stream.
pub(crate) async fn start_submission<F: Future>(
    guard: tokio::sync::MutexGuard<'_, ()>,
    future: F,
) -> F::Output {
    use std::task::Poll;
    tokio::pin!(future);
    let first = std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await;
    drop(guard);
    match first {
        Poll::Ready(result) => result,
        Poll::Pending => future.await,
    }
}

pub(crate) struct Catalog {
    names: BTreeSet<String>,
    metadata: Vec<Value>,
    rules: Arc<[ModelCompactionRule]>,
}

struct CatalogSource {
    id: String,
    official: bool,
    models: Vec<String>,
}
pub(crate) async fn load(state: &Arc<AppState>, client: ClientKind) -> anyhow::Result<Catalog> {
    let (sources, policies, rules) = {
        let _sync = state.routing_sync.lock().await;
        let settings = state.settings.read().await;
        let sources: Vec<CatalogSource> = state.sources.read().await.iter().filter(|runtime| {
            runtime.source.enabled
                && (settings.mode != "fixed" || settings.fixed_source_id.as_ref() == Some(&runtime.source.id))
        }).map(|runtime| CatalogSource {
            id: runtime.source.id.clone(), official: runtime.source.kind == "official", models: runtime.source.models.clone(),
        }).collect();
        (sources, state.model_policies.read().await.clone(), state.model_compaction.read().await.clone())
    };
    let catalog = std::fs::read(state.state_dir.join("model-catalogs/official-latest.json"))
        .ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .or_else(|| crate::compaction::bundled_catalog().ok());
    let metadata = catalog.as_ref().and_then(|value| value["models"].as_array()).cloned().unwrap_or_default();
    let mut names = BTreeSet::new();
    for source in &sources {
        names.extend(source.models.iter().cloned());
        if source.official && source.models.is_empty() {
            names.extend(metadata.iter().filter_map(|entry| entry["slug"].as_str().map(str::to_owned)));
        }
    }
    // Reuse the existing on-demand discovery for uncached API sources. Its result
    // belongs to this snapshot, not to the user's saved capability inventory.
    let requests: Vec<_> = sources.iter().filter(|source| {
        source.models.is_empty() && (!source.official || metadata.is_empty())
    }).map(|source| json!({"source_id":source.id})).collect();
    let discovery_attempted = !requests.is_empty();
    let mut discovery_succeeded = false;
    let mut queries = futures_util::stream::iter(requests).map(|payload| async move {
        crate::discovery::list(state, payload).await
    }).buffer_unordered(4);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while let Ok(Some(result)) = tokio::time::timeout_at(deadline, queries.next()).await {
        if let Ok(result) = result {
            discovery_succeeded = true;
            names.extend(result["models"].as_array().into_iter().flatten()
                .filter_map(|value| value.as_str().map(str::to_owned)));
        }
    }
    drop(queries);
    names.extend(policies.iter().map(|policy| policy.model.clone()));
    names.extend(rules.iter().map(|rule| rule.model.clone()));
    names.retain(|model| model != crate::images::MODEL
        && model_policy::client_allowed(&policies, model, client)
        && sources.iter().any(|source| {
            model_policy::source_allowed(&policies, model, &source.id)
                && (source.models.is_empty() || source.models.iter().any(|id| id == model))
        }));
    if names.is_empty() && discovery_attempted && !discovery_succeeded {
        anyhow::bail!("无法获取来源模型列表，请先在来源页刷新或手动填写");
    }
    Ok(Catalog { names, metadata, rules })
}

impl Catalog {
    pub(crate) fn codex(self) -> anyhow::Result<Value> {
        if self.names.is_empty() { return Ok(json!({"models":[]})); }
        let mut metadata = self.metadata;
        for name in &self.names {
            if !metadata.iter().any(|entry| entry["slug"] == *name) {
                metadata.push(crate::compaction::metadata_for_model(&metadata, name));
            }
        }
        metadata.retain(|entry| entry["slug"].as_str().is_some_and(|id| self.names.contains(id)));
        let rules: Vec<_> = self.rules.iter().filter(|rule| self.names.contains(&rule.model)).cloned().collect();
        crate::compaction::catalog_from_value(json!({"models":metadata}), &toml_edit::DocumentMut::new(), &rules)
    }
    pub(crate) fn models(self) -> Vec<Value> {
        self.names.into_iter().map(|id| {
            let mut model = json!({"id":id,"object":"model","created":0,"owned_by":"codexpress","auto_compact_token_limit":null});
            if let Some(metadata) = self.metadata.iter().find(|entry| entry["slug"] == model["id"]) {
                for (target, source) in [("context_window","context_window"),("max_context_window","max_context_window"),
                    ("max_output_tokens","max_output_tokens"),("name","display_name"),("input","input_modalities"),
                    ("kind","kind"),("thinking","thinking"),("cost","cost"),("pricing","pricing"),
                    ("supported_reasoning_levels","supported_reasoning_levels"),("experimental_supported_tools","experimental_supported_tools")] {
                    if let Some(value) = metadata.get(source) { model[target] = value.clone(); }
                }
                if let Some(levels) = metadata["supported_reasoning_levels"].as_array() { model["reasoning"] = json!(!levels.is_empty()); }
            }
            if let Some(rule) = self.rules.iter().find(|rule| model["id"] == rule.model) {
                if let Some(window) = rule.context_window { model["context_window"] = json!(window); }
                model["auto_compact_token_limit"] = json!(rule.auto_compact_token_limit);
            }
            if let Some(window) = model["context_window"].as_i64() {
                model["max_context_window"] = json!(model["max_context_window"].as_i64().unwrap_or(window).max(window));
            }
            model
        }).collect()
    }
}
