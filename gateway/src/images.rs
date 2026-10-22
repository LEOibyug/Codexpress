//! Loopback-only subscription image API. Only official sources are eligible;
//! local access keys and upstream OAuth credentials never cross that boundary.
mod payload;

use crate::{
    chatbot,
    config::secure_write,
    model::{now, AppState, Source},
    routing, transport,
    usage::{TokenDetails, UsageRecord},
};
use anyhow::{bail, Context, Result};
use axum::{
    body::to_bytes,
    extract::{DefaultBodyLimit, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Mutex as AsyncMutex;

pub const MODEL: &str = "gpt-image-2";
#[cfg(test)]
#[derive(Clone)]
struct TestTarget(String);
const REQUEST_LIMIT: usize = 64 * 1024 * 1024;
const RESPONSE_LIMIT: usize = 128 * 1024 * 1024;
#[derive(Clone, Serialize, Deserialize)]
struct Settings {
    enabled: bool,
    port: u16,
    key: String,
    #[serde(default)]
    source_id: Option<String>,
}
struct Listener {
    task: tokio::task::JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
    port: u16,
}
pub struct ImageAccess {
    settings: Mutex<Settings>,
    path: PathBuf,
    listener: AsyncMutex<Option<Listener>>,
}
fn generate_key() -> String {
    format!(
        "cxi-{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
fn validate_key(key: &str) -> Result<()> {
    if !(16..=256).contains(&key.len()) || !key.bytes().all(|b| b.is_ascii_graphic()) {
        bail!("图片生成 Key 须为 16–256 位不含空白的 ASCII 字符");
    }
    Ok(())
}
impl ImageAccess {
    pub fn open(
        directory: &Path,
        main_port: u16,
        main_key: &str,
        chatbot: &crate::chatbot::ChatbotAccess,
    ) -> Result<Self> {
        let path = directory.join("image-access.json");
        let settings: Settings = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() && meta.len() <= 16 * 1024 => {
                serde_json::from_slice(&std::fs::read(&path)?).context("图片生成接入设置损坏")?
            }
            Ok(_) => bail!("图片生成接入文件无效"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let port = (17866..=17869)
                    .find(|p| *p != main_port && Some(*p) != chatbot.port().ok())
                    .context("没有可用的图片生成默认端口")?;
                let settings = Settings {
                    enabled: true,
                    port,
                    key: generate_key(),
                    source_id: None,
                };
                secure_write(&path, &serde_json::to_vec(&settings)?)?;
                settings
            }
            Err(_) => bail!("无法读取图片生成接入设置"),
        };
        validate_key(&settings.key)?;
        if settings.port == 0 || settings.key == main_key || chatbot.uses_key(&settings.key)? {
            bail!("图片生成接入设置与其他入口冲突");
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
            .map_err(|_| anyhow::anyhow!("图片生成接入忙碌"))
    }
    pub(crate) fn port(&self) -> Result<u16> {
        Ok(self.settings()?.port)
    }
    pub(crate) fn uses_key(&self, key: &str) -> Result<bool> {
        Ok(self.settings()?.key == key)
    }
    pub fn key(&self) -> Result<Value> {
        Ok(json!({"key":self.settings()?.key}))
    }
    pub async fn status(&self) -> Result<Value> {
        let listener = self.listener.lock().await;
        let s = self.settings()?;
        Ok(
            json!({"enabled":s.enabled,"running":listener.as_ref().is_some_and(|l|!l.task.is_finished()),"port":s.port,"base_url":format!("http://127.0.0.1:{}/v1",s.port),"key_set":true,"source_id":s.source_id,"model":MODEL}),
        )
    }
}
pub async fn configure(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    let mut listener = state.images.listener.lock().await;
    let mut next = state.images.settings()?;
    if let Some(v) = payload.get("enabled") {
        next.enabled = v.as_bool().context("请指定是否启用图片生成")?;
    }
    if let Some(v) = payload.get("port") {
        next.port = v
            .as_u64()
            .filter(|p| (1..=65535).contains(p))
            .context("图片生成端口须为 1–65535")? as u16;
    }
    if next.port == state.port || next.port == state.chatbot.port()? {
        bail!("图片生成端口不能与其他入口相同");
    }
    if payload["regenerate_key"] == true {
        next.key = generate_key();
    } else if let Some(v) = payload.get("key") {
        next.key = v.as_str().context("图片生成 Key 格式无效")?.into();
    }
    validate_key(&next.key)?;
    if next.key == state.gateway_key
        || state.chatbot.uses_key(&next.key)?
        || state.remote.uses_key(&next.key)?
        || state
            .sources
            .read()
            .await
            .iter()
            .any(|r| r.source.secret["api_key"].as_str() == Some(&next.key))
    {
        bail!("请为图片生成使用独立 Key");
    }
    if let Some(v) = payload.get("source_id") {
        next.source_id = if v.is_null() {
            None
        } else {
            Some(
                v.as_str()
                    .filter(|s| !s.is_empty())
                    .context("官方账号无效")?
                    .into(),
            )
        };
        if let Some(id) = &next.source_id {
            if !state
                .sources
                .read()
                .await
                .iter()
                .any(|r| r.source.id == *id && r.source.kind == "official")
            {
                bail!("请选择已有的官方账号");
            }
        }
    }
    let current = listener
        .as_ref()
        .is_some_and(|l| l.port == next.port && !l.task.is_finished());
    let socket = if next.enabled && !current {
        Some(
            tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, next.port))
                .await
                .context("图片生成端口被占用，请更换端口")?,
        )
    } else {
        None
    };
    secure_write(&state.images.path, &serde_json::to_vec(&next)?)?;
    *state
        .images
        .settings
        .lock()
        .map_err(|_| anyhow::anyhow!("图片生成接入忙碌"))? = next.clone();
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
            task,
            shutdown,
            port: next.port,
        });
    } else if !next.enabled {
        if let Some(old) = listener.take() {
            let _ = old.shutdown.send(());
        }
    }
    drop(listener);
    state.images.status().await
}
pub async fn resume(state: &Arc<AppState>) -> Result<()> {
    configure(state, json!({})).await.map(|_| ())
}
fn error(status: StatusCode, message: &str) -> Response {
    chatbot::error(status, message, None)
}
async fn gate(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let s = match state.images.settings() {
        Ok(s) => s,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "图片生成接入不可用"),
    };
    let hosts: Vec<_> = request.headers().get_all(header::HOST).iter().collect();
    if hosts.len() != 1
        || !hosts[0].to_str().ok().is_some_and(|h| {
            [
                format!("127.0.0.1:{}", s.port),
                format!("localhost:{}", s.port),
            ]
            .iter()
            .any(|v| h.eq_ignore_ascii_case(v))
        })
    {
        return error(StatusCode::MISDIRECTED_REQUEST, "图片生成地址与端口不匹配");
    }
    if !s.enabled
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
            != Some(&s.key)
    {
        return error(StatusCode::UNAUTHORIZED, "图片生成 API Key 无效");
    }
    next.run(request).await
}
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/models", get(models))
        .route("/models", get(models))
        .route("/v1/images/generations", post(generate))
        .route("/images/generations", post(generate))
        .route("/v1/images/edits", post(edit))
        .route("/images/edits", post(edit))
        .layer(DefaultBodyLimit::max(REQUEST_LIMIT))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, gate))
}
async fn models() -> Json<Value> {
    Json(json!({"object":"list","data":[{"id":MODEL,"object":"model","owned_by":"openai"}]}))
}
async fn generate(State(state): State<Arc<AppState>>, request: Request) -> Response {
    handle(state, request, false).await
}
async fn edit(State(state): State<Arc<AppState>>, request: Request) -> Response {
    handle(state, request, true).await
}
async fn candidates(state: &Arc<AppState>) -> Vec<Source> {
    let selected = state.images.settings().ok().and_then(|s| s.source_id);
    let threshold = state.settings.read().await.official_low_quota_percent;
    let sources = state.sources.read().await;
    let mut candidates: Vec<_> = sources
        .iter()
        .filter(|r| {
            r.source.kind == "official"
                && selected.as_ref().is_none_or(|id| *id == r.source.id)
                && routing::admission(state, r, &sources, threshold)["accepting_new_conversations"]
                    == true
        })
        .map(|r| r.source.clone())
        .collect();
    candidates.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.id.cmp(&b.id)));
    candidates
}
struct Attempt {
    state: Arc<AppState>,
    record: Option<UsageRecord>,
    started: Instant,
}
impl Attempt {
    fn new(state: Arc<AppState>, source: &Source, request_id: &str) -> Self {
        let record = UsageRecord {
            attempt_id: uuid::Uuid::new_v4().to_string(),
            request_id: request_id.into(),
            source_id: source.id.clone(),
            source_name: source.name.clone(),
            model: MODEL.into(),
            started_at: now(),
            status: "connecting".into(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            token_details: TokenDetails {
                source_kind: Some("official".into()),
                ..Default::default()
            },
            complete: false,
            latency_ms: 0,
            first_token_latency_ms: None,
            generation_ms: None,
        };
        state.inflight.fetch_add(1, Ordering::Relaxed);
        state.usage.start(&record);
        Self {
            state,
            record: Some(record),
            started: Instant::now(),
        }
    }
    fn finish(&mut self, status: &str) {
        if let Some(mut record) = self.record.take() {
            record.status = status.into();
            record.complete = status == "success";
            record.token_details.stream_complete = record.complete;
            record.latency_ms = self.started.elapsed().as_millis() as u64;
            let _ = self.state.usage.record(record);
            self.state.inflight.fetch_sub(1, Ordering::Relaxed);
        }
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        self.finish("interrupted");
    }
}
async fn handle(state: Arc<AppState>, request: Request, edit: bool) -> Response {
    let (parts, body) = request.into_parts();
    #[cfg(test)]
    let target_override = parts.extensions.get::<TestTarget>().cloned();
    let mime = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let bytes = match to_bytes(body, REQUEST_LIMIT).await {
        Ok(b) => b,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "图片请求过大或无法读取"),
    };
    let payload = match payload::adapt(&mime, bytes, edit, parts.extensions).await {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let request_id = uuid::Uuid::new_v4().to_string();
    for candidate in candidates(&state).await {
        let source = match crate::official::ensure_access(&state, &candidate.id).await {
            Ok(s) if s.identity() == candidate.identity() => s,
            _ => continue,
        };
        let mut lease = {
            let threshold = state.settings.read().await.official_low_quota_percent;
            let sources = state.sources.read().await;
            let Some(runtime) = sources.iter().find(|r| {
                r.source.id == source.id
                    && r.source.identity() == source.identity()
                    && r.source.proxy == source.proxy
            }) else {
                continue;
            };
            if routing::admission(&state, runtime, &sources, threshold)
                ["accepting_new_conversations"]
                != true
            {
                continue;
            }
            let low = routing::account_remaining(runtime, &sources)
                .is_some_and(|v| v <= f64::from(threshold));
            let Some(lease) = state.affinity.reserve(None, &source, false, low, None) else {
                continue;
            };
            lease
        };
        // Recheck the independent selection and enabled switch after OAuth waits.
        {
            let selected = state.images.settings().ok().and_then(|s| s.source_id);
            let sources = state.sources.read().await;
            if !sources.iter().any(|r| {
                r.source.id == source.id
                    && r.source.kind == "official"
                    && r.source.enabled
                    && r.source.identity() == source.identity()
                    && r.source.proxy == source.proxy
                    && selected.as_ref().is_none_or(|id| id == &source.id)
                    && routing::account_remaining(r, &sources).is_none_or(|v| v > 0.0)
                    && (r.health == "ready"
                        || matches!(r.health.as_str(), "cooldown" | "proxy_error" | "exhausted")
                            && r.cooldown_until.is_some_and(|until| until <= now()))
            }) {
                continue;
            }
        }
        let client = match transport::client(&source) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let endpoint = format!(
            "{}/images/{}",
            source.base_url.trim_end_matches('/'),
            if edit { "edits" } else { "generations" }
        );
        #[cfg(test)]
        let endpoint = target_override
            .as_ref()
            .map(|target| {
                format!(
                    "{}/images/{}",
                    target.0,
                    if edit { "edits" } else { "generations" }
                )
            })
            .unwrap_or(endpoint);
        let mut headers = axum::http::HeaderMap::new();
        let Some(token) = source.secret["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let Ok(auth) = HeaderValue::from_str(&format!("Bearer {token}")) else {
            continue;
        };
        headers.insert(header::AUTHORIZATION, auth);
        headers.insert("originator", HeaderValue::from_static("codex_cli_rs"));
        headers.insert("version", HeaderValue::from_static("0.162.1"));
        headers.insert(
            "x-codex-image-turn-id",
            HeaderValue::from_str(&request_id).unwrap(),
        );
        if let Some(account) = source
            .account_id
            .as_deref()
            .or_else(|| source.secret["account_id"].as_str())
        {
            let Ok(value) = HeaderValue::from_str(account) else {
                continue;
            };
            headers.insert("chatgpt-account-id", value);
        }
        let mut attempt = Attempt::new(state.clone(), &source, &request_id);
        let response = client
            .post(endpoint)
            .headers(headers)
            .json(&payload)
            .timeout(Duration::from_secs(300))
            .send()
            .await;
        let mut upstream = match response {
            Ok(r) => r,
            Err(_) => {
                attempt.finish("failed");
                return error(
                    StatusCode::BAD_GATEWAY,
                    "官方生图连接中断或超时，结果未知，请勿自动重试",
                );
            }
        };
        let status = upstream.status();
        if status.as_u16() == 429 {
            attempt.finish("failed");
            routing::mark_failure(&state, &source.id, 429, None).await;
            continue;
        }
        if !status.is_success() {
            attempt.finish("failed");
            return error(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                match status.as_u16() {
                    401 => "官方生图认证失败，请重新登录该账号",
                    403 => "该账号或工作区未开放生图权限",
                    400 => "官方拒绝生图参数或内容，请调整后重新提交",
                    _ => "官方生图服务返回错误，未自动重试",
                },
            );
        }
        let _ = lease.commit();
        let raw = match transport::read_limited(&mut upstream, RESPONSE_LIMIT).await {
            Ok(raw) => raw,
            Err(_) => {
                attempt.finish("failed");
                return error(
                    StatusCode::BAD_GATEWAY,
                    "图片响应过大或传输中断，未自动重试",
                );
            }
        };
        let result: Value = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(_) => {
                attempt.finish("failed");
                return error(StatusCode::BAD_GATEWAY, "官方返回了无法识别的图片响应");
            }
        };
        let Some(data) = result["data"]
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= 10)
        else {
            attempt.finish("failed");
            return error(StatusCode::BAD_GATEWAY, "官方未返回图片数据");
        };
        if !data
            .iter()
            .all(|v| v["b64_json"].as_str().is_some_and(|s| !s.is_empty()))
        {
            attempt.finish("failed");
            return error(StatusCode::BAD_GATEWAY, "官方未返回可保存的图片内容");
        }
        let clean: Vec<Value> = data
            .iter()
            .map(|item| {
                let mut out = json!({"b64_json":item["b64_json"]});
                for key in ["generation_id", "revised_prompt"] {
                    if item[key].is_string() {
                        out[key] = item[key].clone();
                    }
                }
                out
            })
            .collect();
        let mut result_out =
            json!({"created":result["created"].as_u64().unwrap_or_else(now),"data":clean});
        for key in ["size", "quality", "background", "usage"] {
            if !result[key].is_null() {
                result_out[key] = result[key].clone();
            }
        }
        attempt.finish("success");
        return Json(result_out).into_response();
    }
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "没有可用的官方生图账号，请检查登录、开关、代理及剩余额度",
    )
}
use axum::response::IntoResponse;

#[cfg(test)]
#[path = "images/tests.rs"]
mod tests;
