//! Response observation, usage accounting and streaming lifecycle.
use super::{
    clean_headers, explicit_context_rejection, opaque_bindings, turn_binding, HistoryCapture,
    ManualAcceptance, ResponseContext,
};
use crate::{
    model::{now, AppState, Source},
    routing,
    usage::{content_units, estimated_tokens, text_units, TokenDetails, UsageRecord},
};
use axum::{
    body::Body,
    http::{HeaderMap, Response, StatusCode},
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{atomic::Ordering, Arc},
    time::Instant,
};

const OBSERVATION_LIMIT: usize = 1024 * 1024;

/// Inspect a bounded JSON prefix while preserving full chunks for forwarding.
pub(super) async fn read_error_prefix(
    upstream: &mut reqwest::Response,
    prefix: &mut Vec<Bytes>,
) -> Option<Value> {
    let mut observed = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while observed.len() < 65536 {
        match tokio::time::timeout_at(deadline, upstream.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                observed.extend_from_slice(&chunk[..chunk.len().min(65536 - observed.len())]);
                prefix.push(chunk);
                if serde_json::from_slice::<Value>(&observed).is_ok() {
                    break;
                }
            }
            _ => break,
        }
    }
    serde_json::from_slice(&observed).ok()
}

/// Probe only the initial response before generated output makes replay unsafe.
pub(super) async fn probe_context_rejection(
    upstream: &mut reqwest::Response,
    prefix: &mut Vec<Bytes>,
    request_is_stream: bool,
    attempt: &mut Attempt,
) -> Result<bool, ()> {
    let mut probe = Observer {
        sse: response_is_sse(upstream.headers(), request_is_stream, upstream.status()),
        ..Default::default()
    };
    let mut observed = 0usize;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while observed < 65536 {
        match tokio::time::timeout_at(deadline, upstream.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                observed = observed.saturating_add(chunk.len());
                probe.push(&chunk);
                if !probe.sse {
                    if let Ok(value) = serde_json::from_slice::<Value>(&probe.buffer) {
                        probe.value(&value);
                    }
                }
                prefix.push(chunk);
                if probe.replay_blocked || probe.failed || probe.terminal {
                    break;
                }
            }
            Ok(Err(_)) => {
                attempt.finish("interrupted", Some(&probe));
                return Err(());
            }
            _ => break,
        }
    }
    Ok(probe.context_rejected && !probe.replay_blocked)
}

fn response_is_sse(headers: &HeaderMap, request_is_stream: bool, status: StatusCode) -> bool {
    let content_type = headers.get("content-type");
    content_type.and_then(|v| v.to_str().ok()).is_some_and(|v| {
        v.split(';')
            .next()
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
    }) || (request_is_stream && status.is_success() && content_type.is_none())
}

pub(super) struct Attempt {
    state: Arc<AppState>,
    pub(super) record: Option<UsageRecord>,
    started: Instant,
    published: Instant,
    first_output: Option<Instant>,
}
impl Attempt {
    pub(super) fn new(
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
                source_kind: Some(source.kind.clone()),
                is_subscription: Some(source.is_subscription),
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
    pub(super) fn finish(&mut self, status: &str, observer: Option<&Observer>) {
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
                    billing_model: observer
                        .billing_model
                        .clone()
                        .or_else(|| record.token_details.billing_model.clone()),
                    service_tier: observer
                        .service_tier
                        .clone()
                        .or_else(|| record.token_details.service_tier.clone()),
                    cache_write_retention: record.token_details.cache_write_retention.clone(),
                    source_kind: record.token_details.source_kind.clone(),
                    is_subscription: record.token_details.is_subscription,
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
pub(super) struct Observer {
    defer_bindings: bool,
    protected_bindings: HashSet<String>,
    accepted_completion: bool,
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
    billing_model: Option<String>,
    service_tier: Option<String>,
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
    context_rejected: bool,
    replay_blocked: bool,
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
        if let Some(model) = response["model"]
            .as_str()
            .filter(|v| !v.is_empty() && v.len() <= 256)
        {
            self.billing_model = Some(model.into());
        }
        if let Some(tier) = response["service_tier"].as_str().filter(|v| v.len() <= 32) {
            self.service_tier = Some(tier.into());
        }
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
        self.accepted_completion |=
            matches!(event_type, "response.completed" | "response.incomplete")
                || (!self.sse
                    && matches!(
                        response["status"].as_str(),
                        Some("completed" | "incomplete")
                    )
                    && response["output"].is_array());
        // Any generated item, including a tool call or private reasoning, makes
        // replay unsafe even when no text delta has been seen yet.
        self.replay_blocked |= event_type.starts_with("response.output_")
            || event_type.starts_with("response.refusal.")
            || event_type.starts_with("response.audio.")
            || event_type.starts_with("response.audio_transcript.")
            || event_type.starts_with("response.image_generation_call.")
            || event_type.starts_with("response.function_call_")
            || event_type.starts_with("response.reasoning_")
            || event_type.starts_with("response.custom_tool_")
            || response["output"]
                .as_array()
                .is_some_and(|items| !items.is_empty());
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
            || (!self.sse && response.get("error").is_some_and(|error| !error.is_null()))
        {
            self.failed = true;
            self.context_rejected |= explicit_context_rejection(value);
            self.failure_code = super::retries::permanent_code(response)
                .or_else(|| super::retries::permanent_code(value))
                .or_else(|| response.pointer("/error/code").and_then(Value::as_str))
                .or_else(|| value.get("code").and_then(Value::as_str))
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
    if observer.defer_bindings || observer.ids.is_empty() {
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
        if !observer.protected_bindings.contains(&id) {
            bindings.insert(id, source.id.clone());
        }
    }
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

// Persist positive upstream validation only for this conversation/model and
// source identity. Shared parent ciphertext keeps its original global binding.
async fn confirm_manual_acceptance(
    state: &Arc<AppState>,
    source: &Source,
    observer: &mut Observer,
    accepted: &mut Option<ManualAcceptance>,
    lease: &mut Option<crate::affinity::RouteLease>,
    session: &mut Option<String>,
    upstream_success: bool,
) {
    if !upstream_success
        || !(observer.replay_blocked || (observer.accepted_completion && !observer.failed))
    {
        return;
    }
    let Some(acceptance) = accepted.take() else {
        return;
    };
    {
        let sources = state.sources.read().await;
        if !sources
            .iter()
            .any(|r| r.source.id == source.id && r.source.identity() == source.identity())
        {
            return;
        }
        if let Some(lease) = lease {
            if lease.commit().is_err() {
                eprintln!("会话来源保存失败");
            }
        }
        if let Some(key) = acceptance.key {
            if state
                .affinity
                .confirm_acceptance(&key, &acceptance.hashes, &acceptance.restored)
                .is_err()
            {
                eprintln!("来源兼容状态保存失败");
            }
        }
        if let Some(session) = session.take() {
            observer.ids.push(session);
        }
        observer.defer_bindings = false;
    }
    bind_ids(state, source, observer).await;
}

pub(super) async fn stream_response(
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
        mut session_key,
        request_is_stream,
        rebuild,
        history,
        mut accepted_state,
    } = context;
    if let Some(context) = rebuild {
        let acceptance = accepted_state.get_or_insert_with(|| ManualAcceptance {
            key: context.key,
            hashes: HashSet::new(),
            protected_bindings: HashSet::new(),
            restored: HashSet::new(),
        });
        acceptance.hashes.extend(context.excluded);
        acceptance
            .protected_bindings
            .extend(context.protected_bindings);
    }
    let rejected_attempt = attempt
        .record
        .as_ref()
        .is_some_and(|record| record.status == "failed");
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
            if status.is_success() && !rejected_attempt {
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
    // Official Codex can omit Content-Type while still returning SSE.
    let sse = response_is_sse(&headers, request_is_stream, status);
    let mut observer = Observer {
        sse,
        defer_bindings: accepted_state.is_some() || rejected_attempt,
        protected_bindings: accepted_state
            .as_ref()
            .map(|a| a.protected_bindings.clone())
            .unwrap_or_default(),
        history_request: if status.is_success() { history } else { None },
        ..Default::default()
    };
    if content_length != Some(0) {
        attempt.finish(
            if status.is_success() && !rejected_attempt {
                "streaming"
            } else {
                "failed"
            },
            None,
        );
    }
    if status.is_success() && !rejected_attempt {
        if let Some(lease) = route_lease.as_mut().filter(|_| accepted_state.is_none()) {
            let sources = state.sources.read().await;
            if sources.iter().any(|runtime| {
                runtime.source.id == source.id && runtime.source.identity() == source.identity()
            }) && lease.commit().is_err()
            {
                eprintln!("会话来源保存失败");
            }
        }
        if accepted_state.is_none() {
            if let Some(session_key) = session_key.take() {
                observer.ids.push(session_key);
            }
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
        let mut route_lease=route_lease;
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
                    confirm_manual_acceptance(&state, &source, &mut observer, &mut accepted_state, &mut route_lease, &mut session_key, status.is_success() && !rejected_attempt).await;
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
        confirm_manual_acceptance(&state, &source, &mut observer, &mut accepted_state, &mut route_lease, &mut session_key, status.is_success() && !rejected_attempt).await;
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    use serde_json::json;

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
