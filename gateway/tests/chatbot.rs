#[path = "common/server.rs"]
mod server;
#[path = "common/source.rs"]
mod source_fixture;

use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::StatusCode,
    response::Response,
    routing::post,
    Router,
};
use codexpress_gateway::{
    chatbot, management,
    model::{AppState, SourceRuntime},
};
use serde_json::{json, Value};
use server::serve;
use source_fixture::api_source;
use std::sync::Arc;
use tower::ServiceExt;

async fn state(base: &str) -> (tempfile::TempDir, Arc<AppState>, String, u16) {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().into(), 17863, "PRIVATE-NATIVE-KEY".into()).unwrap();
    state.settings.write().await.strict_priority = false;
    let mut source = api_source("api", "API", base, "PRIVATE-UPSTREAM-KEY");
    source.models = vec!["m".into(), "m2".into()];
    state.sources.write().await.push(SourceRuntime::new(source));
    let key = management::dispatch(&state, "chatbot.key", json!({}))
        .await
        .unwrap()["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let port = management::dispatch(&state, "chatbot.status", json!({}))
        .await
        .unwrap()["port"]
        .as_u64()
        .unwrap() as u16;
    (dir, state, key, port)
}
fn request(port: u16, key: &str, path: &str, body: Option<Value>) -> Request {
    Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("host", format!("127.0.0.1:{port}"))
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(
            body.map(|v| Body::from(v.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap()
}
fn complete() -> Value {
    json!({"id":"resp_fixture","object":"response","status":"completed","model":"m","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"你好"}]}],"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12,"input_tokens_details":{"cached_tokens":4},"output_tokens_details":{"reasoning_tokens":1}}})
}
fn sse_events(events: Vec<Value>) -> Response {
    let bytes = events
        .iter()
        .map(|e| {
            format!(
                "event: {}\r\ndata: {}\r\n\r\n",
                e["type"].as_str().unwrap(),
                e
            )
        })
        .collect::<String>()
        .into_bytes();
    // Deliberately split UTF-8, CRLF and JSON across every possible boundary.
    let chunks = futures_util::stream::iter(
        bytes
            .into_iter()
            .map(|b| Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b]))),
    );
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(chunks))
        .unwrap()
}

#[tokio::test]
async fn nonstreaming_chat_converts_upstream_sse_and_keeps_usage_and_native_bytes_isolated() {
    let (base,task)=serve(Router::new().route("/responses",post(|request:Request|async move{
        assert_eq!(request.headers()["authorization"],"Bearer PRIVATE-UPSTREAM-KEY");
        let body:Value=serde_json::from_slice(&to_bytes(request.into_body(),8192).await.unwrap()).unwrap();
        assert_eq!(body["stream"],true);assert_eq!(body["input"][0]["role"],"system");assert_eq!(body["max_output_tokens"],20);assert!(body.get("messages").is_none());
        sse_events(vec![json!({"type":"response.created","response":{"id":"resp_fixture","model":"m"}}),json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"你好"}),json!({"type":"response.completed","response":complete()})])
    }))).await;
    let (_dir, state, key, port) = state(&base).await;
    let response=chatbot::router(state.clone()).oneshot(request(port,&key,"/v1/chat/completions",Some(json!({"model":"m","messages":[{"role":"system","content":"policy"},{"role":"user","content":"hello"}],"max_tokens":20,"stream":false})))).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    let value: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(value["choices"][0]["message"]["content"], "你好");
    assert_eq!(value["usage"]["prompt_tokens_details"]["cached_tokens"], 4);
    assert_eq!(value["usage"]["completion_tokens"], 2);
    let records = state.usage.summary(1).unwrap();
    assert_eq!(records["recent"][0]["output_tokens"], 2);
    assert!(!value.to_string().contains("PRIVATE"));
    task.abort();
}

#[tokio::test]
async fn streaming_chat_converts_parallel_tool_arguments_without_duplicate_snapshots() {
    let (base,task)=serve(Router::new().route("/responses",post(||async{
        let call=json!({"type":"function_call","id":"fc1","call_id":"call1","name":"f","arguments":"{\"x\":1}"});
        sse_events(vec![json!({"type":"response.created","response":{"id":"resp_tools","model":"m"}}),json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc1","call_id":"call1","name":"f","arguments":""}}),json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"x\":"}),json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"1}"}),json!({"type":"response.output_item.done","output_index":0,"item":call}),json!({"type":"response.completed","response":{"id":"resp_tools","model":"m","status":"completed","output":[call],"usage":{"input_tokens":4,"output_tokens":3}}})])
    }))).await;
    let (_dir, state, key, port) = state(&base).await;
    let response=chatbot::router(state).oneshot(request(port,&key,"/v1/chat/completions",Some(json!({"model":"m","messages":[{"role":"user","content":"call f"}],"stream":true,"stream_options":{"include_usage":true}})))).await.unwrap();
    assert_eq!(response.status(), 200);
    let body = String::from_utf8(
        to_bytes(response.into_body(), 16384)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let events: Vec<Value> = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect();
    let arguments = events
        .iter()
        .filter_map(|e| e["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str())
        .collect::<String>();
    assert_eq!(arguments, "{\"x\":1}");
    assert!(events
        .iter()
        .any(|e| e["choices"][0]["finish_reason"] == "tool_calls"));
    assert!(events.iter().any(|e| e["usage"]["completion_tokens"] == 3));
    assert!(body.ends_with("data: [DONE]\n\n"));
    task.abort();
}

#[tokio::test]
async fn response_clients_accept_hybrid_fields_and_receive_buffered_response_objects() {
    let (base, task) = serve(Router::new().route(
        "/responses",
        post(|request: Request| async {
            let body: Value =
                serde_json::from_slice(&to_bytes(request.into_body(), 4096).await.unwrap())
                    .unwrap();
            assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
            assert_eq!(body["tools"][0]["name"], "f");
            assert_eq!(body["vendor_extension"], true);
            assert_eq!(body["max_output_tokens"], 16);
            sse_events(vec![
                json!({"type":"response.completed","response":complete()}),
            ])
        }),
    ))
    .await;
    let (_dir, state, key, port) = state(&base).await;
    let response=chatbot::router(state).oneshot(request(port,&key,"/v1/responses",Some(json!({"model":"m","input":[{"role":"user","content":"hello"}],"max_tokens":1,"vendor_extension":true,"tools":[{"type":"function","function":{"name":"f"}}],"stream":false})))).await.unwrap();
    assert_eq!(response.status(), 200);
    let value: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(value["object"], "response");
    assert_eq!(value["output"][0]["content"][0]["text"], "你好");
    task.abort();
}

#[tokio::test]
async fn models_authentication_browser_access_rotation_and_settings_are_independent() {
    let (_dir, state, key, port) = state("http://127.0.0.1:1").await;
    let router = chatbot::router(state.clone());
    let inverse = Request::builder()
        .uri("/v1/models")
        .header("host", "127.0.0.1:17863")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        codexpress_gateway::relay::router(state.clone())
            .oneshot(inverse)
            .await
            .unwrap()
            .status(),
        401
    );
    let preflight = Request::builder()
        .method("OPTIONS")
        .uri("/v1/chat/completions")
        .header("host", format!("127.0.0.1:{port}"))
        .header("origin", "https://chat.example")
        .header("access-control-request-method", "POST")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router.clone().oneshot(preflight).await.unwrap().status(),
        204
    );
    assert_eq!(
        router
            .clone()
            .oneshot(request(port, "PRIVATE-NATIVE-KEY", "/v1/models", None))
            .await
            .unwrap()
            .status(),
        401
    );
    let mut query = request(port, &key, "/v1/models", None);
    query
        .headers_mut()
        .insert("origin", HeaderValue::from_static("https://chat.example"));
    let response = router.clone().oneshot(query).await.unwrap();
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    let result: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(result["data"].as_array().unwrap().len(), 2);
    state.settings.write().await.mode = "fixed".into();
    state.settings.write().await.fixed_source_id = Some("missing".into());
    let response = router
        .oneshot(request(port, &key, "/v1/models", None))
        .await
        .unwrap();
    let empty: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(empty["data"], json!([]));
    let available = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let new_port = available.local_addr().unwrap().port();
    drop(available);
    management::dispatch(
        &state,
        "chatbot.configure",
        json!({"port":new_port,"key":"fixture-custom-client-key"}),
    )
    .await
    .unwrap();
    let status = management::dispatch(&state, "chatbot.status", json!({}))
        .await
        .unwrap();
    assert_eq!(status["running"], true);
    assert!(!status.to_string().contains("fixture-custom-client-key"));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://127.0.0.1:{new_port}/v1/models");
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(&key)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("fixture-custom-client-key")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(
        management::dispatch(&state, "chatbot.configure", json!({"port":17863}))
            .await
            .is_err()
    );
    management::dispatch(&state, "chatbot.configure", json!({"enabled":false}))
        .await
        .unwrap();
}

#[tokio::test]
async fn only_explicit_parameter_rejections_allow_bounded_compatibility_retries() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let copy = calls.clone();
    let (base,task)=serve(Router::new().route("/responses",post(move |request:Request|{
        let copy=copy.clone();async move{
            copy.fetch_add(1,Ordering::SeqCst);
            let body:Value=serde_json::from_slice(&to_bytes(request.into_body(),8192).await.unwrap()).unwrap();
            if body["input"][0]["content"][0]["text"]=="reject" {
                return (StatusCode::BAD_REQUEST,axum::Json(json!({"error":{"type":"invalid_request_error","message":"PRIVATE-UNKNOWN-ERROR"}}))).into_response();
            }
            for field in ["max_output_tokens","temperature","metadata"] {
                if body.get(field).is_some(){return (StatusCode::BAD_REQUEST,axum::Json(json!({"error":{"code":"unsupported_parameter","param":field}}))).into_response();}
            }
            assert_eq!(body["instructions"],"");
            sse_events(vec![json!({"type":"response.completed","response":complete()})])
        }
    }))).await;
    let (_dir, state, key, port) = state(&base).await;
    let response=chatbot::router(state.clone()).oneshot(request(port,&key,"/v1/chat/completions",Some(json!({"model":"m","messages":[{"role":"user","content":"hi"}],"max_tokens":20,"temperature":0.5,"metadata":{"PRIVATE":"never exposed"}})))).await.unwrap();
    assert_eq!(response.status(), 200);
    let adjusted = response.headers()["x-codexpress-adapted-parameters"]
        .to_str()
        .unwrap();
    assert!(adjusted.contains("max_output_tokens"));
    assert!(adjusted.contains("temperature"));
    assert!(adjusted.contains("metadata"));
    assert!(!adjusted.contains("PRIVATE"));
    let _ = to_bytes(response.into_body(), 8192).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    let response = chatbot::router(state)
        .oneshot(request(
            port,
            &key,
            "/v1/chat/completions",
            Some(json!({"model":"m","messages":[{"role":"user","content":"reject"}]})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE"));
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    task.abort();
}
use axum::http::HeaderValue;
use axum::response::IntoResponse;

#[tokio::test]
async fn unfinished_streams_do_not_claim_success_and_unsupported_requests_do_not_generate() {
    let (base,task)=serve(Router::new().route("/responses",post(||async{sse_events(vec![json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"partial"})])}))).await;
    let (_dir, state, key, port) = state(&base).await;
    let response = chatbot::router(state.clone())
        .oneshot(request(
            port,
            &key,
            "/v1/chat/completions",
            Some(json!({"model":"m","messages":[{"role":"user","content":"hi"}],"stream":true})),
        ))
        .await
        .unwrap();
    let output =
        String::from_utf8(to_bytes(response.into_body(), 8192).await.unwrap().to_vec()).unwrap();
    assert!(output.contains("incomplete_upstream_stream"));
    assert!(!output.contains("\"finish_reason\":\"stop\""));
    let before = state.usage.summary(1).unwrap()["recent"]
        .as_array()
        .unwrap()
        .len();
    let response = chatbot::router(state.clone())
        .oneshot(request(
            port,
            &key,
            "/v1/chat/completions",
            Some(json!({"model":"m","messages":[{"role":"user","content":"hi"}],"n":2})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        state.usage.summary(1).unwrap()["recent"]
            .as_array()
            .unwrap()
            .len(),
        before
    );
    task.abort();
}

#[tokio::test]
async fn status_is_authenticated_exact_thread_cache_only_and_tracks_actual_attempts() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let inference_calls = Arc::new(AtomicUsize::new(0));
    let balance_calls = Arc::new(AtomicUsize::new(0));
    let (base, task) = serve(Router::new()
        .route("/responses", post({
            let started = started.clone();
            let release = release.clone();
            let calls = inference_calls.clone();
            move || {
                let started = started.clone();
                let release = release.clone();
                let calls = calls.clone();
                async move {
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    started.notify_one();
                    release.notified().await;
                    if call == 0 {
                        sse_events(vec![json!({"type":"response.completed","response":complete()})])
                    } else {
                        Response::builder().status(400).header("content-type", "application/json")
                            .body(Body::from(json!({"error":{"message":"fixture rejection"}}).to_string())).unwrap()
                    }
                }
            }
        }))
        .route("/api/v1/me/quota", axum::routing::get({
            let calls = balance_calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { axum::Json(json!({"data":{"userLimitTotalUsd":100,"userCurrentTotalUsd":12.5}})) }
            }
        }))).await;
    let (_dir, state, key, port) = state(&base).await;
    let app = chatbot::router(state.clone());
    let status_path = "/v1/codexpress/status?model=m&session_id=thread-a";
    let response = app
        .clone()
        .oneshot(request(port, "invalid-key", status_path, None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let poll = |path: &'static str| {
        let app = app.clone();
        let key = key.clone();
        async move {
            let response = app.oneshot(request(port, &key, path, None)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["cache-control"], "no-store");
            serde_json::from_slice::<Value>(&to_bytes(response.into_body(), 8192).await.unwrap())
                .unwrap()
        }
    };
    assert_eq!(poll(status_path).await, json!({"source":null}));
    codexpress_gateway::balance::query(&state, json!({"source_id":"api"}))
        .await
        .unwrap();
    assert_eq!(balance_calls.load(Ordering::SeqCst), 1);
    assert_eq!(poll(status_path).await, json!({"source":null}));
    let launch = || {
        let app = app.clone();
        let mut req = request(
            port,
            &key,
            "/v1/chat/completions",
            Some(
                json!({"model":"m","messages":[{"role":"user","content":"PRIVATE-STATUS-PROMPT"}]}),
            ),
        );
        req.headers_mut()
            .insert("thread-id", "thread-a".parse().unwrap());
        req.headers_mut().insert(
            "x-private-fixture",
            "PRIVATE-STATUS-HEADER".parse().unwrap(),
        );
        // Thread wins over the broader session header.
        req.headers_mut()
            .insert("session-id", "different-session".parse().unwrap());
        tokio::spawn(async move {
            let response = app.oneshot(req).await.unwrap();
            let status = response.status();
            let _ = to_bytes(response.into_body(), 8192).await.unwrap();
            status
        })
    };
    let inference = launch();
    started.notified().await;
    let expected = json!({"source":{"id":"api","name":"API","kind":"api","balance":{"amount":87.5,"currency":"USD"}}});
    let actual = poll(status_path).await;
    assert_eq!(actual, expected);
    for secret in [
        "PRIVATE-STATUS-PROMPT",
        "PRIVATE-STATUS-HEADER",
        "PRIVATE-UPSTREAM-KEY",
        "PRIVATE-NATIVE-KEY",
        key.as_str(),
    ] {
        assert!(!actual.to_string().contains(secret));
    }
    for path in [
        "/v1/codexpress/status?model=m2&session_id=thread-a",
        "/v1/codexpress/status?model=m&session_id=thread-b",
        "/v1/codexpress/status?model=m&session_id=Thread-a",
        "/v1/codexpress/status?model=m&session_id=different-session",
    ] {
        assert_eq!(poll(path).await, json!({"source":null}));
    }
    release.notify_one();
    assert_eq!(inference.await.unwrap(), StatusCode::OK);
    assert_eq!(poll(status_path).await, expected);
    assert_eq!(balance_calls.load(Ordering::SeqCst), 1);
    assert_eq!(inference_calls.load(Ordering::SeqCst), 1);
    let removed = state.sources.write().await.pop().unwrap();
    assert_eq!(poll(status_path).await, json!({"source":null}));
    state.sources.write().await.push(removed);
    let original = state.sources.read().await[0].source.clone();
    state.sources.write().await[0].source.secret["api_key"] = json!("changed-private-fixture");
    assert_eq!(poll(status_path).await, json!({"source":null}));
    state.sources.write().await[0].source = original;
    let rejected = launch();
    started.notified().await;
    release.notify_one();
    assert_eq!(rejected.await.unwrap(), StatusCode::BAD_REQUEST);
    assert_eq!(poll(status_path).await, json!({"source":null}));
    state.sources.write().await.clear();
    assert_eq!(poll(status_path).await, json!({"source":null}));
    assert_eq!(balance_calls.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn localhost_refresh_events_are_authenticated_scoped_and_join_overlap() {
    use futures_util::StreamExt;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let started = Arc::new(tokio::sync::Notify::new());
    let (upstream, upstream_task) = serve(Router::new()
        .route("/responses", post(|| async { sse_events(vec![json!({"type":"response.completed","response":complete()})]) }))
        .route("/api/v1/me/quota", axum::routing::get({
            let calls = calls.clone(); let release = release.clone(); let started = started.clone();
            move || { let calls = calls.clone(); let release = release.clone(); let started = started.clone(); async move {
                calls.fetch_add(1, Ordering::SeqCst); started.notify_one(); release.notified().await;
                axum::Json(json!({"data":{"userLimitTotalUsd":100,"userCurrentTotalUsd":12.5}}))
            }}
        }))).await;
    let (_dir, state, key, _) = state(&upstream).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    management::dispatch(
        &state,
        "chatbot.configure",
        json!({"enabled":true,"port":port}),
    )
    .await
    .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://127.0.0.1:{port}");
    for path in [
        "/v1/codexpress/events?model=m&session_id=a",
        "/v1/codexpress/refresh",
    ] {
        let response = if path.contains("events") {
            client.get(format!("{base}{path}")).send().await.unwrap()
        } else {
            client
                .post(format!("{base}{path}"))
                .json(&json!({"model":"m","session_id":"a"}))
                .send()
                .await
                .unwrap()
        };
        assert_eq!(response.status(), 401);
    }
    let response = client
        .get(format!("{base}/v1/codexpress/events?model=m&session_id=a"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut events = response.bytes_stream();
    let initial = events.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&initial).contains("\"source\":null"));
    let other_response = client
        .get(format!("{base}/v1/codexpress/events?model=m&session_id=b"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    let mut other_events = other_response.bytes_stream();
    assert!(
        String::from_utf8_lossy(&other_events.next().await.unwrap().unwrap())
            .contains("\"source\":null")
    );
    let refresh = |session: &'static str| {
        let client = client.clone();
        let base = base.clone();
        let key = key.clone();
        async move {
            client
                .post(format!("{base}/v1/codexpress/refresh"))
                .bearer_auth(key)
                .json(&json!({"model":"m","session_id":session}))
                .send()
                .await
                .unwrap()
        }
    };
    assert_eq!(
        refresh("a").await.json::<Value>().await.unwrap(),
        json!({"accepted":false})
    );
    let generated = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(&key)
        .header("thread-id", "a")
        .json(&json!({"model":"m","messages":[{"role":"user","content":"fixture"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(generated.status(), 200);
    generated.bytes().await.unwrap();
    let other_generated = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(&key)
        .header("thread-id", "b")
        .json(&json!({"model":"m","messages":[{"role":"user","content":"fixture"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(other_generated.status(), 200);
    other_generated.bytes().await.unwrap();
    assert_eq!(
        refresh("other").await.json::<Value>().await.unwrap(),
        json!({"accepted":false})
    );
    assert_eq!(refresh("a").await.status(), 202);
    started.notified().await;
    assert_eq!(
        refresh("a").await.json::<Value>().await.unwrap(),
        json!({"accepted":true})
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.notify_one();
    let update = tokio::time::timeout(Duration::from_secs(3), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let update = String::from_utf8_lossy(&update);
    assert!(update.contains("event: status"));
    assert!(update.contains("87.5"));
    let other_update = tokio::time::timeout(Duration::from_secs(3), other_events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&other_update).contains("87.5"));
    management::dispatch(&state, "chatbot.configure", json!({"regenerate_key":true}))
        .await
        .unwrap();
    let cleared = tokio::time::timeout(Duration::from_secs(3), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&cleared).contains("\"source\":null"));
    assert!(tokio::time::timeout(Duration::from_secs(3), events.next())
        .await
        .unwrap()
        .is_none());
    management::dispatch(&state, "chatbot.configure", json!({"enabled":false}))
        .await
        .unwrap();
    upstream_task.abort();
}

#[tokio::test]
async fn local_plugin_session_is_consumed_preserves_cache_key_and_defers_to_explicit_thread() {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (base, task) = serve(
        Router::new()
            .route(
                "/responses",
                post({
                    let captured = captured.clone();
                    move |request: Request| {
                        let captured = captured.clone();
                        async move {
                            let thread =
                                request.headers()["thread-id"].to_str().unwrap().to_owned();
                            let payload: Value = serde_json::from_slice(
                                &to_bytes(request.into_body(), 8192).await.unwrap(),
                            )
                            .unwrap();
                            captured.lock().unwrap().push((thread, payload));
                            sse_events(vec![
                                json!({"type":"response.completed","response":complete()}),
                            ])
                        }
                    }
                }),
            )
            .route(
                "/api/v1/me/quota",
                axum::routing::get(|| async {
                    axum::Json(json!({"data":{"userLimitTotalUsd":100,"userCurrentTotalUsd":12.5}}))
                }),
            ),
    )
    .await;
    let (_dir, state, key, port) = state(&base).await;
    codexpress_gateway::balance::query(&state, json!({"source_id":"api"}))
        .await
        .unwrap();
    let app = chatbot::router(state.clone());
    for (path, input, local_id, explicit) in [
        (
            "/v1/chat/completions",
            json!({"messages":[{"role":"user","content":"fixture"}]}),
            "plugin-chat",
            None,
        ),
        (
            "/v1/responses",
            json!({"input":"fixture"}),
            "plugin-responses",
            None,
        ),
        (
            "/v1/responses",
            json!({"input":"fixture"}),
            "ignored-local",
            Some("explicit-thread"),
        ),
    ] {
        let mut payload = input;
        payload["model"] = json!("m");
        payload["prompt_cache_key"] = json!("original-private-cache-key");
        payload["codexpress_session_id"] = json!(local_id);
        let mut req = request(port, &key, path, Some(payload));
        if let Some(thread) = explicit {
            req.headers_mut()
                .insert("thread-id", thread.parse().unwrap());
        }
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), 8192).await.unwrap();
        let session = explicit.unwrap_or(local_id);
        let status_path = format!("/v1/codexpress/status?model=m&session_id={session}");
        let response = app
            .clone()
            .oneshot(request(port, &key, &status_path, None))
            .await
            .unwrap();
        let status: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(status["source"]["id"], "api");
        assert!(!status.to_string().contains("original-private-cache-key"));
    }
    {
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 3);
        for ((thread, payload), expected) in
            captured
                .iter()
                .zip(["plugin-chat", "plugin-responses", "explicit-thread"])
        {
            assert_eq!(thread, expected);
            assert!(payload.get("codexpress_session_id").is_none());
            assert_eq!(payload["prompt_cache_key"], "original-private-cache-key");
        }
    }
    let response = app
        .clone()
        .oneshot(request(
            port,
            &key,
            "/v1/codexpress/status?model=m&session_id=ignored-local",
            None,
        ))
        .await
        .unwrap();
    let status: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(status, json!({"source":null}));
    for invalid in [
        Value::Null,
        json!(42),
        json!(""),
        json!("x".repeat(513)),
        json!("PRIVATE-INVALID-SESSION\n"),
    ] {
        let response = app
            .clone()
            .oneshot(request(
                port,
                &key,
                "/v1/responses",
                Some(json!({"model":"m","input":"fixture","codexpress_session_id":invalid})),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(error["error"]["param"], "codexpress_session_id");
        assert!(!error.to_string().contains("PRIVATE-INVALID-SESSION"));
    }
    assert_eq!(captured.lock().unwrap().len(), 3);
    task.abort();
}
