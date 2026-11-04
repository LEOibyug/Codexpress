use super::http::{self, Fault};
use crate::model::AppState;
use axum::{body::Body, extract::Request, http::{header, HeaderValue, StatusCode}, response::Response};
use serde_json::{json, Map, Value};
use std::sync::Arc;

pub(super) fn is_path(path: &str) -> bool {
    path == "/dashboard" || path.starts_with("/dashboard/")
}
fn asset(path: &str) -> Option<(&'static str, &'static str)> {
    match path {
        "/dashboard" | "/dashboard/" => Some(("text/html; charset=utf-8", include_str!("../../resources/dashboard/index.html"))),
        "/dashboard/app.js" => Some(("text/javascript; charset=utf-8", include_str!("../../resources/dashboard/app.js"))),
        "/dashboard/style.css" => Some(("text/css; charset=utf-8", include_str!("../../resources/dashboard/style.css"))),
        _ => None,
    }
}
pub(super) async fn handle(state: &Arc<AppState>, request: Request) -> Response {
    if let Some((mime, body)) = asset(request.uri().path()) {
        let mut response = Response::new(Body::from(body));
        response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
        return response;
    }
    match dispatch(state, request).await {
        Ok(value) => http::response(StatusCode::OK, value),
        Err(fault) => fault.response(),
    }
}
fn query(raw: Option<&str>, allowed: &[&str]) -> Result<Value, Fault> {
    let mut object = Map::new();
    for (key, value) in url::form_urlencoded::parse(raw.unwrap_or("").as_bytes()) {
        if !allowed.contains(&key.as_ref()) { return Err(Fault::invalid()); }
        let value = match key.as_ref() {
            "from" | "to" | "before" | "limit" | "offset" | "body_offset" | "body_limit" => json!(value.parse::<u64>().map_err(|_| Fault::invalid())?),
            "record_only" => match value.as_ref() { "true" => json!(true), "false" => json!(false), _ => return Err(Fault::invalid()) },
            "day_starts" => serde_json::from_str::<Value>(&value).map_err(|_| Fault::invalid())?,
            _ => Value::String(value.into_owned()),
        };
        if object.insert(key.into_owned(), value).is_some() { return Err(Fault::invalid()); }
    }
    Ok(Value::Object(object))
}
fn identifier(raw: &str) -> Result<String, Fault> {
    let mut fields = url::form_urlencoded::parse(raw.as_bytes());
    let (id, value) = fields.next().ok_or_else(Fault::invalid)?;
    if !value.is_empty() || fields.next().is_some() {
        return Err(Fault::invalid());
    }
    Ok(id.into_owned())
}
async fn dispatch(state: &Arc<AppState>, request: Request) -> Result<Value, Fault> {
    let path = request.uri().path();
    let raw = request.uri().query();
    let value = match path {
        "/dashboard/api/status" => {
            query(raw, &[])?;
            let mut status = crate::management::status(state).await.map_err(|_| Fault::failed())?;
            status["usage"] = http::usage(state, json!({}), "summary").await?;
            status["reconciliation"] = crate::balance::billing::status(state);
            status["control"] = state.control.status().await.map_err(|_| Fault::failed())?;
            status["chatbot"] = state.chatbot.status().await.map_err(|_| Fault::failed())?;
            status["images"] = state.images.status().await.map_err(|_| Fault::failed())?;
            let history = state.history.clone();
            status["history"] = tokio::task::spawn_blocking(move || history.status()).await.map_err(|_| Fault::failed())?.map_err(|_| Fault::failed())?;
            status
        }
        "/dashboard/api/analytics" => {
            let payload = query(raw, &["from", "to", "day_starts", "source_id", "model", "ingress", "status", "search"])?;
            http::usage(state, payload, "analytics").await?
        }
        "/dashboard/api/requests" => {
            let payload = query(raw, &["from", "to", "source_id", "model", "ingress", "status", "search", "before", "limit"])?;
            http::usage(state, payload, "requests").await?
        }
        "/dashboard/api/cache" => {
            let payload = query(raw, &["from", "to", "source_id", "model", "ingress", "attempt_id", "before", "limit"])?;
            let history = state.history.clone();
            tokio::task::spawn_blocking(move || history.inspect_list(&payload)).await.map_err(|_| Fault::failed())?.map_err(|_| Fault::invalid())?
        }
        _ => {
            if let Some(id) = path.strip_prefix("/dashboard/api/requests/") {
                let mut payload = query(raw, &["body_offset", "body_limit", "record_only"])?;
                payload["attempt_id"] = json!(identifier(id)?);
                let usage = state.usage.clone();
                tokio::task::spawn_blocking(move || usage.detail(&payload)).await.map_err(|_| Fault::failed())?.map_err(|_| Fault::invalid())?
            } else if let Some(id) = path.strip_prefix("/dashboard/api/cache/") {
                let mut payload = query(raw, &["offset", "limit"])?;
                payload["id"] = json!(identifier(id)?);
                let history = state.history.clone();
                tokio::task::spawn_blocking(move || history.inspect_detail(&payload)).await.map_err(|_| Fault::failed())?.map_err(|_| Fault::invalid())?
            } else { return Err(Fault::missing()); }
        }
    };
    Ok(value)
}
