use crate::{
    model::AppState,
    compaction::ModelCompactionRule,
    model_policy::{self, ClientKind},
};
use anyhow::{bail, Context, Result};
use axum::{http::{request::Parts, StatusCode}, response::{IntoResponse, Response}, Json};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, future::Future,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path, sync::Arc, time::SystemTime,
};
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

const LOCAL_CAPABILITIES: &str = "model-catalogs/local-capabilities.json";
const MAX_IMPORT_BYTES: usize = 256 * 1024;

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LocalCapabilities {
    models: Vec<LocalCapability>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LocalCapability {
    slug: String,
    max_context_window: i64,
    auto_compact_token_limit: Option<i64>,
}

impl LocalCapabilities {
    fn parse(json: &[u8]) -> Result<Self> {
        if json.len() > MAX_IMPORT_BYTES { bail!("模型能力 JSON 不能超过 256 KiB"); }
        let mut capabilities: Self = serde_json::from_slice(json)
            .map_err(|_| anyhow::anyhow!("模型能力 JSON 格式错误；请只使用 models、slug、max_context_window、auto_compact_token_limit 字段"))?;
        if capabilities.models.len() > 200 { bail!("最多支持导入 200 条模型能力"); }
        capabilities.models.sort_unstable_by(|left, right| left.slug.cmp(&right.slug));
        if capabilities.models.windows(2).any(|entries| entries[0].slug == entries[1].slug) {
            bail!("模型能力包含重复的模型 ID");
        }
        for model in &capabilities.models {
            if model.slug.is_empty() || model.slug.len() > 256
                || model.slug.chars().any(|c| c.is_whitespace() || c.is_control())
            {
                bail!("模型 ID 必须为 1 至 256 字节且不含空白或控制字符");
            }
            if model.slug == crate::images::MODEL { bail!("不支持导入图像模型的文本预算"); }
            crate::compaction::validate_budget(&model.slug, Some(model.max_context_window), model.auto_compact_token_limit)?;
        }
        Ok(capabilities)
    }

    fn load(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        fs::File::open(path)?.take(MAX_IMPORT_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        Self::parse(&bytes)
    }

    fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().context("无法确定模型能力目录")?;
        fs::create_dir_all(parent)?;
        let bytes = serde_json::to_vec(self)?;
        let temporary = parent.join(format!(".codexpress-{}.tmp", uuid::Uuid::new_v4()));
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
        // Rename is the commit point; nothing fallible follows it.
        let result = file.write_all(&bytes).and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&temporary, path));
        if result.is_err() { let _ = fs::remove_file(&temporary); }
        result?;
        Ok(())
    }

    fn merge(&self, entries: &mut BTreeMap<String, Value>) {
        for model in &self.models {
            let metadata = entries.entry(model.slug.clone())
                .or_insert_with(|| crate::compaction::metadata_for_model(&[], &model.slug));
            let context = metadata["context_window"].as_i64().filter(|n| *n >= 2)
                .unwrap_or(model.max_context_window).min(model.max_context_window);
            metadata["context_window"] = json!(context);
            metadata["max_context_window"] = json!(model.max_context_window);
            metadata["auto_compact_token_limit"] = json!(model.auto_compact_token_limit);
            if let Some(fields) = metadata.as_object_mut() {
                for field in ["default_context_window", "default_max_context_window", "default_auto_compact_token_limit"] {
                    fields.remove(field);
                }
            }
        }
    }
}

// Polls only check official-cache identity. Local overrides publish explicitly.
pub(crate) struct MetadataCache {
    signature: Option<(u64, SystemTime)>,
    local: LocalCapabilities,
    metadata: Arc<[Value]>,
    pub(crate) revision: String,
}

impl MetadataCache {
    pub(crate) fn open(state_dir: &Path) -> Self {
        let local = LocalCapabilities::load(&state_dir.join(LOCAL_CAPABILITIES)).unwrap_or_default();
        let mut cache = Self { signature: None, local, metadata: Arc::from([]), revision: String::new() };
        cache.refresh(state_dir);
        cache
    }

    pub(crate) fn refresh(&mut self, state_dir: &Path) {
        self.refresh_from_disk(state_dir, false);
    }

    fn refresh_from_disk(&mut self, state_dir: &Path, force: bool) {
        let path = state_dir.join("model-catalogs/official-latest.json");
        let signature = std::fs::metadata(&path).ok().and_then(|metadata| {
            Some((metadata.len(), metadata.modified().ok()?))
        });
        if !force && !self.revision.is_empty() && signature == self.signature { return; }
        self.signature = signature;
        let mut entries = BTreeMap::new();
        if let Ok(catalog) = crate::compaction::bundled_catalog() {
            merge_metadata(&mut entries, catalog);
        }
        if signature.is_some_and(|(size, _)| size <= 4 * 1024 * 1024) {
            if let Some(catalog) = fs::read(path).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()) {
                merge_metadata(&mut entries, catalog);
            }
        }
        self.local.merge(&mut entries);
        let metadata: Vec<_> = entries.into_values().collect();
        let normalized: Vec<_> = metadata.iter().map(|model| {
            normalized_model(model["slug"].as_str().unwrap_or_default(), Some(model), None)
        }).collect();
        self.revision = format!("{:x}", Sha256::digest(serde_json::to_vec(&normalized).expect("JSON model metadata")));
        self.metadata = metadata.into();
    }
}

pub(crate) async fn replace_metadata(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        if payload.as_object().is_none_or(|fields| fields.len() != 1) {
            bail!("请提供模型能力 JSON 或指定恢复默认");
        }
        let local = match (payload.get("json"), payload.get("reset")) {
            (Some(Value::String(text)), None) => {
                let local = LocalCapabilities::parse(text.as_bytes())?;
                if local.models.is_empty() { bail!("模型能力必须包含非空 models 列表；清空请使用恢复默认"); }
                local
            }
            (None, Some(Value::Bool(true))) => LocalCapabilities::default(),
            _ => bail!("请提供模型能力 JSON 或指定恢复默认"),
        };
        let count = local.models.len();
        // Serialize writes with catalog readers, including if an IPC caller disconnects.
        let mut cache = state.model_metadata.blocking_write();
        local.save(&state.state_dir.join(LOCAL_CAPABILITIES)).context("无法保存本地模型能力，未更改快照")?;
        cache.local = local;
        cache.refresh_from_disk(&state.state_dir, true);
        Ok(json!({"count":count,"revision":cache.revision}))
    }).await.context("模型能力更新失败")?
}

async fn metadata(state: &Arc<AppState>) -> Arc<[Value]> {
    let mut cache = state.model_metadata.write().await;
    cache.refresh(&state.state_dir);
    cache.metadata.clone()
}

pub(crate) async fn info(state: &Arc<AppState>) -> Value {
    let _sync = state.routing_sync.lock().await;
    let mut names = BTreeSet::new();
    let rules = state.model_compaction.read().await;
    names.extend(state.sources.read().await.iter().flat_map(|runtime| runtime.source.models.iter().cloned()));
    names.extend(state.model_policies.read().await.iter().map(|policy| policy.model.clone()));
    names.extend(rules.iter().filter(|rule| rule.context_window.is_some() || rule.auto_compact_token_limit.is_some()).map(|rule| rule.model.clone()));
    let mut cache = state.model_metadata.write().await;
    cache.refresh(&state.state_dir);
    let metadata = &cache.metadata;
    names.extend(metadata.iter().filter_map(|model| model["slug"].as_str().map(str::to_owned)));
    json!({"models": names.into_iter().map(|id| {
        let mut model = normalized_model(&id, metadata.iter().find(|model| model["slug"] == id), rules.iter().find(|rule| rule.model == id));
        if cache.local.models.binary_search_by(|entry| entry.slug.cmp(&id)).is_ok() {
            model["local_metadata"] = Value::Bool(true);
        }
        model
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::SourceRuntime, omp_ingress::budgets_revision};

    #[tokio::test]
    async fn local_capabilities_publish_live_and_failed_replacement_keeps_the_snapshot() {
        let dir = tempfile::tempdir_in(".").unwrap();
        let catalogs = dir.path().join("model-catalogs");
        fs::create_dir(&catalogs).unwrap();
        fs::write(catalogs.join("official-latest.json"), br#"{"models":[{"slug":"gpt-6.2-sol","display_name":"Official fixture","context_window":280000,"max_context_window":300000}]}"#).unwrap();
        let state = AppState::new(dir.path().to_owned(), 0, "fixture".into()).unwrap();
        let source = serde_json::from_value(json!({
            "id":"fixture","name":"Fixture","kind":"api","base_url":"http://127.0.0.1:1","enabled":true,
            "priority":1,"proxy":{"mode":"direct"},"models":["gpt-6.2-sol"],"secret":{"api_key":"fixture"}
        })).unwrap();
        state.sources.write().await.push(SourceRuntime::new(source));
        *state.model_compaction.write().await = Arc::from([ModelCompactionRule {
            model: "gpt-6.2-sol".into(), context_window: Some(700000), auto_compact_token_limit: Some(580000),
        }]);
        let baseline = budgets_revision(&state).await;
        let imported = replace_metadata(&state, json!({"json":r#"{"models":[{"slug":"gpt-6.2-sol","max_context_window":600000,"auto_compact_token_limit":400000}]}"#})).await.unwrap();
        let updated = budgets_revision(&state).await;
        assert_ne!(updated, baseline);
        assert_eq!(imported["count"], 1);

        let info_snapshot = info(&state).await;
        let info_model = info_snapshot["models"].as_array().unwrap().iter().find(|model| model["id"] == "gpt-6.2-sol").unwrap();
        assert_eq!(info_model["name"], "Official fixture");
        assert_eq!(info_model["local_metadata"], true);
        assert_eq!(info_model["default_max_context_window"], 600000);
        assert_eq!(info_model["default_auto_compact_token_limit"], 400000);
        let omp_models = load(&state, ClientKind::Omp).await.unwrap().models();
        assert_eq!(omp_models[0]["context_window"], 600000);
        assert_eq!(omp_models[0]["auto_compact_token_limit"], 540000);

        let invalid = json!({"json":r#"{"models":[{"slug":"gpt-6.2-sol","max_context_window":500000},{"slug":"invalid","max_context_window":20,"auto_compact_token_limit":19}]}"#});
        assert!(replace_metadata(&state, invalid).await.is_err());
        assert_eq!(budgets_revision(&state).await, updated);
        assert_eq!(info(&state).await, info_snapshot);
        assert_eq!(MetadataCache::open(dir.path()).revision, imported["revision"].as_str().unwrap());

        replace_metadata(&state, json!({"reset":true})).await.unwrap();
        assert_eq!(budgets_revision(&state).await, baseline);
        assert_eq!(load(&state, ClientKind::Omp).await.unwrap().models()[0]["context_window"], 300000);
    }
}
