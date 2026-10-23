#[path = "common/server.rs"]
mod server;
#[path = "common/source.rs"]
mod source_fixture;

use axum::{
    body::{to_bytes, Body},
    extract::{ConnectInfo, Request},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Router,
};
use bytes::Bytes;
use codexpress_gateway::{chatbot, management, model::AppState, relay, remote};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};
use tower::ServiceExt;

const LARGE_IMAGE_BYTES: usize = 33 * 1024 * 1024;

fn image() -> String {
    format!("data:image/png;base64,{}", "A".repeat(LARGE_IMAGE_BYTES))
}
fn payload() -> Value {
    json!({"model":"m","input":[{"role":"user","content":[{"type":"input_image","image_url":image()}]}]})
}
async fn configure(state: &Arc<AppState>, base: &str) {
    let source = source_fixture::api_source("api", "Fixture", base, "upstream-fixture");
    let mut value = serde_json::to_value(&source).unwrap();
    value["secret"] = source.secret;
    management::dispatch(state, "sync", json!({"sources":[value]}))
        .await
        .unwrap();
}
fn request(path: &str, host: &str, key: &str, body: Body) -> Request {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("host", host)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(body)
        .unwrap()
}
fn chunked(bytes: Bytes) -> Body {
    let chunks: Vec<_> = (0..bytes.len())
        .step_by(1024 * 1024)
        .map(|start| {
            Ok::<_, std::io::Error>(bytes.slice(start..(start + 1024 * 1024).min(bytes.len())))
        })
        .collect();
    Body::from_stream(futures_util::stream::iter(chunks))
}

#[tokio::test]
async fn large_chunked_native_remote_and_compact_requests_reach_upstream_unchanged() {
    let bytes = Bytes::from(serde_json::to_vec(&payload()).unwrap());
    let expected = Sha256::digest(&bytes);
    let hits = Arc::new(AtomicUsize::new(0));
    let rejected = Arc::new(AtomicBool::new(false));
    let (count, reject) = (hits.clone(), rejected.clone());
    let (base, task) = server::serve(Router::new().fallback(post(move |req: Request| {
        let (count, reject) = (count.clone(), reject.clone());
        async move {
            let raw = to_bytes(req.into_body(), usize::MAX).await.unwrap();
            assert_eq!(Sha256::digest(&raw), expected);
            count.fetch_add(1, Ordering::SeqCst);
            if reject.load(Ordering::SeqCst) {
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    axum::Json(json!({"error":{"code":"upstream_body_limit"}})),
                )
                    .into_response()
            } else {
                axum::Json(json!({"id":"fixture-response","status":"completed","output":[]}))
                    .into_response()
            }
        }
    })))
    .await;
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::new(directory.path().into(), 17863, "local-fixture".into()).unwrap();
    configure(&state, &base).await;
    let peer = management::dispatch(
        &state,
        "remote.preview",
        json!({"id":"peer","name":"Fixture","host":"192.0.2.1","config":"model='m'"}),
    )
    .await
    .unwrap();
    for (path, host, key, ip) in [
        (
            "/v1/responses",
            "127.0.0.1:17863",
            "local-fixture",
            "127.0.0.1:4444",
        ),
        (
            "/v1/responses/compact",
            "127.0.0.1:17863",
            "local-fixture",
            "127.0.0.1:4444",
        ),
        (
            "/v1/responses",
            "192.0.2.1:17863",
            peer["key"].as_str().unwrap(),
            "192.0.2.2:4444",
        ),
    ] {
        let mut req = request(path, host, key, chunked(bytes.clone()));
        req.extensions_mut()
            .insert(ConnectInfo(ip.parse::<SocketAddr>().unwrap()));
        let response = remote::router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 10000).await.unwrap();
    }
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    rejected.store(true, Ordering::SeqCst);
    let response = relay::router(state)
        .oneshot(request(
            "/v1/responses",
            "127.0.0.1:17863",
            "local-fixture",
            chunked(bytes),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let error: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 10000).await.unwrap()).unwrap();
    assert_eq!(error["error"]["code"], "upstream_body_limit");
    assert_eq!(hits.load(Ordering::SeqCst), 4);
    task.abort();
}

#[tokio::test]
async fn large_cached_context_survives_restart_and_rebuilds_without_a_local_size_ceiling() {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let (base, task) = server::serve(Router::new().route(
        "/responses",
        post(move |req: Request| {
            let count = count.clone();
            async move {
                let bytes = to_bytes(req.into_body(), usize::MAX).await.unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                match count.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        assert!(bytes.len() > LARGE_IMAGE_BYTES);
                        axum::Json(json!({"id":"large-parent","status":"completed","output":[]}))
                            .into_response()
                    }
                    1 => (
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({"error":{"code":"previous_response_not_found"}})),
                    )
                        .into_response(),
                    2 => {
                        assert!(bytes.len() > LARGE_IMAGE_BYTES);
                        assert!(body.get("previous_response_id").is_none());
                        assert_eq!(
                            body["input"][0]["content"][0]["image_url"]
                                .as_str()
                                .unwrap()
                                .len(),
                            LARGE_IMAGE_BYTES + "data:image/png;base64,".len()
                        );
                        assert_eq!(
                            body["input"].as_array().unwrap().last().unwrap()["content"],
                            "continue"
                        );
                        axum::Json(json!({"id":"large-child","status":"completed","output":[]}))
                            .into_response()
                    }
                    _ => panic!("unexpected replay"),
                }
            }
        }),
    ))
    .await;
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::new(directory.path().into(), 17863, "local-fixture".into()).unwrap();
    state.history.configure(true, 64_000_000).unwrap();
    configure(&state, &base).await;
    let response = relay::router(state.clone())
        .oneshot(request(
            "/v1/responses",
            "127.0.0.1:17863",
            "local-fixture",
            Body::from(serde_json::to_vec(&payload()).unwrap()),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(state.history.status().unwrap()["skipped_captures"], 0);
    drop(state);
    let restored = AppState::new(directory.path().into(), 17863, "local-fixture".into()).unwrap();
    configure(&restored, &base).await;
    let response = relay::router(restored)
        .oneshot(request(
            "/v1/responses",
            "127.0.0.1:17863",
            "local-fixture",
            Body::from(
                json!({"model":"m","previous_response_id":"large-parent","input":"continue"})
                    .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    task.abort();
}

#[tokio::test]
async fn chatbot_formats_accept_large_images_without_adding_another_ingress_ceiling() {
    let (base, task) = server::serve(Router::new().route(
        "/responses",
        post(|req: Request| async move {
            let body: Value =
                serde_json::from_slice(&to_bytes(req.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(
                body["input"][0]["content"][0]["image_url"]
                    .as_str()
                    .unwrap()
                    .len(),
                LARGE_IMAGE_BYTES + "data:image/png;base64,".len()
            );
            axum::Json(json!({"id":"chatbot-fixture","status":"completed","output":[]}))
        }),
    ))
    .await;
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::new(directory.path().into(), 17863, "local-fixture".into()).unwrap();
    configure(&state, &base).await;
    let key = management::dispatch(&state, "chatbot.key", json!({}))
        .await
        .unwrap();
    for path in ["/v1/chat/completions", "/v1/responses"] {
        let body = if path.ends_with("completions") {
            json!({"model":"m","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":image()}}]}]})
        } else {
            payload()
        };
        let response = chatbot::router(state.clone())
            .oneshot(request(
                path,
                "127.0.0.1:17865",
                key["key"].as_str().unwrap(),
                chunked(Bytes::from(body.to_string())),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 10000).await.unwrap();
    }
    task.abort();
}

#[tokio::test]
async fn incomplete_uploads_are_read_errors_and_never_become_413_or_upstream_attempts() {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::new(directory.path().into(), 17863, "local-fixture".into()).unwrap();
    let chatbot_key = management::dispatch(&state, "chatbot.key", json!({}))
        .await
        .unwrap();
    for chat in [false, true] {
        let body = Body::from_stream(futures_util::stream::iter(vec![
            Ok(Bytes::from_static(b"{\"model\":")),
            Err(std::io::Error::other("PRIVATE-TRANSPORT-ERROR-MARKER")),
        ]));
        let (router, host, key) = if chat {
            (
                chatbot::router(state.clone()),
                "127.0.0.1:17865",
                chatbot_key["key"].as_str().unwrap(),
            )
        } else {
            (
                relay::router(state.clone()),
                "127.0.0.1:17863",
                "local-fixture",
            )
        };
        let response = router
            .oneshot(request("/v1/responses", host, key, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(response.into_body(), 10000).await.unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE-TRANSPORT-ERROR-MARKER"));
        assert!(String::from_utf8_lossy(&bytes).contains("读取中断"));
    }
    assert!(state.usage.summary(1).unwrap()["recent"]
        .as_array()
        .unwrap()
        .is_empty());
}
