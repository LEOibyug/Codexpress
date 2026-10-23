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
use codexpress_gateway::{chatbot, management, model::AppState, relay};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tower::ServiceExt;

async fn state(urls: &[String], attempts: u8, delay: u64) -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::new(directory.path().into(), 0, "fixture".into()).unwrap();
    let sources: Vec<_> = urls
        .iter()
        .enumerate()
        .map(|(i, base)| {
            let source =
                source_fixture::api_source(&format!("s{i}"), "Fixture", base, "fixture-upstream");
            let mut value = serde_json::to_value(&source).unwrap();
            value["secret"] = source.secret;
            value["priority"] = json!(i);
            value
        })
        .collect();
    management::dispatch(&state, "sync", json!({"sources":sources,"upstream_attempts":attempts,"upstream_retry_delay_seconds":delay})).await.unwrap();
    (directory, state)
}
fn request(body: &str) -> Request {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("host", "127.0.0.1:0")
        .header("authorization", "Bearer fixture")
        .body(Body::from(body.to_owned()))
        .unwrap()
}
const BODY: &str = r#"{ "model":"m", "input":"hello", "extension":true }"#;

#[tokio::test]
async fn transient_errors_retry_the_same_source_after_each_failure_before_fallback() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let copy = events.clone();
    let (first, a) = server::serve(Router::new().route(
        "/responses",
        post(move |req: Request| {
            let copy = copy.clone();
            async move {
                assert_eq!(
                    &to_bytes(req.into_body(), 10000).await.unwrap()[..],
                    BODY.as_bytes()
                );
                let call = {
                    let mut events = copy.lock().unwrap();
                    events.push(("a", Instant::now()));
                    events.len()
                };
                if call < 3 {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                } else {
                    axum::Json(json!({"status":"completed","output":[]})).into_response()
                }
            }
        }),
    ))
    .await;
    let copy = events.clone();
    let (second, b) = server::serve(Router::new().route(
        "/responses",
        post(move || {
            let copy = copy.clone();
            async move {
                copy.lock().unwrap().push(("b", Instant::now()));
                StatusCode::OK
            }
        }),
    ))
    .await;
    let (_directory, state) = state(&[first, second], 3, 1).await;
    let response = relay::router(state).oneshot(request(BODY)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    let events = events.lock().unwrap();
    assert_eq!(
        events.iter().map(|e| e.0).collect::<Vec<_>>(),
        ["a", "a", "a"]
    );
    for pair in events.windows(2) {
        assert!(pair[1].1.duration_since(pair[0].1) >= Duration::from_millis(1100));
    }
    a.abort();
    b.abort();
}

#[tokio::test]
async fn exhausting_the_configured_budget_falls_back_and_each_source_has_its_own_budget() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let copy = events.clone();
    let (first, a) = server::serve(Router::new().route(
        "/responses",
        post(move || {
            let copy = copy.clone();
            async move {
                copy.lock().unwrap().push("a");
                StatusCode::BAD_GATEWAY
            }
        }),
    ))
    .await;
    let copy = events.clone();
    let (second, b) = server::serve(Router::new().route(
        "/responses",
        post(move || {
            let copy = copy.clone();
            async move {
                let count = {
                    let mut events = copy.lock().unwrap();
                    events.push("b");
                    events.iter().filter(|s| **s == "b").count()
                };
                if count == 1 {
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                } else {
                    axum::Json(json!({"status":"completed","output":[]})).into_response()
                }
            }
        }),
    ))
    .await;
    let (_directory, state) = state(&[first, second], 3, 0).await;
    let response = relay::router(state).oneshot(request(BODY)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(*events.lock().unwrap(), ["a", "a", "a", "b", "b"]);
    a.abort();
    b.abort();
}

#[tokio::test]
async fn connection_failure_waits_for_the_same_source_to_recover() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let (_directory, state) = state(&[format!("http://{address}")], 3, 1).await;
    let sending = tokio::spawn(relay::router(state).oneshot(request(BODY)));
    tokio::time::sleep(Duration::from_millis(150)).await;
    let listener = tokio::net::TcpListener::bind(address).await.unwrap();
    let upstream = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/responses",
                post(|| async { axum::Json(json!({"status":"completed","output":[]})) }),
            ),
        )
        .await
        .unwrap();
    });
    let response = sending.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    upstream.abort();
}

#[tokio::test]
async fn role_repairs_and_chatbot_ingress_do_not_consume_the_network_retry_budget() {
    for chat in [false, true] {
        let calls = Arc::new(Mutex::new(0));
        let copy = calls.clone();
        let (base, task) = server::serve(Router::new().route("/responses", post(move |req: Request| {
            let copy = copy.clone(); async move {
                let body: Value = serde_json::from_slice(&to_bytes(req.into_body(),10000).await.unwrap()).unwrap();
                let count = { let mut count = copy.lock().unwrap(); *count+=1; *count };
                if count==1 {
                    assert_eq!(body["input"][0]["role"],"system");
                    (StatusCode::BAD_REQUEST,axum::Json(json!({"error":{"code":"unsupported_value","param":"input[0].role"}}))).into_response()
                } else {
                    assert_eq!(body["input"][0]["role"],"developer");
                    if count<4 { StatusCode::BAD_GATEWAY.into_response() }
                    else { axum::Json(json!({"status":"completed","output":[]})).into_response() }
                }
            }
        }))).await;
        let (_directory, state) = state(&[base], 3, 0).await;
        let mut req = request(r#"{"model":"m","input":[{"role":"system","content":"policy"}]}"#);
        let app = if chat {
            let key = management::dispatch(&state, "chatbot.key", json!({}))
                .await
                .unwrap();
            let port = management::dispatch(&state, "chatbot.status", json!({}))
                .await
                .unwrap()["port"]
                .as_u64()
                .unwrap();
            req.headers_mut()
                .insert("host", format!("127.0.0.1:{port}").parse().unwrap());
            req.headers_mut().insert(
                "authorization",
                format!("Bearer {}", key["key"].as_str().unwrap())
                    .parse()
                    .unwrap(),
            );
            chatbot::router(state)
        } else {
            relay::router(state)
        };
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["x-codexpress-adapted-parameters"],
            "input.role"
        );
        to_bytes(response.into_body(), 10000).await.unwrap();
        assert_eq!(*calls.lock().unwrap(), 4);
        task.abort();
    }
}

#[tokio::test]
async fn permanent_errors_and_already_started_streams_are_not_replayed() {
    for status in [400, 401, 403, 429, 504, 200] {
        let calls = Arc::new(Mutex::new(0));
        let copy = calls.clone();
        let (base,task)=server::serve(Router::new().route("/responses",post(move || {
            let copy=copy.clone(); async move {
                *copy.lock().unwrap()+=1;
                if status==200 { ([("content-type","text/event-stream")],"data: {\"type\":\"response.output_text.delta\",\"delta\":\"started\"}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\"}}}\n\n").into_response() }
                else { (StatusCode::from_u16(status).unwrap(),"original error").into_response() }
            }
        }))).await;
        let (_directory, state) = state(&[base], 3, 0).await;
        let response = relay::router(state).oneshot(request(BODY)).await.unwrap();
        assert_eq!(response.status().as_u16(), status);
        to_bytes(response.into_body(), 10000).await.unwrap();
        assert_eq!(*calls.lock().unwrap(), 1);
        task.abort();
    }
}

#[tokio::test]
async fn retry_wait_holds_capacity_and_revalidates_disabled_sources_before_sending_again() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let failed = Arc::new(tokio::sync::Notify::new());
    let copy = events.clone();
    let signal = failed.clone();
    let (first, a) = server::serve(Router::new().route(
        "/responses",
        post(move || {
            let copy = copy.clone();
            let signal = signal.clone();
            async move {
                copy.lock().unwrap().push("a");
                signal.notify_one();
                StatusCode::SERVICE_UNAVAILABLE
            }
        }),
    ))
    .await;
    let copy = events.clone();
    let (second, b) = server::serve(Router::new().route(
        "/responses",
        post(move || {
            let copy = copy.clone();
            async move {
                copy.lock().unwrap().push("b");
                axum::Json(json!({"status":"completed","output":[]}))
            }
        }),
    ))
    .await;
    let (_directory, state) = state(&[first, second], 3, 1).await;
    state.sources.write().await[0]
        .source
        .max_active_conversations = Some(1);
    let sending = tokio::spawn(relay::router(state.clone()).oneshot(request(BODY)));
    tokio::time::timeout(Duration::from_secs(3), failed.notified())
        .await
        .unwrap();
    let sources: Vec<_> = state
        .sources
        .read()
        .await
        .iter()
        .map(|r| r.source.clone())
        .collect();
    assert!(!state.affinity.capacity_available(None, &sources[0]));
    assert!(state.inflight.load(std::sync::atomic::Ordering::Relaxed) > 0);
    let mut values: Vec<_> = sources
        .iter()
        .map(|s| {
            let mut value = serde_json::to_value(s).unwrap();
            value["secret"] = s.secret.clone();
            value
        })
        .collect();
    values[0]["enabled"] = json!(false);
    management::dispatch(
        &state,
        "sync",
        json!({"sources":values,"upstream_attempts":3,"upstream_retry_delay_seconds":1}),
    )
    .await
    .unwrap();
    let response = sending.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(*events.lock().unwrap(), ["a", "b"]);
    assert!(state.affinity.capacity_available(None, &sources[0]));
    a.abort();
    b.abort();
}

#[tokio::test]
async fn exhausted_budgets_survive_context_reconstruction_and_configuration_replanning() {
    for replan in [false, true] {
        let calls = Arc::new(Mutex::new(0));
        let copy = calls.clone();
        let failed = Arc::new(tokio::sync::Notify::new());
        let signal = failed.clone();
        let (base, task) = server::serve(Router::new().route(
            "/responses",
            post(move || {
                let copy = copy.clone();
                let signal = signal.clone();
                async move {
                    *copy.lock().unwrap() += 1;
                    signal.notify_one();
                    StatusCode::SERVICE_UNAVAILABLE
                }
            }),
        ))
        .await;
        let (_directory, state) = state(&[base], 3, u64::from(replan)).await;
        let mut req = request(
            r#"{"model":"m","input":[{"type":"reasoning","encrypted_content":"old-cipher"},{"role":"user","content":"complete visible history"}]}"#,
        );
        req.headers_mut()
            .insert("thread-id", "fixture-thread".parse().unwrap());
        let sending = tokio::spawn(relay::router(state.clone()).oneshot(req));
        if replan {
            tokio::time::timeout(Duration::from_secs(3), failed.notified())
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
            management::dispatch(
                &state,
                "sync",
                json!({"sources":sources,"upstream_attempts":3,"upstream_retry_delay_seconds":0}),
            )
            .await
            .unwrap();
        }
        let response = sending.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        to_bytes(response.into_body(), 10000).await.unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            3,
            "one source must not regain its budget after reconstruction/replan"
        );
        task.abort();
    }
}

#[tokio::test]
async fn explicit_permanent_rejections_inside_transient_http_statuses_are_not_retried() {
    for (problem, health) in [
        (json!({"code":"insufficient_quota"}), "exhausted"),
        (
            json!({"code":null,"type":"insufficient_quota"}),
            "exhausted",
        ),
        (
            json!({"code":"unknown_wrapper","type":"insufficient_quota"}),
            "exhausted",
        ),
        (json!({"code":"invalid_api_key"}), "auth_required"),
        (json!({"code":"unsupported_parameter"}), "cooldown"),
    ] {
        let calls = Arc::new(Mutex::new(0));
        let copy = calls.clone();
        let expected = json!({"error":problem});
        let upstream_error = expected.clone();
        let (base, task) = server::serve(Router::new().route(
            "/responses",
            post(move || {
                let copy = copy.clone();
                let problem = upstream_error.clone();
                async move {
                    *copy.lock().unwrap() += 1;
                    (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(problem))
                }
            }),
        ))
        .await;
        let (_directory, state) = state(&[base], 3, 0).await;
        let response = relay::router(state.clone())
            .oneshot(request(BODY))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = to_bytes(response.into_body(), 10000).await.unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), expected);
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(state.sources.read().await[0].health, health);
        task.abort();
    }
}
