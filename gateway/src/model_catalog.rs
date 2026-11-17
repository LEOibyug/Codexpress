use crate::{
    model::AppState,
    compaction::ModelCompactionRule,
    model_policy::{self, ClientKind},
};
use axum::{http::{request::Parts, StatusCode}, response::{IntoResponse, Response}, Json};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{collections::{BTreeMap, BTreeSet}, future::Future, path::Path, sync::Arc, time::SystemTime};
use sha2::{Digest, Sha256};

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

pub(crate) fn configuration_revision(
    rules: &[ModelCompactionRule], sources: &[crate::model::SourceRuntime],
    policies: &[crate::model_policy::ModelPolicy], mode: &str, fixed: Option<&str>,
) -> String {
    let mut sources: Vec<_> = sources.iter().map(|runtime| {
        let source = &runtime.source;
        let models: BTreeSet<_> = source.models.iter().collect();
        (source.id.as_str(), json!({"id":source.id,"enabled":source.enabled,"kind":source.kind,"models":models}))
    }).collect();
    sources.sort_unstable_by(|left, right| left.0.cmp(right.0));
    let mut policies: Vec<_> = policies.iter().collect();
    policies.sort_unstable_by(|left, right| left.model.cmp(&right.model));
    let snapshot = json!({"rules":crate::compaction::revision(rules),"sources":sources,"policies":policies,"mode":mode,"fixed":fixed});
    format!("{:x}", Sha256::digest(serde_json::to_vec(&snapshot).expect("JSON catalog configuration")))
}

fn merge_metadata(entries: &mut BTreeMap<String, Value>, mut catalog: Value) {
    let Some(models) = catalog.get_mut("models").and_then(Value::as_array_mut) else { return; };
    for model in models.drain(..) {
        if let Some(id) = model["slug"].as_str() { entries.insert(id.to_owned(), model); }
    }
}

// Filesystem identity checks are cheap enough for budget polling; parsing and
// canonical hashing happen only after the local catalog file actually changes.
pub(crate) struct MetadataCache {
    signature: Option<(u64, SystemTime)>,
    metadata: Arc<[Value]>,
    pub(crate) revision: String,
}

impl MetadataCache {
    pub(crate) fn open(state_dir: &Path) -> Self {
        let mut cache = Self { signature: None, metadata: Arc::from([]), revision: String::new() };
        cache.refresh(state_dir);
        cache
    }

    pub(crate) fn refresh(&mut self, state_dir: &Path) {
        let path = state_dir.join("model-catalogs/official-latest.json");
        let signature = std::fs::metadata(&path).ok().and_then(|metadata| {
            Some((metadata.len(), metadata.modified().ok()?))
        });
        if !self.revision.is_empty() && signature == self.signature { return; }
        self.signature = signature;
        let mut entries = BTreeMap::new();
        if let Ok(catalog) = crate::compaction::bundled_catalog() {
            merge_metadata(&mut entries, catalog);
        }
        if signature.is_some_and(|(size, _)| size <= 4 * 1024 * 1024) {
            if let Some(catalog) = std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()) {
                merge_metadata(&mut entries, catalog);
            }
        }
        let metadata: Vec<_> = entries.into_values().collect();
        let normalized: Vec<_> = metadata.iter().map(|model| {
            normalized_model(model["slug"].as_str().unwrap_or_default(), Some(model), None)
        }).collect();
        self.revision = format!("{:x}", Sha256::digest(serde_json::to_vec(&normalized).expect("JSON model metadata")));
        self.metadata = metadata.into();
    }
}

async fn metadata(state: &Arc<AppState>) -> Arc<[Value]> {
    let mut cache = state.model_metadata.write().await;
    cache.refresh(&state.state_dir);
    cache.metadata.clone()
}

pub(crate) async fn info(state: &Arc<AppState>) -> Value {
    let _sync = state.routing_sync.lock().await;
    let metadata = metadata(state).await;
    let rules = state.model_compaction.read().await;
    let mut names: BTreeSet<String> = metadata.iter().filter_map(|model| model["slug"].as_str().map(str::to_owned)).collect();
    names.extend(state.sources.read().await.iter().flat_map(|runtime| runtime.source.models.iter().cloned()));
    names.extend(state.model_policies.read().await.iter().map(|policy| policy.model.clone()));
    names.extend(rules.iter().filter(|rule| rule.context_window.is_some() || rule.auto_compact_token_limit.is_some()).map(|rule| rule.model.clone()));
    json!({"models": names.into_iter().map(|id| {
        normalized_model(&id, metadata.iter().find(|model| model["slug"] == id), rules.iter().find(|rule| rule.model == id))
    }).collect::<Vec<_>>()})
}

fn normalized_model(id: &str, metadata: Option<&Value>, rule: Option<&ModelCompactionRule>) -> Value {
    let mut model = json!({"id":id,"name":id,"object":"model","created":0,"owned_by":"codexpress","auto_compact_token_limit":null});
    if let Some(metadata) = metadata {
        for (target, source) in [("context_window","context_window"),("max_context_window","max_context_window"),
            ("auto_compact_token_limit","auto_compact_token_limit"),("max_output_tokens","max_output_tokens"),
            ("name","display_name"),("kind","kind"),("thinking","thinking"),("cost","cost"),("pricing","pricing"),
            ("supported_reasoning_levels","supported_reasoning_levels"),("experimental_supported_tools","experimental_supported_tools")] {
            if let Some(value) = metadata.get(source) { model[target] = value.clone(); }
        }
        if let Some(input) = metadata["input_modalities"].as_array() {
            model["input"] = Value::Array(input.clone());
        }
        if let Some(levels) = metadata["supported_reasoning_levels"].as_array() {
            let efforts: Vec<_> = levels.iter().filter_map(|level| level.as_str().or_else(|| level["effort"].as_str())).collect();
            model["reasoning"] = json!(!efforts.is_empty());
            if !efforts.is_empty() && model["thinking"]["mode"].as_str().is_none() {
                model["thinking"] = json!({"mode":"effort","efforts":efforts});
            }
        }
    }
    if model.get("cost").is_none_or(Value::is_null) {
        if let Some(cost) = crate::pricing::model_cost(id) { model["cost"] = cost; }
    }
    crate::compaction::apply_budget(&mut model, rule);
    if model.get("auto_compact_token_limit").is_none() { model["auto_compact_token_limit"] = Value::Null; }
    model
}

pub(crate) struct Catalog {
    names: BTreeSet<String>,
    metadata: Arc<[Value]>,
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
    let metadata = metadata(state).await;
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
    names.extend(rules.iter().filter(|rule| rule.context_window.is_some() || rule.auto_compact_token_limit.is_some()).map(|rule| rule.model.clone()));
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
        let mut metadata = self.metadata.to_vec();
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
            normalized_model(&id, self.metadata.iter().find(|model| model["slug"] == id), self.rules.iter().find(|rule| rule.model == id))
        }).collect()
    }
}
