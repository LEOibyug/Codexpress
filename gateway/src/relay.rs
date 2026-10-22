mod response;

use self::response::{probe_context_rejection, read_error_prefix, stream_response, Attempt};
use crate::{
    model::{AppState, Source},
    routing, transport,
    usage::content_units,
};
use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode},
    routing::{get, post},
    Router,
};
use bytes::Bytes;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{atomic::Ordering, Arc},
};

const BODY_LIMIT: usize = 32 * 1024 * 1024;
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
    let mut response = error(
        StatusCode::SERVICE_UNAVAILABLE,
        "来源仍在冷却或等待空闲名额，已等待恢复；请稍后重试。",
    );
    response.headers_mut().insert(
        "retry-after",
        HeaderValue::from_str(&retry_after.max(1).to_string()).expect("seconds"),
    );
    response
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
}

fn scoped_session(parsed: &Value, parts: &axum::http::request::Parts) -> Option<String> {
    if let Some(session) = parts.extensions.get::<PreservedScopedSession>() {
        // This value already includes any ingress namespace. Reconstruction
        // removed wire state, not the identity of the local conversation.
        return Some(session.0.clone());
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
            .or_else(|| problem.get("type"))
            .and_then(Value::as_str),
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
    let mut retry = Request::new(Body::from(serde_json::to_vec(&portable).expect("JSON")));
    *retry.method_mut() = parts.method.clone();
    *retry.uri_mut() = parts.uri.clone();
    *retry.headers_mut() = headers;
    if let Some(session) = scoped_session(parsed, parts) {
        retry
            .extensions_mut()
            .insert(PreservedScopedSession(session));
    }
    if let Some(ingress) = parts.extensions.get::<crate::remote::RemoteIngress>() {
        retry.extensions_mut().insert(ingress.clone());
    }
    if let Some(ingress) = parts.extensions.get::<crate::chatbot::ChatbotIngress>() {
        retry.extensions_mut().insert(ingress.clone());
    }
    if let Some(window) = parts.extensions.get::<RecoveryWindow>() {
        retry.extensions_mut().insert(window.clone());
    }
    Ok((
        retry,
        RebuildContext {
            key: affinity.map(str::to_owned),
            excluded,
            scope,
            route: None,
            rejected_source: None,
        },
    ))
}

struct RebuildContext {
    key: Option<String>,
    excluded: HashSet<String>,
    scope: String,
    route: Option<(Source, crate::affinity::RouteLease)>,
    rejected_source: Option<String>,
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
    if !degraded && !discovery && parts.uri.path() == "/v1/responses" && !tool_result {
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
                        let mut retry = Request::new(Body::from(bytes.clone()));
                        *retry.method_mut() = parts.method.clone();
                        *retry.uri_mut() = parts.uri.clone();
                        *retry.headers_mut() = parts.headers.clone();
                        if let Some(ingress) =
                            parts.extensions.get::<crate::remote::RemoteIngress>()
                        {
                            retry.extensions_mut().insert(ingress.clone());
                        }
                        if let Some(ingress) =
                            parts.extensions.get::<crate::chatbot::ChatbotIngress>()
                        {
                            retry.extensions_mut().insert(ingress.clone());
                        }
                        if let Some(session) = parts.extensions.get::<PreservedScopedSession>() {
                            retry.extensions_mut().insert(session.clone());
                        }
                        if let Some(window) = parts.extensions.get::<RecoveryWindow>() {
                            retry.extensions_mut().insert(window.clone());
                        }
                        Some((
                            retry,
                            RebuildContext {
                                key: Some(key.to_owned()),
                                excluded: HashSet::from(["rebuilt".into()]),
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
            routing::candidates_with_session(
                &state,
                model,
                routing_binding.as_deref(),
                affinity_key.as_deref(),
            )
            .await
        }
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
                .all(|key| bindings.get(*key).is_none_or(|id| id == &source.id))
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
        if !degraded && !discovery && (binding.is_some() || opaque_state) {
            let eligible =
                !routing::candidates_with_session(&state, model, None, affinity_key.as_deref())
                    .await
                    .is_empty();
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
        if let Some(response) = restore_error {
            return *response;
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
    let previous_preference = affinity_key
        .as_deref()
        .and_then(|key| state.affinity.preferred(key, &sources));
    let mut last_error = None;
    let source_count = sources.len();
    for (candidate_index, candidate) in sources.into_iter().enumerate() {
        if candidate.kind == "official"
            && !matches!(parts.uri.path(), "/v1/responses" | "/v1/responses/compact")
        {
            last_error = Some(error(StatusCode::NOT_IMPLEMENTED, "官方来源尚未验证此端点"));
            continue;
        }
        let mut route_lease = if let Some((_, lease)) = prepared_route.take() {
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
            let reserved = if rebuild
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
        let compatibility = parts
            .extensions
            .get::<crate::chatbot::ChatbotIngress>()
            .is_some();
        let mut compatibility_body = if compatibility {
            parsed.clone()
        } else {
            Value::Null
        };
        let mut compatibility_changes = Vec::new();
        let mut auth_refreshed = false;
        for _attempt_number in 0..if compatibility { 5 } else { 2 } {
            let eligible = routing::source_is_eligible(&state, &source, model).await;
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
            let (wire_bytes, mut ignored_parameters) = if parts
                .extensions
                .get::<crate::chatbot::ChatbotIngress>()
                .is_some()
            {
                match crate::chatbot::upstream_payload(
                    &compatibility_body,
                    source.kind == "official",
                    history_capture.as_ref().map(|h| &h.request["input"]),
                ) {
                    Ok((body, ignored)) => (Bytes::from(body), ignored),
                    Err(problem) => {
                        return crate::chatbot::error(
                            StatusCode::BAD_REQUEST,
                            problem.message,
                            Some(problem.param),
                        )
                    }
                }
            } else {
                (bytes.clone(), Vec::new())
            };
            ignored_parameters.extend(compatibility_changes.iter().cloned());
            ignored_parameters.sort();
            ignored_parameters.dedup();
            let mut attempt = Attempt::new(state.clone(), &source, &request_id, model, input_units);
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
                if compatibility {
                    headers.insert("originator", HeaderValue::from_static("codex_cli_rs"));
                    headers.insert("version", HeaderValue::from_static("0.162.1"));
                }
                if let Some(account) = &source.account_id {
                    let Ok(value) = HeaderValue::from_str(account) else {
                        return error(StatusCode::BAD_GATEWAY, "来源账号标识无效");
                    };
                    headers.insert(HeaderName::from_static("chatgpt-account-id"), value);
                }
            }
            let settings = state.settings.read().await.clone();
            let source_index = state.sources.read().await.iter().position(|runtime| {
                runtime.source.id == source.id && runtime.source.identity() == source.identity()
            });
            let selection = if degraded {
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
            } else if settings.load_balance {
                "load_balance"
            } else {
                "priority"
            };
            eprintln!(
                "routing_attempt unix={} source_index={} selection={} rebuilt={} bound={} opaque={}",
                crate::model::now(),
                source_index.map_or(-1, |index| index as i64),
                selection,
                degraded,
                binding.is_some(),
                opaque_state
            );
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
            let upstream = tokio::time::timeout(
                std::time::Duration::from_secs(120),
                client
                    .request(parts.method.clone(), url)
                    .headers(headers)
                    .body(wire_bytes)
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
                    eprintln!(
                        "upstream_transport_failure unix={} kind={} connect={} timeout={} uncertain={}",
                        crate::model::now(),
                        if source.kind == "official" { "official" } else { "api" },
                        definitely_unsubmitted,
                        err.is_timeout(),
                        !definitely_unsubmitted
                    );
                    attempt.finish("failed", None);
                    routing::mark_transport_failure(&state, &source.id, &err).await;
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
            let status = upstream.status();
            if !status.is_success() {
                eprintln!(
                    "upstream_http_failure unix={} kind={} status={}",
                    crate::model::now(),
                    if source.kind == "official" {
                        "official"
                    } else {
                        "api"
                    },
                    status.as_u16()
                );
            }
            let retry_after = upstream
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            if source.kind == "official" && status == StatusCode::UNAUTHORIZED && !auth_refreshed {
                auth_refreshed = true;
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
            let mut prefix = Vec::new();
            let mut context_rejected = false;
            if matches!(status.as_u16(), 400 | 404 | 409 | 422)
                && (binding.is_some() || opaque_state || compatibility)
            {
                if let Some(value) = read_error_prefix(&mut upstream, &mut prefix).await {
                    if compatibility
                        && status == StatusCode::BAD_REQUEST
                        && compatibility_changes.len() < 3
                    {
                        if let Some(field) = crate::chatbot::repair(&mut compatibility_body, &value)
                        {
                            compatibility_changes.push(field);
                            attempt.finish("failed", None);
                            drop(upstream);
                            continue;
                        }
                    }
                    context_rejected = explicit_context_rejection(&value);
                }
            }
            if !degraded
                && status.is_success()
                && (binding.is_some() || opaque_state)
                && (probe_unknown_id || state.settings.read().await.degraded_context_fallback)
            {
                // Hold only an initial bounded prefix. A failed Responses SSE
                // can use HTTP 200, so headers alone do not prove acceptance.
                // Once any output is seen (or the probe is inconclusive), its
                // original bytes are streamed without replay.
                context_rejected = match probe_context_rejection(
                    &mut upstream,
                    &mut prefix,
                    parsed["stream"].as_bool() == Some(true),
                    &mut attempt,
                )
                .await
                {
                    Ok(rejected) => rejected,
                    Err(()) => {
                        return error(
                            StatusCode::BAD_GATEWAY,
                            "来源响应中断，提交结果不明确；未自动重试",
                        );
                    }
                };
            }
            if matches!(status.as_u16(), 401 | 402 | 403 | 429 | 500..=599) {
                routing::mark_failure(&state, &source.id, status.as_u16(), retry_after).await;
                if matches!(status.as_u16(), 402 | 403 | 429) {
                    // Read only a bounded error prefix, retaining the original
                    // chunks for exact forwarding if this is the final source.
                    if let Some(value) = read_error_prefix(&mut upstream, &mut prefix).await {
                        let code = value
                            .pointer("/error/code")
                            .or_else(|| value.pointer("/error/type"))
                            .and_then(Value::as_str);
                        routing::mark_rejection(&state, &source.id, code, retry_after).await;
                    }
                }
            }
            if probe_unknown_id && context_rejected && candidate_index + 1 < source_count {
                // A missing local mapping can mean a different candidate owns
                // this ID. Explicit rejection permits a bounded unchanged probe
                // before requiring local history for a new context.
                attempt.finish("failed", None);
                drop(route_lease.take());
                last_error = Some(
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
                        },
                        None,
                    )
                    .await,
                );
                break;
            }
            if !degraded
                && (binding.is_some() || opaque_state || origin.is_some())
                && (context_rejected || matches!(status.as_u16(), 401 | 402 | 403 | 429))
                && state.settings.read().await.degraded_context_fallback
                && !routing::candidates_with_session(&state, model, None, affinity_key.as_deref())
                    .await
                    .is_empty()
            {
                attempt.finish("failed", None);
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
                if context_rejected {
                    context.rejected_source = Some(source.id.clone());
                }
                return Box::pin(forward_once(state, retry, Some(context), replans)).await;
            }
            let safe_to_fallback = binding.is_none()
                && !opaque_state
                && (origin.is_none() || planned_return)
                && (matches!(status.as_u16(), 401 | 402 | 403 | 429)
                    || (degraded && context_rejected));
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
            let mut response = stream_response(
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
            if !ignored_parameters.is_empty() {
                if let Ok(value) = HeaderValue::from_str(&ignored_parameters.join(", ")) {
                    response
                        .headers_mut()
                        .insert("x-codexpress-adapted-parameters", value);
                }
            }
            return response;
        }
    }
    last_error.unwrap_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "所有来源暂时不可用"))
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
}
