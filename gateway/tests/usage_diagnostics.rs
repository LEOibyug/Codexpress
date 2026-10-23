#[path = "common/server.rs"]
mod server;
#[path = "common/source.rs"]
mod source_fixture;

use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use codexpress_gateway::{
    model::{AppState, SourceRuntime},
    relay,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tower::ServiceExt;

async fn setup(
    router: Router,
) -> (
    tempfile::TempDir,
    Arc<AppState>,
    tokio::task::JoinHandle<()>,
) {
    let (address, task) = server::serve(router).await;
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::new(directory.path().into(), 0, "private-local-key".into()).unwrap();
    let source = source_fixture::api_source("fixture", "Fixture", &address, "private-upstream-key");
    state.sources.write().await.push(SourceRuntime::new(source));
    state.settings.write().await.upstream_attempts = 2;
    state.settings.write().await.upstream_retry_delay_seconds = 0;
    (directory, state, task)
}
fn request(input: Value) -> Request {
    Request::builder()
        .method("POST")
        .uri("/v1/responses?api_key=private-local-key")
        .header("host", "127.0.0.1:0")
        .header("authorization", "Bearer private-local-key")
        .header("content-type", "application/json")
        .body(Body::from(input.to_string()))
        .unwrap()
}
fn details(state: &AppState) -> Vec<Value> {
    state.usage.summary(1).unwrap()["recent"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| {
            state
                .usage
                .detail(&json!({"attempt_id":record["attempt_id"],"body_limit":262144}))
                .unwrap()
        })
        .collect()
}

#[tokio::test]
async fn discarded_retry_errors_keep_full_feedback_and_success_keeps_only_metadata() {
    let calls = Arc::new(AtomicUsize::new(0));
    let copy = calls.clone();
    let (_dir, state, task) = setup(Router::new().route("/responses", post(move || {
        let copy=copy.clone(); async move {
            if copy.fetch_add(1,Ordering::Relaxed)==0 {
                (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":{"code":"busy","message":"temporary","extra":{"provider":"fixture","api_key":"private-upstream-key"}},"detail":"完整错误反馈"}))).into_response()
            } else { Json(json!({"id":"success-id","output":[{"content":"PRIVATE-SUCCESS-BODY"}],"usage":{"input_tokens":4,"output_tokens":2}})).into_response() }
        }
    }))).await;
    let response = relay::router(state.clone())
        .oneshot(request(
            json!({"model":"gpt-6.1-sol","input":"PRIVATE-REQUEST-BODY"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(String::from_utf8(
        to_bytes(response.into_body(), 10000)
            .await
            .unwrap()
            .to_vec()
    )
    .unwrap()
    .contains("PRIVATE-SUCCESS-BODY"));
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    let records = details(&state);
    assert_eq!(records.len(), 2);
    let failed = records
        .iter()
        .find(|record| record["record"]["status"] == "failed")
        .unwrap();
    let body: Value =
        serde_json::from_str(failed["diagnostic"]["body_text"].as_str().unwrap()).unwrap();
    assert_eq!(body["detail"], "完整错误反馈");
    assert_eq!(body["error"]["extra"]["provider"], "fixture");
    assert_eq!(body["error"]["extra"]["api_key"], "[已隐藏]");
    assert_eq!(failed["diagnostic"]["http_status"], 503);
    assert_eq!(failed["diagnostic"]["body_complete"], true);
    assert!(!failed["diagnostic"]["upstream_url"]
        .as_str()
        .unwrap()
        .contains('?'));
    let success = records
        .iter()
        .find(|record| record["record"]["status"] == "success")
        .unwrap();
    assert_eq!(success["diagnostic"]["body_saved_bytes"], 0);
    for record in records {
        let encoded = record.to_string();
        for secret in [
            "private-upstream-key",
            "private-local-key",
            "PRIVATE-SUCCESS-BODY",
            "PRIVATE-REQUEST-BODY",
        ] {
            assert!(!encoded.contains(secret));
        }
    }
    task.abort();
}

#[tokio::test]
async fn role_repair_keeps_rejection_and_tracks_adapted_field_on_next_attempt() {
    let calls = Arc::new(AtomicUsize::new(0));
    let copy = calls.clone();
    let (_dir,state,task)=setup(Router::new().route("/responses",post(move || { let copy=copy.clone(); async move {
        if copy.fetch_add(1,Ordering::Relaxed)==0 { (StatusCode::BAD_REQUEST,Json(json!({"error":{"code":"unsupported_value","param":"input[0].role","message":"system messages are not supported"},"more":"provider detail"}))).into_response() }
        else { Json(json!({"id":"repaired","output":[],"usage":{"input_tokens":1,"output_tokens":0}})).into_response() }
    }}))).await;
    let response = relay::router(state.clone())
        .oneshot(request(
            json!({"model":"gpt-6.1-sol","input":[{"role":"system","content":"instruction"}]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    to_bytes(response.into_body(), 10000).await.unwrap();
    let records = details(&state);
    assert_eq!(records.len(), 2);
    let failed = records
        .iter()
        .find(|r| r["record"]["status"] == "failed")
        .unwrap();
    assert!(failed["diagnostic"]["body_text"]
        .as_str()
        .unwrap()
        .contains("provider detail"));
    let success = records
        .iter()
        .find(|r| r["record"]["status"] == "success")
        .unwrap();
    assert_eq!(
        success["diagnostic"]["adapted_parameters"],
        json!(["input.role"])
    );
    task.abort();
}

#[tokio::test]
async fn final_error_forwarding_is_unchanged_and_failed_sse_saves_only_error() {
    let wire =
        json!({"error":{"code":"invalid_request","message":"bad input"},"extra":"原始完整反馈"})
            .to_string();
    let expected = wire.clone();
    let (_dir, state, task) = setup(Router::new().route(
        "/responses",
        post(move || {
            let wire = wire.clone();
            async move {
                Response::builder()
                    .status(400)
                    .header("content-type", "application/json")
                    .body(Body::from(wire))
                    .unwrap()
            }
        }),
    ))
    .await;
    let response = relay::router(state.clone())
        .oneshot(request(json!({"model":"gpt-6.1-sol","input":"x"})))
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(
        to_bytes(response.into_body(), 10000)
            .await
            .unwrap()
            .as_ref(),
        expected.as_bytes()
    );
    assert_eq!(
        details(&state)[0]["diagnostic"]["error_code"],
        "invalid_request"
    );
    task.abort();

    let wire="data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"output\":[{\"content\":\"PRIVATE-PARTIAL-OUTPUT\"}],\"error\":{\"code\":\"server_error\",\"message\":\"try later\",\"extra\":\"full detail\"}}}\n\n";
    let (_dir, state, task) = setup(Router::new().route(
        "/responses",
        post(move || async move {
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from(wire))
                .unwrap()
        }),
    ))
    .await;
    let response = relay::router(state.clone())
        .oneshot(request(
            json!({"model":"gpt-6.1-sol","input":"x","stream":true}),
        ))
        .await
        .unwrap();
    assert_eq!(
        to_bytes(response.into_body(), 10000)
            .await
            .unwrap()
            .as_ref(),
        wire.as_bytes()
    );
    let detail = &details(&state)[0];
    assert_eq!(detail["diagnostic"]["error_kind"], "upstream_event");
    assert!(detail["diagnostic"]["body_text"]
        .as_str()
        .unwrap()
        .contains("full detail"));
    assert!(!detail.to_string().contains("PRIVATE-PARTIAL-OUTPUT"));
    task.abort();
}

#[tokio::test]
async fn compressed_error_is_forwarded_raw_and_diagnostic_pages_decode_utf8_safely() {
    use std::io::Write;
    let expected = json!({"error":{"code":"invalid_request","message":"参数不匹配","api_key":"private-upstream-key"},"detail":"完整中文反馈".repeat(15000)});
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(expected.to_string().as_bytes()).unwrap();
    let wire = encoder.finish().unwrap();
    let copy = wire.clone();
    let (_dir, state, task) = setup(Router::new().route(
        "/responses",
        post(move || {
            let copy = copy.clone();
            async move {
                Response::builder()
                    .status(400)
                    .header("content-type", "application/json")
                    .header("content-encoding", "gzip")
                    .body(Body::from(copy))
                    .unwrap()
            }
        }),
    ))
    .await;
    let response = relay::router(state.clone())
        .oneshot(request(json!({"model":"gpt-6.1-sol","input":"x"})))
        .await
        .unwrap();
    assert_eq!(response.headers()["content-encoding"], "gzip");
    assert_eq!(
        to_bytes(response.into_body(), 200000)
            .await
            .unwrap()
            .as_ref(),
        wire.as_slice()
    );
    let id = state.usage.summary(1).unwrap()["recent"][0]["attempt_id"].clone();
    let mut text = String::new();
    let mut offset = 0;
    loop {
        let detail = state
            .usage
            .detail(&json!({"attempt_id":id,"body_offset":offset,"body_limit":65536}))
            .unwrap();
        text.push_str(detail["diagnostic"]["body_text"].as_str().unwrap());
        if let Some(next) = detail["diagnostic"]["body_next_offset"].as_u64() {
            assert!(next > offset);
            offset = next;
        } else {
            break;
        }
    }
    let result: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(result["detail"], expected["detail"]);
    assert_eq!(result["error"]["api_key"], "[已隐藏]");
    task.abort();
}

#[tokio::test]
async fn interrupted_error_body_stays_interrupted_for_the_client_and_detail() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut data = [0; 8192];
        assert!(socket.read(&mut data).await.unwrap() > 0);
        socket.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial").await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().into(), 0, "private-local-key".into()).unwrap();
    state
        .sources
        .write()
        .await
        .push(SourceRuntime::new(source_fixture::api_source(
            "fixture",
            "Fixture",
            &address,
            "private-upstream-key",
        )));
    let response = relay::router(state.clone())
        .oneshot(request(json!({"model":"gpt-6.1-sol","input":"x"})))
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(to_bytes(response.into_body(), 10000).await.is_err());
    let record = &details(&state)[0];
    assert_eq!(record["diagnostic"]["body_complete"], false);
    assert_eq!(record["diagnostic"]["error_kind"], "body_transport");
    upstream.await.unwrap();
}

#[tokio::test]
async fn final_error_headers_are_immediate_and_later_failure_event_replaces_initial_error() {
    let (_dir,state,task)=setup(Router::new().route("/responses",post(||async {
        let body=async_stream::stream! {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            yield Ok::<_,std::io::Error>(bytes::Bytes::from_static(b"{\"error\":{\"message\":\"delayed error\"}}"));
        };
        Response::builder().status(400).header("content-type","application/json").body(Body::from_stream(body)).unwrap()
    }))).await;
    let response = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        relay::router(state.clone()).oneshot(request(json!({"model":"gpt-6.1-sol","input":"x"}))),
    )
    .await
    .expect("final error headers must not wait for diagnostic body capture")
    .unwrap();
    assert_eq!(response.status(), 400);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert!(details(&state)[0]["diagnostic"]["body_text"]
        .as_str()
        .unwrap()
        .contains("delayed error"));
    task.abort();
    let (_dir,state,task)=setup(Router::new().route("/responses",post(||async {
        let body=async_stream::stream! {
            yield Ok::<_,std::io::Error>(bytes::Bytes::from_static(b"data: {\"type\":\"error\",\"code\":\"initial\",\"message\":\"short\"}\n\n"));
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            yield Ok::<_,std::io::Error>(bytes::Bytes::from_static(b"data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"final\",\"message\":\"FULL-FINAL-DETAIL\"}}}\n\n"));
        };
        Response::builder().header("content-type","text/event-stream").body(Body::from_stream(body)).unwrap()
    }))).await;
    let response = relay::router(state.clone())
        .oneshot(request(
            json!({"model":"gpt-6.1-sol","input":"x","stream":true}),
        ))
        .await
        .unwrap();
    to_bytes(response.into_body(), 10000).await.unwrap();
    let detail = &details(&state)[0];
    assert_eq!(detail["diagnostic"]["error_code"], "final");
    assert!(detail["diagnostic"]["body_text"]
        .as_str()
        .unwrap()
        .contains("FULL-FINAL-DETAIL"));
    task.abort();
}

#[tokio::test]
async fn failure_event_larger_than_legacy_observation_budget_keeps_complete_feedback() {
    let error = json!({"type":"response.failed","response":{"status":"failed","error":{"code":"large_failure","message":"large error","extra":"x".repeat(1100000)}}});
    let wire = format!("data: {error}\n\n");
    let expected = wire.clone();
    let (_dir, state, task) = setup(Router::new().route(
        "/responses",
        post(move || {
            let wire = wire.clone();
            async move {
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(wire))
                    .unwrap()
            }
        }),
    ))
    .await;
    let response = relay::router(state.clone())
        .oneshot(request(
            json!({"model":"gpt-6.1-sol","input":"x","stream":true}),
        ))
        .await
        .unwrap();
    assert_eq!(
        to_bytes(response.into_body(), 2000000)
            .await
            .unwrap()
            .as_ref(),
        expected.as_bytes()
    );
    let detail = &details(&state)[0];
    assert_eq!(detail["record"]["status"], "failed");
    assert_eq!(detail["diagnostic"]["error_code"], "large_failure");
    assert!(detail["diagnostic"]["body_saved_bytes"].as_u64().unwrap() > 1100000);
    assert_eq!(detail["diagnostic"]["body_truncated"], false);
    task.abort();
}
