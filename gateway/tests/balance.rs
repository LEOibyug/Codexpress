#[path = "common/server.rs"]
mod server;
#[path = "common/source.rs"]
mod source_fixture;

use axum::{
    extract::Request,
    http::StatusCode,
    response::Response,
    routing::{get, post},
    Json, Router,
};
use codexpress_gateway::{
    management,
    model::{AppState, SourceRuntime},
};
use serde_json::{json, Value};
use server::serve;
use source_fixture::api_source;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

async fn state(base: &str) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().into(), 0, "local".into()).unwrap();
    state
        .sources
        .write()
        .await
        .push(SourceRuntime::new(api_source(
            "balance-api",
            "API",
            base,
            "PRIVATE-TEST-KEY",
        )));
    (dir, state)
}
async fn query(state: &Arc<AppState>) -> Value {
    management::dispatch(state, "balance.query", json!({"source_id":"balance-api"}))
        .await
        .unwrap()
}

#[tokio::test]
async fn cch_logs_in_once_and_only_reports_total_account_balance() {
    let logins = Arc::new(AtomicUsize::new(0));
    let copy = logins.clone();
    let (address, task) = serve(Router::new()
        .route("/mounted/api/v1/me/quota", get(|request: Request| async move {
            assert!(request.headers().get("authorization").is_none());
            if request.headers().get("cookie").and_then(|v| v.to_str().ok()) != Some("auth-token=PRIVATE-SESSION") {
                return (StatusCode::UNAUTHORIZED, Json(json!({"type":"urn:claude-code-hub:problem:auth.invalid", "errorCode":"auth.invalid"})));
            }
            (StatusCode::OK, Json(json!({"userLimitTotalUsd":1000,"userCurrentTotalUsd":125.5,
                "keyLimitTotalUsd":500,"keyCurrentTotalUsd":80,"userLimitDailyUsd":10,"userCurrentDailyUsd":9.9,
                "userName":"PRIVATE-USER"})))
        }))
        .route("/mounted/api/auth/login", post(move |Json(body): Json<Value>| {
            copy.fetch_add(1, Ordering::SeqCst);
            async move {
                assert_eq!(body, json!({"key":"PRIVATE-TEST-KEY"}));
                Response::builder().header("set-cookie", "auth-token=PRIVATE-SESSION; Path=/; HttpOnly; Secure")
                    .header("content-type", "application/json").body(axum::body::Body::from("{\"ok\":true}")).unwrap()
            }
        }))
        .fallback(|| async { StatusCode::NOT_FOUND })).await;
    let (_dir, state) = state(&format!("{address}/mounted/v1/responses")).await;
    let result = query(&state).await;
    assert_eq!(result["status"], "available");
    assert_eq!(result["amount"], 874.5);
    assert_eq!(result["currency"], "USD");
    assert_eq!(result["scope"], "account");
    assert!(!result.to_string().contains("PRIVATE"));
    assert_eq!(query(&state).await, result);
    assert_eq!(logins.load(Ordering::SeqCst), 1);
    assert!(state.usage.summary(1).unwrap()["recent"]
        .as_array()
        .unwrap()
        .is_empty());
    task.abort();
}

#[tokio::test]
async fn detects_new_api_units_and_invalidates_cache_when_key_changes() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let (address, task) = serve(Router::new()
        .route("/api/usage/token/", get(move |request: Request| {
            copy.fetch_add(1, Ordering::SeqCst);
            async move {
                let amount = if request.headers()["authorization"] == "Bearer PRIVATE-TEST-KEY" { 1_250_000 } else { 0 };
                Json(json!({"code":true,"data":{"total_granted":2_500_000,"total_used":1_250_000,"total_available":amount}}))
            }
        }))
        .fallback(|| async { StatusCode::NOT_FOUND })).await;
    let (_dir, state) = state(&format!("{address}/v1")).await;
    let a = query(&state).await;
    assert_eq!(a["amount"], 2.5);
    assert_eq!(a["scope"], "key");
    assert_eq!(query(&state).await, a);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    state.sources.write().await[0].source.secret = json!({"api_key":"PRIVATE-REPLACEMENT"});
    assert_eq!(query(&state).await["amount"], 0.0);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn probes_other_protocols_after_rejections_and_rejects_periodic_quotas() {
    let (address, task) = serve(Router::new()
        .route("/api/usage/token/", get(|| async { (StatusCode::UNAUTHORIZED, Json(json!({"error":"PRIVATE"}))) }))
        .route("/v1/usage", get(|| async { Json(json!({"isValid":true,"remaining":999,"subscription":{"daily_limit":1000}})) }))
        .route("/user/balance", get(|| async { Json(json!({"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"24.35"}]})) }))
        .fallback(|| async { StatusCode::NOT_FOUND })).await;
    let (_dir, state) = state(&format!("{address}/v1")).await;
    let result = query(&state).await;
    assert_eq!(result["amount"], 24.35);
    assert_eq!(result["currency"], "CNY");
    task.abort();
}

#[tokio::test]
async fn unsupported_or_invalid_balances_are_not_invented_or_logged() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let (address, task) = serve(Router::new().fallback(move || {
        copy.fetch_add(1, Ordering::SeqCst);
        async { Json(json!({"success":false,"data":{"balance":777},"message":"PRIVATE-ERROR"})) }
    }))
    .await;
    let (_dir, state) = state(&format!("{address}/v1")).await;
    let result = query(&state).await;
    assert_eq!(result["status"], "unavailable");
    assert!(result["amount"].is_null());
    assert!(!result.to_string().contains("PRIVATE"));
    let count = hits.load(Ordering::SeqCst);
    assert_eq!(query(&state).await, result);
    assert_eq!(hits.load(Ordering::SeqCst), count);
    task.abort();
}

#[tokio::test]
async fn balance_uses_explicit_proxy_without_falling_back_to_direct() {
    let direct_hits = Arc::new(AtomicUsize::new(0));
    let copy = direct_hits.clone();
    let (address, upstream) = serve(Router::new().fallback(move || {
        copy.fetch_add(1, Ordering::SeqCst);
        async { StatusCode::IM_A_TEAPOT }
    }))
    .await;
    let (proxy, proxy_task) = serve(Router::new().fallback(|request: Request| async move {
        assert_eq!(request.uri().path(), "/api/v1/me/quota");
        assert_eq!(request.headers()["x-api-key"], "PRIVATE-TEST-KEY");
        Json(json!({"userLimitTotalUsd":100,"userCurrentTotalUsd":20}))
    }))
    .await;
    let (_dir, state) = state(&format!("{address}/v1")).await;
    state.sources.write().await[0].source.proxy =
        serde_json::from_value(json!({"mode":"explicit","url":proxy})).unwrap();
    assert_eq!(query(&state).await["amount"], 80.0);
    proxy_task.abort();
    let unavailable = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = unavailable.local_addr().unwrap().port();
    drop(unavailable);
    state.sources.write().await[0].source.proxy.url = Some(format!("http://127.0.0.1:{port}"));
    assert_eq!(query(&state).await["status"], "unavailable");
    assert_eq!(direct_hits.load(Ordering::SeqCst), 0);
    assert_eq!(state.sources.read().await[0].health, "ready");
    upstream.abort();
}

#[tokio::test]
async fn redirects_do_not_forward_secrets_and_rate_limits_stop_the_probe() {
    let leaked = Arc::new(AtomicUsize::new(0));
    let copy = leaked.clone();
    let (elsewhere, other_task) = serve(Router::new().fallback(move || {
        copy.fetch_add(1, Ordering::SeqCst);
        async { StatusCode::OK }
    }))
    .await;
    let (address, task) = serve(Router::new().fallback(move || {
        let elsewhere = elsewhere.clone();
        async move {
            Response::builder()
                .status(302)
                .header("location", elsewhere)
                .body(axum::body::Body::empty())
                .unwrap()
        }
    }))
    .await;
    let (_dir, first) = state(&format!("{address}/v1")).await;
    assert_eq!(query(&first).await["status"], "unavailable");
    assert_eq!(leaked.load(Ordering::SeqCst), 0);
    task.abort();
    other_task.abort();

    let calls = Arc::new(AtomicUsize::new(0));
    let copy = calls.clone();
    let (address, task) = serve(Router::new().fallback(move || {
        copy.fetch_add(1, Ordering::SeqCst);
        async { StatusCode::TOO_MANY_REQUESTS }
    }))
    .await;
    let (_dir, second) = state(&format!("{address}/v1")).await;
    assert_eq!(query(&second).await["status"], "unavailable");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn editing_a_key_during_query_discards_the_old_account_result() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (a, b) = (entered.clone(), release.clone());
    let (address, task) = serve(Router::new().route(
        "/api/v1/me/quota",
        get(move || {
            let (a, b) = (a.clone(), b.clone());
            async move {
                a.notify_one();
                b.notified().await;
                Json(json!({"userLimitTotalUsd":100,"userCurrentTotalUsd":10}))
            }
        }),
    ))
    .await;
    let (_dir, state) = state(&format!("{address}/v1")).await;
    let copy = state.clone();
    let pending = tokio::spawn(async move {
        management::dispatch(&copy, "balance.query", json!({"source_id":"balance-api"})).await
    });
    entered.notified().await;
    state.sources.write().await[0].source.secret = json!({"api_key":"PRIVATE-REPLACEMENT"});
    release.notify_one();
    let error = pending.await.unwrap().unwrap_err().to_string();
    assert!(error.contains("来源已变化"));
    assert!(!error.contains("PRIVATE"));
    task.abort();
}
