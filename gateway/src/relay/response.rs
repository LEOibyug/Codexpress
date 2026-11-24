//! Response observation, usage accounting and streaming lifecycle.
use super::{
    clean_headers, explicit_context_rejection, opaque_bindings, turn_binding, HistoryCapture,
    ManualAcceptance, ResponseContext,
};
use crate::{
    model::{now, AppState, Source},
    routing,
    usage::{
        content_units, diagnostics::Capture, estimated_tokens, text_units, TokenDetails,
        UsageRecord,
    },
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
// Failure feedback uses the diagnostic budget; history snapshots retain their smaller limit.
const EVENT_LIMIT: usize = crate::usage::diagnostics::BODY_LIMIT;

/// Inspect a bounded JSON prefix while preserving full chunks for forwarding.
pub(super) async fn read_error_prefix(
    upstream: &mut reqwest::Response,
    prefix: &mut Vec<(Bytes, Instant)>,
    attempt: &mut Attempt,
) -> Option<Value> {
    let mut observed = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while observed.len() < 65536 {
        match tokio::time::timeout_at(deadline, upstream.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                let received_at = Instant::now();
                observed.extend_from_slice(&chunk[..chunk.len().min(65536 - observed.len())]);
                prefix.push((chunk, received_at));
                if serde_json::from_slice::<Value>(&observed).is_ok() {
                    break;
                }
            }
            Ok(Err(_)) => {
                attempt.captured_transport_error = true;
                attempt.diagnostic.transport("body_transport", "上游错误响应传输中断");
                return None;
            }
            _ => break,
        }
    }
    serde_json::from_slice(&observed).ok()
}

/// Probe only the initial response before generated output makes replay unsafe.
pub(super) struct ResponseProbe {
    pub context_rejected: bool,
    pub failure: Option<Value>,
    pub quota_code: Option<String>,
}

pub(super) async fn probe_context_rejection(
    upstream: &mut reqwest::Response,
    prefix: &mut Vec<(Bytes, Instant)>,
    request_is_stream: bool,
    attempt: &mut Attempt,
) -> Result<ResponseProbe, ()> {
    let mut probe = Observer {
        sse: response_is_sse(upstream.headers(), request_is_stream, upstream.status()),
        ..Default::default()
    };
    let mut observed = 0usize;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while observed < 65536 {
        match tokio::time::timeout_at(deadline, upstream.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                let received_at = Instant::now();
                observed = observed.saturating_add(chunk.len());
                probe.replay_blocked |= observed > 65536;
                probe.push(&chunk, received_at);
                if !probe.sse {
                    if let Ok(value) = serde_json::from_slice::<Value>(&probe.buffer) {
                        probe.value(&value, received_at);
                    }
                }
                prefix.push((chunk, received_at));
                if probe.replay_blocked || probe.failed || probe.terminal {
                    break;
                }
            }
            Ok(Err(_)) => {
                attempt
                    .diagnostic
                    .transport("body_transport", "上游响应传输中断");
                attempt.finish("interrupted", Some(&probe));
                return Err(());
            }
            _ => break,
        }
    }
    if let Some(value) = &probe.failure_event {
        attempt.diagnostic.failed_event(value);
    }
    Ok(ResponseProbe {
        context_rejected: probe.context_rejected && !probe.replay_blocked,
        failure: if probe.replay_blocked {
            None
        } else {
            probe.failure_event
        },
        quota_code: probe.quota_code,
    })
}

pub(super) fn response_is_sse(headers: &HeaderMap, request_is_stream: bool, status: StatusCode) -> bool {
    let content_type = headers.get("content-type");
    content_type.and_then(|v| v.to_str().ok()).is_some_and(|v| {
        v.split(';')
            .next()
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
    }) || (request_is_stream && status.is_success() && content_type.is_none())
}

pub(super) fn response_has_output(value: &Value) -> bool {
    let response = value.get("response").unwrap_or(value);
    response["output"].as_array().is_some_and(|items| !items.is_empty())
        || response["usage"]["output_tokens"].as_u64().is_some_and(|tokens| tokens > 0)
}

// Preserve ordinary event bytes. Hold only a bounded incomplete frame so a
// split quota event cannot leak its native hard error before we recognize it.
#[derive(Default)]
struct SseQuotaFilter {
    pending: Vec<u8>,
    line_len: usize,
    line_cr: bool,
    disabled: bool,
}

impl SseQuotaFilter {
    fn flush(&mut self) -> Bytes {
        Bytes::from(std::mem::take(&mut self.pending))
    }

    fn push(&mut self, chunk: Bytes, observed_quota: bool) -> Bytes {
        if self.disabled { return chunk; }
        let mut boundary = 0;
        for (index, &byte) in chunk.iter().enumerate() {
            if byte == b'\n' {
                if self.line_len == 0 || (self.line_len == 1 && self.line_cr) {
                    boundary = index + 1;
                }
                self.line_len = 0;
            } else {
                if self.line_len == 0 { self.line_cr = byte == b'\r'; }
                self.line_len = (self.line_len + 1).min(2);
            }
        }
        let tail = &chunk[boundary..];
        if tail.len() + if boundary == 0 { self.pending.len() } else { 0 } > EVENT_LIMIT {
            self.disabled = true;
            boundary = chunk.len();
        }
        if boundary == 0 {
            self.pending.extend_from_slice(tail);
            return Bytes::new();
        }
        let ready = if self.pending.is_empty() {
            chunk.slice(..boundary)
        } else {
            let mut ready = std::mem::take(&mut self.pending);
            ready.extend_from_slice(&chunk[..boundary]);
            Bytes::from(ready)
        };
        self.pending.extend_from_slice(&chunk[boundary..]);
        if observed_quota { rewrite_quota_frames(ready) } else { ready }
    }
}

fn rewrite_quota_frames(bytes: Bytes) -> Bytes {
    let mut start = 0;
    let mut end = 0;
    let mut written = 0;
    let mut rewritten = Vec::new();
    for line in bytes.split_inclusive(|&byte| byte == b'\n') {
        end += line.len();
        if line != b"\n" && line != b"\r\n" { continue; }
        let frame = &bytes[start..end];
        if frame.len() <= EVENT_LIMIT {
            if let Some(value) = neutral_quota_event(frame) {
                rewritten.extend_from_slice(&bytes[written..start]);
                let mut data_written = false;
                for line in frame.split_inclusive(|&byte| byte == b'\n') {
                    if line.starts_with(b"data:") {
                        if !data_written {
                            rewritten.extend_from_slice(b"data: ");
                            serde_json::to_writer(&mut rewritten, &value).expect("JSON event");
                            rewritten.extend_from_slice(if line.ends_with(b"\r\n") { b"\r\n" } else { b"\n" });
                            data_written = true;
                        }
                    } else {
                        rewritten.extend_from_slice(line);
                    }
                }
                written = end;
            }
        }
        start = end;
    }
    if written == 0 { return bytes; }
    rewritten.extend_from_slice(&bytes[written..]);
    Bytes::from(rewritten)
}

fn neutral_quota_event(frame: &[u8]) -> Option<Value> {
    let mut name = None;
    let mut data = Vec::new();
    for line in frame.split(|&byte| byte == b'\n') {
        if let Some(line) = line.strip_prefix(b"data:") {
            if !data.is_empty() { data.push(b'\n'); }
            data.extend_from_slice(line.strip_prefix(b" ").unwrap_or(line));
        }
        if let Some(line) = line.strip_prefix(b"event:") {
            name = std::str::from_utf8(line).ok().map(str::trim);
        }
    }
    let mut value: Value = serde_json::from_slice(&data).ok()?;
    let event_type = value["type"].as_str().or(name).unwrap_or("");
    let root_error = event_type == "error";
    if !matches!(event_type, "error" | "response.failed")
        && value.get("response").unwrap_or(&value)["status"] != "failed"
    {
        return None;
    }
    super::retries::quota_code(&value)?;
    const MESSAGE: &str = "来源暂不可用，请稍后重试。";
    let neutral = serde_json::json!({"code":"upstream_unavailable", "type":"server_error", "message":MESSAGE});
    if let Some(error) = value.pointer_mut("/response/error") {
        *error = neutral.clone();
    }
    if let Some(error) = value.get_mut("error") {
        *error = neutral;
    } else if root_error {
        value["code"] = "upstream_unavailable".into();
        value["message"] = MESSAGE.into();
    }
    Some(value)
}

pub(super) struct Attempt {
    state: Arc<AppState>,
    pub(super) record: Option<UsageRecord>,
    started: Instant,
    published: Instant,
    pub(super) diagnostic: Capture,
    captured_prefix: usize,
    captured_transport_error: bool,
    saved_failure: Option<Value>,
    pub(super) manual_trial: Option<routing::manual::Trial>,
    pub(super) dispatch_ticket: Option<u64>,
    quota_reported: bool,
}
impl Attempt {
    pub(super) async fn report_quota(&mut self, source: &Source, code: &str, retry_after: Option<u64>) {
        if self.quota_reported {
            return;
        }
        if source.kind == "official" {
            if let Some(ticket) = self.dispatch_ticket {
                crate::official::quota_rejected(&self.state, source, ticket).await;
            }
        } else {
            routing::mark_rejection(&self.state, source, Some(code), retry_after).await;
        }
        self.quota_reported = true;
    }

    pub(super) fn new(
        state: Arc<AppState>,
        source: &Source,
        request_id: &str,
        model: &str,
        input_units: u64,
        ingress: crate::usage::Ingress,
    ) -> Self {
        state.inflight.fetch_add(1, Ordering::Relaxed);
        let record = UsageRecord {
            attempt_id: uuid::Uuid::new_v4().to_string(),
            request_id: request_id.into(),
            ingress,
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
                source_identity: Some(source.identity()),
                ..Default::default()
            },
            complete: false,
            latency_ms: 0,
            first_token_latency_ms: None,
            generation_ms: None,
        };
        state.usage.start_for_source(&record, source);
        let diagnostic = Capture::new(
            &record.attempt_id,
            record.started_at,
            source,
            &state.gateway_key,
            ingress,
        );
        Self {
            state,
            started: Instant::now(),
            published: Instant::now(),
            record: Some(record),
            diagnostic,
            captured_prefix: 0,
            captured_transport_error: false,
            saved_failure: None,
            manual_trial: None,
            dispatch_ticket: None,
            quota_reported: false,
        }
    }
    pub(super) fn save_diagnostic(&self, body: bool) {
        if self.state.usage.diagnostic(&self.diagnostic, body).is_err() {
            eprintln!("请求诊断写入失败");
        }
    }
    /// Preserve error bytes for forwarding and for attempts discarded during retry.
    pub(super) async fn collect_error(
        &mut self,
        upstream: &mut reqwest::Response,
        prefix: &mut Vec<(Bytes, Instant)>,
    ) {
        if !upstream.status().is_client_error() && !upstream.status().is_server_error() {
            return;
        }
        for (chunk, _) in prefix.iter().skip(self.captured_prefix) {
            self.diagnostic.chunk(chunk);
        }
        self.captured_prefix = prefix.len();
        let mut received: u64 = prefix.iter().map(|(chunk, _)| chunk.len() as u64).sum();
        let length = upstream.content_length();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if length == Some(received) {
                self.diagnostic.complete();
                break;
            }
            if self.diagnostic.full() {
                break;
            }
            match tokio::time::timeout_at(deadline, upstream.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    let received_at = Instant::now();
                    received = received.saturating_add(chunk.len() as u64);
                    self.diagnostic.chunk(&chunk);
                    prefix.push((chunk, received_at));
                    self.captured_prefix += 1;
                }
                Ok(Ok(None)) => {
                    self.diagnostic.complete();
                    break;
                }
                Ok(Err(_)) => {
                    self.captured_transport_error = true;
                    self.diagnostic
                        .transport("body_transport", "上游错误响应传输中断");
                    break;
                }
                Err(_) => {
                    self.diagnostic
                        .transport("body_timeout", "上游错误响应尚未完整返回");
                    break;
                }
            }
        }
        self.save_diagnostic(true);
    }
    pub(super) fn finish(&mut self, status: &str, observer: Option<&Observer>) {
        if matches!(status, "failed" | "interrupted") {
            if let Some(trial) = &self.manual_trial {
                self.state.manual_routing.stop(trial);
            }
        }
        if let Some(value) = observer.and_then(|observer| observer.failure_event.as_ref()) {
            if self.saved_failure.as_ref() != Some(value) {
                self.diagnostic.failed_event(value);
                self.save_diagnostic(true);
                self.saved_failure = Some(value.clone());
            }
        }
        if let Some(record) = &mut self.record {
            let mut changed = record.status != status;
            record.status = status.into();
            if let Some(observer) = observer {
                changed |= observer.terminal && !record.token_details.stream_complete;
                if let Some(first) = observer.first_output {
                    changed |= record.first_token_latency_ms.is_none();
                    record.first_token_latency_ms = Some(
                        first.saturating_duration_since(self.started).as_millis() as u64,
                    );
                    record.generation_ms = observer.terminal_at.map(|terminal| {
                        terminal.saturating_duration_since(first).as_millis() as u64
                    });
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
                    cache_write_30m_tokens: observer.cache_write_30m,
                    cache_write_12h_tokens: observer.cache_write_12h,
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
                    cache_write_retention: observer
                        .cache_write_retention
                        .clone()
                        .or_else(|| record.token_details.cache_write_retention.clone()),
                    source_kind: record.token_details.source_kind.clone(),
                    is_subscription: record.token_details.is_subscription,
                    source_identity: record.token_details.source_identity.clone(),
                    upstream_cost: observer
                        .upstream_cost
                        .clone()
                        .or_else(|| record.token_details.upstream_cost.clone()),
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
            if record.status != "success" {
                if let Some(trial) = &self.manual_trial {
                    self.state.manual_routing.stop(trial);
                }
            }
            record.latency_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            self.save_diagnostic(true);
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
    event_name: Option<String>,
    discarding_line: bool,
    discarding_event: bool,
    oversized: bool,
    sse: bool,
    terminal: bool,
    failed: bool,
    failure_code: Option<String>,
    quota_code: Option<String>,
    failure_event: Option<Value>,
    input: Option<u64>,
    output: Option<u64>,
    cached: Option<u64>,
    cache_write: Option<u64>,
    cache_write_30m: Option<u64>,
    cache_write_12h: Option<u64>,
    billing_model: Option<String>,
    service_tier: Option<String>,
    cache_write_retention: Option<String>,
    reasoning: Option<u64>,
    total: Option<u64>,
    upstream_cost: Option<crate::usage::ReportedCost>,
    delta_units: HashMap<String, u64>,
    item_units: HashMap<String, u64>,
    final_units: Option<u64>,
    output_observed: bool,
    first_output: Option<Instant>,
    terminal_at: Option<Instant>,
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
    fn value(&mut self, value: &Value, received_at: Instant) {
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
        if let Some(retention) = response["prompt_cache_retention"]
            .as_str()
            .filter(|v| !v.is_empty() && v.len() <= 32)
        {
            self.cache_write_retention = Some(retention.into());
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
        self.replay_blocked |= self.accepted_completion
            || (self.sse && !matches!(event_type,
                "response.created" | "response.queued" | "response.in_progress"
                | "response.metadata" | "rate_limits.updated" | "codex.rate_limits"
                | "response.failed" | "error"))
            || response_has_output(value);
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
                && self.first_output.is_some()
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
                if self.sse && !text.is_empty() && self.terminal_at.is_none() {
                    self.first_output.get_or_insert(received_at);
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
            if self.sse {
                self.terminal_at.get_or_insert(received_at);
            }
            self.upstream_cost =
                crate::pricing::response_charge(response).or_else(|| self.upstream_cost.clone());
        }
        if matches!(event_type, "response.failed" | "error")
            || response.get("status").and_then(Value::as_str) == Some("failed")
            || (!self.sse && response.get("error").is_some_and(|error| !error.is_null()))
        {
            self.failed = true;
            self.failure_event = Some(value.clone());
            self.context_rejected |= explicit_context_rejection(value);
            if self.quota_code.is_none() {
                self.quota_code = super::retries::quota_code(value).map(String::from);
                if self.sse && self.quota_code.is_some() {
                    self.terminal = true;
                    self.terminal_at.get_or_insert(received_at);
                }
            }
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
            self.cache_write_30m = usage
                .pointer("/input_tokens_details/cache_write_tokens_details/ttl_30m_tokens")
                .and_then(Value::as_u64)
                .or(self.cache_write_30m);
            self.cache_write_12h = usage
                .pointer("/input_tokens_details/cache_write_tokens_details/ttl_12h_tokens")
                .and_then(Value::as_u64)
                .or(self.cache_write_12h);
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
    fn event(&mut self, received_at: Instant) {
        if self.discarding_event {
            self.history_invalid = true;
            self.replay_blocked = true;
        }
        if !self.discarding_event {
            if let Ok(mut value) = serde_json::from_slice::<Value>(&self.data) {
                if let Some((name, object)) = self.event_name.take().zip(value.as_object_mut()) {
                    object.entry("type").or_insert(Value::String(name));
                }
                self.value(&value, received_at);
            } else if !self.data.is_empty() {
                self.replay_blocked = true;
            }
        }
        self.data.clear();
        self.event_name = None;
        self.discarding_event = false;
    }
    fn push(&mut self, bytes: &[u8], received_at: Instant) {
        if !self.sse {
            if !self.oversized && self.buffer.len().saturating_add(bytes.len()) <= EVENT_LIMIT {
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
                        self.event(received_at);
                    } else if let Some(raw) = self.line.strip_prefix(b"data:") {
                        let raw = raw.strip_prefix(b" ").unwrap_or(raw);
                        if self.data.len().saturating_add(raw.len() + 1) <= EVENT_LIMIT {
                            if !self.data.is_empty() {
                                self.data.push(b'\n');
                            }
                            self.data.extend_from_slice(raw);
                        } else {
                            self.discarding_event = true;
                            self.data.clear();
                        }
                    } else if let Some(raw) = self.line.strip_prefix(b"event:") {
                        self.event_name = std::str::from_utf8(raw).ok()
                            .map(str::trim).filter(|name| name.len() <= 128).map(String::from);
                    }
                }
                self.line.clear();
                self.discarding_line = false;
            } else if !self.discarding_line {
                if self.line.len() < EVENT_LIMIT {
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
                self.value(&value, Instant::now());
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
        if let Some(mut capture) = observer.history_request.take() {
            if let Some(metadata) = &mut capture.metadata {
                metadata.captured_at = now();
            }
            let history = state.history.clone();
            let _ = tokio::task::spawn_blocking(move || {
                history.capture_with_metadata_at(
                    &capture.scope,
                    &capture.request,
                    &response,
                    capture.generation,
                    capture.metadata.as_ref(),
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
    if let Some(code) = &observer.quota_code {
        attempt.report_quota(source, code, None).await;
    } else if observer.failed {
        routing::mark_rejection(state, source, observer.failure_code.as_deref(), None).await;
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
        state.manual_routing.refresh(&sources, &state.affinity);
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

type TimedByteStream = std::pin::Pin<
    Box<dyn futures_util::Stream<Item = Result<(Bytes, Instant), std::io::Error>> + Send>,
>;
type TranslatedByteStream = std::pin::Pin<
    Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send>,
>;

pub(super) async fn stream_response(
    state: Arc<AppState>,
    source: Source,
    upstream: reqwest::Response,
    prefix: Vec<(Bytes, Instant)>,
    attempt: Attempt,
    context: ResponseContext,
    route_lease: Option<crate::affinity::RouteLease>,
) -> Response<Body> {
    let status = upstream.status();
    let headers = clean_headers(upstream.headers(), false);
    let body_stream: TimedByteStream = Box::pin(
        upstream
            .bytes_stream()
            .map(|result| result.map(|chunk| (chunk, Instant::now())).map_err(|_| std::io::Error::other("来源流已中断"))),
    );
    stream_body_response(
        state,
        source,
        status,
        headers,
        prefix,
        body_stream,
        attempt,
        context,
        route_lease,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn stream_translated_response(
    state: Arc<AppState>,
    source: Source,
    sse: bool,
    content_length: Option<u64>,
    body_stream: TranslatedByteStream,
    attempt: Attempt,
    context: ResponseContext,
    route_lease: Option<crate::affinity::RouteLease>,
) -> Response<Body> {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(if sse {
            "text/event-stream; charset=utf-8"
        } else {
            "application/json"
        }),
    );
    headers.insert(
        axum::http::header::CONTENT_ENCODING,
        axum::http::HeaderValue::from_static("identity"),
    );
    if let Some(len) = content_length {
        if let Ok(value) = axum::http::HeaderValue::from_str(&len.to_string()) {
            headers.insert(axum::http::header::CONTENT_LENGTH, value);
        }
    }
    let body_stream: TimedByteStream =
        Box::pin(body_stream.map(|result| result.map(|chunk| (chunk, Instant::now()))));
    stream_body_response(
        state,
        source,
        StatusCode::OK,
        headers,
        Vec::new(),
        body_stream,
        attempt,
        context,
        route_lease,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn stream_body_response(
    state: Arc<AppState>,
    source: Source,
    status: StatusCode,
    mut headers: HeaderMap,
    prefix: Vec<(Bytes, Instant)>,
    upstream: TimedByteStream,
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
    let content_length = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    // A zero-length body may never be polled by Hyper either. Usage stays
    // unknown, but receiving an empty successful response is not cancellation.
    if content_length == Some(0) {
        attempt.diagnostic.complete();
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
    let rewrite_quota = sse && content_length != Some(0)
        && headers.get("content-encoding").is_none_or(|encoding| {
            encoding.to_str().is_ok_and(|value| value.eq_ignore_ascii_case("identity"))
        });
    if rewrite_quota { super::clear_body_headers(&mut headers); }
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
        let captured_prefix = attempt.captured_prefix;
        let prefix = futures_util::stream::iter(prefix.into_iter().map(Ok));
        let mut upstream = prefix.chain(upstream);
        let mut received = 0u64;
        let mut finalized = content_length == Some(0);
        let mut chunk_index = 0usize;
        let mut filter = SseQuotaFilter { disabled: !rewrite_quota, ..Default::default() };
        loop {
            let chunk = if filter.pending.is_empty() {
                upstream.next().await
            } else {
                match tokio::time::timeout(std::time::Duration::from_secs(1), upstream.next()).await {
                    Ok(chunk) => chunk,
                    Err(_) => {
                        filter.disabled = true;
                        yield Ok::<Bytes, std::io::Error>(filter.flush());
                        continue;
                    }
                }
            };
            let Some(chunk) = chunk else { break; };
            match chunk {
                Ok((chunk, received_at)) => {
                    if chunk_index >= captured_prefix { attempt.diagnostic.chunk(&chunk); }
                    chunk_index += 1;
                    observer.push(&chunk, received_at);
                    if let Some(code) = &observer.quota_code {
                        attempt.report_quota(&source, code, None).await;
                    }
                    persist_history(&state, &mut observer).await;
                    bind_ids(&state, &source, &mut observer).await;
                    received = received.saturating_add(chunk.len() as u64);
                    let complete_body = content_length == Some(received);
                    if complete_body { attempt.diagnostic.complete(); }
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
                    let chunk = filter.push(chunk, observer.quota_code.is_some());
                    if !chunk.is_empty() { yield Ok::<Bytes, std::io::Error>(chunk); }
                    if complete_body {
                        if !filter.pending.is_empty() { yield Ok(filter.flush()); }
                        return;
                    }
                },
                Err(_) => {
                    attempt.diagnostic.transport("body_transport", "上游响应传输中断");
                    if !finalized { attempt.finish("interrupted", Some(&observer)); }
                    if !filter.pending.is_empty() { yield Ok(filter.flush()); }
                    yield Err(std::io::Error::other("来源流已中断"));
                    return;
                }
            }
        }
        if attempt.captured_transport_error {
            if !finalized { attempt.finish("failed", Some(&observer)); }
            if !filter.pending.is_empty() { yield Ok(filter.flush()); }
            yield Err(std::io::Error::other("来源流已中断"));
            return;
        }
        attempt.diagnostic.complete();
        if !finalized { finalize_response(&state, &source, status, discovery, &mut observer, &mut attempt).await; }
        confirm_manual_acceptance(&state, &source, &mut observer, &mut accepted_state, &mut route_lease, &mut session_key, status.is_success() && !rejected_attempt).await;
        if !filter.pending.is_empty() { yield Ok(filter.flush()); }
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
                metadata: None,
            }),
            ..Default::default()
        };
        observer.value(&json!({"type":"response.output_item.done","output_index":0,"item":{"role":"assistant","content":"full answer"}}), Instant::now());
        observer.value(&json!({"type":"response.completed","response":{"id":"r","status":"completed","output":[]}}), Instant::now());
        assert_eq!(
            observer.history_response.as_ref().unwrap()["output"][0]["content"],
            "full answer"
        );
        observer.history_response = None;
        observer.history_invalid = true;
        observer.value(&json!({"type":"response.completed","response":{"id":"r","output":[]}}), Instant::now());
        assert!(observer.history_response.is_none());
    }

    #[test]
    fn mixed_output_item_snapshots_and_deltas_sum_each_item_once() {
        let mut observer = Observer {
            sse: true,
            ..Default::default()
        };
        observer.value(&json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"abcdefgh"}]}}), Instant::now());
        observer.value(&json!({"type":"response.output_text.delta","output_index":1,"delta":"你好"}), Instant::now());
        assert_eq!(estimated_tokens(observer.output_units()), 4);
        observer.value(&json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message","content":[{"type":"output_text","text":"你好"}]}}), Instant::now());
        assert_eq!(estimated_tokens(observer.output_units()), 4);
        observer.value(&json!({"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"abcdefgh"}]},{"type":"message","content":[{"type":"output_text","text":"你好"}]}]}}), Instant::now());
        assert_eq!(estimated_tokens(observer.output_units()), 4);
    }

    #[test]
    fn generation_timing_stops_at_terminal_event_before_stream_is_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(directory.path().to_owned(), 0, "fixture-key".into()).unwrap();
        let source:Source = serde_json::from_value(json!({"id":"fixture","name":"fixture","kind":"api","base_url":"http://127.0.0.1:1","enabled":true,"priority":0,"proxy":{"mode":"direct"},"models":[],"secret":{"api_key":"fixture"}})).unwrap();
        let mut attempt = Attempt::new(state.clone(), &source, "request", "model", 8, crate::usage::Ingress::Native);
        let mut observer = Observer {
            sse: true,
            ..Default::default()
        };
        use std::time::Duration;
        let start = attempt.started;
        observer.push(b"data: {\"type\":\"response.output_item.added\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"\"}\n\n", start + Duration::from_millis(100));
        attempt.finish("streaming", Some(&observer));
        assert!(state.usage.summary(1).unwrap()["recent"][0]["first_token_latency_ms"].is_null());
        observer.push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"abcdefgh\"}\n\n", start + Duration::from_secs(2));
        attempt.finish("streaming", Some(&observer));
        let active = state.usage.summary(1).unwrap();
        assert_eq!(active["recent"][0]["first_token_latency_ms"], 2000);
        assert!(active["recent"][0]["output_tps"].is_null());
        observer.push(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n\n", start + Duration::from_millis(2020));
        attempt.finish("success", Some(&observer));
        observer.push(b": trailer\n\n", start + Duration::from_secs(20));
        drop(attempt);
        let final_usage = state.usage.summary(1).unwrap();
        assert_eq!(final_usage["recent"][0]["generation_ms"], 20);
        assert_eq!(final_usage["recent"][0]["output_tps"], 100.0);
    }

    #[test]
    fn extracts_gpt6_cache_write_ttl_breakdown_and_retention_for_pricing() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(directory.path().to_owned(), 0, "fixture-key".into()).unwrap();
        let source: Source = serde_json::from_value(json!({
            "id": "openai-src", "name": "OpenAI", "kind": "openai",
            "base_url": "https://api.openai.com/v1", "enabled": true, "priority": 0,
            "proxy": {"mode": "direct"}, "models": ["gpt-6-luna"],
            "secret": {"api_key": "fixture"}
        }))
        .unwrap();
        let mut attempt = Attempt::new(
            state.clone(),
            &source,
            "request",
            "gpt-6-luna",
            1000,
            crate::usage::Ingress::Native,
        );
        let mut observer = Observer {
            sse: true,
            ..Default::default()
        };
        let completed = json!({
            "type": "response.completed",
            "response": {
                "model": "gpt-6-luna",
                "service_tier": "default",
                "prompt_cache_retention": "24h",
                "usage": {
                    "input_tokens": 1953,
                    "input_tokens_details": {
                        "cached_tokens": 0,
                        "cache_write_tokens": 1950,
                        "cache_write_tokens_details": {
                            "ttl_30m_tokens": 1950,
                            "ttl_12h_tokens": 0
                        }
                    },
                    "output_tokens": 20,
                    "output_tokens_details": { "reasoning_tokens": 12 },
                    "total_tokens": 1973
                }
            }
        });
        observer.value(&completed, Instant::now());
        attempt.finish("success", Some(&observer));
        drop(attempt);
        let summary = state.usage.summary(1).unwrap();
        let rec = &summary["recent"][0];
        assert_eq!(rec["token_details"]["cache_write_30m_tokens"], 1950);
        assert_eq!(rec["token_details"]["cache_write_12h_tokens"], 0);
        assert_eq!(rec["token_details"]["cache_write_retention"], "24h");
        assert_eq!(rec["cost"]["status"], "reported");
        assert_eq!(rec["cost"]["unpriced_cache_write_tokens"], 0);
    }
}
