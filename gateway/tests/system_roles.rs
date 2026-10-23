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
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tower::ServiceExt;

async fn state(urls: &[String]) -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::new(directory.path().into(), 17863, "fixture".into()).unwrap();
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
    management::dispatch(
        &state,
        "sync",
        json!({"sources":sources,"upstream_attempts":1,"upstream_retry_delay_seconds":0}),
    )
    .await
    .unwrap();
    (directory, state)
}
fn request(path: &str, key: &str, body: &str) -> Request {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "127.0.0.1:17863")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

#[tokio::test]
async fn explicit_system_role_rejections_are_repaired_on_all_text_ingress_paths() {
    for mode in 0..4 {
        let calls = Arc::new(AtomicUsize::new(0));
        let copy = calls.clone();
        let (base, task) = server::serve(Router::new().fallback(post(move |req: Request| {
            let copy = copy.clone();
            async move {
                let (parts, bytes) = req.into_parts();
                let bytes = to_bytes(bytes, 10000).await.unwrap();
                assert_eq!(parts.headers["content-length"].to_str().unwrap().parse::<usize>().unwrap(), bytes.len());
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                let count = copy.fetch_add(1, Ordering::SeqCst);
                assert_eq!(body["instructions"], if mode==3 {""} else {"top-level instructions"});
                assert_eq!(body["input"][0]["content"][0]["text"], "keep this instruction");
                if mode!=3 { assert_eq!(body["vendor_extension"], true); }
                if count == 0 {
                    assert_eq!(body["input"][0]["role"], "system");
                    let problem = if mode == 3 {
                        json!({"detail":"System messages are not allowed"})
                    } else if mode == 2 {
                        json!({"error":{"message":"Provider returned 400: Bad Request | Upstream: {\"detail\":\"System messages are not allowed\"}"}})
                    } else if mode == 1 {
                        json!({"detail":[{"type":"literal_error","loc":["body","input",0,"role"],"msg":"Input should be 'user', 'assistant' or 'developer'","input":"system"}]})
                    } else {
                        json!({"error":{"type":"invalid_request_error","code":"unsupported_value","param":"input[0].role","message":"Unsupported value: 'system'. Supported values are: 'user', 'assistant', 'developer'."}})
                    };
                    (if mode==1 {StatusCode::UNPROCESSABLE_ENTITY} else {StatusCode::BAD_REQUEST}, axum::Json(problem)).into_response()
                } else {
                    assert_eq!(count, 1, "repair must be bounded");
                    assert_eq!(body["input"][0]["role"], "developer");
                    if mode < 2 {
                        assert!(parts.headers.get("digest").is_none());
                        assert!(parts.headers.get("content-md5").is_none());
                    }
                    axum::Json(json!({"id":"accepted","status":"completed","output":[]})).into_response()
                }
            }
        }))).await;
        let (_dir, state) = state(&[base]).await;
        let value = json!({"model":"m","instructions":"top-level instructions","input":[{"role":"system","content":[{"type":"input_text","text":"keep this instruction"}]},{"role":"user","content":"hello"}],"vendor_extension":true});
        let native = mode < 2;
        let key = if native {
            "fixture".to_owned()
        } else {
            management::dispatch(&state, "chatbot.key", json!({}))
                .await
                .unwrap()["key"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let mut value = value;
        let path = if mode == 1 {
            "/v1/responses/compact"
        } else if mode == 3 {
            value.as_object_mut().unwrap().remove("input");
            value["messages"] = json!([{"role":"system","content":"keep this instruction"},{"role":"user","content":"hello"}]);
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        };
        let mut req = request(path, &key, &value.to_string());
        if native {
            req.headers_mut().insert(
                "content-length",
                value.to_string().len().to_string().parse().unwrap(),
            );
            req.headers_mut()
                .insert("digest", "old-body-digest".parse().unwrap());
            req.headers_mut()
                .insert("content-md5", "old-md5".parse().unwrap());
        } else {
            let port = management::dispatch(&state, "chatbot.status", json!({}))
                .await
                .unwrap()["port"]
                .as_u64()
                .unwrap();
            req.headers_mut()
                .insert("host", format!("127.0.0.1:{port}").parse().unwrap());
        }
        let app = if native {
            relay::router(state.clone())
        } else {
            chatbot::router(state.clone())
        };
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["x-codexpress-adapted-parameters"],
            "input.role"
        );
        to_bytes(response.into_body(), 10000).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        task.abort();
    }
}

#[tokio::test]
async fn repaired_roles_stay_source_local_and_other_sources_still_receive_original_bytes() {
    let original = r#"{ "model":"m", "input":[{"role":"system","content":"policy"},{"role":"user","content":"hello"}], "unknown": 123 }"#;
    let calls = Arc::new(AtomicUsize::new(0));
    let copy = calls.clone();
    let (first, task_a) = server::serve(Router::new().route("/responses", post(move |req: Request| {
        let copy = copy.clone(); async move {
            let body: Value = serde_json::from_slice(&to_bytes(req.into_body(),10000).await.unwrap()).unwrap();
            let count = copy.fetch_add(1,Ordering::SeqCst);
            if count==0 {
                assert_eq!(body["input"][0]["role"],"system");
                (StatusCode::BAD_REQUEST,axum::Json(json!({"error":{"code":"unsupported_value","param":"input[0].role","message":"Unsupported value: 'system'"}}))).into_response()
            } else {
                assert_eq!(count,1); assert_eq!(body["input"][0]["role"],"developer");
                StatusCode::TOO_MANY_REQUESTS.into_response()
            }
        }
    }))).await;
    let (second, task_b) = server::serve(Router::new().route(
        "/responses",
        post(move |req: Request| async move {
            assert_eq!(
                &to_bytes(req.into_body(), 10000).await.unwrap()[..],
                original.as_bytes()
            );
            axum::Json(json!({"status":"completed","output":[]}))
        }),
    ))
    .await;
    let (_dir, state) = state(&[first, second]).await;
    let response = relay::router(state)
        .oneshot(request("/v1/responses", "fixture", original))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    task_a.abort();
    task_b.abort();
}

#[tokio::test]
async fn arbitrary_errors_and_output_streams_never_authorize_role_replay() {
    for case in 0..4 {
        let calls = Arc::new(AtomicUsize::new(0));
        let copy = calls.clone();
        let (base,task)=server::serve(Router::new().route("/responses",post(move || {
            let copy=copy.clone();async move {
                copy.fetch_add(1,Ordering::SeqCst);
                match case {
                    0=>(StatusCode::BAD_REQUEST,axum::Json(json!({"error":{"code":"invalid_value","param":"input[0].content","message":"System messages are not allowed"}}))).into_response(),
                    1=>(StatusCode::BAD_REQUEST,axum::Json(json!({"error":{"message":"generic request failure"}}))).into_response(),
                    2=>([("content-type","text/event-stream")],"data: {\"type\":\"response.output_text.delta\",\"delta\":\"started\"}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"unsupported_value\",\"param\":\"input[0].role\",\"message\":\"Unsupported value: 'system'\"}}}\n\n").into_response(),
                    _=>axum::Json(json!({"status":"completed","output":[]})).into_response(),
                }
            }
        }))).await;
        let (_dir, state) = state(&[base]).await;
        let response = relay::router(state)
            .oneshot(request(
                "/v1/responses",
                "fixture",
                r#"{"model":"m","input":[{"role":"system","content":"policy"}]}"#,
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if case < 2 {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::OK
            }
        );
        to_bytes(response.into_body(), 10000).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        task.abort();
    }
}

#[tokio::test]
async fn learned_role_requirements_survive_restart_and_follow_source_identity() {
    let calls = Arc::new(AtomicUsize::new(0));
    let copy = calls.clone();
    let (base, task) = server::serve(Router::new().route(
        "/responses",
        post(move |req: Request| {
            let copy = copy.clone();
            async move {
                let count = copy.fetch_add(1, Ordering::SeqCst);
                let body: Value =
                    serde_json::from_slice(&to_bytes(req.into_body(), 10000).await.unwrap())
                        .unwrap();
                assert_eq!(body["input"][0]["content"], "keep this instruction");
                if matches!(count, 0 | 4) {
                    assert_eq!(body["input"][0]["role"], "system");
                    (
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({"detail":"System messages are not allowed"})),
                    )
                        .into_response()
                } else {
                    assert_eq!(body["input"][0]["role"], "developer");
                    axum::Json(json!({"status":"completed","output":[]})).into_response()
                }
            }
        }),
    ))
    .await;
    let (directory, original) = state(&[base]).await;
    let body = r#"{"model":"m","input":[{"role":"system","content":"keep this instruction"},{"role":"user","content":"hello"}]}"#;
    async fn send(state: Arc<AppState>, body: &str) {
        let response = relay::router(state)
            .oneshot(request("/v1/responses", "fixture", body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["x-codexpress-adapted-parameters"],
            "input.role"
        );
        to_bytes(response.into_body(), 10000).await.unwrap();
    }
    send(original.clone(), body).await;
    send(original.clone(), body).await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let cache =
        std::fs::read_to_string(directory.path().join("source-compatibility.json")).unwrap();
    assert!(!cache.contains("fixture-upstream"));
    assert!(!cache.contains("keep this instruction"));
    let runtime = original.sources.read().await[0].source.clone();
    let mut saved = serde_json::to_value(&runtime).unwrap();
    saved["secret"] = runtime.secret;
    drop(original);
    let restarted = AppState::new(directory.path().into(), 17863, "fixture".into()).unwrap();
    management::dispatch(&restarted, "sync", json!({"sources":[saved.clone()]}))
        .await
        .unwrap();
    send(restarted.clone(), body).await;
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    saved["secret"] = json!({"api_key":"fixture-new-credential"});
    management::dispatch(&restarted, "sync", json!({"sources":[saved]}))
        .await
        .unwrap();
    send(restarted, body).await;
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    task.abort();
}
