//! Separate, loopback-only OpenAI-compatible ingress. Native Codex traffic
//! never enters this adapter and keeps its original request/response bytes.
mod requests;
mod responses;

use crate::{config::secure_write, model::AppState};
use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
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
struct Listener {
    port: u16,
    task: tokio::task::JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
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
            json!({"enabled":settings.enabled,"running":listener.as_ref().is_some_and(|l|!l.task.is_finished()),"port":settings.port,"base_url":format!("http://127.0.0.1:{}/v1",settings.port),"key_set":true}),
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
    let current = listener
        .as_ref()
        .is_some_and(|l| l.port == next.port && !l.task.is_finished());
    let socket = if next.enabled && !current {
        Some(
            tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, next.port))
                .await
                .context("聊天客户端端口被占用，请更换端口")?,
        )
    } else {
        None
    };
    secure_write(&state.chatbot.path, &serde_json::to_vec(&next)?)?;
    *state
        .chatbot
        .settings
        .lock()
        .map_err(|_| anyhow::anyhow!("聊天客户端接入忙碌"))? = next.clone();
    if let Some(socket) = socket {
        let shared = state.clone();
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = axum::serve(socket, router(shared))
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await;
        });
        if let Some(old) = listener.take() {
            let _ = old.shutdown.send(());
        }
        *listener = Some(Listener {
            port: next.port,
            task,
            shutdown,
        });
    } else if !next.enabled {
        if let Some(old) = listener.take() {
            let _ = old.shutdown.send(());
        }
    }
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
    let expected = [
        format!("127.0.0.1:{}", settings.port),
        format!("localhost:{}", settings.port),
    ];
    let hosts: Vec<_> = request.headers().get_all(header::HOST).iter().collect();
    if hosts.len() != 1
        || !hosts[0]
            .to_str()
            .ok()
            .is_some_and(|h| expected.iter().any(|e| e.eq_ignore_ascii_case(h)))
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
        HeaderValue::from_static("x-codexpress-adapted-parameters"),
    );
    response
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/models", get(models))
        .route("/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .route("/chat/completions", post(chat))
        .route("/v1/responses", post(responses))
        .route("/responses", post(responses))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, gate))
}
async fn models(State(state): State<Arc<AppState>>) -> Response {
    use futures_util::StreamExt;
    let sources = crate::routing::candidates(&state, "", None).await;
    let mut names = BTreeSet::new();
    for source in &sources {
        names.extend(source.models.clone());
    }
    let uncached: Vec<_> = sources
        .into_iter()
        .filter(|source| source.models.is_empty())
        .collect();
    let had_uncached = !uncached.is_empty();
    let mut queries = futures_util::stream::iter(uncached)
        .map(|source| {
            let state = state.clone();
            async move { crate::discovery::list(&state, json!({"source_id":source.id})).await }
        })
        .buffer_unordered(4);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while let Ok(Some(result)) = tokio::time::timeout_at(deadline, queries.next()).await {
        if let Ok(result) = result {
            for name in result["models"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                names.insert(name.to_owned());
                if names.len() >= 10_000 {
                    break;
                }
            }
        }
        if names.len() >= 10_000 {
            break;
        }
    }
    if had_uncached && names.is_empty() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "无法获取来源模型列表，请先在来源页刷新模型或手动填写",
            None,
        );
    }
    let mut data = Vec::new();
    for name in names.into_iter().take(10_000) {
        if !crate::routing::candidates(&state, &name, None)
            .await
            .is_empty()
        {
            data.push(json!({"id":name,"object":"model","created":0,"owned_by":"codexpress"}));
        }
    }
    use axum::response::IntoResponse;
    Json(json!({"object":"list","data":data})).into_response()
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
    // Only the authenticated adapter can mint this internal request. Incoming
    // headers are restricted so client keys/cookies and vendor account IDs never
    // reach the upstream or another ingress's history namespace.
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
    if !headers.contains_key("thread-id") && !headers.contains_key("session-id") {
        if let Some(session) = session {
            headers.insert("session-id", session.parse().unwrap());
        }
    }
    headers.insert(
        header::HOST,
        format!("127.0.0.1:{}", state.port).parse().unwrap(),
    );
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {}", state.gateway_key).parse().unwrap(),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("Mozilla/5.0 Codexpress/0.1"),
    );
    parts.headers = headers;
    parts.uri = "/v1/responses".parse().unwrap();
    let request = Request::from_parts(parts, Body::from(serde_json::to_vec(&body).unwrap()));
    let response = crate::relay::forward(State(state), request).await;
    responses::adapt(response, options).await
}

pub(crate) use requests::repair;
pub(crate) use requests::upstream_payload;
pub(crate) use requests::Invalid;
