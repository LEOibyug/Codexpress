#[path = "common/server.rs"]
mod server;
#[path = "common/source.rs"]
mod source_fixture;

use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::{HeaderValue, StatusCode},
    routing::get,
    Json, Router,
};
use codexpress_gateway::{
    control,
    model::{now, AppState, Source, SourceRuntime},
    usage::{TokenDetails, UsageRecord},
};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tower::ServiceExt;

fn available_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
async fn enabled() -> (tempfile::TempDir, Arc<AppState>, String, String) {
    let directory = tempfile::tempdir().unwrap();
    let state =
        AppState::new(directory.path().into(), 17863, "PRIVATE-GATEWAY-KEY".into()).unwrap();
    assert_eq!(
        control::handle(&state, "control.status", json!({}))
            .await
            .unwrap()["enabled"],
        false
    );
    let status = control::configure(&state, json!({"enabled":true,"port":available_port()}))
        .await
        .unwrap();
    assert_eq!(status["running"], true);
    let key = state.control.key().unwrap()["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let base = status["base_url"].as_str().unwrap().to_owned();
    (directory, state, key, base)
}
fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}
async fn read(response: reqwest::Response) -> Value {
    response.json().await.unwrap()
}
async fn stop(state: &Arc<AppState>) {
    control::configure(state, json!({"enabled":false}))
        .await
        .unwrap();
}
fn request(state: &AppState, key: &str, path: &str, body: Option<Value>) -> Request {
    Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header(
            "host",
            format!("127.0.0.1:{}", state.control.port().unwrap()),
        )
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(
            body.map(|value| Body::from(value.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap()
}
async fn operation(client: &reqwest::Client, base: &str, key: &str, id: &Value) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let value = read(
                client
                    .get(format!("{base}/operations/{}", id.as_str().unwrap()))
                    .bearer_auth(key)
                    .send()
                    .await
                    .unwrap(),
            )
            .await;
            if value["status"] != "running" {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}
fn official(proxy: &str) -> SourceRuntime {
    let source: Source = serde_json::from_value(json!({
        "id":"official","name":"官方测试账号","kind":"official","base_url":"https://chatgpt.com/backend-api/codex",
        "enabled":true,"priority":1,"account_id":"account-fixture","proxy":{"mode":"explicit","url":proxy},
        "secret":{"account_id":"account-fixture","access_token":"PRIVATE-OFFICIAL-TOKEN","expires_at":now()+3600}
    })).unwrap();
    SourceRuntime::new(source)
}
fn record() -> UsageRecord {
    UsageRecord {
        attempt_id: "attempt-fixture".into(),
        request_id: "request-fixture".into(),
        source_id: "api".into(),
        source_name: "API".into(),
        model: "gpt-6.1-sol".into(),
        started_at: now(),
        status: "success".into(),
        input_tokens: 100,
        output_tokens: 20,
        cached_input_tokens: 10,
        token_details: TokenDetails {
            source_kind: Some("api".into()),
            output_available: true,
            cached_input_reported: true,
            ..Default::default()
        },
        complete: true,
        latency_ms: 400,
        first_token_latency_ms: Some(50),
        generation_ms: Some(300),
    }
}

#[tokio::test]
async fn localhost_auth_host_cors_and_revocation_are_independent_of_browser_origin() {
    let (directory, state, key, base) = enabled().await;
    let http = client();
    assert_eq!(
        std::fs::metadata(directory.path().join("control-access.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    for wrong in ["PRIVATE-GATEWAY-KEY", "PRIVATE-UPSTREAM-KEY", "wrong"] {
        let response = http
            .get(format!("{base}/status"))
            .bearer_auth(wrong)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(!response.text().await.unwrap().contains("PRIVATE"));
    }
    let response = http
        .get(format!("{base}/status"))
        .header("origin", "https://untrusted.example")
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    assert!(!response
        .headers()
        .contains_key("access-control-allow-credentials"));
    let body = response.text().await.unwrap();
    assert!(!body.contains(&key));
    assert!(!body.contains("PRIVATE"));
    for host in [
        "attacker.example",
        "127.0.0.1",
        "127.0.0.1:1",
        "localhost.evil:17867",
    ] {
        assert_eq!(
            http.get(format!("{base}/status"))
                .header("host", host)
                .bearer_auth(&key)
                .send()
                .await
                .unwrap()
                .status(),
            421
        );
    }
    assert_eq!(
        http.get(format!("{base}/status"))
            .header("cookie", "session=anything")
            .bearer_auth(&key)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        http.get(format!("{base}/status?api_key={key}"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let preflight = http
        .request(reqwest::Method::OPTIONS, format!("{base}/status"))
        .header("origin", "https://untrusted.example")
        .header("access-control-request-method", "GET")
        .header("access-control-request-headers", "authorization")
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), 204);
    assert_eq!(
        http.get(format!("{base}/status"))
            .header("origin", "http://localhost")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    for name in ["host", "authorization", "origin", "content-type"] {
        let mut request = request(&state, &key, "/v1/status", None);
        let value = request
            .headers()
            .get(name)
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static("https://example.test"));
        request.headers_mut().append(name, value.clone());
        if name == "origin" {
            request.headers_mut().append(name, value);
        }
        let response = control::router(state.clone())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "duplicate {name}");
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    control::configure(&state, json!({"regenerate_key":true}))
        .await
        .unwrap();
    assert_eq!(
        http.get(format!("{base}/status"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let rotated = state.control.key().unwrap()["key"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(key, rotated);
    assert_eq!(
        http.get(format!("{base}/status"))
            .bearer_auth(&rotated)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    stop(&state).await;
    assert_eq!(
        control::router(state.clone())
            .oneshot(request(&state, &rotated, "/v1/status", None))
            .await
            .unwrap()
            .status(),
        503
    );
}

#[tokio::test]
async fn http_has_no_arbitrary_ipc_configuration_body_or_confirmation_surface() {
    let (_directory, state, key, base) = enabled().await;
    let http = client();
    for path in [
        "dispatch",
        "control/key",
        "control/configure",
        "config/attach",
        "credentials",
        "official/credits/consume",
        "credits/reset-requests/confirm",
    ] {
        let response = http
            .post(format!("{base}/{path}"))
            .bearer_auth(&key)
            .json(&json!({"op":"official.credits.consume","confirmed":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{path}");
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    assert_eq!(
        http.post(format!("{base}/credits/reset-requests"))
            .bearer_auth(&key)
            .json(
                &json!({"request_id":uuid::Uuid::new_v4(),"source_id":"official","confirmed":true})
            )
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        http.get(format!("{base}/usage/requests?from=1&to=2&body_limit=100"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        http.get(format!("{base}/usage/requests?from=1&from=2&to=3"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        http.get(format!(
            "{base}/usage/requests?from=1&to=2&search=hidden-error"
        ))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap()
        .status(),
        400
    );
    let response = http
        .post(format!("{base}/routing/takeover"))
        .bearer_auth(&key)
        .header("content-type", "application/json")
        .body("x".repeat(65 * 1024))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(control::configure(&state, json!({"port":state.port}))
        .await
        .is_err());
    for status in [
        state.chatbot.status().await.unwrap(),
        state.images.status().await.unwrap(),
    ] {
        assert!(control::configure(&state, json!({"port":status["port"]}))
            .await
            .is_err());
    }
    for reused in [
        json!({"key":state.gateway_key}),
        state.chatbot.key().unwrap(),
        state.images.key().unwrap(),
    ] {
        assert!(control::configure(&state, reused).await.is_err());
    }
    stop(&state).await;
}

#[tokio::test]
async fn explicit_balance_refresh_and_statistics_work_without_read_triggered_upstream_requests() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let (upstream, task) = server::serve(Router::new().route("/api/usage/token/", get(move |request: Request| {
        copy.fetch_add(1, Ordering::SeqCst);
        async move {
            assert_eq!(request.headers()["authorization"], "Bearer PRIVATE-UPSTREAM-KEY");
            Json(json!({"code":true,"data":{"total_granted":2_500_000,"total_used":1_250_000,"total_available":1_250_000}}))
        }
    })).fallback(|| async { StatusCode::NOT_FOUND })).await;
    let (_directory, state, key, base) = enabled().await;
    let mut runtime = SourceRuntime::new(source_fixture::api_source(
        "api",
        "API",
        &format!("{upstream}/v1"),
        "PRIVATE-UPSTREAM-KEY",
    ));
    runtime.last_error = Some("PRIVATE-ERROR-BODY".into());
    state.sources.write().await.push(runtime);
    assert!(
        control::configure(&state, json!({"key":"PRIVATE-UPSTREAM-KEY"}))
            .await
            .is_err()
    );
    state.usage.record(record()).unwrap();
    let mut active = record();
    active.attempt_id = "active-fixture".into();
    active.status = "active".into();
    state.usage.start(&active);
    let http = client();
    let before = read(
        http.get(format!("{base}/status"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_eq!(before["sources"][0]["active_requests"], 1);
    assert!(before["sources"][0]["admission"].is_object());
    assert!(before["sources"][0]["balance"].is_null());
    assert!(!before.to_string().contains("PRIVATE"));
    let mut refresh_id = Value::Null;
    for _ in 0..2 {
        let refresh = read(
            http.post(format!("{base}/refresh/balances"))
                .bearer_auth(&key)
                .json(&json!({"source_id":"api"}))
                .send()
                .await
                .unwrap(),
        )
        .await;
        let done = operation(&http, &base, &key, &refresh["id"]).await;
        assert_eq!(done["status"], "completed");
        assert_eq!(done["result"]["sources"][0]["result"]["amount"], 2.5);
        refresh_id = refresh["id"].clone();
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2, "显式刷新必须越过普通 TTL");
    let snapshot = read(
        http.get(format!("{base}/sources"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(snapshot["sources"][0]["balance"]["amount"], 2.5);
    let summary = read(
        http.get(format!("{base}/usage"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(summary["rows"][0]["requests"], 2);
    let from = now() / 86400 * 86400;
    let to = from + 86400;
    let analytics = read(
        http.get(format!("{base}/usage/analytics"))
            .bearer_auth(&key)
            .query(&[
                ("from", from.to_string()),
                ("to", to.to_string()),
                ("day_starts", format!("[{from},{to}]")),
            ])
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(analytics["groups"][0]["requests"], 2);
    let requests = read(
        http.get(format!("{base}/usage/requests?from={from}&to={to}&limit=1"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(requests["records"].is_array());
    assert!(!requests.to_string().contains("PRIVATE"));
    let takeover = read(
        http.post(format!("{base}/routing/takeover"))
            .bearer_auth(&key)
            .json(&json!({"source_id":"api"}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(takeover["id"].is_string());
    let cancelled = read(
        http.post(format!("{base}/routing/cancel"))
            .bearer_auth(&key)
            .json(&json!({"id":takeover["id"]}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(cancelled["cancelled"], true);
    state.sources.write().await[0].health = "cooldown".into();
    let recovered = read(
        http.post(format!("{base}/routing/recover"))
            .bearer_auth(&key)
            .json(&json!({"source_id":"api"}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(recovered["recovered"], true);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "读取统计、余额、抢占和恢复不能顺带联网"
    );
    state.sources.write().await[0].source.secret = json!({"api_key":"PRIVATE-REPLACEMENT-KEY"});
    let old = operation(&http, &base, &key, &refresh_id).await;
    assert_eq!(old["status"], "failed");
    assert_eq!(old["result"]["sources"][0]["code"], "source_changed");
    assert!(old["result"]["sources"][0]["result"].is_null());
    assert!(read(
        http.get(format!("{base}/sources"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap()
    )
    .await["sources"][0]["balance"]
        .is_null());
    stop(&state).await;
    task.abort();
}

#[tokio::test]
async fn reset_claim_cancel_duplicate_and_rebound_accounts_never_consume_or_contact_upstream() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let (proxy, task) = server::serve(Router::new().fallback(move || {
        copy.fetch_add(1, Ordering::SeqCst);
        async { StatusCode::BAD_GATEWAY }
    }))
    .await;
    let (directory, state, key, base) = enabled().await;
    state.sources.write().await.push(official(&proxy));
    let http = client();
    let id = uuid::Uuid::new_v4().to_string();
    let body = json!({"request_id":id,"source_id":"official","credit_id":"card-a"});
    let queued = read(
        http.post(format!("{base}/credits/reset-requests"))
            .bearer_auth(&key)
            .json(&body)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(queued["status"], "pending_confirmation");
    let duplicate = read(
        http.post(format!("{base}/credits/reset-requests"))
            .bearer_auth(&key)
            .json(&body)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(duplicate["id"], queued["id"]);
    assert_eq!(
        http.post(format!("{base}/credits/reset-requests"))
            .bearer_auth(&key)
            .json(&json!({"request_id":id,"source_id":"official","credit_id":"card-b"}))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let confirm = json!({"id":id,"source_id":"official","credit_id":"card-a","expected_account_id":"account-fixture","operation_id":id,"confirmed":true});
    assert!(
        control::handle(&state, "control.reset.confirm", confirm.clone())
            .await
            .is_err(),
        "未展示弹窗时必须拒绝"
    );
    assert_eq!(
        control::handle(&state, "control.reset.present", json!({"id":id}))
            .await
            .unwrap()["claimed"],
        true
    );
    assert_eq!(
        control::handle(&state, "control.reset.present", json!({"id":id}))
            .await
            .unwrap()["claimed"],
        false
    );
    for (field, value) in [
        ("credit_id", json!("card-b")),
        ("expected_account_id", json!("other-account")),
        ("operation_id", json!(uuid::Uuid::new_v4())),
        ("confirmed", json!(false)),
    ] {
        let mut invalid = confirm.clone();
        invalid[field] = value;
        assert!(control::handle(&state, "control.reset.confirm", invalid)
            .await
            .is_err());
    }
    assert_eq!(
        control::handle(&state, "control.reset.cancel", json!({"id":id}))
            .await
            .unwrap()["status"],
        "cancelled"
    );
    assert!(control::handle(&state, "control.reset.confirm", confirm)
        .await
        .is_err());
    assert_eq!(
        read(
            http.post(format!("{base}/credits/reset-requests"))
                .bearer_auth(&key)
                .json(&body)
                .send()
                .await
                .unwrap()
        )
        .await["status"],
        "cancelled"
    );
    let rebound_id = uuid::Uuid::new_v4().to_string();
    let rebound_body = json!({"request_id":rebound_id,"source_id":"official"});
    assert_eq!(
        http.post(format!("{base}/credits/reset-requests"))
            .bearer_auth(&key)
            .json(&rebound_body)
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    {
        let mut sources = state.sources.write().await;
        sources[0].source.account_id = Some("replacement-account".into());
        sources[0].source.secret["account_id"] = json!("replacement-account");
    }
    assert_eq!(
        http.post(format!("{base}/credits/reset-requests"))
            .bearer_auth(&key)
            .json(&rebound_body)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert_eq!(
        control::handle(&state, "control.reset.present", json!({"id":rebound_id}))
            .await
            .unwrap()["status"],
        "cancelled"
    );
    let restart_id = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        http.post(format!("{base}/credits/reset-requests"))
            .bearer_auth(&key)
            .json(&json!({"request_id":restart_id,"source_id":"official"}))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    assert_eq!(
        control::handle(&state, "control.reset.present", json!({"id":restart_id}))
            .await
            .unwrap()["claimed"],
        true
    );
    stop(&state).await;
    let reopened =
        AppState::new(directory.path().into(), 17863, "PRIVATE-GATEWAY-KEY".into()).unwrap();
    assert_eq!(reopened.control.key().unwrap()["key"], key);
    assert_eq!(reopened.control.status().await.unwrap()["enabled"], false);
    let mut source = official(&proxy);
    source.source.account_id = Some("replacement-account".into());
    source.source.secret["account_id"] = json!("replacement-account");
    reopened.sources.write().await.push(source);
    let pending = control::handle(&reopened, "control.reset.pending", json!({}))
        .await
        .unwrap();
    assert_eq!(pending["requests"].as_array().unwrap().len(), 1);
    assert_eq!(pending["requests"][0]["id"], restart_id);
    assert_eq!(pending["requests"][0]["status"], "pending_confirmation");
    assert_eq!(
        std::fs::metadata(directory.path().join("control-reset-requests.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "排队、认领、取消和身份变化不得查询或消费重置卡"
    );
    task.abort();
}

#[tokio::test]
async fn rotating_or_disabling_access_while_reading_a_body_revokes_the_accepted_request() {
    let (_directory, state, _key, _base) = enabled().await;
    state
        .sources
        .write()
        .await
        .push(SourceRuntime::new(source_fixture::api_source(
            "api",
            "API",
            "http://127.0.0.1:1",
            "PRIVATE-UPSTREAM-KEY",
        )));
    for change in [json!({"regenerate_key":true}), json!({"enabled":false})] {
        let key = state.control.key().unwrap()["key"]
            .as_str()
            .unwrap()
            .to_owned();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let (started, finish) = (entered.clone(), release.clone());
        let body = Body::from_stream(async_stream::stream! {
            started.notify_one();
            finish.notified().await;
            yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"{\"source_id\":\"api\"}"));
        });
        let mut request = request(&state, &key, "/v1/routing/takeover", Some(json!({})));
        *request.body_mut() = body;
        let router = control::router(state.clone());
        let pending = tokio::spawn(async move { router.oneshot(request).await.unwrap() });
        entered.notified().await;
        control::configure(&state, change).await.unwrap();
        release.notify_one();
        let response = pending.await.unwrap();
        assert_eq!(response.status(), 401);
        let value: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(value["error"]["code"], "access_changed");
        assert!(state.manual_routing.status().is_null());
    }
    stop(&state).await;
}

#[tokio::test]
async fn billing_refresh_reaches_local_ledger_and_publishes_actual_cost_through_http() {
    let ledger_hits = Arc::new(AtomicUsize::new(0));
    let copy = ledger_hits.clone();
    let (upstream, task) = server::serve(Router::new()
        .route("/api/v1/me/quota", get(|| async { Json(json!({"userLimitTotalUsd":100,"userCurrentTotalUsd":1})) }))
        .route("/api/v1/me/usage-logs", get(move |request: Request| {
            copy.fetch_add(1, Ordering::SeqCst);
            async move {
                assert_eq!(request.headers()["x-api-key"], "PRIVATE-UPSTREAM-KEY");
                Json(json!({"items":[{"id":42,"createdAt":"2026-09-21T14:13:49Z","model":"gpt-6.1-sol","billingModel":"gpt-6.1-sol","inputTokens":90,"outputTokens":20,"cacheReadInputTokens":10,"cacheCreationInputTokens":0,"cost":0.125,"statusCode":200,"duration":400,"endpoint":"/v1/responses"}],"pageInfo":{"hasMore":false}}))
            }
        }))).await;
    let (_directory, state, key, base) = enabled().await;
    state
        .sources
        .write()
        .await
        .push(SourceRuntime::new(source_fixture::api_source(
            "api",
            "API",
            &format!("{upstream}/v1"),
            "PRIVATE-UPSTREAM-KEY",
        )));
    let mut usage = record();
    usage.started_at = 1_790_000_029;
    state.usage.record(usage).unwrap();
    let http = client();
    let response = http
        .post(format!("{base}/refresh/billing"))
        .bearer_auth(&key)
        .json(&json!({"source_id":"api"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let value = read(
                http.get(format!("{base}/reconciliation"))
                    .bearer_auth(&key)
                    .send()
                    .await
                    .unwrap(),
            )
            .await;
            if value["running"] == false {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(result["errors"], 0);
    assert_eq!(result["updated"], 1);
    assert_eq!(ledger_hits.load(Ordering::SeqCst), 1);
    let summary = read(
        http.get(format!("{base}/usage?days=3660"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        summary["recent"][0]["cost"]["billing_basis"],
        "upstream_charge"
    );
    assert_eq!(summary["recent"][0]["cost"]["total_usd"], 0.125);
    assert_eq!(ledger_hits.load(Ordering::SeqCst), 1);
    stop(&state).await;
    task.abort();
}

#[tokio::test]
async fn official_refresh_failure_is_an_async_safe_result_and_never_bypasses_the_local_proxy() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let (proxy, task) = server::serve(Router::new().fallback(move |request: Request| {
        copy.fetch_add(1, Ordering::SeqCst);
        async move {
            assert_eq!(request.method(), "CONNECT");
            (StatusCode::BAD_GATEWAY, "PRIVATE-PROXY-ERROR-BODY")
        }
    }))
    .await;
    let (_directory, state, key, base) = enabled().await;
    let mut runtime = official(&proxy);
    runtime.quota = json!({"windows":[{"name":"primary","used_percent":15,"reset_at":now()+3600}],"fetched_at":now(),"stale":false});
    runtime.credits =
        json!({"available_count":1,"credits":[{"id":"card-a","status":"available"}],"stale":false});
    state.sources.write().await.push(runtime);
    let http = client();
    let status = read(
        http.get(format!("{base}/status"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status["sources"][0]["credits"]["available_count"], 1);
    assert_eq!(
        status["sources"][0]["quota"]["windows"][0]["used_percent"],
        15
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let response = http
        .post(format!("{base}/refresh/quotas"))
        .bearer_auth(&key)
        .json(&json!({"source_id":"official","include_credits":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let refresh = read(response).await;
    let result = operation(&http, &base, &key, &refresh["id"]).await;
    assert_eq!(result["status"], "failed");
    assert_eq!(result["result"]["sources"][0]["code"], "refresh_failed");
    assert!(!result.to_string().contains("PRIVATE"));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    stop(&state).await;
    task.abort();
}

#[tokio::test]
async fn dashboard_local_reads_do_not_require_login() {
    let (_directory, state, _, base) = enabled().await;
    let root = base.strip_suffix("/v1").unwrap();
    let http = client();
    let response = http.get(format!("{root}/dashboard/")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    let url = format!("{root}/dashboard/api/status");
    let response = http.get(&url).header("cookie", "unrelated=x").header("origin", "https://example.test").send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(!response.headers().contains_key("access-control-allow-origin"));
    let status = read(response).await;
    assert_eq!(status["running"], true);
    assert!(status["history"]["enabled"].is_boolean());
    assert!(status["reconciliation"].is_object());
    assert_eq!(http.get(&url).header("host", "example.test").send().await.unwrap().status(), 421);
    for method in [reqwest::Method::POST, reqwest::Method::OPTIONS] {
        assert_eq!(http.request(method, &url).send().await.unwrap().status(), 405);
    }
    control::configure(&state, json!({"regenerate_key":true})).await.unwrap();
    assert_eq!(http.get(&url).send().await.unwrap().status(), 200);
    stop(&state).await;
    let request = request(&state, "", "/dashboard/api/status", None);
    assert_eq!(control::router(state).oneshot(request).await.unwrap().status(), 503);
}

#[tokio::test]
async fn dashboard_queries_filter_all_statistics_and_read_saved_error_and_cache_pages() {
    let (_directory, state, key, base) = enabled().await;
    let root = base.strip_suffix("/v1").unwrap();
    let http = client();
    let source = source_fixture::api_source("api", "API", "http://127.0.0.1:1", "PRIVATE-UPSTREAM-KEY");
    let mut failure = record();
    failure.attempt_id = uuid::Uuid::new_v4().to_string();
    failure.status = "failed".into();
    failure.latency_ms = 90;
    failure.complete = false;
    state.usage.record(failure.clone()).unwrap();
    let mut diagnostic = codexpress_gateway::usage::diagnostics::Capture::new(&failure.attempt_id, failure.started_at, &source, &state.gateway_key);
    diagnostic.request("native", "POST", "/v1/responses", "http://127.0.0.1:1/v1/responses", &[]);
    diagnostic.response(429, &Default::default());
    diagnostic.chunk("{\"error\":{\"message\":\"稍后重试 <script>literal</script>\",\"code\":\"rate_limit\"}}".as_bytes());
    diagnostic.complete();
    state.usage.diagnostic(&diagnostic, true).unwrap();
    let mut success = failure.clone();
    success.attempt_id = uuid::Uuid::new_v4().to_string();
    success.status = "success".into();
    success.complete = true;
    success.latency_ms = 900;
    state.usage.record(success).unwrap();
    let mut live = failure.clone();
    live.attempt_id = "active-dashboard".into();
    live.status = "streaming".into();
    state.usage.start(&live);
    let from = failure.started_at.saturating_sub(100);
    let to = from + 86400;
    let get = |path: String| http.get(format!("{root}{path}"));
    let filters = [("from", from.to_string()), ("to", to.to_string()), ("day_starts", json!([from,to]).to_string()), ("source_id", "api".into()), ("model", "gpt-6.1-sol".into()), ("ingress", "native".into()), ("status", "unsuccessful".into()), ("search", "稍后重试".into())];
    let analytics = read(get("/dashboard/api/analytics".into()).query(&filters).send().await.unwrap()).await;
    assert_eq!(analytics["groups"][0]["requests"], 1);
    assert_eq!(analytics["groups"][0]["failures"], 1);
    assert_eq!(analytics["daily"][0]["requests"], 1);
    assert_eq!(analytics["performance"]["p50_latency_ms"], 90.0);
    let active = read(get("/dashboard/api/analytics".into()).query(&[("from",from.to_string()),("to",to.to_string()),("day_starts",json!([from,to]).to_string()),("status","active".into()),("search","active-dashboard".into())]).send().await.unwrap()).await;
    assert_eq!(active["groups"][0]["requests"], 1);
    assert_eq!(active["groups"][0]["active_requests"], 1);
    assert_eq!(active["daily"][0]["requests"], 1);
    assert!(active["performance"]["p50_latency_ms"].is_null());
    let list = read(get("/dashboard/api/requests".into()).query(&filters.iter().filter(|(key,_)| *key != "day_starts").collect::<Vec<_>>()).send().await.unwrap()).await;
    assert_eq!(list["total"], 1);
    assert_eq!(list["records"][0]["attempt_id"], failure.attempt_id);
    let mut body = String::new();
    let mut offset = 0;
    loop {
        let page = read(get(format!("/dashboard/api/requests/{}?body_offset={offset}&body_limit=7", failure.attempt_id)).send().await.unwrap()).await;
        assert_eq!(page["related"].as_array().unwrap().len(), 2);
        assert_eq!(page["diagnostic"]["error_code"], "rate_limit");
        body.push_str(page["diagnostic"]["body_text"].as_str().unwrap());
        match page["diagnostic"]["body_next_offset"].as_u64() { Some(next) => { assert!(next > offset); offset = next; }, None => break }
    }
    let error: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(error["error"]["message"], "稍后重试 <script>literal</script>");
    let record_only = read(get(format!("/dashboard/api/requests/{}?record_only=true", failure.attempt_id)).send().await.unwrap()).await;
    assert_eq!(record_only.as_object().unwrap().len(), 1);
    for suffix in ["body_limit=0", "record_only=1", "arbitrary=1"] {
        assert_eq!(get(format!("/dashboard/api/requests/{}?{suffix}", failure.attempt_id)).send().await.unwrap().status(), 400);
    }
    assert_eq!(get("/dashboard/api/requests/legacy%3A999999".into()).send().await.unwrap().status(), 400);
    let public = http.get(format!("{base}/usage/requests")).bearer_auth(&key).query(&[("from",from),("to",to)]).send().await.unwrap().text().await.unwrap();
    assert!(!public.contains("稍后重试"));
    assert!(!public.contains("body_text"));
    assert_eq!(http.get(format!("{base}/usage/requests?search=needle")).bearer_auth(&key).send().await.unwrap().status(), 400);
    assert_eq!(http.get(format!("{base}/usage/analytics")).bearer_auth(&key).query(&filters).send().await.unwrap().status(), 400);
    state.history.configure(true, 64 * 1024 * 1024).unwrap();
    let input = json!({"input":[{"role":"user","content":"原文 <script>literal</script> PRIVATE-UPSTREAM-KEY"},{"type":"function_call","name":"tool","arguments":"{\"api_key\":\"literal-kept\"}"}],"tools":[{"name":"tool","parameters":{"type":"object","properties":{"api_key":{"type":"string"}}}}]});
    state.history.capture("fixture", &input, &json!({"id":"cached","status":"completed","output":[{"role":"assistant","content":"完整回答"}]})).unwrap();
    let cache = read(get("/dashboard/api/cache".into()).send().await.unwrap()).await;
    assert_eq!(cache["total"], 1);
    assert_eq!(cache["legacy_total"], 1);
    assert!(cache["records"][0]["source_id"].is_null());
    let id = cache["records"][0]["id"].as_str().unwrap();
    let mut text = String::new();
    let mut offset = 0;
    loop {
        let page = read(get(format!("/dashboard/api/cache/{id}?offset={offset}&limit=17")).send().await.unwrap()).await;
        assert_eq!(page["status"], "complete");
        text.push_str(page["text"].as_str().unwrap());
        match page["next_offset"].as_u64() { Some(next) => { assert!(next > offset); offset = next; }, None => break }
    }
    assert!(text.contains("PRIVATE-UPSTREAM-KEY"));
    assert!(text.contains("api_key"));
    assert!(text.contains("literal-kept"));
    assert!(text.contains("完整回答"));
    assert!(text.contains("<script>literal</script>"));
    stop(&state).await;
}
