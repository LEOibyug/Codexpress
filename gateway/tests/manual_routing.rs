#[path = "common/server.rs"]
mod server;
#[path = "common/source.rs"]
mod source_fixture;

use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Router,
};
use codexpress_gateway::{
    continuity, management,
    model::{AppState, Source, SourceRuntime},
    relay,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tower::ServiceExt;

fn source(id: &str, url: &str, priority: i32) -> Source {
    Source {
        priority,
        ..source_fixture::api_source(id, id, url, "fixture")
    }
}
fn request(body: &[u8], thread: &str) -> Request {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("host", "127.0.0.1:0")
        .header("authorization", "Bearer fixture")
        .header("thread-id", thread)
        .body(Body::from(body.to_vec()))
        .unwrap()
}
async fn state(sources: Vec<Source>) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().to_owned(), 0, "fixture".into()).unwrap();
    state.settings.write().await.strict_priority = false;
    // These fixtures isolate manual handoff; transient retries have their own suite.
    state.settings.write().await.upstream_attempts = 1;
    state.settings.write().await.upstream_retry_delay_seconds = 0;
    *state.sources.write().await = sources.into_iter().map(SourceRuntime::new).collect();
    state.settings.write().await.degraded_context_fallback = false;
    (dir, state)
}
async fn insert(state: &Arc<AppState>, id: &str) -> Value {
    management::dispatch(state, "routing.try", json!({"source_id":id}))
        .await
        .unwrap()
}

#[tokio::test]
async fn recovering_only_clears_transient_failure_and_trial_cancel_is_compare_and_swap() {
    let (_dir, state) = state(vec![source("a", "http://127.0.0.1:1", 0)]).await;
    {
        let mut sources = state.sources.write().await;
        sources[0].health = "proxy_error".into();
        sources[0].last_error = Some("fixture".into());
        sources[0].cooldown_until = Some(u64::MAX);
        sources[0].quota = json!({"fixture":"preserved"});
    }
    management::dispatch(&state, "routing.recover", json!({"source_id":"a"}))
        .await
        .unwrap();
    {
        let sources = state.sources.read().await;
        assert_eq!(sources[0].health, "ready");
        assert_eq!(sources[0].cooldown_until, None);
        assert_eq!(sources[0].last_error, None);
        assert_eq!(sources[0].quota["fixture"], "preserved");
        assert_eq!(sources[0].source.secret["api_key"], "fixture");
    }
    state.sources.write().await[0].health = "auth_required".into();
    assert!(
        management::dispatch(&state, "routing.recover", json!({"source_id":"a"}))
            .await
            .is_err()
    );
    let old = insert(&state, "a").await;
    let new = insert(&state, "a").await;
    assert_eq!(
        management::dispatch(&state, "routing.cancel", old)
            .await
            .unwrap()["cancelled"],
        false
    );
    assert_eq!(state.manual_routing.status()["id"], new["id"]);
    assert_eq!(
        management::dispatch(&state, "routing.cancel", new)
            .await
            .unwrap()["cancelled"],
        true
    );
    assert!(state.manual_routing.status().is_null());
    state.sources.write().await[0].source.enabled = false;
    assert!(
        management::dispatch(&state, "routing.try", json!({"source_id":"a"}))
            .await
            .is_err()
    );
    state.sources.write().await[0].source.enabled = true;
    insert(&state, "a").await;
    management::dispatch(&state, "sync", json!({"sources":[],"mode":"auto"}))
        .await
        .unwrap();
    assert!(
        state.manual_routing.status().is_null(),
        "removing a source cancels its pending selection"
    );
}

#[tokio::test]
async fn takeover_failure_stops_further_claims_without_changing_fixed_settings_or_cooldown() {
    let a_hits = Arc::new(AtomicUsize::new(0));
    let a_copy = a_hits.clone();
    let (a, a_task) = server::serve(Router::new().route(
        "/responses",
        post(move || {
            let hits = a_copy.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({"status":"completed","output":[]}))
            }
        }),
    ))
    .await;
    let b_hits = Arc::new(AtomicUsize::new(0));
    let b_copy = b_hits.clone();
    let (b, b_task) = server::serve(Router::new().route(
        "/responses",
        post(move || {
            let hits = b_copy.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                StatusCode::TOO_MANY_REQUESTS
            }
        }),
    ))
    .await;
    let (_dir, state) = state(vec![source("a", &a, 0), source("b", &b, 1)]).await;
    {
        let mut settings = state.settings.write().await;
        settings.mode = "fixed".into();
        settings.fixed_source_id = Some("a".into());
    }
    {
        let mut sources = state.sources.write().await;
        sources[1].health = "cooldown".into();
        sources[1].cooldown_until = Some(u64::MAX);
    }
    insert(&state, "b").await;
    let app = relay::router(state.clone());
    let mut tasks = Vec::new();
    for index in 0..8 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            let response = app
                .oneshot(request(
                    br#"{"model":"m","input":"hello"}"#,
                    &format!("thread-{index}"),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            to_bytes(response.into_body(), 10000).await.unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(a_hits.load(Ordering::SeqCst), 8);
    assert_eq!(b_hits.load(Ordering::SeqCst), 1);
    assert!(state.manual_routing.status().is_null());
    assert_eq!(
        state.settings.read().await.fixed_source_id.as_deref(),
        Some("a")
    );
    assert_eq!(state.sources.read().await[1].health, "cooldown");
    a_task.abort();
    b_task.abort();
}

#[tokio::test]
async fn manual_encrypted_probe_is_unchanged_and_acceptance_is_conversation_scoped_and_durable() {
    let a_hits = Arc::new(AtomicUsize::new(0));
    let a_copy = a_hits.clone();
    let (a, a_task) = server::serve(Router::new().route(
        "/responses",
        post(move || {
            let hits = a_copy.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({"id":"a-result","status":"completed","output":[]}))
            }
        }),
    ))
    .await;
    let wire = br#"{ "model":"m", "previous_response_id":"old", "input":[{"type":"reasoning","encrypted_content":"shared-cipher"},{"role":"user","content":"next"}] }"#;
    let b_hits = Arc::new(AtomicUsize::new(0));
    let b_copy = b_hits.clone();
    let (b, b_task) = server::serve(Router::new().route(
        "/responses",
        post(move |req: Request| {
            let hits = b_copy.clone();
            async move {
                assert_eq!(req.headers()["x-codex-turn-state"], "old-turn");
                assert_eq!(
                    to_bytes(req.into_body(), 10000).await.unwrap().as_ref(),
                    wire
                );
                hits.fetch_add(1, Ordering::SeqCst);
                ([("x-codex-turn-state", "old-turn")], axum::Json(json!({"id":"b-result","conversation":"old","status":"completed","output":[{"type":"reasoning","encrypted_content":"shared-cipher"}]}))).into_response()
            }
        }),
    ))
    .await;
    let first = source("a", &a, 0);
    let second = source("b", &b, 1);
    let (dir, state) = state(vec![first.clone(), second.clone()]).await;
    {
        let mut bindings = state.bindings.lock().await;
        for key in [
            "old".into(),
            continuity::digest("encrypted", "shared-cipher"),
            continuity::digest("turn", "old-turn"),
        ] {
            bindings.insert(key, "a".into());
        }
    }
    state.history.configure(true, 64_000_000).unwrap();
    let child_key = codexpress_gateway::affinity::key("thread:child", "m");
    let original: Value = serde_json::from_slice(wire).unwrap();
    state
        .affinity
        .confirm_exclusions(&child_key, &continuity::keys(&original, Some("old-turn")))
        .unwrap();
    insert(&state, "b").await;
    let app = relay::router(state.clone());
    for _ in 0..2 {
        let mut req = request(wire, "child");
        req.headers_mut()
            .insert("x-codex-turn-state", "old-turn".parse().unwrap());
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 10000).await.unwrap();
    }
    assert_eq!(
        state.bindings.lock().await.get("old").map(String::as_str),
        Some("a")
    );
    for shared in [
        continuity::digest("turn", "old-turn"),
        continuity::digest("encrypted", "shared-cipher"),
    ] {
        assert_eq!(
            state.bindings.lock().await.get(&shared).map(String::as_str),
            Some("a")
        );
    }
    let mut parent = request(wire, "parent");
    parent
        .headers_mut()
        .insert("x-codex-turn-state", "old-turn".parse().unwrap());
    let response = app.clone().oneshot(parent).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert!(state.notifications.list().is_empty());
    drop(app);
    drop(state);
    let restored = AppState::new(dir.path().to_owned(), 0, "fixture".into()).unwrap();
    restored.settings.write().await.strict_priority = false;
    *restored.sources.write().await = vec![SourceRuntime::new(first), SourceRuntime::new(second)];
    restored.settings.write().await.degraded_context_fallback = false;
    let mut req = request(wire, "child");
    req.headers_mut()
        .insert("x-codex-turn-state", "old-turn".parse().unwrap());
    let response = relay::router(restored.clone()).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(b_hits.load(Ordering::SeqCst), 3);
    assert_eq!(a_hits.load(Ordering::SeqCst), 1);
    assert!(restored.notifications.list().is_empty());
    a_task.abort();
    b_task.abort();
}

#[tokio::test]
async fn explicit_encrypted_rejection_and_http_failures_fall_back_to_original_bytes_without_rebuilding(
) {
    for failure in [0, 1, 2] {
        let wire = br#"{ "model":"m","previous_response_id":"old","input":[{"type":"reasoning","encrypted_content":"shared-cipher"},{"role":"user","content":"next"}] }"#;
        let a_hits = Arc::new(AtomicUsize::new(0));
        let a_copy = a_hits.clone();
        let (a, a_task) = server::serve(Router::new().route(
            "/responses",
            post(move |req: Request| {
                let hits = a_copy.clone();
                async move {
                    assert_eq!(
                        to_bytes(req.into_body(), 10000).await.unwrap().as_ref(),
                        wire
                    );
                    hits.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({"status":"completed","output":[]}))
                }
            }),
        ))
        .await;
        let b_hits = Arc::new(AtomicUsize::new(0));
        let b_copy = b_hits.clone();
        let (b, b_task) = server::serve(Router::new().route("/responses", post(move |req: Request| {
            let hits = b_copy.clone(); async move {
                assert_eq!(to_bytes(req.into_body(), 10000).await.unwrap().as_ref(), wire);
                hits.fetch_add(1, Ordering::SeqCst);
                match failure {
                    0 => (StatusCode::BAD_REQUEST, axum::Json(json!({"error":{"code":"invalid_encrypted_content"}}))).into_response(),
                    1 => ([("content-type", "text/event-stream")], "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"invalid_encrypted_content\"}}}\n\n").into_response(),
                    _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                }
            }
        }))).await;
        let (_dir, state) = state(vec![source("a", &a, 0), source("b", &b, 1)]).await;
        state.history.configure(true, 64_000_000).unwrap();
        state.bindings.lock().await.insert("old".into(), "a".into());
        insert(&state, "b").await;
        let response = relay::router(state.clone())
            .oneshot(request(wire, "child"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 10000).await.unwrap();
        assert_eq!(a_hits.load(Ordering::SeqCst), 1);
        assert_eq!(b_hits.load(Ordering::SeqCst), 1);
        let summary = state.usage.summary(1).unwrap();
        assert_eq!(
            summary["recent"]
                .as_array()
                .unwrap()
                .iter()
                .find(|record| record["source_id"] == "b")
                .unwrap()["status"],
            "failed"
        );
        assert!(state.notifications.list().is_empty());
        a_task.abort();
        b_task.abort();
    }
}

#[tokio::test]
async fn an_unknown_failed_stream_does_not_replay_or_replace_the_previous_affinity() {
    for anonymous in [false, true] {
        let a_hits = Arc::new(AtomicUsize::new(0));
        let copy = a_hits.clone();
        let (a, a_task) = server::serve(Router::new().route(
            "/responses",
            post(move || {
                let hits = copy.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        ))
        .await;
        let (b, b_task) = server::serve(Router::new().route("/responses", post(|| async {
        ([("content-type", "text/event-stream"), ("x-codex-turn-state", "shared-turn")], "data: {\"type\":\"response.metadata\",\"headers\":{\"x-codex-turn-state\":\"shared-turn\"}}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"unknown_failure\"}}}\n\n")
    }))).await;
        let first = source("a", &a, 0);
        let second = source("b", &b, 1);
        let (_dir, state) = state(vec![first.clone(), second.clone()]).await;
        let key = codexpress_gateway::affinity::key("thread:existing", "m");
        let mut old = state
            .affinity
            .reserve(Some(&key), &first, false, false, None)
            .unwrap();
        old.commit().unwrap();
        drop(old);
        state.bindings.lock().await.insert("old".into(), "a".into());
        state
            .bindings
            .lock()
            .await
            .insert(continuity::digest("turn", "shared-turn"), "a".into());
        insert(&state, "b").await;
        let mut req = request(
            br#"{"model":"m","previous_response_id":"old","stream":true,"input":"next"}"#,
            "existing",
        );
        if anonymous {
            req.headers_mut().remove("thread-id");
        }
        let response = relay::router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 10000).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("unknown_failure"));
        assert_eq!(a_hits.load(Ordering::SeqCst), 0);
        assert_eq!(
            state.affinity.preferred(&key, &[first, second]).as_deref(),
            Some("a")
        );
        assert!(state.affinity.exclusions(&key).is_empty());
        assert_eq!(
            state
                .bindings
                .lock()
                .await
                .get(&continuity::digest("turn", "shared-turn"))
                .map(String::as_str),
            Some("a")
        );
        a_task.abort();
        b_task.abort();
    }
}

#[tokio::test]
async fn http_errors_and_arbitrary_json_never_confirm_manual_compatibility() {
    for status in [StatusCode::OK, StatusCode::GATEWAY_TIMEOUT] {
        let (url, task) = server::serve(Router::new().route(
            "/responses",
            post(move || async move {
                (
                    status,
                    axum::Json(if status == StatusCode::OK {
                        json!({"detail":"not a response"})
                    } else {
                        json!({"status":"completed","output":[]})
                    }),
                )
            }),
        ))
        .await;
        let target = source("target", &url, 1);
        let (_dir, state) = state(vec![target.clone()]).await;
        let key = codexpress_gateway::affinity::key("thread:existing", "m");
        insert(&state, "target").await;
        let response = relay::router(state.clone())
            .oneshot(request(br#"{"model":"m","input":"hello"}"#, "existing"))
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        to_bytes(response.into_body(), 10000).await.unwrap();
        assert!(state.affinity.preferred(&key, &[target]).is_none());
        assert!(state.affinity.exclusions(&key).is_empty());
        task.abort();
    }
}

#[tokio::test]
async fn manual_selection_survives_configuration_replanning_before_submission() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let (proxy, proxy_task) = server::serve(Router::new().fallback(move || {
        let copy = copy.clone();
        async move {
            copy.fetch_add(1, Ordering::SeqCst);
            StatusCode::BAD_GATEWAY
        }
    }))
    .await;
    let (api, api_task) = server::serve(Router::new().route(
        "/responses",
        post(|| async { axum::Json(json!({"status":"completed","output":[]})) }),
    ))
    .await;
    let a = source("a", &api, 0);
    let mut official = source("official", "https://chatgpt.com/backend-api/codex", 1);
    official.kind = "official".into();
    official.account_id = Some("fixture-account".into());
    official.proxy.mode = "explicit".into();
    official.proxy.url = Some(proxy);
    official.secret = json!({"access_token":"fixture","account_id":"fixture-account","expires_at":codexpress_gateway::model::now()+3600});
    let (_dir, state) = state(vec![a, official.clone()]).await;
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    state
        .official_locks
        .lock()
        .await
        .insert("token:fixture-account".into(), lock.clone());
    let guard = lock.lock().await;
    insert(&state, "official").await;
    let app = relay::router(state.clone());
    let pending = tokio::spawn(async move {
        let response = app
            .oneshot(request(br#"{"model":"m","input":"hello"}"#, "existing"))
            .await
            .unwrap();
        let status = response.status();
        to_bytes(response.into_body(), 10000).await.unwrap();
        status
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while state.affinity.active_count(&official) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let sources: Vec<_> = state
        .sources
        .read()
        .await
        .iter()
        .map(|r| {
            let mut value = serde_json::to_value(&r.source).unwrap();
            value["secret"] = r.source.secret.clone();
            value
        })
        .collect();
    management::dispatch(&state, "sync", json!({"sources":sources,"mode":"auto","upstream_attempts":1,"upstream_retry_delay_seconds":0}))
        .await
        .unwrap();
    drop(guard);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), pending)
            .await
            .unwrap()
            .unwrap(),
        StatusCode::OK
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    proxy_task.abort();
    api_task.abort();
}

#[tokio::test]
async fn manual_selection_respects_capacity_and_invalid_requests_do_not_consume_it() {
    let b_hits = Arc::new(AtomicUsize::new(0));
    let b_copy = b_hits.clone();
    let (a, a_task) = server::serve(Router::new().route(
        "/responses",
        post(|| async { axum::Json(json!({"status":"completed","output":[]})) }),
    ))
    .await;
    let (b, b_task) = server::serve(Router::new().route(
        "/responses",
        post(move || {
            let hits = b_copy.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                StatusCode::OK
            }
        }),
    ))
    .await;
    let mut second = source("b", &b, 1);
    second.max_active_conversations = Some(1);
    let (_dir, state) = state(vec![source("a", &a, 0), second.clone()]).await;
    let occupied = state
        .affinity
        .reserve(Some("occupied"), &second, false, false, None)
        .unwrap();
    insert(&state, "b").await;
    let app = relay::router(state.clone());
    let mut bad = request(b"{}", "bad");
    bad.headers_mut()
        .insert("authorization", "Bearer wrong".parse().unwrap());
    assert_eq!(
        app.clone().oneshot(bad).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.clone()
            .oneshot(request(b"invalid", "bad"))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(state.manual_routing.status()["source_id"], "b");
    let response = app
        .oneshot(request(br#"{"model":"m","input":"next"}"#, "new"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(b_hits.load(Ordering::SeqCst), 0);
    assert!(state.manual_routing.status().is_null());
    drop(occupied);
    a_task.abort();
    b_task.abort();
}

struct TakeoverFixture {
    _directory: tempfile::TempDir,
    state: Arc<AppState>,
    a_hits: Arc<AtomicUsize>,
    b_hits: Arc<AtomicUsize>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for TakeoverFixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl TakeoverFixture {
    async fn new(limit: Option<u32>, replies: Vec<u16>) -> Self {
        let a_hits = Arc::new(AtomicUsize::new(0));
        let copy = a_hits.clone();
        let (a, a_task) = server::serve(Router::new().route(
            "/responses",
            post(move || {
                let hits = copy.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({"status":"completed","output":[]}))
                }
            }),
        ))
        .await;
        let b_hits = Arc::new(AtomicUsize::new(0));
        let copy = b_hits.clone();
        let replies = Arc::new(replies);
        let (b, b_task) = server::serve(Router::new().route(
            "/responses",
            post(move |request: Request| {
                let hits = copy.clone();
                let replies = replies.clone();
                async move {
                    let index = hits.fetch_add(1, Ordering::SeqCst);
                    if replies.first() == Some(&409) {
                        let body: Value = serde_json::from_slice(
                            &to_bytes(request.into_body(), 10000).await.unwrap(),
                        )
                        .unwrap();
                        assert_eq!(
                            body["input"][0]["encrypted_content"].is_string(),
                            index == 0
                        );
                    }
                    let status = replies.get(index).copied().unwrap_or(200);
                    if status == 200 {
                        axum::Json(json!({"status":"completed","output":[]})).into_response()
                    } else {
                        let code = match status {
                            429 => "insufficient_quota",
                            409 => "invalid_encrypted_content",
                            _ => "server_error",
                        };
                        (
                            StatusCode::from_u16(status).unwrap(),
                            axum::Json(json!({"error":{"code":code}})),
                        )
                            .into_response()
                    }
                }
            }),
        ))
        .await;
        let mut target = source("b", &b, 1);
        target.max_active_conversations = limit;
        target.active_conversation_window_seconds = 300;
        let (_directory, state) = state(vec![source("a", &a, 0), target]).await;
        state.settings.write().await.upstream_attempts = 3;
        Self {
            _directory,
            state,
            a_hits,
            b_hits,
            tasks: vec![a_task, b_task],
        }
    }
    async fn send(&self, thread: &str) {
        let response = relay::router(self.state.clone())
            .oneshot(request(br#"{"model":"m","input":"next"}"#, thread))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 10000).await.unwrap();
    }
}

#[tokio::test]
async fn takeover_continues_until_capacity_or_other_conversations_are_drained() {
    for limit in [None, Some(2)] {
        let fixture = TakeoverFixture::new(limit, vec![]).await;
        for thread in ["first", "second", "third"] {
            fixture.send(thread).await;
        }
        insert(&fixture.state, "b").await;
        fixture.send("first").await;
        assert_eq!(fixture.state.manual_routing.status()["source_id"], "b");
        fixture.send("second").await;
        fixture.send("third").await;
        let transferred = if limit.is_some() { 2 } else { 3 };
        assert_eq!(fixture.b_hits.load(Ordering::SeqCst), transferred);
        assert!(fixture.state.manual_routing.status().is_null());
        fixture.send("fresh-after-takeover").await;
        assert_eq!(fixture.b_hits.load(Ordering::SeqCst), transferred);
        assert_eq!(
            fixture.a_hits.load(Ordering::SeqCst),
            3 + (3 - transferred) + 1
        );
    }
}

#[tokio::test]
async fn takeover_retries_before_abandoning_and_success_keeps_it_active() {
    let fixture = TakeoverFixture::new(None, vec![503, 503, 200, 503, 503, 503]).await;
    fixture
        .state
        .settings
        .write()
        .await
        .upstream_retry_delay_seconds = 1;
    fixture.send("first").await;
    fixture.send("second").await;
    insert(&fixture.state, "b").await;
    let started = std::time::Instant::now();
    fixture.send("first").await;
    assert!(started.elapsed() >= std::time::Duration::from_secs(2));
    assert_eq!(fixture.b_hits.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.a_hits.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.state.manual_routing.status()["source_id"], "b");
    fixture.send("second").await;
    assert_eq!(fixture.b_hits.load(Ordering::SeqCst), 6);
    assert_eq!(fixture.a_hits.load(Ordering::SeqCst), 3);
    assert!(fixture.state.manual_routing.status().is_null());
    fixture.send("third").await;
    assert_eq!(fixture.b_hits.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn takeover_stops_on_permanent_rejection_without_extra_retries() {
    let fixture = TakeoverFixture::new(None, vec![429]).await;
    fixture.send("first").await;
    fixture.send("second").await;
    insert(&fixture.state, "b").await;
    fixture.send("first").await;
    assert_eq!(fixture.b_hits.load(Ordering::SeqCst), 1);
    assert!(fixture.state.manual_routing.status().is_null());
    fixture.send("second").await;
    assert_eq!(fixture.b_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn parallel_takeover_requests_never_exceed_the_target_capacity() {
    let fixture = TakeoverFixture::new(Some(2), vec![]).await;
    for thread in ["first", "second", "third"] {
        fixture.send(thread).await;
    }
    insert(&fixture.state, "b").await;
    let mut tasks = Vec::new();
    for index in 0..8 {
        let app = relay::router(fixture.state.clone());
        tasks.push(tokio::spawn(async move {
            let response = app
                .oneshot(request(
                    br#"{"model":"m","input":"next"}"#,
                    &format!("new-{index}"),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            to_bytes(response.into_body(), 10000).await.unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(fixture.b_hits.load(Ordering::SeqCst), 2);
    assert!(fixture.state.manual_routing.status().is_null());
}

#[tokio::test]
async fn takeover_retries_explicit_stream_errors_but_never_replays_generated_output() {
    for generated in [false, true] {
        let a_hits = Arc::new(AtomicUsize::new(0));
        let copy = a_hits.clone();
        let (a, a_task) = server::serve(Router::new().route(
            "/responses",
            post(move || {
                let hits = copy.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        ))
        .await;
        let b_hits = Arc::new(AtomicUsize::new(0));
        let copy = b_hits.clone();
        let (b, b_task) = server::serve(Router::new().route("/responses", post(move || {
            let hits = copy.clone(); async move {
                let call = hits.fetch_add(1, Ordering::SeqCst);
                let mut events = String::new();
                if generated { events.push_str("data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hello\"}\n\n"); }
                if generated || call < 2 {
                    events.push_str("data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"server_error\"}}}\n\n");
                } else {
                    events.push_str("data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n");
                }
                ([("content-type", "text/event-stream")], events)
            }
        }))).await;
        let original = source("a", &a, 0);
        let (_dir, state) = state(vec![original.clone(), source("b", &b, 1)]).await;
        state.settings.write().await.upstream_attempts = 3;
        for thread in ["thread:first", "thread:second"] {
            let key = codexpress_gateway::affinity::key(thread, "m");
            let mut lease = state
                .affinity
                .reserve(Some(&key), &original, false, false, None)
                .unwrap();
            lease.commit().unwrap();
        }
        insert(&state, "b").await;
        let response = relay::router(state.clone())
            .oneshot(request(
                br#"{"model":"m","stream":true,"input":"next"}"#,
                "first",
            ))
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), 10000).await.unwrap();
        assert_eq!(a_hits.load(Ordering::SeqCst), 0);
        assert_eq!(b_hits.load(Ordering::SeqCst), if generated { 1 } else { 3 });
        if generated {
            assert!(String::from_utf8_lossy(&body).contains("hello"));
            assert!(state.manual_routing.status().is_null());
        } else {
            assert!(String::from_utf8_lossy(&body).contains("response.completed"));
            assert_eq!(state.manual_routing.status()["source_id"], "b");
        }
        a_task.abort();
        b_task.abort();
    }
}

#[tokio::test]
async fn takeover_repairs_recoverable_context_before_giving_up_the_selected_source() {
    let fixture = TakeoverFixture::new(None, vec![409]).await;
    fixture
        .state
        .settings
        .write()
        .await
        .degraded_context_fallback = true;
    fixture.send("first").await;
    fixture.send("second").await;
    insert(&fixture.state, "b").await;
    let body = br#"{"model":"m","input":[{"type":"reasoning","encrypted_content":"fixture-cipher"},{"role":"user","content":"next"}]}"#;
    let response = relay::router(fixture.state.clone())
        .oneshot(request(body, "first"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(fixture.b_hits.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.a_hits.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.state.manual_routing.status()["source_id"], "b");
}

#[tokio::test]
async fn failed_context_repair_skips_the_abandoned_source_even_when_it_has_highest_priority() {
    let fixture = TakeoverFixture::new(None, vec![409, 400]).await;
    fixture
        .state
        .settings
        .write()
        .await
        .degraded_context_fallback = true;
    fixture.send("first").await;
    fixture.send("second").await;
    {
        let mut sources = fixture.state.sources.write().await;
        sources[0].source.priority = 1;
        sources[1].source.priority = 0;
    }
    insert(&fixture.state, "b").await;
    let body = br#"{"model":"m","input":[{"type":"reasoning","encrypted_content":"fixture-cipher"},{"role":"user","content":"next"}]}"#;
    let mut request = request(body, "first");
    request.headers_mut().remove("thread-id");
    let response = relay::router(fixture.state.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(fixture.b_hits.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.a_hits.load(Ordering::SeqCst), 3);
    assert!(fixture.state.manual_routing.status().is_null());
}
