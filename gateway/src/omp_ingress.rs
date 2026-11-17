use crate::model::AppState;
use axum::{body::Body, extract::{Query, Request, State}, http::{HeaderValue, StatusCode}, response::{IntoResponse, Response}, routing::{get, post}, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct OmpIngress {
    pub namespace: String,
    pub normalized: bool,
    pub cache_group: Option<String>,
}

pub(crate) async fn binding(state: &Arc<AppState>, enable: bool) -> anyhow::Result<Value> {
    if enable {
        crate::chatbot::configure(state, json!({"enabled":true})).await?;
    }
    let access = state.chatbot.status().await?;
    let mut models = crate::model_catalog::load(state, crate::model_policy::ClientKind::Omp).await?.models();
    let image_available = crate::images::discoverable_for(state, crate::model_policy::ClientKind::Omp).await;
    if image_available {
        models.push(json!({"id":crate::images::MODEL,"name":"GPT Image 2","kind":"image",
            "input":["text","image"],"output":["image"],"owned_by":"openai","auto_compact_token_limit":null}));
    }
    Ok(json!({"provider":"codexpress","api":"openai-codex-responses",
        "base_url":format!("http://127.0.0.1:{}/omp/v1",access["port"]),
        "key":state.chatbot.key()?["key"],"enabled":access["enabled"],"running":access["running"],
        "models":models,
        "capabilities":{"websocket":false,"remote_compaction_v2":null,"legacy_budget_migration":true}}))
}

pub(crate) async fn budgets_revision(state: &Arc<AppState>) -> Value {
    let _sync = state.routing_sync.lock().await;
    let mut catalog = state.model_metadata.write().await;
    catalog.refresh(&state.state_dir);
    json!({"revision": format!("{}:{}", &*state.model_compaction_revision.read().await, catalog.revision)})
}

pub(crate) fn router(_state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/omp/v1/responses", post(forward).get(|| async {
            (StatusCode::NOT_IMPLEMENTED, Json(json!({"error":{"code":"unsupported_websocket","message":"此入口未实现 WebSocket，请使用 SSE"}})))
        }))
        .route("/omp/v1/responses/compact", post(forward))
        .route("/omp/v1/codex/responses", post(forward).get(|| async {
            (StatusCode::NOT_IMPLEMENTED, Json(json!({"error":{"code":"unsupported_websocket","message":"此入口未实现 WebSocket，请使用 SSE"}})))
        }))
        .route("/omp/v1/codex/responses/compact", post(forward))
        .route("/omp/v1/models", get(models))
        .route("/omp/v1/budgets/revision", get(|State(state): State<Arc<AppState>>| async move {
            Json(budgets_revision(&state).await)
        }))
        .route("/omp/v1/remote-check", get(|| async { Json(json!({"service":"codexpress","mode":"omp","version":1})) }))
        .route("/omp/v1/status", get(status))
        .route("/omp/v1/capabilities", get(capabilities))
}
async fn forward(State(state): State<Arc<AppState>>, mut request: Request) -> Response {
    let Some(ingress) = request.extensions_mut().remove::<crate::chatbot::ChatbotIngress>() else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    request.extensions_mut().insert(OmpIngress { namespace: ingress.namespace, normalized: false, cache_group: None });
    let suffix = request.uri().path().strip_prefix("/omp/v1/codex")
        .map(|suffix| format!("/v1{suffix}"))
        .unwrap_or_else(|| request.uri().path().strip_prefix("/omp").unwrap_or(request.uri().path()).to_owned());
    let uri = match request.uri().query() {
        Some(query) => format!("{suffix}?{query}"),
        None => suffix,
    };
    *request.uri_mut() = uri.parse().expect("existing URI");
    request.headers_mut().insert("authorization", HeaderValue::from_str(&format!("Bearer {}",state.gateway_key)).expect("gateway key"));
    crate::relay::forward(State(state), request).await
}

async fn models(State(state): State<Arc<AppState>>) -> Response {
    match binding(&state, false).await {
        Ok(binding) => Json(json!({"object":"list","data":binding["models"]})).into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "无法读取来源模型目录").into_response(),
    }
}
#[derive(Deserialize)]
struct StatusQuery {
    include_usage: Option<bool>,
}
async fn status(State(state): State<Arc<AppState>>, Query(query): Query<StatusQuery>) -> Response {
    match crate::management::status(&state).await {
        Ok(mut snapshot) => {
            let include_usage = query.include_usage != Some(false);
            let store = state.clone();
            let (usage, history) = tokio::task::spawn_blocking(move || (
                if include_usage {
                    store.usage.summary_filtered(30, None, None).unwrap_or(Value::Null)
                } else { Value::Null },
                store.history.status().unwrap_or(Value::Null)
            )).await.unwrap_or((Value::Null, Value::Null));
            if include_usage { snapshot["usage"] = usage; }
            snapshot["history"] = history;
            snapshot["reconciliation"] = crate::balance::billing::status(&state);
            snapshot["control"] = state.control.status().await.unwrap_or(Value::Null);
            snapshot["chatbot"] = state.chatbot.status().await.unwrap_or(Value::Null);
            snapshot["images"] = state.images.status().await.unwrap_or(Value::Null);
            snapshot["remote"] = state.remote.status().await.unwrap_or(Value::Null);
            Json(snapshot).into_response()
        },
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "无法读取网关状态").into_response(),
    }
}
#[derive(Deserialize)]
struct CapabilityQuery {
    model: String,
    session_id: Option<String>,
}
async fn capabilities(State(state): State<Arc<AppState>>, Query(query): Query<CapabilityQuery>, request: Request<Body>) -> Response {
    if let Some(problem) = crate::model_catalog::check_client(&state, &query.model, crate::model_policy::ClientKind::Omp).await { return problem; }
    let namespace = request.extensions().get::<crate::chatbot::ChatbotIngress>().map(|i|i.namespace.as_str()).unwrap_or("");
    let identity = session_identity(namespace, request.headers(), query.session_id.as_deref());
    let session = identity.map(|id| format!("omp:{namespace}:thread:{id}"));
    let affinity = session.as_ref().map(|session|crate::affinity::key(session, &query.model));
    let sources = crate::routing::candidates_with_session(&state, &query.model, None, affinity.as_deref()).await;
    let preferred = affinity.as_deref().and_then(|key|state.affinity.preferred(key, &sources));
    let source = preferred.as_ref().and_then(|id|sources.iter().find(|source|&source.id == id)).or_else(||sources.first());
    Json(json!({"source_id":source.map(|s|&s.id),"source_identity":source.map(|s|s.identity()),"websocket":false,"remote_compaction_v2":source.and_then(compaction_capability)})).into_response()
}

fn unsupported_sources() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static SOURCES: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::LazyLock::new(Default::default);
    &SOURCES
}
pub(crate) fn compaction_capability(source: &crate::model::Source) -> Option<bool> {
    unsupported_sources().lock().ok()?.contains(&source.identity()).then_some(false)
}
pub(crate) fn observe_compaction_rejection(source: &crate::model::Source, status: StatusCode, value: &Value) {
    if !matches!(status.as_u16(), 400 | 404 | 422 | 501) { return; }
    let error = value.get("error").unwrap_or(value);
    let code = error["code"].as_str().unwrap_or("");
    let param = error["param"].as_str().unwrap_or("");
    if code == "unsupported_compaction" || code == "unsupported_remote_compaction_v2"
        || (code == "unsupported_parameter" && param == "compaction_trigger") {
        if let Ok(mut sources) = unsupported_sources().lock() { sources.insert(source.identity()); }
    }
}

#[derive(Deserialize)]
struct NativeSessionMetadata {
    session_id: String,
}

pub(crate) fn session_identity(namespace: &str, headers: &axum::http::HeaderMap, private: Option<&str>) -> Option<String> {
    let native = if private.is_none() {
        headers.get("x-codex-turn-metadata").and_then(|value| value.to_str().ok())
            .and_then(|value| serde_json::from_str::<NativeSessionMetadata>(value).ok())
    } else { None };
    let id = private.or_else(|| native.as_ref().map(|metadata| metadata.session_id.as_str())
        .filter(|id| !id.is_empty() && id.len() <= 512))
        .or_else(|| ["session-id", "session_id", "x-session-id", "thread-id", "thread_id"]
            .iter().find_map(|name| headers.get(*name).and_then(|value| value.to_str().ok())))?;
    (!id.is_empty() && id.len() <= 512)
        .then(|| crate::continuity::digest("omp-session", &format!("{namespace}:{id}")))
}

// Compaction V1/V2 bypass the extension's before_provider_request hook. Retain
// only their opaque group, bounded to this AppState and keyed by the real session.
#[derive(Default)]
pub(crate) struct CacheGroups {
    sessions: std::collections::HashMap<String, (String, std::time::Instant)>,
}

impl CacheGroups {
    pub fn resolve(&mut self, session: Option<&str>, hint: Option<Value>, compaction: bool) -> Option<String> {
        let session = session?;
        match hint {
            None if compaction => self.sessions.get_mut(session).map(|(group, used)| {
                *used = std::time::Instant::now();
                group.clone()
            }),
            None => None,
            Some(Value::String(group)) if group.len() == 64
                && group.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) => {
                let now = std::time::Instant::now();
                if let Some((existing, used)) = self.sessions.get_mut(session) {
                    if existing != &group { existing.clone_from(&group); }
                    *used = now;
                } else {
                    const MAX_SESSIONS: usize = 4096;
                    if self.sessions.len() >= MAX_SESSIONS {
                        if let Some(oldest) = self.sessions.iter().min_by_key(|(_, (_, used))| *used).map(|(key, _)| key.clone()) {
                            self.sessions.remove(&oldest);
                        }
                    }
                    self.sessions.insert(session.to_owned(), (group.clone(), now));
                }
                Some(group)
            }
            Some(_) => { self.sessions.remove(session); None }
        }
    }
}

impl OmpIngress {
    pub fn cache_session(&self, account: &str, model: &str, tier: Option<&str>) -> Option<String> {
        let group = self.cache_group.as_deref()?;
        let name = serde_json::to_vec(&("codexpress-omp-cache-v1", &self.namespace, account, model, tier, group)).expect("strings");
        Some(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, &name).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_affinity_separates_accounts_models_tiers_groups_and_clients() {
        let ingress = OmpIngress { namespace: "client-a".into(), normalized: true, cache_group: Some("a".repeat(64)) };
        let baseline = ingress.cache_session("account-a", "model-a", None).unwrap();
        assert_eq!(uuid::Uuid::parse_str(&baseline).unwrap().get_version_num(), 5);
        let restarted = OmpIngress { normalized: false, ..ingress.clone() };
        assert_eq!(restarted.cache_session("account-a", "model-a", None).as_deref(), Some(baseline.as_str()));
        let other_client = OmpIngress { namespace: "client-b".into(), ..ingress.clone() };
        let other_group = OmpIngress { cache_group: Some("b".repeat(64)), ..ingress.clone() };
        let affinities = [
            baseline,
            ingress.cache_session("account-b", "model-a", None).unwrap(),
            ingress.cache_session("account-a", "model-b", None).unwrap(),
            ingress.cache_session("account-a", "model-a", Some("priority")).unwrap(),
            other_client.cache_session("account-a", "model-a", None).unwrap(),
            other_group.cache_session("account-a", "model-a", None).unwrap(),
        ];
        assert_eq!(affinities.into_iter().collect::<std::collections::HashSet<_>>().len(), 6);
        let disabled = OmpIngress { cache_group: None, ..ingress };
        assert_eq!(disabled.cache_session("account-a", "model-a", None), None);
    }

    #[test]
    fn only_compaction_inherits_a_real_sessions_cache_group_and_opt_out_clears_it() {
        let mut groups = CacheGroups::default();
        let shared = "a".repeat(64);
        assert_eq!(groups.resolve(None, Some(json!(shared)), false), None);
        groups.resolve(Some("child-a"), Some(json!(shared)), false);
        groups.resolve(Some("child-b"), Some(json!(shared)), false);
        assert_eq!(groups.resolve(Some("child-a"), None, false), None);
        assert_eq!(groups.resolve(Some("unknown-child"), None, true), None);
        assert_eq!(groups.resolve(Some("child-a"), None, true).as_deref(), Some(shared.as_str()));
        groups.resolve(Some("child-a"), Some(Value::Null), false);
        assert_eq!(groups.resolve(Some("child-a"), None, true), None);
        assert_eq!(groups.resolve(Some("child-b"), None, true).as_deref(), Some(shared.as_str()));
        let replacement = "b".repeat(64);
        groups.resolve(Some("child-b"), Some(json!(replacement)), false);
        assert_eq!(groups.resolve(Some("child-b"), None, true).as_deref(), Some(replacement.as_str()));
        groups.resolve(Some("child-b"), Some(json!("invalid-group")), true);
        assert_eq!(groups.resolve(Some("child-b"), None, true), None);
    }

    #[test]
    fn only_explicit_unsupported_marks_actual_source_identity() {
        let source: crate::model::Source = serde_json::from_value(json!({
            "id":"capability-test","name":"测试","kind":"api","base_url":"http://127.0.0.1:1",
            "enabled":true,"priority":0,"proxy":{"mode":"direct"},"secret":{"api_key":"first"}
        })).unwrap();
        for status in [StatusCode::TOO_MANY_REQUESTS, StatusCode::SERVICE_UNAVAILABLE] {
            observe_compaction_rejection(&source, status, &json!({"error":{"code":"unsupported_compaction"}}));
            assert_eq!(compaction_capability(&source), None);
        }
        observe_compaction_rejection(&source, StatusCode::BAD_REQUEST, &json!({"error":{"code":"invalid_request"}}));
        assert_eq!(compaction_capability(&source), None);
        observe_compaction_rejection(&source, StatusCode::BAD_REQUEST, &json!({"error":{"code":"unsupported_parameter","param":"compaction_trigger"}}));
        assert_eq!(compaction_capability(&source), Some(false));
        let mut changed = source.clone();
        changed.secret = json!({"api_key":"second"});
        assert_eq!(compaction_capability(&changed), None);
    }
}
