mod request;
mod response;
mod retries;
mod roles;
pub(crate) use roles::RoleCapabilities;

use self::{
    request::{clear_body_headers, retry_request, Payload},
    response::{probe_context_rejection, read_error_prefix, response_has_output, response_is_sse, stream_response, stream_translated_response, Attempt},
};
use crate::{
    model::{AppState, Source},
    routing, transport,
    usage::content_units,
};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use bytes::Bytes;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{atomic::Ordering, Arc},
};

const RECOVERY_WAIT_SECONDS: u64 = 20;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/responses", post(forward))
        .route("/v1/responses/compact", post(forward))
        .route("/v1/models", get(forward))
        .with_state(state)
}

fn error(status: StatusCode, message: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"error":{"message":message,"type":"gateway_error"}}).to_string(),
        ))
        .unwrap()
}

fn recovery_timeout(retry_after: u64) -> Response<Body> {
    let mut response = (StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!({"error": {
        "code": "upstream_unavailable", "type": "server_error",
        "message": "来源正在等待恢复或可用名额，请稍后重试。"
    }}))).into_response();
    response.headers_mut().insert(
        "retry-after",
        HeaderValue::from_str(&retry_after.max(1).to_string()).expect("seconds"),
    );
    response
}

async fn quota_unavailable(state: &Arc<AppState>, model: &str, saw_quota: bool) -> Option<Response<Body>> {
    let settings = state.settings.read().await.clone();
    let policies = state.model_policies.read().await;
    let sources = state.sources.read().await;
    let matching = sources.iter().filter(|runtime| {
        let source = &runtime.source;
        source.enabled
            && crate::model_policy::source_allowed(&policies, model, &source.id)
            && (model.is_empty() || source.models.is_empty() || source.models.iter().any(|m| m == model))
            && (settings.mode != "fixed" || settings.fixed_source_id.as_ref() == Some(&source.id))
    });
    let mut had_quota = saw_quota;
    let mut all_exhausted = true;
    let mut matched = false;
    for runtime in matching {
        let unconfirmed = state.quota_recovery.unconfirmed(&runtime.source);
        let exhausted = routing::official_exhausted(runtime, &sources)
            || (runtime.source.kind != "official" && runtime.health == "exhausted"
                && runtime.cooldown_until.is_none_or(|until| until > crate::model::now()));
        matched = true;
        had_quota |= exhausted || unconfirmed;
        all_exhausted &= exhausted && !unconfirmed;
    }
    if state.quota_recovery.try_admit().is_some() && matched && all_exhausted {
        return Some((StatusCode::TOO_MANY_REQUESTS, axum::Json(json!({"error": {
            "code": "all_sources_quota_exhausted", "type": "gateway_error",
            "message": "所有支持此模型的已配置来源额度均已耗尽。"
        }}))).into_response());
    }
    had_quota.then(|| recovery_timeout(1))
}

fn sensitive(name: &str) -> bool {
    matches!(
        name,
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "set-cookie"
            | "x-api-key"
            | "api-key"
            | "x-account-id"
            | "x-organization-id"
            | "x-user-id"
            | "x-workspace-id"
            | "openai-organization"
            | "openai-project"
            | "openai-account-id"
            | "x-openai-organization"
            | "x-openai-project"
            | "x-openai-account-id"
    ) || name.starts_with("chatgpt-")
}
fn clean_headers(headers: &HeaderMap, request: bool) -> HeaderMap {
    let mut excluded: HashSet<String> = [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "host",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    for value in headers.get_all("connection") {
        if let Ok(value) = value.to_str() {
            excluded.extend(value.split(',').map(|v| v.trim().to_ascii_lowercase()));
        }
    }
    let mut clean = HeaderMap::new();
    for (key, value) in headers {
        if !excluded.contains(key.as_str())
            && (!request || !sensitive(key.as_str()))
            && (request
                || !matches!(
                    key.as_str(),
                    "set-cookie" | "authorization" | "proxy-authorization"
                ))
        {
            clean.append(key.clone(), value.clone());
        }
    }
    clean
}
fn target(source: &Source, path: &str, query: Option<&str>) -> anyhow::Result<url::Url> {
    let mut url = url::Url::parse(&source.base_url)?;
    let suffix = path.strip_prefix("/v1").unwrap_or(path);
    let base = url.path().trim_end_matches('/');
    // A base ending in /responses is accepted for Responses endpoints without
    // accidentally producing /responses/responses.
    let base = base.strip_suffix("/responses").unwrap_or(base);
    url.set_path(&format!("{base}{suffix}"));
    url.set_query(query);
    Ok(url)
}

fn opaque_bindings(value: &Value, keys: &mut Vec<String>) {
    use sha2::{Digest, Sha256};
    match value {
        Value::Object(object) => {
            if let Some(Value::String(encrypted)) = object.get("encrypted_content") {
                keys.push(format!(
                    "encrypted:{:x}",
                    Sha256::digest(encrypted.as_bytes())
                ));
            }
            for (name, value) in object {
                if matches!(
                    name.as_str(),
                    "response" | "input" | "output" | "item" | "content"
                ) && !(name == "output"
                    && object.get("type").and_then(Value::as_str) == Some("function_call_output"))
                {
                    opaque_bindings(value, keys);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                opaque_bindings(value, keys);
            }
        }
        _ => {}
    }
}

fn turn_binding(token: &str) -> String {
    use sha2::{Digest, Sha256};
    // Upstream tokens have no response-ID length constraint. Keep only their
    // digest in memory while forwarding the original header/event unchanged.
    format!("turn:{:x}", Sha256::digest(token.as_bytes()))
}

fn session_binding(parsed: &Value, headers: &HeaderMap) -> Option<String> {
    // Current Codex can share a session-id across threads/subagents. The more
    // specific thread-id must win, and its namespace must remain separate.
    let (namespace, id) = if let Some(thread) = headers
        .get("thread-id")
        .or_else(|| headers.get("thread_id"))
    {
        ("thread", thread.to_str().ok()?)
    } else {
        let id = parsed
            .get("session_id")
            .or_else(|| parsed.get("conversation_id"))
            .and_then(Value::as_str)
            .or_else(|| {
                headers
                    .get("session-id")
                    .or_else(|| headers.get("x-session-id"))
                    .or_else(|| headers.get("session_id"))
                    .and_then(|v| v.to_str().ok())
            })?;
        ("session", id)
    };
    (!id.is_empty() && id.len() <= 512).then(|| format!("{namespace}:{id}"))
}

#[derive(Clone)]
struct PreservedScopedSession(String);

#[derive(Clone)]
struct RecoveryWindow {
    deadline: tokio::time::Instant,
    attempted: bool,
    quota_deadline: Option<tokio::time::Instant>,
    quota_retry: Option<Source>,
}

impl RecoveryWindow {
    fn quota_deadline(&mut self) -> tokio::time::Instant {
        *self.quota_deadline.get_or_insert_with(|| {
            tokio::time::Instant::now() + std::time::Duration::from_secs(RECOVERY_WAIT_SECONDS)
        })
    }

    fn retries_quota(&self, source: &Source) -> bool {
        self.quota_retry.as_ref().is_some_and(|target| {
            target.id == source.id && target.identity() == source.identity()
        })
    }
}

#[derive(Clone)]
struct ManualRequest {
    trial: Option<routing::manual::Trial>,
    attempted: bool,
}
impl ManualRequest {
    fn abandon(&self, state: &AppState) {
        if let Some(trial) = &self.trial {
            state.manual_routing.stop(trial);
        }
    }
}

fn state_matches_source(
    bindings: &std::collections::HashMap<String, String>,
    key: &str,
    source: &Source,
    accepted: &HashSet<String>,
) -> bool {
    bindings.get(key).is_none_or(|id| id == &source.id)
        || accepted.contains(&crate::continuity::accepted_state(&source.identity(), key))
}

fn scoped_session(parsed: &Value, parts: &axum::http::request::Parts) -> Option<String> {
    if let Some(session) = parts.extensions.get::<PreservedScopedSession>() {
        // This value already includes any ingress namespace. Reconstruction
        // removed wire state, not the identity of the local conversation.
        return Some(session.0.clone());
    }
    if let Some(ingress) = parts.extensions.get::<crate::omp_ingress::OmpIngress>() {
        return session_binding(parsed, &parts.headers)
            .map(|session| format!("omp:{}:{session}", ingress.namespace));
    }
    let session = session_binding(parsed, &parts.headers);
    match parts.extensions.get::<crate::remote::RemoteIngress>() {
        Some(ingress) => session
            .or_else(|| {
                parsed["prompt_cache_key"]
                    .as_str()
                    .map(|s| format!("cache:{s}"))
            })
            .map(|s| format!("remote:{}:{s}", ingress.peer_id)),
        None => match parts.extensions.get::<crate::chatbot::ChatbotIngress>() {
            Some(ingress) => session
                .or_else(|| {
                    parsed["prompt_cache_key"]
                        .as_str()
                        .map(|s| format!("cache:{s}"))
                })
                .map(|s| format!("chatbot:{}:{s}", ingress.namespace)),
            None => session,
        },
    }
}

pub(crate) async fn forward(
    State(state): State<Arc<AppState>>,
    mut request: Request<Body>,
) -> Response<Body> {
    request.extensions_mut().insert(RecoveryWindow {
        deadline: tokio::time::Instant::now()
            + std::time::Duration::from_secs(RECOVERY_WAIT_SECONDS),
        attempted: false,
        quota_deadline: None,
        quota_retry: None,
    });
    forward_once(state, request, None, 0).await
}

fn explicit_context_rejection(value: &Value) -> bool {
    let problem = value
        .pointer("/response/error")
        .or_else(|| value.get("error"))
        .unwrap_or(value);
    if matches!(
        problem
            .get("code")
            .and_then(Value::as_str)
            .or_else(|| problem.get("type").and_then(Value::as_str)),
        Some(
            "invalid_encrypted_content"
                | "invalid_previous_response_id"
                | "previous_response_not_found"
                | "response_not_found"
                | "conversation_not_found"
        )
    ) {
        return true;
    }
    let Some(message) = problem
        .get("message")
        .or_else(|| problem.get("detail"))
        .and_then(Value::as_str)
        .or_else(|| problem.as_str())
    else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    let missing = [
        "not found",
        "does not exist",
        "could not be found",
        "cannot be found",
    ]
    .iter()
    .any(|phrase| message.contains(phrase));
    let response_id = message.contains("previous_response_id")
        || message.contains("previous response")
        || message.contains("no response found with id");
    let conversation_id = message.contains("conversation_id")
        || message.contains("conversation id")
        || message.contains("conversation with id")
        || message.contains("conversation not found")
        || message.contains("no conversation found with id");
    let encrypted = message.contains("encrypted_content")
        || message.contains("encrypted content")
        || message.contains("compaction blob");
    (missing && (response_id || conversation_id))
        || message.contains("no response found with id")
        || message.contains("no conversation found with id")
        || (encrypted
            && [
                "could not be verified",
                "was not issued to this caller",
                "could not decrypt",
                "could not decode",
                "invalid encrypted",
                "encrypted_content is invalid",
            ]
            .iter()
            .any(|phrase| message.contains(phrase)))
}

fn strip_opaque_state(value: &mut Value) {
    if let Some(items) = value.get_mut("input").and_then(Value::as_array_mut) {
        items.retain(|item| {
            !matches!(item["type"].as_str(), Some("reasoning" | "compaction"))
                || item.get("encrypted_content").is_none()
        });
        for item in items {
            if let Some(fields) = item.as_object_mut() {
                fields.remove("encrypted_content");
            }
        }
    }
}

async fn portable_request(
    state: &Arc<AppState>,
    parts: &axum::http::request::Parts,
    parsed: &Value,
    affinity: Option<&str>,
) -> Result<(Request<Body>, RebuildContext), Box<Response<Body>>> {
    let mut excluded = crate::continuity::keys(
        parsed,
        parts
            .headers
            .get("x-codex-turn-state")
            .and_then(|v| v.to_str().ok()),
    );
    excluded.insert("rebuilt".into());
    let mut portable = parsed.clone();
    let scope = history_scope(parts, parsed);
    if let Some(previous) = parsed["previous_response_id"]
        .as_str()
        .filter(|_| state.history.enabled())
    {
        portable["input"] = match cached_input(state, &scope, previous, parsed).await {
            Ok(input) => input,
            Err((status, message)) => return Err(Box::new(error(status, message))),
        };
    } else if state.history.enabled() && parsed.get("conversation").is_some_and(|v| !v.is_null()) {
        return Err(Box::new(error(
            StatusCode::CONFLICT,
            "此上游会话未保存完整本地历史，请让客户端发送完整上下文。",
        )));
    }
    excluded.extend(crate::continuity::keys(&portable, None));
    let mut protected = Vec::new();
    opaque_bindings(&portable, &mut protected);
    protected.extend(parsed["previous_response_id"].as_str().map(str::to_owned));
    protected.extend(
        parsed["conversation"]
            .as_str()
            .or_else(|| parsed["conversation"]["id"].as_str())
            .map(str::to_owned),
    );
    protected.extend(
        parts
            .headers
            .get("x-codex-turn-state")
            .and_then(|v| v.to_str().ok())
            .map(turn_binding),
    );
    strip_opaque_state(&mut portable);
    for field in ["previous_response_id", "conversation", "conversation_id"] {
        portable.as_object_mut().expect("object").remove(field);
    }
    let mut retry = retry_request(
        parts,
        Bytes::from(serde_json::to_vec(&portable).expect("JSON")),
    );
    let headers = retry.headers_mut();
    headers.remove("x-codex-turn-state");
    clear_body_headers(headers);
    if parts
        .extensions
        .get::<crate::remote::RemoteIngress>()
        .is_none()
    {
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {}", state.gateway_key)).expect("local key"),
        );
    }
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    if let Some(session) = scoped_session(parsed, parts) {
        retry
            .extensions_mut()
            .insert(PreservedScopedSession(session));
    }
    Ok((
        retry,
        RebuildContext {
            key: affinity.map(str::to_owned),
            excluded,
            protected_bindings: protected.into_iter().collect(),
            scope,
            route: None,
            rejected_source: None,
        },
    ))
}

struct RebuildContext {
    key: Option<String>,
    excluded: HashSet<String>,
    protected_bindings: HashSet<String>,
    scope: String,
    route: Option<(Source, crate::affinity::RouteLease)>,
    rejected_source: Option<String>,
}

#[derive(Clone)]
struct HistoryCapture {
    scope: String,
    request: Value,
    generation: u64,
    metadata: Option<crate::history::CaptureMetadata>,
}
struct ResponseContext {
    discovery: bool,
    session_key: Option<String>,
    request_is_stream: bool,
    rebuild: Option<RebuildContext>,
    history: Option<HistoryCapture>,
    accepted_state: Option<ManualAcceptance>,
}
struct ManualAcceptance {
    key: Option<String>,
    hashes: HashSet<String>,
    protected_bindings: HashSet<String>,
    restored: HashSet<String>,
}

fn history_scope(parts: &axum::http::request::Parts, parsed: &Value) -> String {
    if let Some(ingress) = parts.extensions.get::<crate::omp_ingress::OmpIngress>() {
        return crate::continuity::digest("omp-history", &serde_json::to_string(
            &(&ingress.namespace, scoped_session(parsed, parts))
        ).expect("strings"));
    }
    let session = scoped_session(parsed, parts).or_else(|| {
        parsed["prompt_cache_key"]
            .as_str()
            .map(|s| format!("cache:{s}"))
    });
    let credential = parts
        .extensions
        .get::<crate::chatbot::ChatbotIngress>()
        .map(|ingress| ingress.namespace.as_str())
        .unwrap_or_else(|| {
            parts
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
        });
    crate::continuity::digest(
        "history",
        &serde_json::to_string(&(credential, session)).expect("strings"),
    )
}

async fn cached_input(
    state: &Arc<AppState>,
    scope: &str,
    previous: &str,
    parsed: &Value,
) -> Result<Value, (StatusCode, &'static str)> {
    let history = state.history.clone();
    let scope = scope.to_owned();
    let previous = previous.to_owned();
    let mut items =
        match tokio::task::spawn_blocking(move || history.restore(&scope, &previous)).await {
            Ok(Ok(items)) => items,
            _ => {
                return Err((
                    StatusCode::CONFLICT,
                    "本地历史不完整：所需增量块已淘汰或未采集。请让客户端发送完整上下文后重试。",
                ))
            }
        };
    match parsed.get("input") {
        Some(Value::Array(input)) => items.extend(input.iter().cloned()),
        Some(Value::String(text)) => items.push(json!({"role":"user","content":text})),
        _ => return Err((StatusCode::BAD_REQUEST, "无法重建此输入格式")),
    }
    Ok(Value::Array(items))
}

async fn after_manual_failure(
    state: Arc<AppState>,
    mut parts: axum::http::request::Parts,
    bytes: Bytes,
    rebuild: Option<RebuildContext>,
    replans: u8,
    stop_takeover: bool,
) -> Response<Body> {
    if let Some(manual) = parts.extensions.get_mut::<ManualRequest>() {
        manual.attempted = true;
        if stop_takeover {
            manual.abandon(&state);
        }
    }
    if !stop_takeover {
        let sources = state.sources.read().await;
        state.manual_routing.refresh(&sources, &state.affinity);
    }
    Box::pin(forward_once(
        state,
        Request::from_parts(parts, Body::from(bytes)),
        rebuild,
        replans,
    ))
    .await
}

async fn forward_once(
    state: Arc<AppState>,
    request: Request<Body>,
    mut rebuild: Option<RebuildContext>,
    replans: u8,
) -> Response<Body> {
    let selected_revision = state.routing_revision.load(Ordering::Acquire);
    if selected_revision % 2 != 0 {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "来源设置正在更新，请稍后重试",
        );
    }
    let degraded = rebuild.is_some();
    let mut prepared_route = rebuild.as_mut().and_then(|context| context.route.take());
    let planned_return = prepared_route.is_some();
    let remote_ingress = request.extensions().get::<crate::remote::RemoteIngress>();
    let omp_ingress = request.extensions().get::<crate::omp_ingress::OmpIngress>().is_some();
    let remote_peer = state.remote.peer(request.headers());
    if remote_ingress.is_some_and(|ingress| remote_peer.as_ref() != Some(&ingress.peer_id)) {
        return error(StatusCode::UNAUTHORIZED, "远端网关认证失败");
    }
    let credential = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let origin = match credential {
        Some(_) if remote_ingress.is_some() => None,
        Some(credential) if credential == state.gateway_key => None,
        Some(credential) => match crate::config::confirmed_origin(&state.state_dir, credential) {
            Ok(Some(origin)) => Some(origin),
            _ => return error(StatusCode::UNAUTHORIZED, "本地网关认证失败"),
        },
        None => return error(StatusCode::UNAUTHORIZED, "本地网关认证失败"),
    };
    if origin
        .as_ref()
        .is_some_and(|origin| origin.source_identity.is_empty())
    {
        return error(StatusCode::UNAUTHORIZED, "本地网关认证失败");
    }
    let host = request.headers().get_all("host");
    let mut hosts = host.iter();
    let expected_ip = format!("127.0.0.1:{}", state.port);
    let expected_local = format!("localhost:{}", state.port);
    let valid_host = hosts
        .next()
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case(&expected_local) || v == expected_ip)
        && hosts.next().is_none();
    if !valid_host && remote_ingress.is_none() && !omp_ingress {
        return error(
            StatusCode::MISDIRECTED_REQUEST,
            "网关只接受匹配端口的本机 Host",
        );
    }
    if request.headers().contains_key("origin") {
        return error(StatusCode::FORBIDDEN, "网关不接受浏览器跨站请求");
    }
    let (mut parts, body) = request.into_parts();
    let request_retries = parts
        .extensions
        .get::<retries::RequestRetries>()
        .cloned()
        .unwrap_or_default();
    parts.extensions.insert(request_retries.clone());
    let ingress = crate::usage::Ingress::from_extensions(&parts.extensions);
    let mut bytes = match crate::request_body::read(body, ingress.as_str()).await {
        Ok(body) => body,
        Err(problem) => return error(problem.status, problem.message),
    };
    let discovery = parts.method == axum::http::Method::GET;
    let mut parsed = if discovery {
        json!({})
    } else {
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) if value.is_object() => value,
            _ => return error(StatusCode::BAD_REQUEST, "Responses 请求需要 JSON 对象"),
        }
    };
    let client_kind = crate::model_catalog::client(&parts);
    if discovery {
        return match crate::model_catalog::load(&state, client_kind).await.and_then(|catalog| catalog.codex()) {
            Ok(catalog) => axum::Json(catalog).into_response(),
            Err(_) => error(StatusCode::SERVICE_UNAVAILABLE, "无法获取来源模型列表，请先在来源页刷新或手动填写"),
        };
    }
    let native_compaction =
        parts.uri.path() == "/v1/responses/compact" || crate::chatbot::is_compaction_v2(&parsed);
    if let Some(ingress) = parts.extensions.get::<crate::omp_ingress::OmpIngress>().filter(|ingress| !ingress.normalized) {
        let private = parsed.get_mut("client_metadata").and_then(Value::as_object_mut)
            .and_then(|metadata| metadata.remove("codexpress_session_id"));
        let cache_hint = parsed.get_mut("client_metadata").and_then(Value::as_object_mut)
            .and_then(|metadata| metadata.remove("codexpress_cache_group"));
        let canonical = parsed["client_metadata"]["session_id"].as_str();
        let identity = crate::omp_ingress::session_identity(&ingress.namespace, &parts.headers,
            private.as_ref().and_then(Value::as_str).or(canonical));
        let had_cache_hint = cache_hint.is_some();
        let cache_group = if had_cache_hint || native_compaction {
            state.omp_cache_groups.lock().await.resolve(identity.as_deref(), cache_hint, native_compaction)
        } else { None };
        if let Some(identity) = identity {
            parts.headers.remove("thread_id");
            parts.headers.insert("thread-id", HeaderValue::from_str(&identity).expect("session digest"));
        }
        if private.is_some() || had_cache_hint {
            bytes = Bytes::from(serde_json::to_vec(&parsed).expect("JSON"));
            clear_body_headers(&mut parts.headers);
        }
        let ingress = parts.extensions.get_mut::<crate::omp_ingress::OmpIngress>().expect("OMP ingress");
        ingress.cache_group = cache_group;
        ingress.normalized = true;
    }
    if parts.extensions.get::<crate::omp_ingress::OmpIngress>().is_some()
        && parts.uri.path() == "/v1/responses"
        && crate::images::native::is_request(&parsed)
    {
        return crate::images::native::handle(state, parsed, parts).await;
    }
    if let Some(problem) = crate::model_catalog::check_client(&state, parsed["model"].as_str().unwrap_or(""), client_kind).await {
        return problem;
    }
    let route_epoch = loop {
        if let Some(epoch) = state.quota_recovery.try_admit() { break epoch; }
        drop(prepared_route.take());
        let deadline = parts.extensions.get_mut::<RecoveryWindow>().expect("request recovery window").quota_deadline();
        if tokio::time::timeout_at(deadline, state.quota_recovery.wait_idle()).await.is_err() {
            return recovery_timeout(1);
        }
    };
    // Each client request can join the same takeover. Internal replans retain
    // their marker and cannot pick up a newer operation or retry a failed probe.
    let mut manual = match parts.extensions.get::<ManualRequest>().cloned() {
        Some(manual) => manual,
        None => ManualRequest {
            trial: if !discovery && !native_compaction && parts.uri.path() == "/v1/responses" {
                routing::manual::claim(&state, parsed["model"].as_str().unwrap_or("")).await
            } else {
                None
            },
            attempted: false,
        },
    };
    let manual_target = if !manual.attempted {
        match &manual.trial {
            Some(trial) => {
                routing::manual::source_for_trial(
                    &state,
                    trial,
                    parsed["model"].as_str().unwrap_or(""),
                )
                .await
            }
            None => None,
        }
    } else {
        None
    };
    parts.extensions.insert(manual.clone());
    let route_settings = state.settings.read().await.clone();
    let custom_sources = crate::model_policy::has_custom_sources(
        &state.model_policies.read().await, parsed["model"].as_str().unwrap_or(""));
    let priority_probe = !discovery
        && !native_compaction
        && parts.uri.path() == "/v1/responses"
        && route_settings.strict_priority
        && route_settings.mode == "auto"
        && (!route_settings.load_balance || custom_sources);
    let stored_key = scoped_session(&parsed, &parts)
        .or_else(|| {
            if omp_ingress { return None; }
            parsed["prompt_cache_key"].as_str().map(|s| format!("cache:{s}"))
        })
        .map(|session| crate::affinity::key(&session, parsed["model"].as_str().unwrap_or("")));
    let exclusions = stored_key
        .as_deref()
        .map(|key| state.affinity.exclusions(key))
        .unwrap_or_default();
    let previously_rebuilt = !exclusions.is_empty();
    if previously_rebuilt && manual_target.is_none() && !(priority_probe && !degraded) {
        if let Some(previous) = parsed["previous_response_id"].as_str().filter(|id| {
            state.history.enabled()
                && exclusions.contains(&crate::continuity::digest("previous_response_id", id))
        }) {
            parsed["input"] = match cached_input(
                &state,
                &history_scope(&parts, &parsed),
                previous,
                &parsed,
            )
            .await
            {
                Ok(input) => input,
                Err((status, message)) => return error(status, message),
            };
        }
        let (changed, remove_turn) = crate::continuity::apply(
            &mut parsed,
            parts
                .headers
                .get("x-codex-turn-state")
                .and_then(|v| v.to_str().ok()),
            &exclusions,
        );
        if remove_turn {
            parts.headers.remove("x-codex-turn-state");
        }
        if changed {
            bytes = Bytes::from(serde_json::to_vec(&parsed).expect("JSON"));
            clear_body_headers(&mut parts.headers);
        }
    }
    let model = parsed.get("model").and_then(Value::as_str).unwrap_or("");
    let mut opaque_keys = Vec::new();
    opaque_bindings(&parsed, &mut opaque_keys);
    let session_key = scoped_session(&parsed, &parts);
    let session_known = if let Some(key) = &session_key {
        state.bindings.lock().await.contains_key(key)
    } else {
        false
    };
    let opaque_state = !opaque_keys.is_empty();
    let turn_key = parts
        .headers
        .get("x-codex-turn-state")
        .and_then(|v| v.to_str().ok())
        .map(turn_binding);
    // Ordinary session IDs describe portable client history. They constrain
    // routing only when opaque upstream context needs an origin clue.
    let binding = parsed
        .get("previous_response_id")
        .and_then(Value::as_str)
        .or_else(|| {
            parsed
                .get("conversation")
                .and_then(|v| v.as_str().or_else(|| v.get("id").and_then(Value::as_str)))
        })
        .or(turn_key.as_deref())
        .or_else(|| {
            session_key
                .as_deref()
                .filter(|_| opaque_state && session_known)
        })
        .or_else(|| opaque_keys.first().map(String::as_str));
    if parsed
        .get("previous_response_id")
        .is_some_and(|v| !v.is_null() && !v.is_string())
    {
        return error(StatusCode::BAD_REQUEST, "previous_response_id 格式无效");
    }
    let affinity_session = session_key.clone().or_else(|| {
        if parts.extensions.get::<crate::omp_ingress::OmpIngress>().is_some() {
            return None;
        }
        parsed.get("prompt_cache_key").and_then(Value::as_str)
            .filter(|key| !key.is_empty() && key.len() <= 4096)
            .map(|key| format!("cache:{key}"))
    });
    let affinity_key = affinity_session
        .as_deref()
        .map(|session| crate::affinity::key(session, model));
    // Reconsider priority only at an idle Responses turn boundary. Compact and
    // tool-result continuations keep their current upstream state/cache.
    let tool_result = parsed["input"]
        .as_array()
        .and_then(|items| {
            items.iter().rev().find(|item| {
                item["role"] == "user"
                    || item["role"] == "tool"
                    || matches!(
                        item["type"].as_str(),
                        Some("function_call_output" | "custom_tool_call_output")
                    )
            })
        })
        .is_some_and(|item| {
            matches!(
                item["type"].as_str(),
                Some("function_call_output" | "custom_tool_call_output")
            ) || item["role"] == "tool"
        });
    if !degraded
        && !discovery
        && !native_compaction
        && parts.uri.path() == "/v1/responses"
        && !tool_result
        && manual_target.is_none()
        && !priority_probe
        && !request_retries.saw_quota()
    {
        if let Some(key) = affinity_key.as_deref() {
            let has_state = binding.is_some() || opaque_state;
            if !has_state || state.settings.read().await.degraded_context_fallback {
                if let Some(route) = routing::reserve_idle_return(&state, model, key).await {
                    let prepared = if has_state {
                        // A missing cache chain must leave the working route in
                        // place, rather than submit an incomplete conversation.
                        portable_request(&state, &parts, &parsed, Some(key))
                            .await
                            .ok()
                    } else {
                        let retry = retry_request(&parts, bytes.clone());
                        Some((
                            retry,
                            RebuildContext {
                                key: Some(key.to_owned()),
                                excluded: HashSet::from(["rebuilt".into()]),
                                protected_bindings: HashSet::new(),
                                scope: history_scope(&parts, &parsed),
                                route: None,
                                rejected_source: None,
                            },
                        ))
                    };
                    if let Some((retry, mut context)) = prepared {
                        context.route = Some(route);
                        return Box::pin(forward_once(state, retry, Some(context), replans)).await;
                    }
                }
            }
        }
    }
    let mut hard_keys: Vec<&str> = opaque_keys.iter().map(String::as_str).collect();
    hard_keys.extend(parsed.get("previous_response_id").and_then(Value::as_str));
    hard_keys.extend(
        parsed
            .get("conversation")
            .and_then(|v| v.as_str().or_else(|| v.get("id").and_then(Value::as_str))),
    );
    hard_keys.extend(turn_key.as_deref());
    if opaque_state && session_known {
        hard_keys.extend(session_key.as_deref());
    }
    let has_upstream_id = parsed["previous_response_id"].is_string()
        || parsed
            .get("conversation")
            .and_then(|v| v.as_str().or_else(|| v.get("id").and_then(Value::as_str)))
            .is_some();
    let routing_binding = {
        let bindings = state.bindings.lock().await;
        // Missing local metadata is not evidence that an upstream ID expired.
        // Prefer any confirmed state/thread origin, then affinity/priority.
        hard_keys
            .iter()
            .find(|key| bindings.contains_key(**key))
            .copied()
            .or_else(|| {
                session_key
                    .as_deref()
                    .filter(|key| has_upstream_id && bindings.contains_key(*key))
            })
            // Opaque-only requests still require a known origin. An external
            // response/conversation ID can instead be checked by the upstream.
            .or_else(|| (!has_upstream_id).then_some(binding).flatten())
            .map(str::to_owned)
    };
    // Prior migrations constrain unchanged upstream state. Once this request
    // has safely rebuilt its context, that old target must not lock it there.
    let migrated_source = if previously_rebuilt && !degraded {
        let runtimes = state.sources.read().await;
        let all: Vec<_> = runtimes.iter().map(|r| r.source.clone()).collect();
        stored_key
            .as_deref()
            .and_then(|key| state.affinity.preferred(key, &all))
    } else {
        None
    };
    let probe_unknown_id = !degraded
        && has_upstream_id
        && !opaque_state
        && turn_key.is_none()
        && origin.is_none()
        && migrated_source.is_none()
        && {
            let bindings = state.bindings.lock().await;
            hard_keys.iter().all(|key| !bindings.contains_key(*key))
        };
    let saw_quota = request_retries.saw_quota();
    let window = parts.extensions.get::<RecoveryWindow>().expect("request recovery window");
    let retry_quota = |source: &Source| window.retries_quota(source);
    let mut sources = if let Some((source, _)) = &prepared_route {
        let mut candidates = vec![source.clone()];
        candidates.extend(
            routing::candidates_with_session(&state, model, None, affinity_key.as_deref())
                .await
                .into_iter()
                .filter(|candidate| candidate.id != source.id),
        );
        candidates
    } else if let Some(origin) = &origin {
        // A scoped legacy-provider credential explicitly confirms the original
        // account/key. It can carry old opaque state only to that exact identity.
        let all =
            routing::candidates_with_session(&state, model, None, affinity_key.as_deref()).await;
        all.into_iter()
            .filter(|source| {
                migrated_source.as_ref().is_some_and(|id| id == &source.id)
                    || (migrated_source.is_none()
                        && source.id == origin.source_id
                        && source.identity() == origin.source_identity)
            })
            .collect()
    } else if let Some(migrated) = &migrated_source {
        routing::candidates_with_session(&state, model, None, affinity_key.as_deref())
            .await
            .into_iter()
            .filter(|source| source.id == *migrated)
            .collect()
    } else {
        routing::candidates_with_session(
            &state,
            model,
            routing_binding.as_deref(),
            affinity_key.as_deref(),
        )
        .await
    };
    if probe_unknown_id {
        // A thread or affinity is an origin hint for an unobserved upstream ID,
        // not proof of ownership. Probe it first, then at most four alternatives
        // only if each previous candidate explicitly rejects the unchanged ID.
        let alternatives =
            routing::candidates_with_session(&state, model, None, affinity_key.as_deref()).await;
        for alternative in alternatives {
            if !sources.iter().any(|source| source.id == alternative.id) {
                sources.push(alternative);
            }
        }
        sources.truncate(5);
    }
    {
        let bindings = state.bindings.lock().await;
        sources.retain(|source| {
            hard_keys
                .iter()
                .all(|key| state_matches_source(&bindings, key, source, &exclusions))
        });
    }
    if let Some(rejected) = rebuild
        .as_ref()
        .and_then(|context| context.rejected_source.as_deref())
    {
        // A complete reconstruction can use the original source as a final
        // option, but another source should handle the handoff first.
        sources.sort_by_key(|source| source.id == rejected);
    }
    if opaque_state && session_known {
        let bindings = state.bindings.lock().await;
        sources.retain(|source| {
            session_key.as_ref().and_then(|key| bindings.get(key)) == Some(&source.id)
        });
    }
    if priority_probe {
        let priority = routing::priority::candidates(&state, model, affinity_key.as_deref()).await;
        if !saw_quota || (binding.is_none() && origin.is_none()) {
            sources = priority;
        } else if let Some(recovered) = priority.into_iter().find(&retry_quota) {
            // The prior priority probe already used these exact wire bytes.
            if !sources.iter().any(&retry_quota) {
                sources.insert(0, recovered);
            }
        }
    }
    if saw_quota && probe_unknown_id {
        // Quota does not prove that an unknown remote ID belongs elsewhere.
        sources.retain(retry_quota);
    }
    if manual.attempted {
        if let Some(trial) = &manual.trial {
            sources.retain(|source| source.id != trial.source_id);
        }
    }
    if let Some(target) = &manual_target {
        // Explicit local action: let the selected upstream validate unchanged
        // state before rebuilding. Normal alternatives keep all origin checks.
        sources = vec![target.clone()];
    }
    let retry_limit = state.settings.read().await.upstream_attempts;
    let had_candidates = !sources.is_empty();
    sources.retain(|source| retry_quota(source) || !request_retries.exhausted(source, retry_limit));
    sources.sort_by_key(|source| !retry_quota(source));
    if had_candidates && sources.is_empty() && !saw_quota {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "本次请求已用完来源尝试次数，请稍后重试",
        );
    }
    if sources.is_empty() {
        let settings = state.settings.read().await.clone();
        let diagnostics =
            routing::rejection_diagnostics(&state, model, affinity_key.as_deref()).await;
        eprintln!(
            "routing_candidates_empty unix={} rebuilt={} bound={} opaque={} fallback={} cache={} sources={}",
            crate::model::now(),
            degraded,
            binding.is_some(),
            opaque_state,
            settings.degraded_context_fallback,
            state.history.enabled(),
            diagnostics
        );
        let mut restore_error = None;
        if !degraded && !discovery && (binding.is_some() || opaque_state || saw_quota && origin.is_some()) {
            let eligible =
                routing::candidates_with_session(&state, model, None, affinity_key.as_deref())
                    .await.iter().any(|source| !request_retries.exhausted(source, retry_limit));
            if eligible && settings.degraded_context_fallback {
                match portable_request(&state, &parts, &parsed, affinity_key.as_deref()).await {
                    Ok((retry, context)) => {
                        return Box::pin(forward_once(state, retry, Some(context), replans)).await;
                    }
                    Err(response) => {
                        eprintln!(
                            "context_restore_failed unix={} status={}",
                            crate::model::now(),
                            response.status().as_u16()
                        );
                        restore_error = Some(response);
                    }
                }
            }
        }
        // A transient admission failure must not burn the client's retry budget
        // before the upstream has a chance to recover. Preserve the original
        // wire context when rebuilding was unavailable, and share this single
        // bounded window across every internal replan and reconstruction.
        if !discovery {
            if let Some(mut window) = parts
                .extensions
                .get::<RecoveryWindow>()
                .cloned()
                .filter(|w| !w.attempted)
            {
                window.attempted = true;
                parts.extensions.insert(window.clone());
                drop(prepared_route.take());
                eprintln!("routing_recovery_wait unix={}", crate::model::now());
                let recovery_origin = migrated_source
                    .as_deref()
                    .map(|source_id| routing::RecoveryOrigin {
                        source_id,
                        identity: None,
                    })
                    .or_else(|| {
                        origin.as_ref().map(|origin| routing::RecoveryOrigin {
                            source_id: &origin.source_id,
                            identity: Some(&origin.source_identity),
                        })
                    });
                match routing::wait_for_recovery(
                    &state,
                    model,
                    routing_binding.as_deref(),
                    affinity_key.as_deref(),
                    recovery_origin,
                    window.deadline,
                )
                .await
                {
                    routing::RecoveryWait::Ready => {
                        eprintln!("routing_recovery_ready unix={}", crate::model::now());
                        let retry = Request::from_parts(parts, Body::from(bytes));
                        return Box::pin(forward_once(state, retry, rebuild, replans)).await;
                    }
                    routing::RecoveryWait::TimedOut { retry_after } => {
                        eprintln!(
                            "routing_recovery_timeout unix={} retry_after={}",
                            crate::model::now(),
                            retry_after
                        );
                        return recovery_timeout(retry_after);
                    }
                    routing::RecoveryWait::Unavailable => {}
                }
            }
        }
        if let Some(response) = restore_error.filter(|_| !saw_quota) {
            return *response;
        }
        if let Some(response) = quota_unavailable(&state, model, saw_quota).await {
            return response;
        }
        return error(
            if binding.is_some() {
                StatusCode::CONFLICT
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            },
            if binding.is_some() {
                "原会话来源不可用或未找到绑定，请提供完整历史后新建请求"
            } else {
                "没有可用且支持该模型的来源"
            },
        );
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    let input_units = ["input", "instructions", "tools"]
        .iter()
        .map(|key| content_units(&parsed[*key]))
        .sum();
    let history_capture =
        if !discovery && parts.uri.path() == "/v1/responses" && state.history.enabled() {
            let generation = state.history.generation();
            let scope = rebuild
                .as_ref()
                .map(|r| r.scope.clone())
                .unwrap_or_else(|| history_scope(&parts, &parsed));
            let mut request = parsed.clone();
            let mut complete = true;
            if let Some(previous) = parsed["previous_response_id"].as_str() {
                match cached_input(&state, &scope, previous, &parsed).await {
                    Ok(input) => {
                        request["input"] = input;
                        request
                            .as_object_mut()
                            .expect("object")
                            .remove("previous_response_id");
                    }
                    Err(_) => {
                        state.history.record_skip();
                        complete = false;
                    }
                }
            }
            complete.then_some(HistoryCapture {
                scope,
                request,
                generation,
                metadata: None,
            })
        } else {
            None
        };
    let previous_preference = affinity_key
        .as_deref()
        .and_then(|key| state.affinity.preferred(key, &sources));
    let mut last_error = None;
    let source_count = sources.len();
    for (candidate_index, candidate) in sources.into_iter().enumerate() {
        let manual_candidate = manual_target.as_ref().is_some_and(|target| {
            target.id == candidate.id && target.identity() == candidate.identity()
        });
        if candidate.kind == "official"
            && !matches!(parts.uri.path(), "/v1/responses" | "/v1/responses/compact")
        {
            last_error = Some(error(StatusCode::NOT_IMPLEMENTED, "官方来源尚未验证此端点"));
            continue;
        }
        let mut route_lease = if manual_candidate || priority_probe {
            match routing::manual::reserve(&state, &candidate, affinity_key.as_deref(), model).await
            {
                Some(lease) => Some(lease),
                None => {
                    if manual_candidate {
                        return after_manual_failure(state, parts, bytes, rebuild, replans, false)
                            .await;
                    }
                    last_error = Some(error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "来源活跃对话名额已满",
                    ));
                    continue;
                }
            }
        } else if prepared_route.as_ref().is_some_and(|(source, _)| {
            source.id == candidate.id && source.identity() == candidate.identity()
        }) {
            let (_, lease) = prepared_route.take().expect("matched route lease");
            // Reconstruction may await disk work. Honor source removal, model
            // changes, quota updates and fixed-mode changes before submission.
            if !routing::source_is_eligible(&state, &candidate, model).await {
                drop(lease);
                continue;
            }
            Some(lease)
        } else if discovery {
            None
        } else {
            let reserved = if last_error.is_some()
                || (saw_quota && !parts.extensions.get::<RecoveryWindow>().expect("request recovery window").retries_quota(&candidate))
                || rebuild
                    .as_ref()
                    .is_some_and(|context| context.rejected_source.is_some())
                || (probe_unknown_id && candidate_index > 0)
            {
                routing::reserve_context_handoff_for_model(
                    &state,
                    &candidate,
                    affinity_key.as_deref(),
                    model,
                )
                .await
            } else {
                routing::reserve_for_model(
                    &state,
                    &candidate,
                    affinity_key.as_deref(),
                    binding.is_some() || origin.is_some(),
                    model,
                )
                .await
            };
            match reserved {
                Some(lease) => Some(lease),
                None => {
                    last_error = Some(error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "来源正为已有对话保留余量，暂无可分配来源",
                    ));
                    continue;
                }
            }
        };
        if omp_ingress && parts.uri.path() == "/v1/responses"
            && crate::chatbot::is_compaction_v2(&parsed)
            && crate::omp_ingress::compaction_capability(&candidate, parts.extensions.get::<crate::remote::PublicDevice>().is_some()) == Some(false)
        {
            let mut response = (StatusCode::BAD_REQUEST, axum::Json(json!({"error":{
                "code":"unsupported_compaction","type":"invalid_request_error",
                "message":"This source does not support remote compaction V2",
                "param":"compaction_trigger"}}))).into_response();
            for (name, value) in [("x-codexpress-source-id", candidate.id.clone()),
                ("x-codexpress-source-identity", candidate.identity())] {
                if let Ok(value) = HeaderValue::from_str(&value) { response.headers_mut().insert(name, value); }
            }
            return response;
        }
        if candidate.kind == "google"
            && (parts.uri.path() == "/v1/responses/compact" || native_compaction)
        {
            let mut response = (StatusCode::BAD_REQUEST, axum::Json(json!({"error":{
                "code":"unsupported_compaction","type":"invalid_request_error",
                "message":"Google AI Studio 来源不支持原生上下文压缩",
                "param":"compaction_trigger"}}))).into_response();
            for (name, value) in [("x-codexpress-source-id", candidate.id.clone()),
                ("x-codexpress-source-identity", candidate.identity())] {
                if let Ok(value) = HeaderValue::from_str(&value) { response.headers_mut().insert(name, value); }
            }
            return response;
        }
        let mut source = if candidate.kind == "official" {
            match crate::official::ensure_access(&state, &candidate.id).await {
                Ok(source) if source.identity() == candidate.identity() => source,
                Ok(_) => return error(StatusCode::CONFLICT, "来源身份已经变化，请重新开始请求"),
                Err(_) => {
                    routing::mark_auth_preflight_failure(&state, &candidate).await;
                    request_retries.block(&candidate);
                    if manual_candidate {
                        drop(route_lease.take());
                        return after_manual_failure(state, parts, bytes, rebuild, replans, true)
                            .await;
                    }
                    last_error = Some(error(
                        StatusCode::BAD_GATEWAY,
                        "官方来源认证暂不可用，请检查登录或网络",
                    ));
                    continue;
                }
            }
        } else {
            candidate
        };
        // Native V1/V2 compaction must not receive generation-only rewrites.
        let compatibility = parts
            .extensions
            .get::<crate::chatbot::ChatbotIngress>()
            .is_some()
            && parts.uri.path() == "/v1/responses"
            && !native_compaction;
        let mut payload = Payload::new(
            &parsed,
            &bytes,
            compatibility,
            &source,
            &state.role_capabilities,
        );
        let mut auth_refreshed = false;
        let retries =
            retries::UpstreamRetries::new(&request_retries, &source, &*state.settings.read().await);
        let attempt_limit = (if compatibility { 6 } else { 3 }) + retries.extra_attempts();
        let mut google_thinking_override: Option<crate::google::PreparedGoogleRequest> = None;
        let mut google_thinking_retried = false;
        let mut attempt_number = 0;
        while attempt_number < attempt_limit {
            attempt_number += 1;
            let eligible = if manual_candidate {
                match &manual.trial {
                    Some(trial) => routing::manual::source_for_trial(&state, trial, model)
                        .await
                        .is_some(),
                    None => false,
                }
            } else if priority_probe {
                routing::priority::eligible(&state, &source, model).await
            } else {
                routing::source_is_eligible(&state, &source, model).await
            };
            let current_revision = state.routing_revision.load(Ordering::Acquire);
            if current_revision != selected_revision {
                // OAuth and history preparation may await while the user edits
                // sources. No inference has been submitted for this attempt.
                drop(route_lease.take());
                if current_revision % 2 != 0 || replans >= 3 {
                    return error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "来源设置已变化，请稍后重试",
                    );
                }
                let retry = Request::from_parts(parts, Body::from(bytes));
                return Box::pin(forward_once(state, retry, rebuild, replans + 1)).await;
            }
            if !eligible {
                break;
            }
            let is_google = source.kind == "google";
            let (wire_bytes, ignored_parameters) = match payload.wire(
                source.kind == "official",
                history_capture.as_ref().map(|h| &h.request["input"]),
            ) {
                Ok(prepared) => prepared,
                Err(problem) => {
                    return crate::chatbot::error(
                        StatusCode::BAD_REQUEST,
                        problem.message,
                        Some(problem.param),
                    )
                }
            };
            let wire_bytes = if source.kind == "official" && !native_compaction
                && parts.extensions.get::<crate::omp_ingress::OmpIngress>().is_some() {
                let mut body: Value = serde_json::from_slice(&wire_bytes).expect("validated JSON");
                body["stream"] = json!(true);
                body["store"] = json!(false);
                if body.get("instructions").is_none() { body["instructions"] = json!(""); }
                Bytes::from(serde_json::to_vec(&body).expect("JSON"))
            } else { wire_bytes };
            let (wire_bytes, openai_sanitized) = if source.kind == "openai" && !native_compaction {
                if let Ok(mut body) = serde_json::from_slice::<Value>(&wire_bytes) {
                    let mut changed = false;
                    if let Some(map) = body.as_object_mut() {
                        changed |= map.remove("client_metadata").is_some();
                        changed |= map.remove("access_programs").is_some();
                        if let Some(stream_options) = map.get_mut("stream_options").and_then(Value::as_object_mut) {
                            if stream_options.remove("reasoning_summary_delivery").is_some() {
                                changed = true;
                            }
                            if stream_options.is_empty() {
                                map.remove("stream_options");
                            }
                        }
                    }
                    if changed {
                        (Bytes::from(serde_json::to_vec(&body).expect("JSON")), true)
                    } else {
                        (wire_bytes, false)
                    }
                } else {
                    (wire_bytes, false)
                }
            } else {
                (wire_bytes, false)
            };
            let (url, wire_bytes, google_stream) = if is_google {
                if let Some(prepared) = google_thinking_override.take() {
                    (prepared.url, prepared.body, Some(prepared.stream))
                } else {
                    let mut effective_body: Value = match serde_json::from_slice(&wire_bytes) {
                        Ok(value) => value,
                        Err(_) => return error(StatusCode::BAD_REQUEST, "Responses 请求需要有效 JSON"),
                    };
                    if effective_body.get("previous_response_id").is_some_and(|v| !v.is_null()) {
                        if let Some(expanded) = history_capture.as_ref().map(|h| &h.request["input"]) {
                            effective_body["input"] = expanded.clone();
                            if let Some(obj) = effective_body.as_object_mut() {
                                obj.remove("previous_response_id");
                            }
                        }
                    }
                    match crate::google::build_request(&source, &effective_body, &state.google_signatures) {
                        Ok(prepared) => (prepared.url, prepared.body, Some(prepared.stream)),
                        Err((err_status, message)) => return error(err_status, message),
                    }
                }
            } else {
                let url = match target(&source, parts.uri.path(), parts.uri.query()) {
                    Ok(url) => url,
                    Err(_) => return error(StatusCode::BAD_GATEWAY, "来源地址无效"),
                };
                (url, wire_bytes, None)
            };
            let google_sent_body = (is_google && !google_thinking_retried).then(|| wire_bytes.clone());
            let mut attempt = Attempt::new(state.clone(), &source, &request_id, model, input_units,
                ingress, crate::remote::public_peer(&parts.extensions));
            if let Some(record) = &mut attempt.record {
                record.token_details.service_tier = parsed["service_tier"]
                    .as_str()
                    .filter(|v| v.len() <= 32)
                    .map(String::from);
                record.token_details.cache_write_retention = parsed["prompt_cache_retention"]
                    .as_str()
                    .filter(|v| v.len() <= 32)
                    .map(String::from);
            }
            let client = match transport::client(&source) {
                Ok(client) => client,
                Err(_) => {
                    attempt
                        .diagnostic
                        .transport("configuration", "来源网络策略无法建立连接");
                    attempt.finish("failed", None);
                    if manual_candidate {
                        drop(route_lease.take());
                        drop(attempt);
                        return after_manual_failure(state, parts, bytes, rebuild, replans, true)
                            .await;
                    }
                    last_error = Some(error(StatusCode::BAD_GATEWAY, "来源网络策略无法建立连接"));
                    break;
                }
            };
            attempt.diagnostic.request(
                ingress.as_str(),
                parts.method.as_str(),
                parts.uri.path(),
                url.as_str(),
                &ignored_parameters,
            );
            attempt.save_diagnostic(false);
            let headers = if is_google {
                let mut h = HeaderMap::new();
                h.insert("content-type", HeaderValue::from_static("application/json"));
                h.insert(
                    "accept",
                    HeaderValue::from_static(if google_stream == Some(true) {
                        "text/event-stream"
                    } else {
                        "application/json"
                    }),
                );
                h.insert("accept-encoding", HeaderValue::from_static("identity"));
                let api_key = source
                    .secret
                    .get("api_key")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let value = match HeaderValue::from_str(api_key) {
                    Ok(value) if !api_key.is_empty() => value,
                    _ => {
                        attempt.finish("failed", None);
                        return error(StatusCode::BAD_GATEWAY, "来源凭据无效");
                    }
                };
                h.insert(HeaderName::from_static("x-goog-api-key"), value);
                h
            } else {
                let mut headers = clean_headers(&parts.headers, true);
                if openai_sanitized
                    || payload.role_repaired()
                    || (source.kind == "official" && !native_compaction
                        && parts.extensions.get::<crate::omp_ingress::OmpIngress>().is_some())
                {
                    clear_body_headers(&mut headers);
                }
                let credential = source
                    .secret
                    .get(if source.kind == "official" {
                        "access_token"
                    } else {
                        "api_key"
                    })
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let value = match HeaderValue::from_str(&format!("Bearer {credential}")) {
                    Ok(value) if !credential.is_empty() => value,
                    _ => {
                        attempt.finish("failed", None);
                        return error(StatusCode::BAD_GATEWAY, "来源凭据无效");
                    }
                };
                headers.insert("authorization", value);
                if source.kind == "official" {
                    if parts
                        .extensions
                        .get::<crate::chatbot::ChatbotIngress>()
                        .is_some() || parts.extensions.get::<crate::omp_ingress::OmpIngress>().is_some()
                    {
                        headers.insert("originator", HeaderValue::from_static("codex_cli_rs"));
                        headers.insert("version", HeaderValue::from_static(crate::official::CODEX_CLIENT_VERSION));
                    }
                    if let Some(account) = &source.account_id {
                        let Ok(value) = HeaderValue::from_str(account) else {
                            return error(StatusCode::BAD_GATEWAY, "来源账号标识无效");
                        };
                        headers.insert(HeaderName::from_static("chatgpt-account-id"), value);
                        if let Some(session) = parts.extensions.get::<crate::omp_ingress::OmpIngress>()
                            .and_then(|ingress| ingress.cache_session(account, model, parsed["service_tier"].as_str())) {
                            headers.insert("session-id", HeaderValue::from_str(&session).expect("cache UUID"));
                        }
                    }
                }
                headers
            };
            let settings = state.settings.read().await.clone();
            let source_index = state.sources.read().await.iter().position(|runtime| {
                runtime.source.id == source.id && runtime.source.identity() == source.identity()
            });
            let selection = if manual_candidate {
                "manual_trial"
            } else if degraded {
                "context_handoff"
            } else if origin.is_some() {
                "legacy_origin"
            } else if migrated_source.is_some() {
                "prior_handoff"
            } else if routing_binding.is_some() {
                "bound_context"
            } else if previous_preference.as_deref() == Some(&source.id) {
                "conversation_affinity"
            } else if settings.mode == "fixed" {
                "fixed"
            } else if settings.load_balance && !custom_sources {
                "load_balance"
            } else {
                "priority"
            };
            eprintln!(
                "routing_attempt unix={} ingress={} source_index={} selection={} rebuilt={} bound={} opaque={}",
                crate::model::now(),
                ingress.as_str(),
                source_index.map_or(-1, |index| index as i64),
                selection,
                degraded,
                binding.is_some(),
                opaque_state
            );
            let submission = state.routing_sync.lock().await;
            if let Some(problem) = crate::model_catalog::check_client(&state, model, client_kind).await {
                return problem;
            }
            let current_revision = state.routing_revision.load(Ordering::Acquire);
            if current_revision != selected_revision {
                drop(submission);
                drop(route_lease.take());
                if current_revision % 2 != 0 || replans >= 3 {
                    return error(StatusCode::SERVICE_UNAVAILABLE, "来源设置已变化，请稍后重试");
                }
                let retry = Request::from_parts(parts, Body::from(bytes));
                return Box::pin(forward_once(state, retry, rebuild, replans + 1)).await;
            }
            let Some(ticket) = state.quota_recovery.try_admit().filter(|epoch| *epoch == route_epoch) else {
                drop(submission);
                drop(route_lease.take());
                drop(attempt);
                if replans >= 3 {
                    return recovery_timeout(1);
                }
                let retry = Request::from_parts(parts, Body::from(bytes));
                return Box::pin(forward_once(state, retry, rebuild, replans + 1)).await;
            };
            attempt.dispatch_ticket = Some(ticket);
            let window = parts.extensions.get_mut::<RecoveryWindow>().expect("request recovery window");
            let quota_replay = window.retries_quota(&source);
            if quota_replay {
                window.quota_retry = None;
            }
            if manual_candidate {
                manual.attempted = true;
                parts.extensions.insert(manual.clone());
            }
            // Preparation can be abandoned during OAuth or route revalidation.
            // Notify only when the reconstructed request is actually submitted.
            if let Some(context) = rebuild
                .as_ref()
                .filter(|context| context.excluded.iter().any(|key| key != "rebuilt"))
            {
                state
                    .notifications
                    .fallback(context.key.as_deref().unwrap_or(&request_id));
            }
            let upstream = crate::model_catalog::start_submission(submission, tokio::time::timeout(
                std::time::Duration::from_secs(120),
                client
                    .request(parts.method.clone(), url)
                    .headers(headers)
                    .body(wire_bytes)
                    .send(),
            ))
            .await;
            let upstream = match upstream {
                Ok(result) => result,
                Err(_) => {
                    attempt
                        .diagnostic
                        .transport("timeout", "来源提交结果超时且不明确；未自动重试");
                    attempt.finish("failed", None);
                    if manual_candidate {
                        manual.abandon(&state);
                    }
                    return error(
                        StatusCode::GATEWAY_TIMEOUT,
                        "来源提交结果超时且不明确；未自动重试",
                    );
                }
            };
            let mut upstream = match upstream {
                Ok(response) => response,
                Err(err) => {
                    let definitely_unsubmitted = err.is_connect();
                    eprintln!(
                        "upstream_transport_failure unix={} kind={} connect={} timeout={} uncertain={}",
                        crate::model::now(),
                        source.kind.as_str(),
                        definitely_unsubmitted,
                        err.is_timeout(),
                        !definitely_unsubmitted
                    );
                    attempt.diagnostic.transport(
                        if err.is_connect() {
                            "connect"
                        } else if err.is_timeout() {
                            "timeout"
                        } else {
                            "transport"
                        },
                        transport::failure_reason(&err),
                    );
                    attempt.finish("failed", None);
                    if definitely_unsubmitted && !quota_replay {
                        if let Some(delay) = retries.failed(false) {
                            drop(attempt);
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                    }
                    if manual_candidate && definitely_unsubmitted {
                        drop(route_lease.take());
                        drop(attempt);
                        return after_manual_failure(state, parts, bytes, rebuild, replans, true)
                            .await;
                    }
                    if priority_probe && definitely_unsubmitted {
                        drop(route_lease.take());
                        last_error = Some(error(
                            StatusCode::BAD_GATEWAY,
                            transport::failure_reason(&err),
                        ));
                        break;
                    }
                    if definitely_unsubmitted
                        && !degraded
                        && !probe_unknown_id
                        && (binding.is_some() || opaque_state || origin.is_some())
                        && state.settings.read().await.degraded_context_fallback
                        && !routing::candidates_with_session(
                            &state,
                            model,
                            None,
                            affinity_key.as_deref(),
                        )
                        .await
                        .is_empty()
                    {
                        // A connect failure did not submit inference. Rebuild
                        // before changing sources instead of returning the old
                        // opaque state to the client's limited retry loop.
                        drop(route_lease.take());
                        drop(attempt);
                        let (retry, mut context) = match portable_request(
                            &state,
                            &parts,
                            &parsed,
                            affinity_key.as_deref(),
                        )
                        .await
                        {
                            Ok(value) => value,
                            Err(response) => return *response,
                        };
                        context.rejected_source = Some(source.id.clone());
                        return Box::pin(forward_once(state, retry, Some(context), replans)).await;
                    }
                    if !definitely_unsubmitted || opaque_state {
                        if manual_candidate {
                            manual.abandon(&state);
                        }
                        return error(
                            StatusCode::BAD_GATEWAY,
                            "来源响应中断，提交结果不明确；未自动重试",
                        );
                    }
                    last_error = Some(error(
                        StatusCode::BAD_GATEWAY,
                        transport::failure_reason(&err),
                    ));
                    break;
                }
            };
            attempt
                .diagnostic
                .response(upstream.status().as_u16(), upstream.headers());
            attempt.save_diagnostic(false);
            let upstream_status = upstream.status();
            if !upstream_status.is_success() {
                eprintln!(
                    "upstream_http_failure unix={} kind={} status={}",
                    crate::model::now(),
                    source.kind.as_str(),
                    upstream_status.as_u16()
                );
            }
            let retry_after = upstream
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let mut prefix = Vec::new();
            let request_is_stream = google_stream.unwrap_or_else(|| {
                parsed["stream"].as_bool() == Some(true)
                    || (source.kind == "official" && omp_ingress && !native_compaction)
            });
            let upstream_sse = response_is_sse(upstream.headers(), request_is_stream, upstream_status);
            let code = upstream_status.as_u16();
            let omp_v2_error = omp_ingress && parts.uri.path() == "/v1/responses"
                && matches!(code, 400 | 404 | 422 | 501) && crate::chatbot::is_compaction_v2(&parsed);
            let context_error = matches!(code, 400 | 404 | 409 | 422)
                && (binding.is_some() || opaque_state || payload.may_repair());
            let http_error = if !upstream_sse && ((is_google && !upstream_status.is_success())
                || omp_v2_error || context_error
                || matches!(code, 402 | 403) || retries::transient_status(code))
            {
                read_error_prefix(&mut upstream, &mut prefix, &mut attempt).await
            } else {
                None
            };
            let google_error = if is_google && !upstream_status.is_success() {
                http_error
                    .as_ref()
                    .map(|value| crate::google::normalize_error_value(upstream_status, value))
            } else {
                None
            };
            let effective_http_error = google_error.as_ref().or(http_error.as_ref());
            let http_output = !is_google && http_error.as_ref().is_some_and(response_has_output);
            let ambiguous_status = matches!(upstream_status.as_u16(), 408 | 504);
            // SSE and output-bearing JSON must not inherit replay permission
            // from the HTTP status alone. The actual upstream status is forwarded.
            let status = if upstream_sse || http_output { StatusCode::OK } else { upstream_status };
            let probe = if !is_google && (upstream_sse || (upstream_status.is_success()
                && (manual_candidate || (!degraded && (binding.is_some() || opaque_state)
                    && (priority_probe || probe_unknown_id
                        || state.settings.read().await.degraded_context_fallback)))))
            {
                // Hold a bounded prefix until output or a refusal is observed.
                match probe_context_rejection(&mut upstream, &mut prefix, request_is_stream, &mut attempt).await {
                    Ok(probe) => Some(probe),
                    Err(()) => {
                        if manual_candidate { manual.abandon(&state); }
                        return error(StatusCode::BAD_GATEWAY, "来源响应中断，提交结果不明确；未自动重试");
                    }
                }
            } else {
                None
            };
            let http_quota = effective_http_error.and_then(retries::quota_code);
            let stream_quota = probe.as_ref().and_then(|probe| probe.quota_code.as_deref());
            let saw_quota_in_response = http_quota.is_some() || stream_quota.is_some();
            if let Some(code) = http_quota.or(stream_quota) {
                // Even after output, report the refusal with its original dispatch epoch.
                attempt.report_quota(&source, code, retry_after).await;
                let safe_refusal = !ambiguous_status && (http_quota.is_some() && !http_output
                    || probe.as_ref().and_then(|probe| probe.failure.as_ref()).and_then(retries::quota_code).is_some());
                if safe_refusal {
                    let first = request_retries.reject_quota(&source);
                    let retry_official = source.kind == "official" && first;
                    let window = parts.extensions.get_mut::<RecoveryWindow>().expect("request recovery window");
                    if retry_official {
                        window.quota_retry = Some(source.clone());
                        window.quota_deadline();
                    }
                    attempt.finish("failed", None);
                    drop(route_lease.take());
                    drop(upstream);
                    drop(attempt);
                    if manual_candidate {
                        if !retry_official {
                            return after_manual_failure(state, parts, bytes, rebuild, replans, true).await;
                        }
                        manual.attempted = false;
                        parts.extensions.insert(manual.clone());
                    }
                    let retry = Request::from_parts(parts, Body::from(bytes));
                    return Box::pin(forward_once(state, retry, rebuild, replans)).await;
                }
            }
            let permanent_rejection = retries::transient_status(status.as_u16()) && effective_http_error
                .is_some_and(|value| retries::permanent_code(value).is_some());
            if permanent_rejection {
                request_retries.block(&source);
            } else if !quota_replay && retries::transient_status(status.as_u16()) {
                if let Some(delay) = retries.failed(true) {
                    attempt.finish("failed", None);
                    // Keep the capacity lease, but release the failed transport.
                    attempt.collect_error(&mut upstream, &mut prefix).await;
                    drop(upstream);
                    drop(attempt);
                    tokio::time::sleep(delay).await;
                    continue;
                }
            }
            if !quota_replay && source.kind == "official" && status == StatusCode::UNAUTHORIZED && !auth_refreshed {
                auth_refreshed = true;
                attempt.finish("failed", None);
                attempt.collect_error(&mut upstream, &mut prefix).await;
                drop(upstream);
                match crate::official::refresh_access(&state, &source.id).await {
                    Ok(fresh) if fresh.identity() == source.identity() => {
                        source = fresh;
                        continue;
                    }
                    Ok(_) => {
                        return error(StatusCode::CONFLICT, "来源身份已经变化，请重新开始请求")
                    }
                    Err(_) => {
                        routing::mark_auth_preflight_failure(&state, &source).await;
                        if manual_candidate {
                            drop(route_lease.take());
                            drop(attempt);
                            return after_manual_failure(
                                state, parts, bytes, rebuild, replans, true,
                            )
                            .await;
                        }
                        if priority_probe {
                            last_error = Some(error(
                                StatusCode::BAD_GATEWAY,
                                "官方来源认证刷新失败，请检查登录或网络",
                            ));
                            break;
                        }
                        if !degraded
                            && state.settings.read().await.degraded_context_fallback
                            && !routing::candidates_with_session(
                                &state,
                                model,
                                None,
                                affinity_key.as_deref(),
                            )
                            .await
                            .is_empty()
                        {
                            drop(route_lease.take());
                            drop(attempt);
                            let (retry, context) = match portable_request(
                                &state,
                                &parts,
                                &parsed,
                                affinity_key.as_deref(),
                            )
                            .await
                            {
                                Ok(value) => value,
                                Err(response) => return *response,
                            };
                            return Box::pin(forward_once(state, retry, Some(context), replans))
                                .await;
                        }
                        last_error = Some(error(
                            StatusCode::BAD_GATEWAY,
                            "官方来源认证刷新失败，请检查登录或网络",
                        ));
                        break;
                    }
                }
            }
            if omp_v2_error {
                if let Some(value) = &http_error {
                    crate::omp_ingress::observe_compaction_rejection(&source, status, value);
                }
            }
            let mut context_rejected = false;
            if !is_google && context_error {
                if let Some(value) = &http_error {
                    if !quota_replay && payload.repair_rejection(status, value, &source, &state.role_capabilities) {
                        attempt.finish("failed", None);
                        attempt.collect_error(&mut upstream, &mut prefix).await;
                        drop(upstream);
                        continue;
                    }
                    context_rejected = explicit_context_rejection(value);
                }
            }
            if is_google && status == StatusCode::BAD_REQUEST && !google_thinking_retried {
                if let (Some(sent_body), Some(err_val)) = (google_sent_body.as_deref(), effective_http_error) {
                    if let Some(repaired) = crate::google::fallback_thinking_request(
                        &source,
                        &parsed,
                        &state.google_signatures,
                        sent_body,
                        status,
                        err_val,
                    ) {
                        google_thinking_retried = true;
                        google_thinking_override = Some(repaired);
                        attempt.finish("failed", None);
                        attempt.collect_error(&mut upstream, &mut prefix).await;
                        drop(upstream);
                        drop(attempt);
                        attempt_number -= 1;
                        continue;
                    }
                }
            }
            let mut stream_transient = false;
            if let Some(probe) = probe.filter(|probe| probe.quota_code.is_none() && !ambiguous_status) {
                context_rejected = probe.context_rejected;
                stream_transient = saw_quota && probe.failure.as_ref().is_some_and(retries::transient_error);
                if manual_candidate {
                    if let Some(failure) = probe.failure {
                        let transient = retries::transient_error(&failure);
                        let response = failure.get("response").unwrap_or(&failure);
                        let permanent = retries::permanent_code(response);
                        if transient && !quota_replay {
                            if let Some(delay) = retries.failed(true) {
                                attempt.finish("failed", None);
                                drop(upstream);
                                drop(attempt);
                                tokio::time::sleep(delay).await;
                                continue;
                            }
                            routing::mark_upstream_failure(
                                &state,
                                &source,
                                503,
                                None,
                                retries.confirmed_errors(),
                            )
                            .await;
                        }
                        if !context_rejected && (transient || permanent.is_some()) {
                            routing::mark_rejection(&state, &source, permanent, None).await;
                            attempt.finish("failed", None);
                            drop(upstream);
                            drop(route_lease.take());
                            drop(attempt);
                            return after_manual_failure(
                                state, parts, bytes, rebuild, replans, true,
                            )
                            .await;
                        }
                        // Unrecognized failures may have submitted inference.
                        // Stop takeover, preserving the upstream response without replay.
                        if !context_rejected {
                            manual.abandon(&state);
                        }
                    }
                }
            }
            if matches!(status.as_u16(), 401 | 402 | 403 | 429 | 500..=599) {
                routing::mark_upstream_failure(
                    &state,
                    &source,
                    status.as_u16(),
                    retry_after,
                    retries.confirmed_errors(),
                )
                .await;
                if matches!(status.as_u16(), 402 | 403 | 429) || permanent_rejection {
                    if let Some(value) = effective_http_error {
                        let code = retries::permanent_code(value).or_else(|| {
                            value
                                .pointer("/error/code")
                                .and_then(Value::as_str)
                                .or_else(|| value.pointer("/error/type").and_then(Value::as_str))
                        });
                        routing::mark_rejection(&state, &source, code, retry_after).await;
                    }
                }
            }
            if manual_candidate
                && (context_rejected
                    || ((status.is_client_error() || status.is_server_error())
                        && !matches!(status.as_u16(), 408 | 504)))
            {
                // A rejected upstream ID/ciphertext may still be recoverable
                // with local history. Try the selected source with portable
                // context once before abandoning the takeover.
                if context_rejected
                    && !degraded
                    && state.settings.read().await.degraded_context_fallback
                {
                    if let Ok((mut retry, context)) =
                        portable_request(&state, &parts, &parsed, affinity_key.as_deref()).await
                    {
                        attempt.finish("failed", None);
                        attempt.collect_error(&mut upstream, &mut prefix).await;
                        drop(upstream);
                        drop(route_lease.take());
                        drop(attempt);
                        if let Some(manual) = retry.extensions_mut().get_mut::<ManualRequest>() {
                            manual.attempted = false;
                        }
                        return Box::pin(forward_once(state, retry, Some(context), replans)).await;
                    }
                }
                attempt.finish("failed", None);
                drop(route_lease.take());
                attempt.collect_error(&mut upstream, &mut prefix).await;
                drop(upstream);
                drop(attempt);
                return after_manual_failure(state, parts, bytes, rebuild, replans, true).await;
            }
            if ((probe_unknown_id && context_rejected)
                || (priority_probe
                    && (context_rejected
                        || ((status.is_client_error() || status.is_server_error())
                            && !matches!(status.as_u16(), 408 | 504)))))
                && candidate_index + 1 < source_count
            {
                // A missing local mapping can mean a different candidate owns
                // this ID. Explicit rejection permits a bounded unchanged probe
                // before requiring local history for a new context.
                attempt.finish("failed", None);
                attempt.collect_error(&mut upstream, &mut prefix).await;
                drop(route_lease.take());
                last_error = Some(if is_google {
                    google_error_response(upstream_status, &source, &prefix)
                } else {
                    stream_response(
                        state.clone(),
                        source.clone(),
                        upstream,
                        prefix,
                        attempt,
                        ResponseContext {
                            discovery,
                            session_key: None,
                            request_is_stream: parsed["stream"].as_bool() == Some(true),
                            rebuild: None,
                            history: None,
                            accepted_state: None,
                        },
                        None,
                    )
                    .await
                });
                break;
            }
            if !degraded
                && (binding.is_some() || opaque_state || origin.is_some())
                && (context_rejected
                    || matches!(status.as_u16(), 401 | 402 | 403 | 429)
                    || retries::transient_status(status.as_u16()) || stream_transient)
                && state.settings.read().await.degraded_context_fallback
                && if priority_probe {
                    !routing::priority::candidates(&state, model, affinity_key.as_deref())
                        .await
                        .iter()
                        .all(|s| request_retries.exhausted(s, retry_limit))
                } else {
                    !routing::candidates_with_session(&state, model, None, affinity_key.as_deref())
                        .await
                        .iter()
                        .all(|s| request_retries.exhausted(s, retry_limit))
                }
            {
                attempt.finish("failed", None);
                attempt.collect_error(&mut upstream, &mut prefix).await;
                drop(upstream);
                drop(route_lease.take());
                drop(attempt);
                let (retry, mut context) = match portable_request(
                    &state,
                    &parts,
                    &parsed,
                    affinity_key.as_deref(),
                )
                .await
                {
                    Ok(value) => value,
                    Err(response) => return *response,
                };
                if context_rejected || retries::transient_status(status.as_u16()) {
                    context.rejected_source = Some(source.id.clone());
                }
                return Box::pin(forward_once(state, retry, Some(context), replans)).await;
            }
            let safe_to_fallback = binding.is_none()
                && !opaque_state
                && (origin.is_none() || planned_return)
                && (matches!(status.as_u16(), 401 | 402 | 403 | 429)
                    || retries::transient_status(status.as_u16())
                    || stream_transient || (degraded && context_rejected));
            if saw_quota && (stream_transient || matches!(status.as_u16(), 401 | 402 | 403 | 429)
                || retries::transient_status(status.as_u16())) && !safe_to_fallback
            {
                attempt.finish("failed", None);
                return recovery_timeout(1);
            }
            if safe_to_fallback {
                let quota_failover = saw_quota && !context_rejected;
                if quota_failover { request_retries.block(&source); }
                attempt.finish("failed", None);
                drop(route_lease.take());
                if quota_failover {
                    if !stream_transient { attempt.collect_error(&mut upstream, &mut prefix).await; }
                    break;
                }
                // Preserve the final source's native error body when there is no
                // later candidate; streaming this response must not trigger replay.
                if candidate_index + 1 < source_count || is_google {
                    attempt.collect_error(&mut upstream, &mut prefix).await;
                }
                last_error = Some(if is_google {
                    google_error_response(upstream_status, &source, &prefix)
                } else {
                    stream_response(
                        state.clone(),
                        source.clone(),
                        upstream,
                        prefix,
                        attempt,
                        ResponseContext {
                            discovery,
                            session_key: session_key.clone(),
                            request_is_stream,
                            rebuild: None,
                            history: None,
                            accepted_state: None,
                        },
                        None,
                    )
                    .await
                });
                break;
            }
            if upstream_status.is_success() && !saw_quota_in_response {
                routing::mark_ready(&state, &source).await;
            }
            if manual_candidate {
                attempt.manual_trial = manual.trial.clone();
            }
            let validated_continuation = hard_keys.iter().any(|key| {
                exclusions.contains(&crate::continuity::accepted_state(&source.identity(), key))
            });
            let accepted_state = if manual_candidate || priority_probe || validated_continuation {
                Some(ManualAcceptance {
                    key: affinity_key.clone(),
                    hashes: hard_keys
                        .iter()
                        .map(|key| crate::continuity::accepted_state(&source.identity(), key))
                        .collect(),
                    protected_bindings: hard_keys
                        .iter()
                        .filter(|key| Some(**key) != session_key.as_deref())
                        .map(|key| (*key).to_owned())
                        .collect(),
                    restored: crate::continuity::keys(
                        &parsed,
                        parts
                            .headers
                            .get("x-codex-turn-state")
                            .and_then(|value| value.to_str().ok()),
                    ),
                })
            } else {
                None
            };
            let history_capture = history_capture.map(|mut capture| {
                capture.metadata = attempt.record.as_ref().map(|record| crate::history::CaptureMetadata {
                    captured_at: crate::model::now(),
                    source_id: record.source_id.clone(),
                    source_name: record.source_name.clone(),
                    model: record.model.clone(),
                    ingress: ingress.as_str().into(),
                    request_id: record.request_id.clone(),
                    attempt_id: record.attempt_id.clone(),
                });
                capture
            });
            let context = ResponseContext {
                discovery,
                session_key: session_key.clone(),
                request_is_stream,
                rebuild,
                history: history_capture,
                accepted_state,
            };
            let mut response = if is_google {
                if !upstream_status.is_success() {
                    attempt.finish("failed", None);
                    attempt.collect_error(&mut upstream, &mut prefix).await;
                    google_error_response(upstream_status, &source, &prefix)
                } else if google_stream == Some(true) {
                    let translated = Box::pin(crate::google::translate_sse_stream(
                        upstream.bytes_stream(),
                        model.to_owned(),
                        state.google_signatures.clone(),
                    ));
                    stream_translated_response(
                        state,
                        source.clone(),
                        true,
                        None,
                        translated,
                        attempt,
                        context,
                        route_lease.take(),
                    )
                    .await
                } else {
                    match transport::read_limited(&mut upstream, 32 * 1024 * 1024).await {
                        Ok(raw) => match crate::google::translate_non_streaming(
                            &raw,
                            model,
                            &state.google_signatures,
                        ) {
                            Ok(json_bytes) => {
                                let len = json_bytes.len() as u64;
                                let translated = Box::pin(futures_util::stream::once(async move {
                                    Ok::<Bytes, std::io::Error>(json_bytes)
                                }));
                                stream_translated_response(
                                    state,
                                    source.clone(),
                                    false,
                                    Some(len),
                                    translated,
                                    attempt,
                                    context,
                                    route_lease.take(),
                                )
                                .await
                            }
                            Err(message) => {
                                attempt.diagnostic.transport("upstream_protocol", message);
                                attempt.finish("failed", None);
                                error(StatusCode::BAD_GATEWAY, message)
                            }
                        },
                        Err(transport::BodyReadError::TooLarge) => {
                            attempt
                                .diagnostic
                                .transport("body_too_large", "Google AI Studio 响应过大");
                            attempt.finish("failed", None);
                            error(StatusCode::BAD_GATEWAY, "来源响应过大")
                        }
                        Err(transport::BodyReadError::ReadFailed) => {
                            attempt
                                .diagnostic
                                .transport("body_transport", "上游响应传输中断");
                            attempt.finish("interrupted", None);
                            error(StatusCode::BAD_GATEWAY, "来源响应中断")
                        }
                    }
                }
            } else {
                stream_response(
                    state,
                    source.clone(),
                    upstream,
                    prefix,
                    attempt,
                    context,
                    route_lease.take(),
                )
                .await
            };
            for (name, value) in [("x-codexpress-source-id", source.id.clone()),
                ("x-codexpress-source-identity", source.identity())] {
                if let Ok(value) = HeaderValue::from_str(&value) {
                    response.headers_mut().insert(name, value);
                }
            }
            if !ignored_parameters.is_empty() {
                if let Ok(value) = HeaderValue::from_str(&ignored_parameters.join(", ")) {
                    response
                        .headers_mut()
                        .insert("x-codexpress-adapted-parameters", value);
                }
            }
            return response;
        }
        if manual_candidate {
            drop(route_lease.take());
            return after_manual_failure(state, parts, bytes, rebuild, replans, true).await;
        }
    }
    if saw_quota {
        return quota_unavailable(&state, model, true).await.expect("quota failure response");
    }
    last_error.unwrap_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "所有来源暂时不可用"))
}

fn google_error_response(
    status: StatusCode,
    source: &Source,
    prefix: &[(Bytes, std::time::Instant)],
) -> Response<Body> {
    let raw: Vec<u8> = prefix.iter().flat_map(|(chunk, _)| chunk.iter().copied()).collect();
    let body = crate::google::translate_error(status, &raw);
    let mut response = Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    for (name, value) in [
        ("x-codexpress-source-id", source.id.clone()),
        ("x-codexpress-source-identity", source.identity()),
    ] {
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[tokio::test]
    async fn cooldown_deadline_returns_503_with_retry_after_without_contacting_another_source() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(directory.path().to_owned(), 0, "fixture".into()).unwrap();
        let source: Source = serde_json::from_value(json!({
            "id":"only","name":"Fixture","kind":"api","base_url":"http://127.0.0.1:1",
            "enabled":true,"priority":0,"proxy":{"mode":"direct"},"secret":{"api_key":"fixture"}
        }))
        .unwrap();
        let mut runtime = crate::model::SourceRuntime::new(source);
        runtime.health = "cooldown".into();
        runtime.cooldown_until = Some(crate::model::now() + 60);
        *state.sources.write().await = vec![runtime];
        {
            let mut settings = state.settings.write().await;
            settings.mode = "fixed".into();
            settings.fixed_source_id = Some("only".into());
        }
        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("host", "127.0.0.1:0")
            .header("authorization", "Bearer fixture")
            .body(Body::from(r#"{"model":"m","input":"next"}"#))
            .unwrap();
        let started = tokio::time::Instant::now();
        request.extensions_mut().insert(RecoveryWindow {
            deadline: started + std::time::Duration::from_millis(60),
            attempted: false,
            quota_deadline: None,
            quota_retry: None,
        });
        let response = forward_once(state.clone(), request, None, 0).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            response.headers()["retry-after"]
                .to_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                > 0
        );
        assert!(started.elapsed() >= std::time::Duration::from_millis(50));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(state.usage.summary(1).unwrap()["recent"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn an_unrelated_ready_text_source_waits_for_official_quota_recovery() {
        use tokio::sync::Notify;
        use tower::ServiceExt;

        let usage_started = Arc::new(Notify::new());
        let release_usage = Arc::new(Notify::new());
        let inference = Arc::new(Notify::new());
        let fixture = Router::new()
            .route("/usage", get({
                let (started, release) = (usage_started.clone(), release_usage.clone());
                move || async move {
                    started.notify_one();
                    release.notified().await;
                    axum::Json(json!({"rate_limit": {
                        "primary_window": {"used_percent":100,"reset_at":2000000000,"limit_window_seconds":18000},
                        "secondary_window": {"used_percent":100,"reset_at":2000500000,"limit_window_seconds":604800}
                    }}))
                }
            }))
            .route("/responses", post({
                let inference = inference.clone();
                move || async move {
                    inference.notify_one();
                    axum::Json(json!({"status":"completed","output":[]}))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(directory.path().into(), 0, "fixture".into()).unwrap();
        let official: Source = serde_json::from_value(json!({
            "id":"official","name":"Official fixture","kind":"official","enabled":true,"priority":0,
            "base_url":"https://chatgpt.com/backend-api/codex","proxy":{"mode":"explicit","url":url},
            "models":["separate-model"],"account_id":"fixture-account",
            "secret":{"access_token":"access","refresh_token":"refresh","expires_at":crate::model::now()+3600,"account_id":"fixture-account"}
        })).unwrap();
        let api: Source = serde_json::from_value(json!({
            "id":"api","name":"API fixture","kind":"api","enabled":true,"priority":1,
            "base_url":url,"proxy":{"mode":"direct"},"secret":{"api_key":"fixture"}
        })).unwrap();
        *state.sources.write().await = vec![official.clone(), api].into_iter().map(crate::model::SourceRuntime::new).collect();
        let ticket = state.quota_recovery.try_admit().unwrap();
        crate::official::fixture::quota_rejected(&state, &official, ticket).await;
        tokio::time::timeout(std::time::Duration::from_millis(500), usage_started.notified()).await.unwrap();
        let request = Request::builder().method("POST").uri("/v1/responses")
            .header("host", "127.0.0.1:0").header("authorization", "Bearer fixture")
            .body(Body::from(r#"{"model":"m"}"#)).unwrap();
        let task = tokio::spawn(router(state.clone()).oneshot(request));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), inference.notified()).await.is_err());
        release_usage.notify_one();
        let response = tokio::time::timeout(std::time::Duration::from_secs(2), task).await.unwrap().unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-codexpress-source-id"], "api");
        server.abort();
    }
}

#[cfg(test)]
mod omp_thread_tests {
    use super::*;
    #[test]
    fn omp_thread_identity_never_uses_shared_cache_key() {
        let parsed = json!({"session_id":"real-session","prompt_cache_key":"shared-cache"});
        let mut request = Request::new(Body::empty());
        request.extensions_mut().insert(crate::omp_ingress::OmpIngress { namespace:"client".into(), normalized:false, cache_group:None });
        request.headers_mut().insert("thread-id", HeaderValue::from_static("thread-a"));
        let (mut parts, _) = request.into_parts();
        let first = scoped_session(&parsed, &parts).unwrap();
        parts.headers.insert("thread-id", HeaderValue::from_static("thread-b"));
        assert_ne!(Some(first), scoped_session(&parsed, &parts));
        parts.headers.remove("thread-id");
        assert_eq!(scoped_session(&parsed, &parts).as_deref(), Some("omp:client:session:real-session"));
        assert_eq!(scoped_session(&json!({"prompt_cache_key":"shared-cache"}), &parts), None);
    }
}
