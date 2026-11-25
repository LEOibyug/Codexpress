//! Separate OpenAI-compatible ingress for local and LAN clients. Native Codex
//! traffic never enters this adapter and keeps its original request/response bytes.
mod requests;
mod responses;

use crate::{config::secure_write, listener::{Listener, ListenerChange}, model::AppState};
use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::Mutex as AsyncMutex;

const RESPONSE_CAPTURE_LIMIT: usize = 32 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct ChatbotIngress {
    pub namespace: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Settings {
    enabled: bool,
    port: u16,
    key: String,
}
pub struct ChatbotAccess {
    settings: Mutex<Settings>,
    path: PathBuf,
    listener: AsyncMutex<Option<Listener>>,
}

fn generate_key() -> String {
    format!(
        "cxb-{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
fn validate_key(key: &str) -> Result<()> {
    if !(16..=256).contains(&key.len()) || !key.bytes().all(|b| b.is_ascii_graphic()) {
        bail!("API Key 须为 16–256 位不含空白的 ASCII 字符");
    }
    Ok(())
}
impl ChatbotAccess {
    pub(crate) fn port(&self) -> Result<u16> {
        Ok(self.settings()?.port)
    }
    pub(crate) fn uses_key(&self, key: &str) -> Result<bool> {
        Ok(self.settings()?.key == key)
    }
    pub fn open(directory: &Path, main_port: u16, gateway_key: &str) -> Result<Self> {
        let path = directory.join("chatbot-access.json");
        let settings: Settings = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() && meta.len() <= 16 * 1024 => {
                serde_json::from_slice(&std::fs::read(&path)?).context("聊天客户端接入设置损坏")?
            }
            Ok(_) => bail!("聊天客户端接入文件无效"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let settings = Settings {
                    enabled: true,
                    port: if main_port == 17865 { 17866 } else { 17865 },
                    key: generate_key(),
                };
                secure_write(&path, &serde_json::to_vec(&settings)?)?;
                settings
            }
            Err(_) => bail!("无法读取聊天客户端接入设置"),
        };
        validate_key(&settings.key)?;
        if settings.port == 0 {
            bail!("聊天客户端端口无效");
        }
        if settings.key == gateway_key {
            bail!("聊天客户端 Key 不能与 Codex 网关 Key 相同");
        }
        Ok(Self {
            settings: Mutex::new(settings),
            path,
            listener: AsyncMutex::new(None),
        })
    }
    fn settings(&self) -> Result<Settings> {
        self.settings
            .lock()
            .map(|s| s.clone())
            .map_err(|_| anyhow::anyhow!("聊天客户端接入忙碌"))
    }
    pub async fn status(&self) -> Result<Value> {
        let listener = self.listener.lock().await;
        let settings = self.settings()?;
        Ok(
            json!({"enabled":settings.enabled,"running":listener.as_ref().is_some_and(Listener::running),"port":settings.port,"base_url":format!("http://127.0.0.1:{}/v1",settings.port),"listen_address":"0.0.0.0","lan_enabled":true,"key_set":true}),
        )
    }
    pub fn key(&self) -> Result<Value> {
        Ok(json!({"key":self.settings()?.key}))
    }
}

pub async fn configure(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    // Cross-entry validation and publication form one configuration change.
    let _configuration = state.config_lock.lock().await;
    let mut listener = state.chatbot.listener.lock().await;
    let mut next = state.chatbot.settings()?;
    if let Some(value) = payload.get("enabled") {
        next.enabled = value.as_bool().context("请指定是否启用聊天客户端接入")?;
    }
    if let Some(value) = payload.get("port") {
        next.port = value
            .as_u64()
            .filter(|p| (1..=65535).contains(p))
            .context("聊天客户端端口须为 1–65535")? as u16;
    }
    if next.port == state.port {
        bail!("聊天客户端端口不能与 Codex／SSH 网关端口相同");
    }
    if next.port == state.images.port()? {
        bail!("此端口已用于图片生成，请选择不同端口");
    }
    if next.port == state.control.port()? {
        bail!("此端口已用于管理 API，请选择不同端口");
    }
    if payload["regenerate_key"] == true {
        next.key = generate_key();
    } else if let Some(value) = payload.get("key") {
        next.key = value.as_str().context("API Key 格式无效")?.to_owned();
    }
    validate_key(&next.key)?;
    if next.key == state.gateway_key {
        bail!("聊天客户端 Key 不能与 Codex 网关 Key 相同");
    }
    if state.images.uses_key(&next.key)? {
        bail!("聊天客户端与图片生成须使用不同 Key");
    }
    if state.control.uses_key(&next.key)? {
        bail!("聊天客户端与管理 API 须使用不同 Key");
    }
    if state.remote.uses_key(&next.key)?
        || state
            .sources
            .read()
            .await
            .iter()
            .any(|r| r.source.secret["api_key"].as_str() == Some(&next.key))
    {
        bail!("请为聊天客户端使用独立 Key，不要复用来源或远端机器密钥");
    }
    let change = ListenerChange::prepare(listener.as_ref(), next.enabled, std::net::Ipv4Addr::UNSPECIFIED, next.port)
        .await.context("聊天客户端端口被占用，请更换端口")?;
    secure_write(&state.chatbot.path, &serde_json::to_vec(&next)?)?;
    *state
        .chatbot
        .settings
        .lock()
        .map_err(|_| anyhow::anyhow!("聊天客户端接入忙碌"))? = next;
    change.apply(&mut listener, || router(state.clone()));
    drop(listener);
    state.chatbot.status().await
}
pub async fn resume(state: &Arc<AppState>) -> Result<()> {
    configure(state, json!({})).await.map(|_| ())
}

pub(crate) fn error(status: StatusCode, message: &str, param: Option<&str>) -> Response {
    let mut response = Response::new(Body::from(json!({"error":{"message":message,"type":"invalid_request_error","param":param,"code":null}}).to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

async fn gate(State(state): State<Arc<AppState>>, mut request: Request, next: Next) -> Response {
    let requested_headers = request
        .headers()
        .get("access-control-request-headers")
        .filter(|v| v.as_bytes().len() <= 2048)
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            v.split(',')
                .all(|name| axum::http::HeaderName::from_bytes(name.trim().as_bytes()).is_ok())
        })
        .and_then(|v| HeaderValue::from_str(v).ok());
    let settings = match state.chatbot.settings() {
        Ok(s) => s,
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "聊天客户端接入不可用",
                None,
            )
        }
    };
    // LAN callers use the Mac's reachable IP or DNS name. Authentication remains
    // the independent Bearer key; neither Host nor forwarded headers grant trust.
    let hosts: Vec<_> = request.headers().get_all(header::HOST).iter().collect();
    if hosts.len() != 1
        || !hosts[0].to_str().ok().is_some_and(|value| {
            !value.contains('@') && value.parse::<http::uri::Authority>().ok().is_some_and(|authority| {
                authority.port_u16() == Some(settings.port)
                    && crate::remote::validate_host(authority.host().trim_matches(['[', ']'])).is_ok()
            })
        })
    {
        return error(
            StatusCode::MISDIRECTED_REQUEST,
            "客户端地址与监听端口不匹配",
            None,
        );
    }
    let mut response = if request.method() == axum::http::Method::OPTIONS {
        // Browser clients authenticate with the explicit independent Bearer key;
        // cookies are never accepted or enabled by CORS.
        Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap()
    } else if !settings.enabled
        || request
            .headers()
            .get_all(header::AUTHORIZATION)
            .iter()
            .count()
            != 1
        || request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            != Some(&settings.key)
    {
        error(StatusCode::UNAUTHORIZED, "聊天客户端 API Key 无效", None)
    } else {
        request.extensions_mut().insert(ChatbotIngress {
            namespace: crate::continuity::digest("chatbot", &settings.key),
        });
        next.run(request).await
    };
    response
        .headers_mut()
        .insert("access-control-allow-origin", HeaderValue::from_static("*"));
    response.headers_mut().insert(
        "access-control-allow-methods",
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    response.headers_mut().insert(
        "access-control-allow-headers",
        requested_headers.unwrap_or_else(|| {
            HeaderValue::from_static(
                "Authorization, Content-Type, X-Session-Id, Thread-Id, OpenAI-Beta",
            )
        }),
    );
    response.headers_mut().insert(
        "access-control-expose-headers",
        HeaderValue::from_static("x-codexpress-adapted-parameters, x-codexpress-source-id, x-codexpress-source-identity"),
    );
    response
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/remote-check", get(remote_check))
        .route("/v1/models", get(models))
        .route("/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .route("/chat/completions", post(chat))
        .route("/v1/responses", post(responses))
        .route("/responses", post(responses))
        .route("/v1/responses/compact", post(compact))
        .route("/responses/compact", post(compact))
        .merge(crate::omp_ingress::local_router())
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state.clone(), crate::maintenance::http))
        .layer(middleware::from_fn_with_state(state, gate))
}

pub(crate) fn device_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/chat/v1/models", get(models))
        .route("/chat/v1/chat/completions", post(chat))
        .route("/chat/v1/responses", post(responses))
        .route("/chat/v1/responses/compact", post(compact))
        .merge(crate::omp_ingress::inference_router())
        .route_layer(middleware::from_fn(|mut request: Request, next: Next| async move {
            let Some(peer) = request.extensions().get::<crate::remote::RemoteIngress>() else {
                return StatusCode::UNAUTHORIZED.into_response();
            };
            let namespace = crate::continuity::digest("device", &peer.peer_id);
            request.extensions_mut().insert(ChatbotIngress { namespace });
            next.run(request).await
        }))
}

async fn remote_check() -> Json<Value> {
    Json(json!({"service":"codexpress","mode":"chatbot","version":1}))
}
pub(crate) async fn models(State(state): State<Arc<AppState>>) -> Response {
    match crate::model_catalog::load(&state, crate::model_policy::ClientKind::Chatbot).await {
        Ok(catalog) => Json(json!({"object":"list","data":catalog.models()})).into_response(),
        Err(_) => error(StatusCode::SERVICE_UNAVAILABLE, "无法获取来源模型列表，请先在来源页刷新或手动填写", None),
    }
}
async fn chat(State(state): State<Arc<AppState>>, request: Request) -> Response {
    infer(state, request, requests::Format::Chat).await
}
async fn responses(State(state): State<Arc<AppState>>, request: Request) -> Response {
    infer(state, request, requests::Format::Responses).await
}

async fn infer(state: Arc<AppState>, request: Request, format: requests::Format) -> Response {
    let (mut parts, body) = request.into_parts();
    let bytes = match crate::request_body::read(body, "chatbot").await {
        Ok(b) => b,
        Err(problem) => return error(problem.status, problem.message, None),
    };
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return error(StatusCode::BAD_REQUEST, "请求须为 JSON 对象", None),
    };
    if format == requests::Format::Responses && is_compaction_v2(&body) {
        return forward_native_compaction(
            state,
            parts,
            bytes,
            body,
            "/v1/responses",
            "text/event-stream",
        )
        .await;
    }
    let session = body["session_id"]
        .as_str()
        .or_else(|| body["conversation_id"].as_str())
        .filter(|s| !s.is_empty() && s.len() <= 512)
        .map(|s| crate::continuity::digest("chatbot-session", s));
    let (mut body, options) = match requests::normalize(body, format) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.message, Some(e.param)),
    };
    for field in ["session_id", "conversation_id"] {
        body.as_object_mut().unwrap().remove(field);
    }
    let mut headers = relay_headers(&state, &parts, "text/event-stream");
    if !headers.contains_key("thread-id") && !headers.contains_key("session-id") {
        if let Some(session) = session {
            headers.insert("session-id", session.parse().unwrap());
        }
    }
    parts.headers = headers;
    parts.uri = "/v1/responses".parse().unwrap();
    let request = Request::from_parts(parts, Body::from(serde_json::to_vec(&body).unwrap()));
    let response = crate::relay::forward(State(state), request).await;
    responses::adapt(response, options).await
}

// V1 compaction is a native JSON exchange, not a generation request. Preserve
// opaque history and return the relay body without a presentation adapter.
async fn compact(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match crate::request_body::read(body, "chatbot").await {
        Ok(bytes) => bytes,
        Err(problem) => return error(problem.status, problem.message, None),
    };
    let body: Value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(body) if body.is_object() => body,
        _ => return error(StatusCode::BAD_REQUEST, "请求须为 JSON 对象", None),
    };
    forward_native_compaction(
        state,
        parts,
        bytes,
        body,
        "/v1/responses/compact",
        "application/json",
    )
    .await
}

pub(crate) fn is_compaction_v2(body: &Value) -> bool {
    body["input"].as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item["type"] == "compaction_trigger")
    })
}

async fn forward_native_compaction(
    state: Arc<AppState>,
    mut parts: axum::http::request::Parts,
    bytes: bytes::Bytes,
    body: Value,
    path: &'static str,
    accept: &'static str,
) -> Response {
    if let Err(problem) = requests::model(&body) {
        return error(
            StatusCode::BAD_REQUEST,
            problem.message,
            Some(problem.param),
        );
    }
    let mut headers = relay_headers(&state, &parts, accept);
    for name in [
        "openai-beta",
        "x-codex-beta-features",
        "x-codex-turn-state",
        "x-codex-routing-hint",
        "x-openai-internal-codex-responses-lite",
        "x-client-request-id",
    ] {
        if let Some(value) = parts.headers.get(name) {
            headers.insert(axum::http::HeaderName::from_static(name), value.clone());
        }
    }
    parts.headers = headers;
    parts.uri = match parts.uri.query() {
        Some(query) => format!("{path}?{query}").parse().unwrap(),
        None => path.parse().unwrap(),
    };
    crate::relay::forward(State(state), Request::from_parts(parts, Body::from(bytes))).await
}

fn relay_headers(
    state: &AppState,
    parts: &http::request::Parts,
    accept: &'static str,
) -> axum::http::HeaderMap {
    // Only this authenticated ingress can mint the internal relay request.
    // Client keys, cookies and vendor account IDs never cross this boundary.
    let mut headers = axum::http::HeaderMap::new();
    for name in [
        "thread-id",
        "thread_id",
        "session-id",
        "x-session-id",
        "session_id",
        "x-request-id",
    ] {
        if let Some(value) = parts.headers.get(name) {
            headers.insert(axum::http::HeaderName::from_static(name), value.clone());
        }
    }
    if parts.extensions.get::<crate::remote::RemoteIngress>().is_some() {
        // The relay revalidates this device even on internal replans. Never
        // replace its identity with the credential shared by local adapters.
        for name in [header::HOST, header::AUTHORIZATION] {
            if let Some(value) = parts.headers.get(&name) { headers.insert(name, value.clone()); }
        }
    } else {
        headers.insert(header::HOST, format!("127.0.0.1:{}", state.port).parse().unwrap());
        headers.insert(header::AUTHORIZATION, format!("Bearer {}", state.gateway_key).parse().unwrap());
    }
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    headers.insert(header::ACCEPT, HeaderValue::from_static(accept));
    headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("Mozilla/5.0 Codexpress/0.1"),
    );
    headers
}

pub(crate) use requests::repair;
pub(crate) use requests::upstream_payload;
pub(crate) use requests::Invalid;
