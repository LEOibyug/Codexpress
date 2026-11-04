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
async fn localhost_native_compaction_preserves_json_and_keeps_generation_and_secrets_separate() {
    const COMPACT: &str = "{\n  \"id\": \"cmp_fixture\", \"object\": \"response.compaction\", \"output\": [{\"type\":\"compaction\",\"encrypted_content\":\"opaque+/=\\u003d\"}], \"vendor_extension\": true\n}";
    const NATIVE: &str = "{ \"model\":\"m\", \"instructions\":\"native policy\", \"input\":[{\"type\":\"reasoning\",\"encrypted_content\":\"opaque+/=\"}], \"prompt_cache_key\":\"native-cache\", \"max_output_tokens\":1 }";
    const V2_NATIVE: &str = r#"{"model":"m","input":[{"type":"reasoning","encrypted_content":"opaque+/="},{"type":"compaction_trigger"}],"stream":true,"store":false,"prompt_cache_key":"v2-cache","include":["reasoning.encrypted_content"]}"#;
    const V2_STREAM: &str = "event: response.output_item.done\r\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"native-v2+/=\"}}\r\n\r\nevent: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_v2\",\"status\":\"completed\",\"output\":[{\"type\":\"compaction\",\"encrypted_content\":\"native-v2+/=\"}],\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\r\n\r\n";
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (upstream, upstream_task) = serve(
        Router::new()
            .route(
                "/responses/compact",
                post({
                    let calls = calls.clone();
                    move |request: Request| {
                        let calls = calls.clone();
                        async move {
                            assert_eq!(
                                request.headers()["authorization"],
                                "Bearer PRIVATE-UPSTREAM-KEY"
                            );
                            assert_eq!(request.headers()["accept"], "application/json");
                            if calls.load(std::sync::atomic::Ordering::SeqCst) < 2 {
                                assert_eq!(request.headers()["thread-id"], "compact-thread");
                                assert_eq!(request.headers()["x-session-id"], "compact-session");
                            } else {
                                assert!(!request.headers().contains_key("thread-id"));
                                assert!(!request.headers().contains_key("x-session-id"));
                            }
                            for header in [
                                "cookie",
                                "origin",
                                "chatgpt-account-id",
                                "x-private-fixture",
                            ] {
                                assert!(!request.headers().contains_key(header));
                            }
                            let bytes = to_bytes(request.into_body(), 8192).await.unwrap();
                            let body: Value = serde_json::from_slice(&bytes).unwrap();
                            assert_eq!(body, serde_json::from_str::<Value>(NATIVE).unwrap());
                            assert!(!body.to_string().contains("PRIVATE"));
                            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                                assert_eq!(bytes.as_ref(), NATIVE.as_bytes());
                            }
                            Response::builder()
                                .header("content-type", "application/json")
                                .header("set-cookie", "PRIVATE-UPSTREAM-COOKIE")
                                .header("authorization", "Bearer PRIVATE-UPSTREAM-KEY")
                                .body(Body::from(COMPACT))
                                .unwrap()
                        }
                    }
                }),
            )
            .route(
                "/responses",
                post(|request: Request| async move {
                    assert_eq!(
                        request.headers()["authorization"],
                        "Bearer PRIVATE-UPSTREAM-KEY"
                    );
                    for header in ["cookie", "origin", "x-private-fixture"] {
                        assert!(!request.headers().contains_key(header));
                    }
                    let native_headers = request
                        .headers()
                        .get("openai-beta")
                        .is_some_and(|value| value == "responses=experimental")
                        && request
                            .headers()
                            .get("x-codex-beta-features")
                            .is_some_and(|value| value == "remote_compaction_v2")
                        && request
                            .headers()
                            .get("x-openai-internal-codex-responses-lite")
                            .is_some_and(|value| value == "true");
                    let body: Value =
                        serde_json::from_slice(&to_bytes(request.into_body(), 8192).await.unwrap())
                            .unwrap();
                    assert_eq!(body["stream"], true);
                    assert!(!body.to_string().contains("PRIVATE"));
                    if body["input"][0]["content"][0]["text"] == "seed native history" {
                        let mut response = complete();
                        response["id"] = json!("resp_native_origin");
                        response["output"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!({"type":"reasoning","encrypted_content":"opaque+/="}));
                        return sse_events(vec![
                            json!({"type":"response.completed","response":response}),
                        ]);
                    }
                    if body["input"][1]["type"] == "compaction_trigger" {
                        assert!(
                            native_headers,
                            "native compaction protocol headers must reach the source"
                        );
                        let expected: Value = serde_json::from_str(V2_NATIVE).unwrap();
                        assert_eq!(body, expected);
                        return Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(Body::from(V2_STREAM))
                            .unwrap();
                    }
                    assert_eq!(body["input"][0]["role"], "user");
                    assert!(body.get("messages").is_none());
                    sse_events(vec![
                        json!({"type":"response.completed","response":complete()}),
                    ])
                }),
            ),
    )
    .await;
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
    for path in ["/v1/responses/compact", "/responses/compact"] {
        for secret in ["PRIVATE-NATIVE-KEY", "PRIVATE-UPSTREAM-KEY"] {
            let response = client
                .post(format!("{base}{path}"))
                .bearer_auth(secret)
                .body(NATIVE)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = client
            .post(format!("{base}{path}"))
            .bearer_auth(&key)
            .header("host", "untrusted.invalid")
            .body(NATIVE)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    }
    let invalid = client
        .post(format!("{base}/v1/responses/compact"))
        .bearer_auth(&key)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        invalid.json::<Value>().await.unwrap()["error"]["param"],
        "model"
    );
    // Opaque history must first be observed from this source, not invented by
    // the client: unknown ciphertext correctly takes the portable rebuild path.
    let response = client
        .post(format!("{base}/v1/responses"))
        .bearer_auth(&key)
        .header("x-session-id", "compact-session")
        .header("thread-id", "compact-thread")
        .json(
            &json!({"model":"m","input":"seed native history","stream":false,
            "include":["reasoning.encrypted_content"]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let history: Value = response.json().await.unwrap();
    assert_eq!(
        history["output"][1],
        serde_json::from_str::<Value>(NATIVE).unwrap()["input"][0]
    );
    // A newly preferred source must not receive another source's compact state.
    let preferred_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let copy = preferred_calls.clone();
    let (preferred, preferred_task) = serve(
        Router::new()
            .route(
                "/responses/compact",
                post(|| async {
                    axum::Json(
                        json!({"id":"wrong-origin","object":"response.compaction","output":[]}),
                    )
                }),
            )
            .route(
                "/responses",
                post(move || {
                    let copy = copy.clone();
                    async move {
                        copy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        sse_events(vec![
                            json!({"type":"response.completed","response":complete()}),
                        ])
                    }
                }),
            ),
    )
    .await;
    {
        let mut sources = state.sources.write().await;
        sources[0].source.priority = 1;
        sources.push(SourceRuntime::new(api_source(
            "preferred",
            "Preferred",
            &preferred,
            "PRIVATE-PREFERRED-KEY",
        )));
    }
    state.settings.write().await.strict_priority = true;
    for path in ["/v1/responses/compact", "/responses/compact"] {
        let request = client
            .post(format!("{base}{path}"))
            .bearer_auth(&key)
            .header("content-type", "application/json")
            .header("x-session-id", "compact-session")
            .header("cookie", "PRIVATE-CLIENT-COOKIE")
            .header("origin", "https://untrusted.invalid")
            .header("chatgpt-account-id", "PRIVATE-CLIENT-ACCOUNT")
            .header("x-private-fixture", "PRIVATE-CLIENT-HEADER")
            .header("thread-id", "compact-thread")
            .body(NATIVE);
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "application/json");
        assert!(!response.headers().contains_key("set-cookie"));
        assert!(!response.headers().contains_key("authorization"));
        assert_eq!(response.bytes().await.unwrap().as_ref(), COMPACT.as_bytes());
    }
    // Without a thread hint, the encrypted item itself still proves the origin.
    let response = client
        .post(format!("{base}/v1/responses/compact"))
        .bearer_auth(&key)
        .body(NATIVE)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), COMPACT.as_bytes());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    // V2 uses the existing Responses stream, not a distinct compact endpoint.
    for path in ["/v1/responses", "/responses"] {
        let response = client
            .post(format!("{base}{path}"))
            .bearer_auth(&key)
            .header("content-type", "application/json")
            .header("openai-beta", "responses=experimental")
            .header("x-codex-beta-features", "remote_compaction_v2")
            .header("x-openai-internal-codex-responses-lite", "true")
            .header("cookie", "PRIVATE-CLIENT-COOKIE")
            .header("origin", "https://untrusted.invalid")
            .header("chatgpt-account-id", "PRIVATE-CLIENT-ACCOUNT")
            .header("x-private-fixture", "PRIVATE-CLIENT-HEADER")
            .body(V2_NATIVE)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.bytes().await.unwrap().as_ref(),
            V2_STREAM.as_bytes()
        );
    }
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(&key)
        .json(&json!({"model":"m","messages":[{"role":"user","content":"fixture"}],"stream":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "你好");
    assert_eq!(preferred_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    management::dispatch(&state, "chatbot.configure", json!({"enabled":false}))
        .await
        .unwrap();
    upstream_task.abort();
    preferred_task.abort();
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
    // LAN Host names are valid only with the listener's explicit port, and never
    // replace the separate client credential with native/admin authentication.
    for (host, credential, expected) in [
        (format!("192.0.2.44:{port}"), key.as_str(), 200),
        (format!("mac.vpn.example:{port}"), key.as_str(), 200),
        (format!("mac.vpn.example:{}", port + 1), key.as_str(), 421),
        ("mac.vpn.example".into(), key.as_str(), 421),
        (format!("user@mac.vpn.example:{port}"), key.as_str(), 421),
        (format!("192.0.2.44:{port}"), "PRIVATE-NATIVE-KEY", 401),
        (format!("192.0.2.44:{port}"), "", 401),
    ] {
        let mut probe = request(port, credential, "/v1/remote-check", None);
        probe.headers_mut().insert("host", host.parse().unwrap());
        let response = router.clone().oneshot(probe).await.unwrap();
        assert_eq!(response.status().as_u16(), expected);
        if expected == 200 {
            let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
            assert_eq!(body, json!({"service":"codexpress","mode":"chatbot","version":1}));
        }
    }
    let mut duplicate = request(port, &key, "/v1/remote-check", None);
    duplicate.headers_mut().append("host", format!("mac.vpn.example:{port}").parse().unwrap());
    assert_eq!(router.clone().oneshot(duplicate).await.unwrap().status(), 421);
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
