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
    affinity, continuity, management,
    model::{AppState, Source},
    relay,
};
use serde_json::json;
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
fn request(thread: &str) -> Request {
    Request::builder().method("POST").uri("/v1/responses").header("host", "127.0.0.1:0")
        .header("authorization", "Bearer fixture").header("thread-id", thread)
        .body(Body::from(r#"{ "model":"m","input":[{"type":"reasoning","encrypted_content":"old-cipher"},{"role":"user","content":"next"}] }"#)).unwrap()
}
async fn send(state: Arc<AppState>, thread: &str) -> StatusCode {
    let response = relay::router(state).oneshot(request(thread)).await.unwrap();
    let status = response.status();
    to_bytes(response.into_body(), 10000).await.unwrap();
    status
}
async fn configure(state: &Arc<AppState>, sources: &[Source]) {
    let values: Vec<_> = sources
        .iter()
        .map(|source| {
            let mut value = serde_json::to_value(source).unwrap();
            value["secret"] = source.secret.clone();
            value
        })
        .collect();
    management::dispatch(state, "sync", json!({"sources":values,"mode":"auto"}))
        .await
        .unwrap();
    assert!(state.settings.read().await.strict_priority);
    assert_eq!(state.settings.read().await.idle_return_minutes, 5);
}

#[tokio::test]
async fn new_and_expired_conversations_start_at_priority_one_even_when_it_is_cooling() {
    let first_calls = Arc::new(AtomicUsize::new(0));
    let copy = first_calls.clone();
    let fail = Arc::new(AtomicUsize::new(1));
    let failure = fail.clone();
    let (a, a_task) = server::serve(Router::new().route(
        "/responses",
        post(move |req: Request| {
            let copy = copy.clone();
            let failure = failure.clone();
            async move {
                let body = to_bytes(req.into_body(), 10000).await.unwrap();
                assert!(String::from_utf8_lossy(&body).contains("old-cipher"));
                copy.fetch_add(1, Ordering::SeqCst);
                if failure.load(Ordering::SeqCst) == 1 {
                    StatusCode::TOO_MANY_REQUESTS.into_response()
                } else {
                    axum::Json(json!({"id":"a-result","status":"completed","output":[]}))
                        .into_response()
                }
            }
        }),
    ))
    .await;
    let second_calls = Arc::new(AtomicUsize::new(0));
    let copy = second_calls.clone();
    let (b, b_task) = server::serve(Router::new().route(
        "/responses",
        post(move || {
            let copy = copy.clone();
            async move {
                copy.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({"id":"b-result","status":"completed","output":[]}))
            }
        }),
    ))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().to_owned(), 0, "fixture".into()).unwrap();
    assert!(state.settings.read().await.strict_priority);
    let sources = [source("a", &a, 0), source("b", &b, 1)];
    configure(&state, &sources).await;
    state
        .bindings
        .lock()
        .await
        .insert(continuity::digest("encrypted", "old-cipher"), "b".into());
    assert_eq!(send(state.clone(), "long").await, StatusCode::OK);
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    assert_eq!(state.sources.read().await[0].health, "cooldown");
    let status = management::dispatch(&state, "status", json!({}))
        .await
        .unwrap();
    assert_eq!(
        status["sources"][0]["admission"]["routing_note"],
        "按优先级重试"
    );
    assert_eq!(
        status["sources"][0]["admission"]["accepting_new_conversations"],
        true
    );
    fail.store(0, Ordering::SeqCst);
    // Successful continuation refreshes B, while an unrelated new thread still
    // probes A despite its globally retained cooldown.
    assert_eq!(send(state.clone(), "long").await, StatusCode::OK);
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(send(state.clone(), "new").await, StatusCode::OK);
    assert_eq!(first_calls.load(Ordering::SeqCst), 2);
    assert_eq!(second_calls.load(Ordering::SeqCst), 2);
    let key = affinity::key("thread:long", "m");
    let runtimes = state.sources.read().await.clone();
    drop(state);
    let db = rusqlite::Connection::open(dir.path().join("affinity.sqlite")).unwrap();
    db.execute(
        "UPDATE affinity SET last_seen=?1 WHERE key=?2",
        rusqlite::params![codexpress_gateway::model::now() - 301, key],
    )
    .unwrap();
    drop(db);
    let restored = AppState::new(dir.path().to_owned(), 0, "fixture".into()).unwrap();
    *restored.sources.write().await = runtimes;
    assert_eq!(send(restored.clone(), "long").await, StatusCode::OK);
    assert_eq!(first_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        restored.affinity.preferred(&key, &sources).as_deref(),
        Some("a")
    );
    assert!(restored.notifications.list().is_empty());
    a_task.abort();
    b_task.abort();
}

#[tokio::test]
async fn capacity_is_an_atomic_reason_to_skip_priority_and_http_errors_try_the_next_source() {
    for full in [true, false] {
        let first_calls = Arc::new(AtomicUsize::new(0));
        let copy = first_calls.clone();
        let (a, a_task) = server::serve(Router::new().route(
            "/responses",
            post(move || {
                let copy = copy.clone();
                async move {
                    copy.fetch_add(1, Ordering::SeqCst);
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            }),
        ))
        .await;
        let second_calls = Arc::new(AtomicUsize::new(0));
        let copy = second_calls.clone();
        let (b, b_task) = server::serve(Router::new().route(
            "/responses",
            post(move || {
                let copy = copy.clone();
                async move {
                    copy.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({"status":"completed","output":[]}))
                }
            }),
        ))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(dir.path().to_owned(), 0, "fixture".into()).unwrap();
        let mut first = source("a", &a, 0);
        first.max_active_conversations = Some(1);
        configure(&state, &[first.clone(), source("b", &b, 1)]).await;
        let lease = if full {
            Some(
                state
                    .affinity
                    .reserve(Some("occupied"), &first, false, false, None)
                    .unwrap(),
            )
        } else {
            None
        };
        assert_eq!(send(state, "new").await, StatusCode::OK);
        assert_eq!(first_calls.load(Ordering::SeqCst), usize::from(!full));
        assert_eq!(second_calls.load(Ordering::SeqCst), 1);
        drop(lease);
        a_task.abort();
        b_task.abort();
    }
}

#[tokio::test]
async fn unknown_stream_errors_and_generated_output_never_trigger_priority_replay() {
    for output in [false, true] {
        let text = if output {
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"invalid_encrypted_content\"}}}\n\n"
        } else {
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"unknown_failure\"}}}\n\n"
        };
        let (a, a_task) = server::serve(Router::new().route(
            "/responses",
            post(move || async move { ([("content-type", "text/event-stream")], text) }),
        ))
        .await;
        let hits = Arc::new(AtomicUsize::new(0));
        let copy = hits.clone();
        let (b, b_task) = server::serve(Router::new().route(
            "/responses",
            post(move || {
                let copy = copy.clone();
                async move {
                    copy.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        ))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(dir.path().to_owned(), 0, "fixture".into()).unwrap();
        configure(&state, &[source("a", &a, 0), source("b", &b, 1)]).await;
        assert_eq!(send(state, "new").await, StatusCode::OK);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        a_task.abort();
        b_task.abort();
    }
}

#[tokio::test]
async fn ambiguous_http_timeouts_do_not_replay_to_another_source() {
    for status in [StatusCode::REQUEST_TIMEOUT, StatusCode::GATEWAY_TIMEOUT] {
        let (a, a_task) =
            server::serve(Router::new().route("/responses", post(move || async move { status })))
                .await;
        let hits = Arc::new(AtomicUsize::new(0));
        let copy = hits.clone();
        let (b, b_task) = server::serve(Router::new().route(
            "/responses",
            post(move || {
                let copy = copy.clone();
                async move {
                    copy.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        ))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(dir.path().to_owned(), 0, "fixture".into()).unwrap();
        configure(&state, &[source("a", &a, 0), source("b", &b, 1)]).await;
        assert_eq!(send(state, "new").await, status);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        a_task.abort();
        b_task.abort();
    }
}

#[tokio::test]
async fn reconstruction_is_saved_only_after_positive_upstream_acceptance() {
    for success in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let copy = calls.clone();
        let (a, a_task) = server::serve(Router::new().route("/responses", post(move |req: Request| {
            let copy = copy.clone();
            async move {
                copy.fetch_add(1, Ordering::SeqCst);
                let bytes = to_bytes(req.into_body(), 10000).await.unwrap();
                let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                if body["input"].as_array().unwrap().iter().any(|item| item["encrypted_content"].is_string()) {
                    (StatusCode::BAD_REQUEST, axum::Json(json!({"error":{"code":"invalid_encrypted_content"}}))).into_response()
                } else if success {
                    ([("x-codex-turn-state", "old-turn")], axum::Json(json!({"id":"rebuilt-result","status":"completed","output":[{"type":"reasoning","encrypted_content":"old-cipher"}]}))).into_response()
                } else {
                    ([("content-type", "text/event-stream")], "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"unknown_failure\"}}}\n\n").into_response()
                }
            }
        }))).await;
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(dir.path().to_owned(), 0, "fixture".into()).unwrap();
        let sources = [source("a", &a, 0)];
        configure(&state, &sources).await;
        let inherited = [
            continuity::digest("encrypted", "old-cipher"),
            continuity::digest("turn", "old-turn"),
        ];
        for key in &inherited {
            state
                .bindings
                .lock()
                .await
                .insert(key.clone(), "original-parent-source".into());
        }
        {
            let mut runtimes = state.sources.write().await;
            runtimes[0].health = "cooldown".into();
            runtimes[0].cooldown_until = Some(codexpress_gateway::model::now() + 60);
        }
        let mut req = request("restored");
        req.headers_mut()
            .insert("x-codex-turn-state", "old-turn".parse().unwrap());
        let response = relay::router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 10000).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let key = affinity::key("thread:restored", "m");
        let excluded = state.affinity.exclusions(&key);
        assert_eq!(excluded.contains("rebuilt"), success);
        assert_eq!(
            excluded.contains(&continuity::digest("encrypted", "old-cipher")),
            success
        );
        assert_eq!(state.affinity.preferred(&key, &sources).is_some(), success);
        for key in &inherited {
            assert_eq!(
                state.bindings.lock().await.get(key).map(String::as_str),
                Some("original-parent-source")
            );
        }
        a_task.abort();
    }
}
