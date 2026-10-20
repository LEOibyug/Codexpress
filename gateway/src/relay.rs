use crate::{
    model::{now, AppState, Source},
    routing, transport,
    usage::{content_units, estimated_tokens, text_units, TokenDetails, UsageRecord},
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
    collections::{BTreeMap, HashMap, HashSet},
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
    published: Instant,
    first_output: Option<Instant>,
}
impl Attempt {
    fn new(
        state: Arc<AppState>,
        source: &Source,
        request_id: &str,
        model: &str,
        input_units: u64,
    ) -> Self {
        state.inflight.fetch_add(1, Ordering::Relaxed);
        let record = UsageRecord {
            attempt_id: uuid::Uuid::new_v4().to_string(),
            request_id: request_id.into(),
            source_id: source.id.clone(),
            source_name: source.name.clone(),
            model: model.into(),
            started_at: now(),
            status: "connecting".into(),
            input_tokens: estimated_tokens(input_units),
            output_tokens: 0,
            cached_input_tokens: 0,
            token_details: TokenDetails {
                input_estimated: true,
                output_estimated: true,
                ..Default::default()
            },
            complete: false,
            latency_ms: 0,
            first_token_latency_ms: None,
            generation_ms: None,
        };
        state.usage.start(&record);
        Self {
            state,
            started: Instant::now(),
            published: Instant::now(),
            record: Some(record),
            first_output: None,
        }
    }
    fn finish(&mut self, status: &str, observer: Option<&Observer>) {
        if let Some(record) = &mut self.record {
            let mut changed = record.status != status;
            record.status = status.into();
            if let Some(observer) = observer {
                changed |= observer.terminal && !record.token_details.stream_complete;
                if observer.streaming_output && self.first_output.is_none() {
                    changed = true;
                    let first = Instant::now();
                    record.first_token_latency_ms = Some(
                        first
                            .duration_since(self.started)
                            .as_millis()
                            .min(u64::MAX as u128) as u64,
                    );
                    self.first_output = Some(first);
                }
                if let Some(first) = self.first_output {
                    if !record.token_details.stream_complete {
                        record.generation_ms =
                            Some(first.elapsed().as_millis().min(u64::MAX as u128) as u64);
                    }
                }
                if let Some(input) = observer.input {
                    record.input_tokens = input;
                }
                record.output_tokens = observer
                    .output
                    .unwrap_or_else(|| estimated_tokens(observer.output_units()));
                record.cached_input_tokens = observer
                    .cached
                    .map(|cached| observer.input.map_or(cached, |input| cached.min(input)))
                    .unwrap_or(0);
                record.token_details = TokenDetails {
                    input_estimated: observer.input.is_none(),
                    output_estimated: observer.output.is_none(),
                    output_available: observer.output.is_some() || observer.output_observed,
                    cached_input_reported: observer.cached.is_some(),
                    cache_write_input_tokens: observer.cache_write,
                    reasoning_output_tokens: observer.reasoning,
                    reported_total_tokens: observer.total,
                    stream_complete: observer.terminal,
                };
                record.complete =
                    observer.input.is_some() && observer.output.is_some() && observer.terminal;
            }
            if changed || status != "streaming" || self.published.elapsed().as_millis() >= 100 {
                self.state.usage.update(record);
                self.published = Instant::now();
            }
        }
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        if let Some(mut record) = self.record.take() {
            if matches!(record.status.as_str(), "connecting" | "streaming") {
                record.status = "interrupted".into();
            }
            record.latency_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            if let Some(first) = self
                .first_output
                .filter(|_| !record.token_details.stream_complete)
            {
                record.generation_ms =
                    Some(first.elapsed().as_millis().min(u64::MAX as u128) as u64);
            }
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
/// compressed frames still pass through; unavailable usage is not invented.
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
    input: Option<u64>,
    output: Option<u64>,
    cached: Option<u64>,
    cache_write: Option<u64>,
    reasoning: Option<u64>,
    total: Option<u64>,
    delta_units: HashMap<String, u64>,
    item_units: HashMap<String, u64>,
    final_units: Option<u64>,
    output_observed: bool,
    streaming_output: bool,
    ids: Vec<String>,
    history_request: Option<HistoryCapture>,
    history_response: Option<Value>,
    history_items: BTreeMap<u64, Value>,
    history_bytes: usize,
    history_invalid: bool,
}
impl Observer {
    fn output_units(&self) -> u64 {
        self.final_units.unwrap_or_else(|| {
            let keys: HashSet<_> = self
                .delta_units
                .keys()
                .chain(self.item_units.keys())
                .collect();
            keys.into_iter()
                .map(|key| {
                    self.delta_units
                        .get(key)
                        .copied()
                        .unwrap_or(0)
                        .max(self.item_units.get(key).copied().unwrap_or(0))
                })
                .sum()
        })
    }
    fn output_key(value: &Value) -> String {
        value
            .get("output_index")
            .map(Value::to_string)
            .or_else(|| {
                value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .or_else(|| {
                value
                    .pointer("/item/id")
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .unwrap_or_else(|| "0".into())
    }
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
        if self.history_request.is_some() && event_type == "response.output_item.done" {
            if let (Some(index), Some(item)) = (value["output_index"].as_u64(), value.get("item")) {
                self.history_bytes = self.history_bytes.saturating_add(item.to_string().len());
                if self.history_bytes <= OBSERVATION_LIMIT {
                    self.history_items.insert(index, item.clone());
                } else {
                    self.history_invalid = true;
                    self.history_items.clear();
                }
            } else {
                self.history_invalid = true;
            }
        }
        if self.history_request.is_some()
            && !self.history_invalid
            && (!self.sse || event_type == "response.completed")
            && response.get("output").is_some_and(Value::is_array)
        {
            let mut snapshot = response.clone();
            if snapshot["output"].as_array().is_some_and(Vec::is_empty)
                && !self.history_items.is_empty()
            {
                if self
                    .history_items
                    .keys()
                    .copied()
                    .eq(0..self.history_items.len() as u64)
                {
                    snapshot["output"] =
                        Value::Array(self.history_items.values().cloned().collect());
                } else {
                    self.history_invalid = true;
                }
            } else if snapshot["output"].as_array().is_some_and(Vec::is_empty)
                && self.streaming_output
            {
                self.history_invalid = true;
            }
            if !self.history_invalid {
                self.history_response = Some(snapshot);
            }
        }
        if matches!(
            event_type,
            "response.output_text.delta"
                | "response.refusal.delta"
                | "response.function_call_arguments.delta"
                | "response.reasoning_summary_text.delta"
                | "response.reasoning_text.delta"
                | "response.custom_tool_call_input.delta"
        ) {
            if let Some(text) = value.get("delta").and_then(Value::as_str) {
                let key = Self::output_key(value);
                if self.delta_units.len() < 10000 || self.delta_units.contains_key(&key) {
                    let units = self.delta_units.entry(key).or_default();
                    *units = units.saturating_add(text_units(text));
                }
                self.output_observed = true;
                if !text.is_empty() {
                    self.streaming_output = true;
                }
            }
        }
        if event_type == "response.output_item.done" {
            if let Some(item) = value.get("item") {
                let key = Self::output_key(value);
                if self.item_units.len() < 10000 {
                    self.item_units.insert(key, content_units(item));
                }
                self.output_observed = true;
            }
        }
        if (!self.sse
            || matches!(
                event_type,
                "response.completed" | "response.incomplete" | "response.failed"
            ))
            && response.get("output").is_some_and(Value::is_array)
        {
            self.final_units = Some(content_units(&response["output"]));
            self.output_observed = true;
        }
        if event_type == "response.metadata" {
            if let Some(headers) = value.get("headers").and_then(Value::as_object) {
                for (name, value) in headers {
                    if name.eq_ignore_ascii_case("x-codex-turn-state") {
                        if let Some(token) = value.as_str() {
                            self.ids.push(turn_binding(token));
                        }
                    }
                }
            }
        }
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
            self.input = usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .or(self.input);
            self.output = usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .or(self.output);
            self.cached = usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
                .or(self.cached);
            self.cache_write = usage
                .pointer("/input_tokens_details/cache_write_tokens")
                .and_then(Value::as_u64)
                .or(self.cache_write);
            self.reasoning = usage
                .pointer("/output_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64)
                .or(self.reasoning);
            self.total = usage
                .get("total_tokens")
                .and_then(Value::as_u64)
                .or(self.total);
        }
    }
    fn event(&mut self) {
        if self.discarding_event {
            self.history_invalid = true;
        }
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

async fn bind_ids(state: &Arc<AppState>, source: &Source, observer: &mut Observer) {
    if observer.ids.is_empty() {
        return;
    }
    let sources = state.sources.read().await;
    if !sources.iter().any(|runtime| {
        runtime.source.id == source.id && runtime.source.identity() == source.identity()
    }) {
        observer.ids.clear();
        return;
    }
    // Keep the source read lock through insertion: relogin/sync takes the write
    // lock before clearing bindings, so an old in-flight body cannot rebind it.
    let mut bindings = state.bindings.lock().await;
    // Bound runtime metadata too: do not retain an unbounded response-id history.
    if bindings.len() > 100_000 {
        bindings.clear();
    }
    for id in observer.ids.drain(..) {
        bindings.insert(id, source.id.clone());
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

fn scoped_session(parsed: &Value, parts: &axum::http::request::Parts) -> Option<String> {
    let session = session_binding(parsed, &parts.headers);
    match parts.extensions.get::<crate::remote::RemoteIngress>() {
        Some(ingress) => session
            .or_else(|| {
                parsed["prompt_cache_key"]
                    .as_str()
                    .map(|s| format!("cache:{s}"))
            })
            .map(|s| format!("remote:{}:{s}", ingress.peer_id)),
        None => session,
    }
}

async fn forward(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response<Body> {
    forward_once(state, request, None).await
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
    strip_opaque_state(&mut portable);
    for field in ["previous_response_id", "conversation", "conversation_id"] {
        portable.as_object_mut().expect("object").remove(field);
    }
    let mut headers = parts.headers.clone();
    headers.remove("x-codex-turn-state");
    headers.remove("content-length");
    headers.remove("content-md5");
    headers.remove("digest");
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
    state.notifications.fallback(
        &affinity
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
    );
    let mut retry = Request::new(Body::from(serde_json::to_vec(&portable).expect("JSON")));
    *retry.method_mut() = parts.method.clone();
    *retry.uri_mut() = parts.uri.clone();
    *retry.headers_mut() = headers;
    if let Some(ingress) = parts.extensions.get::<crate::remote::RemoteIngress>() {
        retry.extensions_mut().insert(ingress.clone());
    }
    Ok((
        retry,
        RebuildContext {
            key: affinity.map(str::to_owned),
            excluded,
            scope,
        },
    ))
}

struct RebuildContext {
    key: Option<String>,
    excluded: HashSet<String>,
    scope: String,
}

#[derive(Clone)]
struct HistoryCapture {
    scope: String,
    request: Value,
    generation: u64,
}
struct ResponseContext {
    discovery: bool,
    session_key: Option<String>,
    request_is_stream: bool,
    rebuild: Option<RebuildContext>,
    history: Option<HistoryCapture>,
}
fn history_scope(parts: &axum::http::request::Parts, parsed: &Value) -> String {
    let session = scoped_session(parsed, parts).or_else(|| {
        parsed["prompt_cache_key"]
            .as_str()
            .map(|s| format!("cache:{s}"))
    });
    let credential = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
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
    let input = Value::Array(items);
    if serde_json::to_vec(&input).map_or(true, |v| v.len() > BODY_LIMIT) {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "恢复后的上下文超过 32 MiB，请先压缩上下文。",
        ));
    }
    Ok(input)
}

async fn forward_once(
    state: Arc<AppState>,
    request: Request<Body>,
    rebuild: Option<RebuildContext>,
) -> Response<Body> {
    let degraded = rebuild.is_some();
    let remote_ingress = request.extensions().get::<crate::remote::RemoteIngress>();
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
    if !valid_host && remote_ingress.is_none() {
        return error(
            StatusCode::MISDIRECTED_REQUEST,
            "网关只接受匹配端口的本机 Host",
        );
    }
    if request.headers().contains_key("origin") {
        return error(StatusCode::FORBIDDEN, "网关不接受浏览器跨站请求");
    }
    let (mut parts, body) = request.into_parts();
    let mut bytes = match to_bytes(body, BODY_LIMIT).await {
        Ok(body) => body,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "请求正文过大或无法读取"),
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
    let stored_key = scoped_session(&parsed, &parts)
        .or_else(|| {
            parsed["prompt_cache_key"]
                .as_str()
                .map(|s| format!("cache:{s}"))
        })
        .map(|session| crate::affinity::key(&session, parsed["model"].as_str().unwrap_or("")));
    let exclusions = stored_key
        .as_deref()
        .map(|key| state.affinity.exclusions(key))
        .unwrap_or_default();
    let previously_rebuilt = !exclusions.is_empty();
    if previously_rebuilt {
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
            parts.headers.remove("content-length");
            parts.headers.remove("digest");
            parts.headers.remove("content-md5");
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
        parsed
            .get("prompt_cache_key")
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty() && key.len() <= 4096)
            .map(|key| format!("cache:{key}"))
    });
    let affinity_key = affinity_session
        .as_deref()
        .map(|session| crate::affinity::key(session, model));
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
    let migrated_source = if previously_rebuilt {
        let runtimes = state.sources.read().await;
        let all: Vec<_> = runtimes.iter().map(|r| r.source.clone()).collect();
        stored_key
            .as_deref()
            .and_then(|key| state.affinity.preferred(key, &all))
    } else {
        None
    };
    let mut sources = if let Some(origin) = &origin {
        // A scoped legacy-provider credential explicitly confirms the original
        // account/key. It can carry old opaque state only to that exact identity.
        let all =
            routing::candidates_with_session(&state, model, None, affinity_key.as_deref()).await;
        let bindings = state.bindings.lock().await;
        all.into_iter()
            .filter(|source| {
                (migrated_source.as_ref().is_some_and(|id| id == &source.id)
                    || (migrated_source.is_none()
                        && source.id == origin.source_id
                        && source.identity() == origin.source_identity))
                    && hard_keys
                        .iter()
                        .all(|key| bindings.get(*key).is_none_or(|id| id == &source.id))
            })
            .collect()
    } else {
        if let Some(migrated) = &migrated_source {
            let all =
                routing::candidates_with_session(&state, model, None, affinity_key.as_deref())
                    .await;
            let bindings = state.bindings.lock().await;
            all.into_iter()
                .filter(|source| {
                    source.id == *migrated
                        && hard_keys
                            .iter()
                            .all(|key| bindings.get(*key).is_none_or(|id| id == &source.id))
                })
                .collect()
        } else {
            routing::candidates_with_session(&state, model, binding, affinity_key.as_deref()).await
        }
    };
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
        if !degraded && !discovery && (binding.is_some() || opaque_state) {
            let settings = state.settings.read().await.clone();
            let eligible = !routing::candidates(&state, model, None).await.is_empty();
            if eligible && settings.degraded_context_fallback {
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
                return Box::pin(forward_once(state, retry, Some(context))).await;
            }
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
            })
        } else {
            None
        };
    let mut last_error = None;
    for candidate in sources {
        if candidate.kind == "official"
            && !matches!(parts.uri.path(), "/v1/responses" | "/v1/responses/compact")
        {
            last_error = Some(error(StatusCode::NOT_IMPLEMENTED, "官方来源尚未验证此端点"));
            continue;
        }
        let mut route_lease = if discovery {
            None
        } else {
            match routing::reserve_for_model(
                &state,
                &candidate,
                affinity_key.as_deref(),
                binding.is_some() || origin.is_some(),
                model,
            )
            .await
            {
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
        let mut source = if candidate.kind == "official" {
            match crate::official::ensure_access(&state, &candidate.id).await {
                Ok(source) if source.identity() == candidate.identity() => source,
                Ok(_) => return error(StatusCode::CONFLICT, "来源身份已经变化，请重新开始请求"),
                Err(_) => {
                    routing::mark_auth_preflight_failure(&state, &candidate).await;
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
            let mut attempt = Attempt::new(state.clone(), &source, &request_id, model, input_units);
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
                    Ok(fresh) if fresh.identity() == source.identity() => {
                        source = fresh;
                        continue;
                    }
                    Ok(_) => {
                        return error(StatusCode::CONFLICT, "来源身份已经变化，请重新开始请求")
                    }
                    Err(_) => {
                        routing::mark_auth_preflight_failure(&state, &source).await;
                        if !degraded
                            && state.settings.read().await.degraded_context_fallback
                            && !routing::candidates(&state, model, None).await.is_empty()
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
                            return Box::pin(forward_once(state, retry, Some(context))).await;
                        }
                        last_error = Some(error(
                            StatusCode::BAD_GATEWAY,
                            "官方来源认证刷新失败，请检查登录或网络",
                        ));
                        break;
                    }
                }
            }
            let mut prefix = Vec::new();
            let mut context_rejected = false;
            if matches!(status.as_u16(), 400 | 404) && (binding.is_some() || opaque_state) {
                let mut observed = Vec::new();
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                while observed.len() < 65536 {
                    match tokio::time::timeout_at(deadline, upstream.chunk()).await {
                        Ok(Ok(Some(chunk))) => {
                            observed.extend_from_slice(
                                &chunk[..chunk.len().min(65536 - observed.len())],
                            );
                            prefix.push(chunk);
                            if serde_json::from_slice::<Value>(&observed).is_ok() {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                if let Ok(value) = serde_json::from_slice::<Value>(&observed) {
                    context_rejected = matches!(
                        value.pointer("/error/code").and_then(Value::as_str),
                        Some(
                            "invalid_encrypted_content"
                                | "invalid_previous_response_id"
                                | "previous_response_not_found"
                                | "response_not_found"
                                | "conversation_not_found"
                        )
                    );
                }
            }
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
            if !degraded
                && (binding.is_some() || opaque_state || origin.is_some())
                && (context_rejected || matches!(status.as_u16(), 401 | 402 | 403 | 429))
                && state.settings.read().await.degraded_context_fallback
                && !routing::candidates(&state, model, None).await.is_empty()
            {
                attempt.finish("failed", None);
                drop(upstream);
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
                return Box::pin(forward_once(state, retry, Some(context))).await;
            }
            let safe_to_fallback = binding.is_none()
                && !opaque_state
                && origin.is_none()
                && matches!(status.as_u16(), 401 | 402 | 403 | 429);
            if safe_to_fallback {
                attempt.finish("failed", None);
                drop(route_lease.take());
                // Preserve the final source's native error body when there is no
                // later candidate; streaming this response must not trigger replay.
                last_error = Some(
                    stream_response(
                        state.clone(),
                        source.clone(),
                        upstream,
                        prefix,
                        attempt,
                        ResponseContext {
                            discovery,
                            session_key: session_key.clone(),
                            request_is_stream: parsed["stream"].as_bool() == Some(true),
                            rebuild: None,
                            history: None,
                        },
                        None,
                    )
                    .await,
                );
                break;
            }
            if status.is_success() {
                routing::mark_ready(&state, &source.id).await;
            }
            return stream_response(
                state,
                source.clone(),
                upstream,
                prefix,
                attempt,
                ResponseContext {
                    discovery,
                    session_key: session_key.clone(),
                    request_is_stream: parsed["stream"].as_bool() == Some(true),
                    rebuild,
                    history: history_capture,
                },
                route_lease.take(),
            )
            .await;
        }
    }
    last_error.unwrap_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "所有来源暂时不可用"))
}

async fn persist_history(state: &Arc<AppState>, observer: &mut Observer) {
    if let Some(response) = observer.history_response.take() {
        if let Some(capture) = observer.history_request.take() {
            let history = state.history.clone();
            let _ = tokio::task::spawn_blocking(move || {
                history.capture_at(
                    &capture.scope,
                    &capture.request,
                    &response,
                    capture.generation,
                )
            })
            .await;
        }
    }
}

async fn finalize_response(
    state: &Arc<AppState>,
    source: &Source,
    status: StatusCode,
    discovery: bool,
    observer: &mut Observer,
    attempt: &mut Attempt,
) {
    observer.eof();
    persist_history(state, observer).await;
    if observer.history_request.take().is_some() {
        state.history.record_skip();
    }
    if observer.failed {
        routing::mark_failure(state, &source.id, 500, None).await;
        routing::mark_rejection(state, &source.id, observer.failure_code.as_deref(), None).await;
    }
    bind_ids(state, source, observer).await;
    let outcome = if !status.is_success() || observer.failed {
        "failed"
    } else if observer.sse && !observer.terminal {
        "interrupted"
    } else {
        "success"
    };
    attempt.finish(outcome, Some(observer));
    if discovery {
        if let Some(record) = &mut attempt.record {
            record.complete = true;
        }
    }
}

async fn stream_response(
    state: Arc<AppState>,
    source: Source,
    upstream: reqwest::Response,
    prefix: Vec<Bytes>,
    mut attempt: Attempt,
    context: ResponseContext,
    mut route_lease: Option<crate::affinity::RouteLease>,
) -> Response<Body> {
    let ResponseContext {
        discovery,
        session_key,
        request_is_stream,
        rebuild,
        history,
    } = context;
    let status = upstream.status();
    let headers = clean_headers(upstream.headers(), false);
    let content_length = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    // A zero-length body may never be polled by Hyper either. Usage stays
    // unknown, but receiving an empty successful response is not cancellation.
    if content_length == Some(0) {
        attempt.finish(
            if status.is_success() {
                "success"
            } else {
                "failed"
            },
            None,
        );
        if discovery {
            if let Some(record) = &mut attempt.record {
                record.complete = true;
            }
        }
    }
    let content_type = headers.get("content-type");
    let sse = content_type
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(';').next().is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream")))
        // Official Codex can omit Content-Type while still returning SSE.
        // Preserve explicit JSON/plain responses and all original wire bytes.
        || (request_is_stream && status.is_success() && content_type.is_none());
    let mut observer = Observer {
        sse,
        history_request: if status.is_success() { history } else { None },
        ..Default::default()
    };
    if content_length != Some(0) {
        attempt.finish(
            if status.is_success() {
                "streaming"
            } else {
                "failed"
            },
            None,
        );
    }
    if status.is_success() {
        if let Some(lease) = &mut route_lease {
            let sources = state.sources.read().await;
            if sources.iter().any(|runtime| {
                runtime.source.id == source.id && runtime.source.identity() == source.identity()
            }) && lease.commit().is_err()
            {
                eprintln!("会话来源保存失败");
            }
        }
        if let Some(context) = rebuild {
            let sources = state.sources.read().await;
            if sources
                .iter()
                .any(|r| r.source.id == source.id && r.source.identity() == source.identity())
            {
                if let Some(key) = context.key {
                    if state
                        .affinity
                        .confirm_exclusions(&key, &context.excluded)
                        .is_err()
                    {
                        eprintln!("上下文降级状态保存失败");
                    }
                }
            }
        }
        if let Some(session_key) = session_key {
            observer.ids.push(session_key);
        }
        if let Some(token) = headers
            .get("x-codex-turn-state")
            .and_then(|v| v.to_str().ok())
        {
            observer.ids.push(turn_binding(token));
        }
        // Establish header affinity before returning headers to the client,
        // including empty bodies that Hyper never polls.
        bind_ids(&state, &source, &mut observer).await;
    }
    let stream = async_stream::stream! {
        let _route_lease=route_lease;
        let prefix = futures_util::stream::iter(prefix.into_iter().map(Ok));
        let mut upstream = prefix.chain(upstream.bytes_stream());
        let mut received = 0u64;
        let mut finalized = content_length == Some(0);
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(chunk) => {
                    observer.push(&chunk);
                    persist_history(&state, &mut observer).await;
                    bind_ids(&state, &source, &mut observer).await;
                    received = received.saturating_add(chunk.len() as u64);
                    let complete_body = content_length == Some(received);
                    // A client may stop polling as soon as it sees a terminal
                    // SSE event, just as Hyper can stop at Content-Length.
                    // Record the outcome before yielding those bytes. Continue
                    // forwarding trailers unchanged without reopening the outcome.
                    if !finalized && (complete_body || observer.terminal) {
                        finalize_response(&state, &source, status, discovery, &mut observer, &mut attempt).await;
                        finalized = true;
                    } else if !finalized {
                        attempt.finish(if status.is_success() { "streaming" } else { "failed" }, Some(&observer));
                    }
                    yield Ok::<Bytes, std::io::Error>(chunk);
                    if complete_body { return; }
                },
                Err(_) => {
                    if !finalized { attempt.finish("interrupted", Some(&observer)); }
                    yield Err(std::io::Error::other("来源流已中断"));
                    return;
                }
            }
        }
        if !finalized { finalize_response(&state, &source, status, discovery, &mut observer, &mut attempt).await; }
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

#[cfg(test)]
mod observation_tests {
    use super::*;

    #[test]
    fn history_capture_reconstructs_completed_items_but_never_trusts_missing_stream_frames() {
        let mut observer = Observer {
            sse: true,
            history_request: Some(HistoryCapture {
                scope: "scope".into(),
                request: json!({"input":[]}),
                generation: 0,
            }),
            ..Default::default()
        };
        observer.value(&json!({"type":"response.output_item.done","output_index":0,"item":{"role":"assistant","content":"full answer"}}));
        observer.value(&json!({"type":"response.completed","response":{"id":"r","status":"completed","output":[]}}));
        assert_eq!(
            observer.history_response.as_ref().unwrap()["output"][0]["content"],
            "full answer"
        );
        observer.history_response = None;
        observer.history_invalid = true;
        observer.value(&json!({"type":"response.completed","response":{"id":"r","output":[]}}));
        assert!(observer.history_response.is_none());
    }

    #[test]
    fn mixed_output_item_snapshots_and_deltas_sum_each_item_once() {
        let mut observer = Observer {
            sse: true,
            ..Default::default()
        };
        observer.value(&json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"abcdefgh"}]}}));
        observer
            .value(&json!({"type":"response.output_text.delta","output_index":1,"delta":"你好"}));
        assert_eq!(estimated_tokens(observer.output_units()), 4);
        observer.value(&json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message","content":[{"type":"output_text","text":"你好"}]}}));
        assert_eq!(estimated_tokens(observer.output_units()), 4);
        observer.value(&json!({"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"abcdefgh"}]},{"type":"message","content":[{"type":"output_text","text":"你好"}]}]}}));
        assert_eq!(estimated_tokens(observer.output_units()), 4);
    }

    #[test]
    fn generation_timing_stops_at_terminal_event_before_stream_is_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(directory.path().to_owned(), 0, "fixture-key".into()).unwrap();
        let source:Source = serde_json::from_value(json!({"id":"fixture","name":"fixture","kind":"api","base_url":"http://127.0.0.1:1","enabled":true,"priority":0,"proxy":{"mode":"direct"},"models":[],"secret":{"api_key":"fixture"}})).unwrap();
        let mut attempt = Attempt::new(state.clone(), &source, "request", "model", 8);
        let mut observer = Observer {
            sse: true,
            ..Default::default()
        };
        observer.value(
            &json!({"type":"response.output_text.delta","output_index":0,"delta":"abcdefgh"}),
        );
        attempt.finish("streaming", Some(&observer));
        std::thread::sleep(std::time::Duration::from_millis(10));
        observer.value(&json!({"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":2}}}));
        attempt.finish("streaming", Some(&observer));
        let before = state.usage.summary(1).unwrap();
        assert_eq!(
            before["recent"][0]["token_details"]["stream_complete"],
            true
        );
        let generation = before["recent"][0]["generation_ms"].clone();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let during = state.usage.summary(1).unwrap();
        assert_eq!(during["recent"][0]["generation_ms"], generation);
        attempt.finish("success", Some(&observer));
        drop(attempt);
        let final_usage = state.usage.summary(1).unwrap();
        assert_eq!(final_usage["recent"][0]["generation_ms"], generation);
    }
}
