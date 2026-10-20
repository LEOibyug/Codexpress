use crate::{
    model::{now, AppState, Source},
    routing, transport,
    usage::UsageRecord,
};
use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode},
    routing::{get, post},
    Router,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{atomic::Ordering, Arc},
    time::Instant,
};

const BODY_LIMIT: usize = 32 * 1024 * 1024;
const OBSERVATION_LIMIT: usize = 1024 * 1024;

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

struct Attempt {
    state: Arc<AppState>,
    record: Option<UsageRecord>,
    started: Instant,
}
impl Attempt {
    fn new(state: Arc<AppState>, source: &Source, request_id: &str, model: &str) -> Self {
        state.inflight.fetch_add(1, Ordering::Relaxed);
        Self {
            state,
            started: Instant::now(),
            record: Some(UsageRecord {
                request_id: request_id.into(),
                source_id: source.id.clone(),
                source_name: source.name.clone(),
                model: model.into(),
                started_at: now(),
                status: "interrupted".into(),
                input_tokens: 0,
                output_tokens: 0,
                cached_input_tokens: 0,
                complete: false,
                latency_ms: 0,
            }),
        }
    }
    fn finish(&mut self, status: &str, observer: Option<&Observer>) {
        if let Some(record) = &mut self.record {
            record.status = status.into();
            if let Some(observer) = observer {
                record.input_tokens = observer.input;
                record.output_tokens = observer.output;
                record.cached_input_tokens = observer.cached.min(observer.input);
                record.complete = observer.usage_seen && observer.terminal;
            }
        }
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        if let Some(mut record) = self.record.take() {
            record.latency_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            // Persistence failures cannot justify replaying a billable request.
            // No upstream data or database error is logged here.
            if self.state.usage.record(record).is_err() {
                eprintln!("用量记录写入失败");
            }
        }
        self.state.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Observation is bounded and independent of forwarded bytes. Oversized or
/// compressed frames still pass through; their usage remains explicitly unknown.
#[derive(Default)]
struct Observer {
    buffer: Vec<u8>,
    line: Vec<u8>,
    data: Vec<u8>,
    discarding_line: bool,
    discarding_event: bool,
    oversized: bool,
    sse: bool,
    terminal: bool,
    failed: bool,
    failure_code: Option<String>,
    usage_seen: bool,
    input: u64,
    output: u64,
    cached: u64,
    ids: Vec<String>,
}
impl Observer {
    fn value(&mut self, value: &Value) {
        let mut opaque_keys = Vec::new();
        opaque_bindings(value, &mut opaque_keys);
        for key in opaque_keys {
            if !self.ids.contains(&key) {
                self.ids.push(key);
            }
        }
        let response = value.get("response").unwrap_or(value);
        if let Some(id) = response
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| id.len() <= 512)
        {
            if !self.ids.iter().any(|v| v == id) {
                self.ids.push(id.into());
            }
        }
        if let Some(conversation) = response
            .get("conversation")
            .and_then(|v| v.as_str().or_else(|| v.get("id").and_then(Value::as_str)))
            .filter(|id| id.len() <= 512)
        {
            if !self.ids.iter().any(|id| id == conversation) {
                self.ids.push(conversation.into());
            }
        }
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(
            event_type,
            "response.completed" | "response.incomplete" | "response.failed"
        ) || !self.sse
        {
            self.terminal = true;
        }
        if matches!(event_type, "response.failed" | "error")
            || response.get("status").and_then(Value::as_str) == Some("failed")
        {
            self.failed = true;
            self.failure_code = response
                .pointer("/error/code")
                .or_else(|| value.get("code"))
                .and_then(Value::as_str)
                .filter(|code| code.len() <= 128)
                .map(String::from);
        }
        if let Some(usage) = response.get("usage").filter(|u| u.is_object()) {
            // Snapshot values replace prior observations; final usage includes
            // cached input, so cached tokens must never be added to input.
            if let (Some(input), Some(output)) = (
                usage.get("input_tokens").and_then(Value::as_u64),
                usage.get("output_tokens").and_then(Value::as_u64),
            ) {
                self.input = input;
                self.output = output;
                self.cached = usage
                    .pointer("/input_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                self.usage_seen = true;
            }
        }
    }
    fn event(&mut self) {
        if !self.discarding_event {
            if let Ok(value) = serde_json::from_slice::<Value>(&self.data) {
                self.value(&value);
            }
        }
        self.data.clear();
        self.discarding_event = false;
    }
    fn push(&mut self, bytes: &[u8]) {
        if !self.sse {
            if !self.oversized && self.buffer.len().saturating_add(bytes.len()) <= OBSERVATION_LIMIT
            {
                self.buffer.extend_from_slice(bytes);
            } else {
                self.oversized = true;
                self.buffer.clear();
            }
            return;
        }
        for &byte in bytes {
            if byte == b'\n' {
                if self.discarding_line {
                    self.discarding_event = true;
                } else {
                    if self.line.last() == Some(&b'\r') {
                        self.line.pop();
                    }
                    if self.line.is_empty() {
                        self.event();
                    } else if let Some(raw) = self.line.strip_prefix(b"data:") {
                        let raw = raw.strip_prefix(b" ").unwrap_or(raw);
                        if self.data.len().saturating_add(raw.len() + 1) <= OBSERVATION_LIMIT {
                            if !self.data.is_empty() {
                                self.data.push(b'\n');
                            }
                            self.data.extend_from_slice(raw);
                        } else {
                            self.discarding_event = true;
                            self.data.clear();
                        }
                    }
                }
                self.line.clear();
                self.discarding_line = false;
            } else if !self.discarding_line {
                if self.line.len() < OBSERVATION_LIMIT {
                    self.line.push(byte);
                } else {
                    self.line.clear();
                    self.discarding_line = true;
                }
            }
        }
    }
    fn eof(&mut self) {
        if self.sse {
            // An SSE event is dispatched only at its blank-line boundary.
        } else if !self.oversized {
            if let Ok(value) = serde_json::from_slice::<Value>(&self.buffer) {
                self.value(&value);
            }
        }
    }
}

async fn bind_ids(state: &Arc<AppState>, source: &str, observer: &mut Observer) {
    if observer.ids.is_empty() {
        return;
    }
    let mut bindings = state.bindings.lock().await;
    // Bound runtime metadata too: do not retain an unbounded response-id history.
    if bindings.len() > 100_000 {
        bindings.clear();
    }
    for id in observer.ids.drain(..) {
        bindings.insert(id, source.into());
    }
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
            for value in object.values() {
                opaque_bindings(value, keys);
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

async fn forward(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response<Body> {
    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.strip_prefix("Bearer ") == Some(state.gateway_key.as_str()));
    if !authorized {
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
    if !valid_host {
        return error(
            StatusCode::MISDIRECTED_REQUEST,
            "网关只接受匹配端口的本机 Host",
        );
    }
    if request.headers().contains_key("origin") {
        return error(StatusCode::FORBIDDEN, "网关不接受浏览器跨站请求");
    }
    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, BODY_LIMIT).await {
        Ok(body) => body,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "请求正文过大或无法读取"),
    };
    let discovery = parts.method == axum::http::Method::GET;
    let parsed = if discovery {
        json!({})
    } else {
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) if value.is_object() => value,
            _ => return error(StatusCode::BAD_REQUEST, "Responses 请求需要 JSON 对象"),
        }
    };
    let model = parsed.get("model").and_then(Value::as_str).unwrap_or("");
    let mut opaque_keys = Vec::new();
    opaque_bindings(&parsed, &mut opaque_keys);
    let session_value = parsed
        .get("session_id")
        .or_else(|| parsed.get("conversation_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            parts
                .headers
                .get("x-session-id")
                .or_else(|| parts.headers.get("session_id"))
                .and_then(|v| v.to_str().ok())
        });
    let session_key = session_value
        .filter(|id| !id.is_empty() && id.len() <= 512)
        .map(|id| format!("session:{id}"));
    let session_known = if let Some(key) = &session_key {
        state.bindings.lock().await.contains_key(key)
    } else {
        false
    };
    let opaque_state = !opaque_keys.is_empty();
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
        .or_else(|| {
            parts
                .headers
                .get("x-codex-turn-state")
                .and_then(|v| v.to_str().ok())
        })
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
    let mut sources = routing::candidates(&state, model, binding).await;
    if opaque_state {
        let bindings = state.bindings.lock().await;
        sources.retain(|source| {
            opaque_keys.iter().all(|key| match bindings.get(key) {
                Some(id) => id == &source.id,
                // A known previous response, turn state, session or another
                // opaque item has already established the single source.
                None => true,
            })
        });
    }
    if opaque_state && session_known {
        let bindings = state.bindings.lock().await;
        sources.retain(|source| {
            session_key.as_ref().and_then(|key| bindings.get(key)) == Some(&source.id)
        });
    }
    if sources.is_empty() {
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
    let mut last_error = None;
    for candidate in sources {
        if candidate.kind == "official" && parts.uri.path() != "/v1/responses" {
            last_error = Some(error(StatusCode::NOT_IMPLEMENTED, "官方来源尚未验证此端点"));
            continue;
        }
        let mut source = if candidate.kind == "official" {
            match crate::official::ensure_access(&state, &candidate.id).await {
                Ok(source) => source,
                Err(_) => {
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
        for authentication_attempt in 0..2 {
            let mut attempt = Attempt::new(state.clone(), &source, &request_id, model);
            let client = match transport::client(&source) {
                Ok(client) => client,
                Err(_) => {
                    attempt.finish("failed", None);
                    routing::mark_failure(&state, &source.id, 0, None).await;
                    last_error = Some(error(StatusCode::BAD_GATEWAY, "来源网络策略无法建立连接"));
                    break;
                }
            };
            let url = match target(&source, parts.uri.path(), parts.uri.query()) {
                Ok(url) => url,
                Err(_) => return error(StatusCode::BAD_GATEWAY, "来源地址无效"),
            };
            let mut headers = clean_headers(&parts.headers, true);
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
                if let Some(account) = &source.account_id {
                    let Ok(value) = HeaderValue::from_str(account) else {
                        return error(StatusCode::BAD_GATEWAY, "来源账号标识无效");
                    };
                    headers.insert(HeaderName::from_static("chatgpt-account-id"), value);
                }
            }
            let upstream = tokio::time::timeout(
                std::time::Duration::from_secs(120),
                client
                    .request(parts.method.clone(), url)
                    .headers(headers)
                    .body(bytes.clone())
                    .send(),
            )
            .await;
            let upstream = match upstream {
                Ok(result) => result,
                Err(_) => {
                    attempt.finish("failed", None);
                    routing::mark_failure(&state, &source.id, 504, None).await;
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
                    attempt.finish("failed", None);
                    routing::mark_failure(&state, &source.id, 0, None).await;
                    if !definitely_unsubmitted || opaque_state {
                        return error(
                            StatusCode::BAD_GATEWAY,
                            "来源响应中断，提交结果不明确；未自动重试",
                        );
                    }
                    last_error = Some(error(StatusCode::BAD_GATEWAY, "无法连接来源或指定代理"));
                    break;
                }
            };
            let status = upstream.status();
            let retry_after = upstream
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            if source.kind == "official"
                && status == StatusCode::UNAUTHORIZED
                && authentication_attempt == 0
            {
                attempt.finish("failed", None);
                drop(upstream);
                match crate::official::refresh_access(&state, &source.id).await {
                    Ok(fresh) => {
                        source = fresh;
                        continue;
                    }
                    Err(_) => {
                        last_error = Some(error(
                            StatusCode::BAD_GATEWAY,
                            "官方来源认证刷新失败，请检查登录或网络",
                        ));
                        break;
                    }
                }
            }
            let mut prefix = Vec::new();
            if matches!(status.as_u16(), 401 | 402 | 403 | 429 | 500..=599) {
                routing::mark_failure(&state, &source.id, status.as_u16(), retry_after).await;
                if matches!(status.as_u16(), 402 | 403 | 429) {
                    // Read only a bounded error prefix, retaining the original
                    // chunks for exact forwarding if this is the final source.
                    let mut observed = Vec::new();
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                    while observed.len() < 65536 {
                        match tokio::time::timeout_at(deadline, upstream.chunk()).await {
                            Ok(Ok(Some(chunk))) => {
                                let remaining = 65536 - observed.len();
                                observed.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                                prefix.push(chunk);
                                if serde_json::from_slice::<Value>(&observed).is_ok() {
                                    break;
                                }
                            }
                            _ => break,
                        }
                    }
                    if let Ok(value) = serde_json::from_slice::<Value>(&observed) {
                        let code = value
                            .pointer("/error/code")
                            .or_else(|| value.pointer("/error/type"))
                            .and_then(Value::as_str);
                        routing::mark_rejection(&state, &source.id, code, retry_after).await;
                    }
                }
            }
            let safe_to_fallback = binding.is_none()
                && !opaque_state
                && matches!(status.as_u16(), 401 | 402 | 403 | 429);
            if safe_to_fallback {
                attempt.finish("failed", None);
                // Preserve the final source's native error body when there is no
                // later candidate; streaming this response must not trigger replay.
                last_error = Some(stream_response(
                    state.clone(),
                    source.id.clone(),
                    upstream,
                    prefix,
                    attempt,
                    discovery,
                    session_key.clone(),
                ));
                break;
            }
            if status.is_success() {
                routing::mark_ready(&state, &source.id).await;
            }
            return stream_response(
                state,
                source.id.clone(),
                upstream,
                prefix,
                attempt,
                discovery,
                session_key.clone(),
            );
        }
    }
    last_error.unwrap_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "所有来源暂时不可用"))
}

fn stream_response(
    state: Arc<AppState>,
    source_id: String,
    upstream: reqwest::Response,
    prefix: Vec<Bytes>,
    mut attempt: Attempt,
    discovery: bool,
    session_key: Option<String>,
) -> Response<Body> {
    let status = upstream.status();
    let headers = clean_headers(upstream.headers(), false);
    let sse = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    let turn_state = headers
        .get("x-codex-turn-state")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() <= 512)
        .map(String::from);
    let stream = async_stream::stream! {
        let prefix = futures_util::stream::iter(prefix.into_iter().map(Ok));
        let mut upstream = prefix.chain(upstream.bytes_stream());
        let mut observer = Observer { sse, ..Default::default() };
        if status.is_success() { if let Some(session_key) = session_key { observer.ids.push(session_key); } }
        if let Some(turn_state) = turn_state { observer.ids.push(turn_state); }
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(chunk) => {
                    observer.push(&chunk);
                    bind_ids(&state, &source_id, &mut observer).await;
                    // Update guard before yielding: dropping the downstream body
                    // immediately records the last known usage and cancels reqwest.
                    attempt.finish("interrupted", Some(&observer));
                    yield Ok::<Bytes, std::io::Error>(chunk);
                },
                Err(_) => {
                    attempt.finish("interrupted", Some(&observer));
                    yield Err(std::io::Error::other("来源流已中断"));
                    return;
                }
            }
        }
        observer.eof();
        if observer.failed {
            routing::mark_failure(&state, &source_id, 500, None).await;
            routing::mark_rejection(&state, &source_id, observer.failure_code.as_deref(), None).await;
        }
        bind_ids(&state, &source_id, &mut observer).await;
        let outcome = if !status.is_success() || observer.failed { "failed" }
            else if observer.sse && !observer.terminal { "interrupted" }
            else { "success" };
        attempt.finish(outcome, Some(&observer));
        if discovery { if let Some(record) = &mut attempt.record { record.complete = true; } }
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}
