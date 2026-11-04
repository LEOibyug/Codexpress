use super::{dashboard, jobs, resets, safety};
use crate::model::AppState;
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::Response,
    Router,
};
use serde_json::{json, Map, Value};
use std::{sync::Arc, time::Duration};

const BODY_LIMIT: usize = 64 * 1024;
pub(super) struct Fault {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}
impl Fault {
    pub(super) fn invalid() -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: "请求参数无效",
        }
    }
    pub(super) fn failed() -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "operation_failed",
            message: "操作未完成，请检查来源状态后重试",
        }
    }
    pub(super) fn missing() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: "接口或操作不存在",
        }
    }
    pub(super) fn response(self) -> Response {
        response(
            self.status,
            json!({"error":{"code":self.code,"message":self.message}}),
        )
    }
}
pub(super) fn response(status: StatusCode, value: Value) -> Response {
    let body = if status == StatusCode::NO_CONTENT {
        Body::empty()
    } else {
        Body::from(value.to_string())
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}
fn denied(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    Fault {
        status,
        code,
        message,
    }
    .response()
}
fn finish(mut response: Response, dashboard: bool) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    if dashboard {
        return response;
    }
    // No cookie credentials are ever accepted. Cross-origin clients must carry
    // the independent Bearer key; Origin is deliberately not an auth boundary.
    headers.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    headers.insert(
        "access-control-allow-methods",
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        "access-control-allow-headers",
        HeaderValue::from_static("Authorization, Content-Type"),
    );
    headers.remove("access-control-allow-credentials");
    response
}
async fn gate(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let dashboard = dashboard::is_path(request.uri().path());
    finish(checked(state, request, next).await, dashboard)
}
async fn checked(state: Arc<AppState>, request: Request, next: Next) -> Response {
    let revision = state
        .control
        .revision
        .load(std::sync::atomic::Ordering::Acquire);
    let settings = match state.control.settings() {
        Ok(settings) => settings,
        Err(_) => {
            return denied(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "管理 API 暂不可用",
            )
        }
    };
    if !settings.enabled {
        return denied(
            StatusCode::SERVICE_UNAVAILABLE,
            "disabled",
            "管理 API 未启用",
        );
    }
    let headers = request.headers();
    for name in [
        "host",
        "authorization",
        "origin",
        "content-type",
        "content-length",
        "transfer-encoding",
        "access-control-request-method",
        "access-control-request-headers",
    ] {
        if headers.get_all(name).iter().count() > 1 {
            return denied(
                StatusCode::BAD_REQUEST,
                "duplicate_header",
                "请求头不能重复",
            );
        }
    }
    let expected_hosts = [
        format!("127.0.0.1:{}", settings.port),
        format!("localhost:{}", settings.port),
    ];
    let valid_host = |host: &str| {
        expected_hosts
            .iter()
            .any(|expected| host.eq_ignore_ascii_case(expected))
    };
    if !headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(valid_host)
        || request
            .uri()
            .authority()
            .is_some_and(|authority| !valid_host(authority.as_str()))
    {
        return denied(
            StatusCode::MISDIRECTED_REQUEST,
            "invalid_host",
            "客户端地址与管理 API 端口不匹配",
        );
    }
    if dashboard::is_path(request.uri().path()) {
        if request.method() != Method::GET {
            return denied(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", "后台面板仅支持 GET");
        }
        return next.run(request).await;
    }
    if headers.contains_key(header::COOKIE) {
        return denied(
            StatusCode::BAD_REQUEST,
            "cookies_forbidden",
            "管理 API 不接受 Cookie",
        );
    }
    if request
        .uri()
        .path_and_query()
        .is_some_and(|path| path.as_str().len() > 8192)
    {
        return denied(StatusCode::URI_TOO_LONG, "uri_too_long", "请求地址过长");
    }
    if request.uri().query().is_some_and(|query| {
        url::form_urlencoded::parse(query.as_bytes()).any(|(key, _)| {
            ["key", "api_key", "token", "access_token", "authorization"]
                .iter()
                .any(|name| key.eq_ignore_ascii_case(name))
        })
    }) {
        return denied(
            StatusCode::BAD_REQUEST,
            "query_credentials_forbidden",
            "不能通过查询参数传递密钥",
        );
    }
    if request.method() == Method::OPTIONS {
        let method = headers
            .get("access-control-request-method")
            .and_then(|value| value.to_str().ok());
        let requested = headers
            .get("access-control-request-headers")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !matches!(method, Some("GET" | "POST"))
            || requested.len() > 2048
            || requested.split(',').map(str::trim).any(|name| {
                !name.eq_ignore_ascii_case("authorization")
                    && !name.eq_ignore_ascii_case("content-type")
            })
            || !requested
                .split(',')
                .any(|name| name.trim().eq_ignore_ascii_case("authorization"))
        {
            return denied(
                StatusCode::FORBIDDEN,
                "invalid_preflight",
                "跨域请求必须显式携带 Authorization",
            );
        }
        return response(StatusCode::NO_CONTENT, Value::Null);
    }
    let authorized = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|key| !key.is_empty() && key == settings.key);
    if !authorized {
        return denied(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "请提供有效的管理 API Bearer Key",
        );
    }
    if !matches!(*request.method(), Method::GET | Method::POST) {
        return denied(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "此接口仅支持 GET 或 POST",
        );
    }
    if request.method() == Method::POST {
        if request.uri().query().is_some() {
            return Fault::invalid().response();
        }
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !content_type
            .split(';')
            .next()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
        {
            return denied(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "invalid_content_type",
                "请使用 application/json",
            );
        }
    }
    let is_get = request.method() == Method::GET;
    let (parts, body) = request.into_parts();
    let bytes =
        match tokio::time::timeout(Duration::from_secs(15), to_bytes(body, BODY_LIMIT)).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => {
                return denied(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "body_too_large",
                    "请求正文不能超过 64 KiB",
                )
            }
            Err(_) => return denied(StatusCode::REQUEST_TIMEOUT, "body_timeout", "读取请求超时"),
        };
    if is_get && !bytes.is_empty() {
        return Fault::invalid().response();
    }
    let current = match state.control.settings() {
        Ok(current) => current,
        Err(_) => {
            return denied(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "管理 API 暂不可用",
            )
        }
    };
    if !current.enabled
        || current.port != settings.port
        || current.key != settings.key
        || state
            .control
            .revision
            .load(std::sync::atomic::Ordering::Acquire)
            != revision
    {
        return denied(
            StatusCode::UNAUTHORIZED,
            "access_changed",
            "管理 API 设置已变化，请重新提交",
        );
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}
pub(super) fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .fallback(handle)
        .layer(middleware::from_fn_with_state(state.clone(), gate))
        .with_state(state)
}
async fn handle(State(state): State<Arc<AppState>>, request: Request) -> Response {
    if dashboard::is_path(request.uri().path()) {
        return dashboard::handle(&state, request).await;
    }
    match dispatch(&state, request).await {
        Ok((status, value)) => response(status, safety::expose(&state, value).await),
        Err(fault) => fault.response(),
    }
}
fn query(raw: Option<&str>) -> Result<Value, Fault> {
    let mut object = Map::new();
    for (key, value) in url::form_urlencoded::parse(raw.unwrap_or("").as_bytes()) {
        let parsed = match key.as_ref() {
            "from" | "to" | "before" | "limit" | "days" => {
                json!(value.parse::<u64>().map_err(|_| Fault::invalid())?)
            }
            "day_starts" => serde_json::from_str::<Value>(&value).map_err(|_| Fault::invalid())?,
            "source_id" | "model" | "ingress" | "status" => Value::String(value.into_owned()),
            _ => return Err(Fault::invalid()),
        };
        if object.insert(key.into_owned(), parsed).is_some() {
            return Err(Fault::invalid());
        }
    }
    Ok(Value::Object(object))
}
pub(super) async fn usage(state: &Arc<AppState>, payload: Value, kind: &'static str) -> Result<Value, Fault> {
    let store = state.usage.clone();
    tokio::task::spawn_blocking(move || match kind {
        "analytics" => store.analytics(&payload),
        "requests" => store.requests(&payload),
        _ => {
            safety::fields(&payload, &["days", "source_id", "model"])?;
            let days = match payload.get("days") {
                None => 30,
                Some(value) => value
                    .as_u64()
                    .filter(|days| (1..=3660).contains(days))
                    .context("无效天数")? as u32,
            };
            let source = safety::optional_text(&payload, "source_id")?;
            let model = safety::optional_text(&payload, "model")?;
            store.summary_filtered(days, source.as_deref(), model.as_deref())
        }
    })
    .await
    .map_err(|_| Fault::failed())?
    .map_err(|_| Fault::invalid())
}
use anyhow::Context;
async fn dispatch(state: &Arc<AppState>, request: Request) -> Result<(StatusCode, Value), Fault> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    if method == Method::GET {
        let payload = query(request.uri().query())?;
        let value = match path.as_str() {
            "/v1/status" | "/v1/sources" => {
                safety::fields(&payload, &[]).map_err(|_| Fault::invalid())?;
                let mut status = crate::management::status(state)
                    .await
                    .map_err(|_| Fault::failed())?;
                if path == "/v1/sources" {
                    json!({"sources":status["sources"]})
                } else {
                    status["usage"] = usage(state, json!({}), "summary").await?;
                    status["reconciliation"] = crate::balance::billing::status(state);
                    status["control"] =
                        state.control.status().await.map_err(|_| Fault::failed())?;
                    status["chatbot"] =
                        state.chatbot.status().await.map_err(|_| Fault::failed())?;
                    status["images"] = state.images.status().await.map_err(|_| Fault::failed())?;
                    status
                }
            }
            "/v1/usage" => usage(state, payload, "summary").await?,
            "/v1/usage/analytics" => {
                safety::fields(&payload, &["from", "to", "day_starts", "source_id", "model", "ingress"]).map_err(|_| Fault::invalid())?;
                usage(state, payload, "analytics").await?
            }
            "/v1/usage/requests" => usage(state, payload, "requests").await?,
            "/v1/reconciliation" => {
                safety::fields(&payload, &[]).map_err(|_| Fault::invalid())?;
                crate::balance::billing::status(state)
            }
            _ => {
                safety::fields(&payload, &[]).map_err(|_| Fault::invalid())?;
                if let Some(id) = path.strip_prefix("/v1/operations/") {
                    jobs::get(state, id)
                        .await
                        .map_err(|_| Fault::invalid())?
                        .ok_or_else(Fault::missing)?
                } else if let Some(id) = path.strip_prefix("/v1/credits/reset-requests/") {
                    resets::get(state, id)
                        .map_err(|_| Fault::invalid())?
                        .ok_or_else(Fault::missing)?
                } else {
                    return Err(Fault::missing());
                }
            }
        };
        return Ok((StatusCode::OK, value));
    }
    let bytes = to_bytes(request.into_body(), BODY_LIMIT)
        .await
        .map_err(|_| Fault::invalid())?;
    let payload: Value = serde_json::from_slice(&bytes).map_err(|_| Fault::invalid())?;
    let (status, value) = match path.as_str() {
        "/v1/routing/takeover" | "/v1/routing/cancel" | "/v1/routing/recover" => {
            let (op, field) = match path.as_str() {
                "/v1/routing/takeover" => ("routing.try", "source_id"),
                "/v1/routing/cancel" => ("routing.cancel", "id"),
                _ => ("routing.recover", "source_id"),
            };
            safety::fields(&payload, &[field]).map_err(|_| Fault::invalid())?;
            safety::text(&payload, field).map_err(|_| Fault::invalid())?;
            (
                StatusCode::OK,
                crate::routing::manual::handle(state, op, payload)
                    .await
                    .map_err(|_| Fault::failed())?,
            )
        }
        "/v1/refresh/balances" | "/v1/refresh/quotas" => {
            let kind = if path.ends_with("balances") {
                "balances"
            } else {
                "quotas"
            };
            (
                StatusCode::ACCEPTED,
                jobs::start(state, kind, payload)
                    .await
                    .map_err(|_| Fault::invalid())?,
            )
        }
        "/v1/refresh/billing" => {
            safety::fields(&payload, &["source_id"]).map_err(|_| Fault::invalid())?;
            let source_id =
                safety::optional_text(&payload, "source_id").map_err(|_| Fault::invalid())?;
            if let Some(id) = &source_id {
                if !state
                    .sources
                    .read()
                    .await
                    .iter()
                    .any(|runtime| &runtime.source.id == id && runtime.source.kind == "api")
                {
                    return Err(Fault::invalid());
                }
            }
            (
                StatusCode::ACCEPTED,
                crate::balance::billing::schedule(state, true, source_id.as_deref()),
            )
        }
        "/v1/credits/reset-requests" => {
            safety::fields(&payload, &["request_id", "source_id", "credit_id"])
                .map_err(|_| Fault::invalid())?;
            safety::text(&payload, "source_id").map_err(|_| Fault::invalid())?;
            uuid::Uuid::parse_str(
                safety::text(&payload, "request_id").map_err(|_| Fault::invalid())?,
            )
            .map_err(|_| Fault::invalid())?;
            safety::optional_text(&payload, "credit_id").map_err(|_| Fault::invalid())?;
            (
                StatusCode::ACCEPTED,
                resets::enqueue(state, payload).await.map_err(|_| Fault {
                    status: StatusCode::CONFLICT,
                    code: "reset_conflict",
                    message: "重置请求不能入队：标识、账号绑定或队列状态有冲突",
                })?,
            )
        }
        _ => return Err(Fault::missing()),
    };
    Ok((status, value))
}
