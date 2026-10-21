use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::StatusCode,
    response::Response,
    routing::post,
    Router,
};
use codexpress_gateway::{
    model::{AppState, ProxyPolicy, Source, SourceRuntime},
    relay, routing, transport,
};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::Mutex};
use tower::ServiceExt;

async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (address, task)
}
fn source(id: &str, base: &str, priority: i32) -> Source {
    Source {
        id: id.into(),
        name: format!("Source {id}"),
        kind: "api".into(),
        is_subscription: false,
        base_url: base.into(),
        enabled: true,
        priority,
        proxy: ProxyPolicy {
            mode: "direct".into(),
            url: None,
        },
        models: vec![],
        account_id: None,
        secret: json!({"api_key":format!("upstream-{id}")}),
    }
}
fn request(body: &[u8]) -> Request {
    Request::builder()
        .method("POST")
        .uri("/v1/responses?test=1")
        .header("host", "127.0.0.1:0")
        .header("authorization", "Bearer local-key")
        .header("content-type", "application/json")
        .body(Body::from(body.to_vec()))
        .unwrap()
}
async fn state(sources: Vec<Source>) -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::new(directory.path().to_owned(), 0, "local-key".into()).unwrap();
    *state.sources.write().await = sources.into_iter().map(SourceRuntime::new).collect();
    // Strict-mode safety fixtures; degraded rebuilding is exercised separately.
    state.settings.write().await.degraded_context_fallback = false;
    (directory, state)
}

#[tokio::test]
async fn raw_json_headers_query_and_usage_survive_relay() {
    let seen = Arc::new(Mutex::new(None));
    let copy = seen.clone();
    let response_body = br#"{ "id":"resp-one", "model":"gpt-6.1-sol", "service_tier":"priority", "usage":{"input_tokens":100,"output_tokens":9,"total_tokens":109,"input_tokens_details":{"cached_tokens":40},"output_tokens_details":{"reasoning_tokens":5}}, "unknown":true }"#;
    let (address, task) = serve(Router::new().route(
        "/v1/responses",
        post(move |request: Request| {
            let copy = copy.clone();
            async move {
                let (parts, body) = request.into_parts();
                *copy.lock().await = Some((
                    parts.uri.to_string(),
                    parts.headers,
                    to_bytes(body, 10000).await.unwrap(),
                ));
                Response::builder()
                    .header("x-trace", "native")
                    .header("content-type", "application/json")
                    .body(Body::from(response_body.as_slice()))
                    .unwrap()
            }
        }),
    ))
    .await;
    let mut subscription = source("a", &format!("{address}/v1"), 0);
    subscription.is_subscription = true;
    let (_directory, state) = state(vec![subscription]).await;
    let body = b"{  \"model\":\"gpt-test\",\"input\":[{\"type\":\"message\",\"content\":\"PRIVATE-PROMPT\"}],\"unknown\":true }";
    let mut req = request(body);
    req.headers_mut()
        .insert("chatgpt-account-id", "wrong-account".parse().unwrap());
    req.headers_mut()
        .insert("openai-organization", "wrong-org".parse().unwrap());
    req.headers_mut()
        .insert("cookie", "secret-cookie".parse().unwrap());
    req.headers_mut()
        .insert("openai-beta", "responses=experimental".parse().unwrap());
    req.headers_mut()
        .insert("connection", "x-private".parse().unwrap());
    req.headers_mut()
        .insert("x-private", "must-strip".parse().unwrap());
    let response = relay::router(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(response.headers()["x-trace"], "native");
    assert_eq!(
        &to_bytes(response.into_body(), 10000).await.unwrap()[..],
        response_body
    );
    let seen = seen.lock().await;
    let (uri, headers, received) = seen.as_ref().unwrap();
    assert_eq!(uri, "/v1/responses?test=1");
    assert_eq!(&received[..], body);
    assert_eq!(headers["authorization"], "Bearer upstream-a");
    assert_eq!(headers["openai-beta"], "responses=experimental");
    for key in [
        "chatgpt-account-id",
        "openai-organization",
        "cookie",
        "x-private",
    ] {
        assert!(!headers.contains_key(key));
    }
    let usage = state.usage.summary(30).unwrap();
    assert_eq!(usage["rows"][0]["input_tokens"], 100);
    assert_eq!(usage["rows"][0]["cached_input_tokens"], 40);
    assert_eq!(usage["recent"][0]["complete"], true);
    assert_eq!(usage["recent"][0]["model"], "gpt-test");
    assert_eq!(usage["recent"][0]["cost"]["price_model"], "gpt-6.1-sol");
    assert_eq!(usage["recent"][0]["cost"]["service_tier"], "priority");
    assert_eq!(
        usage["recent"][0]["cost"]["billing_basis"],
        "subscription_reference"
    );
    assert_eq!(usage["recent"][0]["token_details"]["is_subscription"], true);
    state.sources.write().await[0].source.is_subscription = false;
    assert_eq!(
        state.usage.summary(1).unwrap()["recent"][0]["cost"],
        usage["recent"][0]["cost"]
    );
    assert!((usage["recent"][0]["cost"]["total_usd"].as_f64().unwrap() - 0.000428).abs() < 1e-12);
    assert_eq!(
        usage["recent"][0]["token_details"]["reasoning_output_tokens"],
        5
    );
    assert_eq!(
        usage["recent"][0]["token_details"]["reported_total_tokens"],
        109
    );
    assert_eq!(
        usage["recent"][0]["token_details"]["input_estimated"],
        false
    );
    assert_eq!(state.bindings.lock().await.get("resp-one").unwrap(), "a");
    for entry in std::fs::read_dir(_directory.path()).unwrap() {
        let stored = std::fs::read(entry.unwrap().path()).unwrap();
        for secret in ["PRIVATE-PROMPT", "upstream-a", "local-key"] {
            assert!(!stored.windows(secret.len()).any(|w| w == secret.as_bytes()));
        }
    }
    task.abort();
}

#[tokio::test]
async fn live_usage_estimates_stream_then_replaces_with_reported_tokens_without_double_counting() {
    for reported in [false, true] {
        let connected = Arc::new(tokio::sync::Notify::new());
        let headers_gate = Arc::new(tokio::sync::Notify::new());
        let finish_gate = Arc::new(tokio::sync::Notify::new());
        let connected_copy = connected.clone();
        let headers_copy = headers_gate.clone();
        let finish_copy = finish_gate.clone();
        let (origin, task) = serve(Router::new().route("/responses", post(move || {
            let connected = connected_copy.clone();
            let headers_gate = headers_copy.clone();
            let finish_gate = finish_copy.clone();
            async move {
                connected.notify_one();
                headers_gate.notified().await;
                let stream = async_stream::stream! {
                    yield Ok::<_,std::io::Error>(bytes::Bytes::from_static("data: {\"type\":\"response.output_text.delta\",\"delta\":\"你好abcd\"}\n\n".as_bytes()));
                    finish_gate.notified().await;
                    let mut response = json!({"id":"usage-final","output":[{"type":"message","content":[{"type":"output_text","text":"你好abcd"}]}]});
                    if reported { response["usage"] = json!({"input_tokens":40,"output_tokens":20,"total_tokens":60,"input_tokens_details":{"cached_tokens":10,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":8}}); }
                    let event = json!({"type":"response.completed","response":response});
                    yield Ok(bytes::Bytes::from(format!("data: {event}\n\n")));
                };
                Response::builder().header("content-type","text/event-stream").body(Body::from_stream(stream)).unwrap()
            }
        }))).await;
        let (_dir, state) = state(vec![source("a", &origin, 0)]).await;
        let router = relay::router(state.clone());
        let request_task = tokio::spawn(async move {
            router
                .oneshot(request(
                    "{\"model\":\"fixture\",\"input\":\"你好abcd\"}".as_bytes(),
                ))
                .await
                .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(2), connected.notified())
            .await
            .unwrap();
        let connecting = state.usage.summary(1).unwrap();
        assert_eq!(connecting["recent"][0]["status"], "connecting");
        assert_eq!(connecting["recent"][0]["input_tokens"], 3);
        assert_eq!(connecting["rows"][0]["requests"], 1);
        assert_eq!(connecting["rows"][0]["failures"], 0);
        let attempt_id = connecting["recent"][0]["attempt_id"].clone();
        headers_gate.notify_one();
        let response = request_task.await.unwrap();
        let drain =
            tokio::spawn(async move { to_bytes(response.into_body(), 10000).await.unwrap() });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let live = state.usage.summary(1).unwrap();
                if live["recent"][0]["output_tokens"] == 3 {
                    assert_eq!(live["recent"][0]["active"], true);
                    assert_eq!(live["recent"][0]["token_details"]["output_estimated"], true);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        finish_gate.notify_one();
        let wire = drain.await.unwrap();
        assert!(wire
            .windows("你好abcd".len())
            .any(|v| v == "你好abcd".as_bytes()));
        let final_usage = state.usage.summary(1).unwrap();
        let recent = &final_usage["recent"][0];
        assert_eq!(recent["attempt_id"], attempt_id);
        assert_eq!(recent["active"], false);
        assert_eq!(recent["status"], "success");
        assert_eq!(final_usage["rows"][0]["requests"], 1);
        assert_eq!(final_usage["inflight"], 0);
        assert_eq!(recent["input_tokens"], if reported { 40 } else { 3 });
        assert_eq!(recent["output_tokens"], if reported { 20 } else { 3 });
        assert_eq!(recent["token_details"]["input_estimated"], !reported);
        assert_eq!(recent["token_details"]["output_estimated"], !reported);
        assert_eq!(
            recent["token_details"]["cache_write_input_tokens"],
            if reported { json!(2) } else { Value::Null }
        );
        assert_eq!(
            recent["token_details"]["reasoning_output_tokens"],
            if reported { json!(8) } else { Value::Null }
        );
        let duration = recent["generation_ms"].as_f64().unwrap();
        assert!(duration >= 20.0);
        let expected_tps = recent["output_tokens"].as_f64().unwrap() * 1000.0
            / recent["latency_ms"].as_f64().unwrap();
        assert!((recent["output_tps"].as_f64().unwrap() - expected_tps).abs() < 0.001);
        assert_eq!(
            final_usage["rows"][0]["timed_output_tokens"],
            recent["output_tokens"]
        );
        task.abort();
    }
}

#[tokio::test]
async fn partial_usage_keeps_reported_zero_and_estimates_only_missing_input() {
    let (origin, task) = serve(Router::new().route("/responses",post(|| async {
        axum::Json(json!({"id":"zero-output","output":[],"usage":{"output_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}))
    }))).await;
    let (_dir, state) = state(vec![source("a", &origin, 0)]).await;
    let response = relay::router(state.clone())
        .oneshot(request(br#"{"model":"m","input":"abcdefgh"}"#))
        .await
        .unwrap();
    to_bytes(response.into_body(), 10000).await.unwrap();
    let summary = state.usage.summary(1).unwrap();
    let row = &summary["recent"][0];
    assert_eq!(row["input_tokens"], 2);
    assert_eq!(row["output_tokens"], 0);
    assert_eq!(row["token_details"]["input_estimated"], true);
    assert_eq!(row["token_details"]["output_estimated"], false);
    assert_eq!(row["token_details"]["reasoning_output_tokens"], 0);
    assert_eq!(row["token_details"]["cached_input_reported"], true);
    assert!(row["generation_tps"].is_null());
    task.abort();
}

#[tokio::test]
async fn fallback_is_ordered_fixed_mode_and_previous_id_never_cross_sources() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_copy = calls.clone();
    let (first, a) = serve(Router::new().route(
        "/responses",
        post(|| async {
            (
                StatusCode::TOO_MANY_REQUESTS,
                axum::Json(json!({"error":{"code":"rate_limit_exceeded"}})),
            )
        }),
    ))
    .await;
    let (second, b) = serve(Router::new().route(
        "/responses",
        post(move || {
            let calls = calls_copy.clone();
            async move {
                calls.fetch_add(1, Ordering::Relaxed);
                axum::Json(json!({"id":"resp-b","usage":{"input_tokens":5,"output_tokens":2}}))
            }
        }),
    ))
    .await;
    let (_directory, state) = state(vec![source("a", &first, 0), source("b", &second, 1)]).await;
    let app = relay::router(state.clone());
    let body = br#"{"model":"m","input":"test"}"#;
    let response = app.clone().oneshot(request(body)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let summary = state.usage.summary(30).unwrap();
    assert_eq!(summary["recent"].as_array().unwrap().len(), 2);
    assert!(summary["recent"]
        .as_array()
        .unwrap()
        .iter()
        .any(|record| record["status"] == "failed"));
    assert_eq!(
        summary["recent"][0]["request_id"],
        summary["recent"][1]["request_id"]
    );
    assert_eq!(state.sources.read().await[0].health, "cooldown");
    state
        .bindings
        .lock()
        .await
        .insert("resp-a".into(), "a".into());
    let sticky = app
        .clone()
        .oneshot(request(br#"{"model":"m","previous_response_id":"resp-a"}"#))
        .await
        .unwrap();
    assert_eq!(sticky.status(), StatusCode::CONFLICT);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    {
        let mut settings = state.settings.write().await;
        settings.mode = "fixed".into();
        settings.fixed_source_id = Some("a".into());
    }
    assert_eq!(
        app.clone().oneshot(request(body)).await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    {
        let mut sources = state.sources.write().await;
        sources[0].health = "ready".into();
    }
    let fixed = app.oneshot(request(body)).await.unwrap();
    assert_eq!(fixed.status(), StatusCode::TOO_MANY_REQUESTS);
    to_bytes(fixed.into_body(), 10000).await.unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    a.abort();
    b.abort();
}

#[tokio::test]
async fn connection_pool_reuses_transport_without_crossing_source_or_proxy_policy() {
    use axum::extract::ConnectInfo;
    let peers = Arc::new(Mutex::new(Vec::new()));
    let copy = peers.clone();
    let router = Router::new().fallback(
        move |ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>| {
            let copy = copy.clone();
            async move {
                copy.lock().await.push(peer);
                "direct"
            }
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let origin_task = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let mut target = source("pool-a", &origin, 0);
    for _ in 0..2 {
        assert_eq!(
            transport::client(&target)
                .unwrap()
                .get(&origin)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "direct"
        );
    }
    let mut other = target.clone();
    other.id = "pool-b".into();
    transport::client(&other)
        .unwrap()
        .get(&origin)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    {
        let peers = peers.lock().await;
        assert_eq!(peers.len(), 3);
        assert_eq!(peers[0], peers[1]);
        assert_ne!(peers[1], peers[2]);
    }
    let (proxy, proxy_task) = serve(Router::new().fallback(|| async { "proxied" })).await;
    target.proxy = ProxyPolicy {
        mode: "explicit".into(),
        url: Some(proxy),
    };
    assert_eq!(
        transport::client(&target)
            .unwrap()
            .get(&origin)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "proxied"
    );
    assert_eq!(peers.lock().await.len(), 3);
    target.kind = "official".into();
    target.proxy.mode = "direct".into();
    target.proxy.url = None;
    assert!(transport::client(&target).is_err());
    proxy_task.abort();
    origin_task.abort();
}

#[tokio::test]
async fn explicit_proxy_is_used_and_unavailable_proxy_never_bypasses() {
    let direct_hits = Arc::new(AtomicUsize::new(0));
    let copy = direct_hits.clone();
    let (origin, origin_task) = serve(Router::new().fallback(move || {
        let copy = copy.clone();
        async move {
            copy.fetch_add(1, Ordering::Relaxed);
            "origin"
        }
    }))
    .await;
    let seen = Arc::new(Mutex::new(String::new()));
    let copy = seen.clone();
    let (proxy, proxy_task) = serve(Router::new().fallback(move |request: Request| {
        let copy = copy.clone();
        async move {
            *copy.lock().await = request.uri().to_string();
            "proxy"
        }
    }))
    .await;
    let mut target = source("a", &origin, 0);
    target.proxy = ProxyPolicy {
        mode: "explicit".into(),
        url: Some(proxy),
    };
    let response = transport::client(&target)
        .unwrap()
        .get(format!("{origin}/responses"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "proxy");
    assert!(seen.lock().await.starts_with("http://127.0.0.1:"));
    assert_eq!(direct_hits.load(Ordering::Relaxed), 0);
    proxy_task.abort();
    let unavailable = TcpListener::bind("127.0.0.1:0").await.unwrap();
    target.proxy.url = Some(format!("http://{}", unavailable.local_addr().unwrap()));
    drop(unavailable);
    assert!(transport::client(&target)
        .unwrap()
        .get(format!("{origin}/responses"))
        .send()
        .await
        .is_err());
    assert_eq!(direct_hits.load(Ordering::Relaxed), 0);
    target.proxy.mode = "direct".into();
    target.proxy.url = None;
    assert_eq!(
        transport::client(&target)
            .unwrap()
            .get(format!("{origin}/responses"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "origin"
    );
    origin_task.abort();
}

#[tokio::test]
async fn sse_completed_before_client_close_keeps_success_usage_and_cancels_transport() {
    use futures_util::StreamExt;
    let cancelled = Arc::new(AtomicUsize::new(0));
    struct Cancel(Arc<AtomicUsize>);
    impl Drop for Cancel {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let copy = cancelled.clone();
    let payload="event: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-stream\",\"usage\":{\"input_tokens\":30,\"output_tokens\":4,\"input_tokens_details\":{\"cached_tokens\":12}}}}\r\n\r\n";
    let (origin,task)=serve(Router::new().route("/responses",post(move || {let cancelled=copy.clone(); async move {
        let guard=Cancel(cancelled);
        let stream=async_stream::stream! { let _guard=guard; for byte in payload.as_bytes() {yield Ok::<_,std::io::Error>(bytes::Bytes::copy_from_slice(&[*byte]));} tokio::time::sleep(Duration::from_secs(60)).await; };
        Response::builder().header("content-type","text/event-stream").body(Body::from_stream(stream)).unwrap()
    }}))).await;
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = AppState::new(dir.path().to_owned(), address.port(), "local-key".into()).unwrap();
    state
        .sources
        .write()
        .await
        .push(SourceRuntime::new(source("a", &origin, 0)));
    let gateway = format!("http://{address}");
    let app = relay::router(state.clone());
    let gateway_task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/responses"))
        .bearer_auth("local-key")
        .json(&json!({"model":"m"}))
        .send()
        .await
        .unwrap();
    let mut stream = response.bytes_stream();
    let mut received = Vec::new();
    while received.len() < payload.len() {
        received.extend_from_slice(&stream.next().await.unwrap().unwrap());
    }
    assert_eq!(received, payload.as_bytes());
    assert_eq!(state.bindings.lock().await.get("resp-stream").unwrap(), "a");
    drop(stream);
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.inflight.load(Ordering::Relaxed) > 0 || cancelled.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let summary = state.usage.summary(30).unwrap();
    assert_eq!(summary["recent"][0]["status"], "success");
    assert_eq!(summary["rows"][0]["successes"], 1);
    assert_eq!(summary["rows"][0]["failures"], 0);
    assert_eq!(summary["rows"][0]["input_tokens"], 30);
    assert_eq!(summary["rows"][0]["cached_input_tokens"], 12);
    task.abort();
    gateway_task.abort();
}

#[tokio::test]
async fn terminal_sse_before_body_drop_retains_outcome_but_unfinished_output_is_interrupted() {
    use futures_util::StreamExt;
    for (wire, outcome, terminal) in [
        ("data: {\"type\":\"response.completed\",\"response\":{\"id\":\"complete-without-usage\",\"output\":[]}}\n\n", "success", true),
        ("data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"insufficient_quota\"}}}\n\n", "failed", true),
        ("data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n", "success", true),
        ("data: {\"type\":\"response.output_text.delta\",\"delta\":\"not finished\"}\n\n", "interrupted", false),
        // An unterminated SSE event is not evidence of completion.
        ("data: {\"type\":\"response.completed\",\"response\":{\"id\":\"truncated-frame\"}}\n", "interrupted", false),
    ] {
        let (origin, task) = serve(Router::new().route("/responses", post(move || async move {
            let stream = async_stream::stream! {
                yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(wire.as_bytes()));
                futures_util::future::pending::<()>().await;
            };
            Response::builder().header("content-type", "text/event-stream").body(Body::from_stream(stream)).unwrap()
        }))).await;
        let (_directory, state) = state(vec![source("a", &origin, 0)]).await;
        let response = relay::router(state.clone()).oneshot(request(br#"{"model":"m","stream":true}"#)).await.unwrap();
        let mut body = response.into_body().into_data_stream();
        let mut received = Vec::new();
        while received.len() < wire.len() {
            received.extend_from_slice(&body.next().await.unwrap().unwrap());
        }
        assert_eq!(received, wire.as_bytes());
        if terminal {
            assert_eq!(state.usage.summary(1).unwrap()["recent"][0]["status"], outcome, "terminal state must be visible before yielding the last event");
        }
        drop(body); // Client stops polling immediately after the last event.
        assert_eq!(state.inflight.load(Ordering::Relaxed), 0);
        let summary = state.usage.summary(1).unwrap();
        assert_eq!(summary["recent"][0]["status"], outcome);
        assert_eq!(summary["recent"][0]["token_details"]["stream_complete"], terminal);
        assert_eq!(summary["recent"][0]["complete"], false, "missing usage must remain independent of completion");
        if outcome == "failed" { assert_eq!(state.sources.read().await[0].health, "exhausted"); }
        task.abort();
    }
}

#[tokio::test]
async fn transport_error_after_terminal_event_does_not_replace_success_or_failure() {
    use futures_util::StreamExt;
    for (event, outcome) in [
        ("response.completed", "success"),
        ("response.failed", "failed"),
    ] {
        let wire = format!("data: {{\"type\":\"{event}\",\"response\":{{\"id\":\"terminal-then-reset\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":2}}}}}}\n\n");
        let upstream_wire = wire.clone();
        let (origin, task) = serve(Router::new().route(
            "/responses",
            post(move || {
                let wire = upstream_wire.clone();
                async move {
                    let stream = async_stream::stream! {
                        yield Ok::<_, std::io::Error>(bytes::Bytes::from(wire));
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        yield Ok(bytes::Bytes::from_static(b": trailing keep-alive\n\n"));
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        yield Err(std::io::Error::other("fixture transport reset after terminal"));
                    };
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(stream))
                        .unwrap()
                }
            }),
        ))
        .await;
        let (_directory, state) = state(vec![source("a", &origin, 0)]).await;
        let response = relay::router(state.clone())
            .oneshot(request(br#"{"model":"m","stream":true}"#))
            .await
            .unwrap();
        let mut body = response.into_body().into_data_stream();
        let mut received = Vec::new();
        while received.len() < wire.len() {
            received.extend_from_slice(&body.next().await.unwrap().unwrap());
        }
        assert_eq!(received, wire.as_bytes());
        assert_eq!(
            &body.next().await.unwrap().unwrap()[..],
            b": trailing keep-alive\n\n",
            "normal bytes after completion must still be forwarded"
        );
        assert!(
            body.next().await.unwrap().is_err(),
            "transport errors must still reach the client"
        );
        drop(body);
        let summary = state.usage.summary(1).unwrap();
        assert_eq!(summary["recent"][0]["status"], outcome);
        assert_eq!(
            summary["recent"][0]["token_details"]["stream_complete"],
            true
        );
        assert_eq!(summary["recent"][0]["input_tokens"], 10);
        assert_eq!(summary["recent"][0]["output_tokens"], 2);
        assert_eq!(summary["rows"][0]["requests"], 1);
        task.abort();
    }
}

#[tokio::test]
async fn server_failure_is_not_replayed_and_model_filter_is_strict() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let (first, a) = serve(Router::new().route(
        "/responses",
        post(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
    ))
    .await;
    let (second, b) = serve(Router::new().route(
        "/responses",
        post(move || {
            let copy = copy.clone();
            async move {
                copy.fetch_add(1, Ordering::Relaxed);
                "wrong"
            }
        }),
    ))
    .await;
    let (_dir, state) = state(vec![source("a", &first, 0), source("b", &second, 1)]).await;
    let response = relay::router(state.clone())
        .oneshot(request(br#"{"model":"m"}"#))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    state.sources.write().await[1].source.models = vec!["other".into()];
    assert!(routing::candidates(&state, "m", None).await.is_empty());
    a.abort();
    b.abort();
}

#[tokio::test]
async fn authenticated_requests_reject_foreign_hosts_and_browser_origins() {
    let (_dir, state) = state(vec![]).await;
    let app = relay::router(state);
    let mut foreign = request(br#"{"model":"m"}"#);
    foreign
        .headers_mut()
        .insert("host", "attacker.invalid:0".parse().unwrap());
    assert_eq!(
        app.clone().oneshot(foreign).await.unwrap().status(),
        StatusCode::MISDIRECTED_REQUEST
    );
    let mut wrong_port = request(br#"{"model":"m"}"#);
    wrong_port
        .headers_mut()
        .insert("host", "localhost:99".parse().unwrap());
    assert_eq!(
        app.clone().oneshot(wrong_port).await.unwrap().status(),
        StatusCode::MISDIRECTED_REQUEST
    );
    let mut browser = request(br#"{"model":"m"}"#);
    browser
        .headers_mut()
        .insert("origin", "https://attacker.invalid".parse().unwrap());
    assert_eq!(
        app.oneshot(browser).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn official_compaction_uses_required_proxy_without_direct_fallback() {
    let seen = Arc::new(AtomicUsize::new(0));
    let copy = seen.clone();
    let (proxy, task) = serve(Router::new().fallback(move |request: Request| {
        let copy = copy.clone();
        async move {
            copy.fetch_add(1, Ordering::Relaxed);
            assert_eq!(request.method(), "CONNECT");
            assert_eq!(request.uri().to_string(), "chatgpt.com:443");
            Response::builder().status(502).body(Body::empty()).unwrap()
        }
    }))
    .await;
    let mut official = source("official", "https://chatgpt.com/backend-api/codex", 0);
    official.kind = "official".into();
    official.account_id = Some("account-test".into());
    official.proxy = ProxyPolicy {
        mode: "explicit".into(),
        url: Some(proxy),
    };
    official.secret = json!({"access_token":"official-token","expires_at":4000000000_u64,"account_id":"account-test"});
    let (_dir, state) = state(vec![official]).await;
    let mut compact = request(br#"{"model":"m","input":[]}"#);
    *compact.uri_mut() = "/v1/responses/compact".parse().unwrap();
    let response = relay::router(state).oneshot(compact).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(seen.load(Ordering::Relaxed), 1);
    task.abort();
}

#[tokio::test]
async fn explicit_invalid_ciphertext_can_rebuild_once_without_replaying_unknown_errors() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let (origin, task) = serve(Router::new().route(
        "/responses",
        post(move |req: Request| {
            let copy = copy.clone();
            async move {
                let body: Value =
                    serde_json::from_slice(&to_bytes(req.into_body(), 10000).await.unwrap())
                        .unwrap();
                let n = copy.fetch_add(1, Ordering::Relaxed);
                if n == 0 {
                    (
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({"error":{"code":"invalid_encrypted_content"}})),
                    )
                } else {
                    assert_eq!(body["input"].as_array().unwrap().len(), 1);
                    assert!(body.get("previous_response_id").is_none());
                    (
                        StatusCode::OK,
                        axum::Json(
                            json!({"id":"fresh","usage":{"input_tokens":5,"output_tokens":1}}),
                        ),
                    )
                }
            }
        }),
    ))
    .await;
    let (_dir, state) = state(vec![source("a", &origin, 0)]).await;
    state.settings.write().await.degraded_context_fallback = true;
    state
        .bindings
        .lock()
        .await
        .insert("known-old".into(), "a".into());
    let response=relay::router(state.clone()).oneshot(request(br#"{"model":"m","previous_response_id":"known-old","input":[{"type":"message","content":"visible"},{"type":"reasoning","encrypted_content":"invalid-cipher"}]}"#)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(hits.load(Ordering::Relaxed), 2);
    assert_eq!(state.notifications.list().len(), 1);
    task.abort();
}

#[tokio::test]
async fn scoped_origin_migrates_once_preserves_new_ciphertext_and_survives_recovery_and_restart() {
    let original_hits = Arc::new(AtomicUsize::new(0));
    let copy = original_hits.clone();
    let (a, a_task) = serve(Router::new().route(
        "/responses",
        post(move || {
            let copy = copy.clone();
            async move {
                copy.fetch_add(1, Ordering::Relaxed);
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(json!({"error":{"code":"rate_limit_exceeded"}})),
                )
            }
        }),
    ))
    .await;
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let (b,b_task)=serve(Router::new().route("/responses",post(move |req:Request|{let copy=copy.clone();async move{
        let body:Value=serde_json::from_slice(&to_bytes(req.into_body(),10000).await.unwrap()).unwrap();
        let n=copy.fetch_add(1,Ordering::Relaxed);
        assert!(!body.to_string().contains("old-A-cipher"));
        assert_eq!(body.to_string().contains("new-B-cipher"),n>0);
        axum::Json(json!({"id":"response-B","output":[{"type":"reasoning","encrypted_content":"new-B-cipher"}],"usage":{"input_tokens":100,"output_tokens":10}}))
    }}))).await;
    let originals = vec![source("a", &a, 0), source("b", &b, 1)];
    let (dir, state) = state(originals.clone()).await;
    state.settings.write().await.degraded_context_fallback = true;
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!("[model_providers.old]\nname=\"Old\"\nbase_url=\"{a}\"\nwire_api=\"responses\"\n"),
    )
    .unwrap();
    codexpress_gateway::config::handle(&state,"config.attach",json!({"codex_home":home,"provider_origins":[{"provider_id":"old","source_id":"a","source_identity":originals[0].identity(),"confirmed":true}]})).await.unwrap();
    let doc = std::fs::read_to_string(home.join("config.toml"))
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    let credential = doc["model_providers"]["old"]["experimental_bearer_token"]
        .as_str()
        .unwrap();
    let make = |n| {
        let mut body = json!({"model":"m","input":[{"type":"message","role":"user","content":"visible history"},{"type":"reasoning","encrypted_content":"old-A-cipher"}]});
        if n > 0 {
            body["input"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"reasoning","encrypted_content":"new-B-cipher"}));
            body["previous_response_id"] = json!("response-B");
        }
        let mut req = request(&serde_json::to_vec(&body).unwrap());
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {credential}").parse().unwrap(),
        );
        req.headers_mut()
            .insert("thread-id", "legacy-migrating-thread".parse().unwrap());
        req
    };
    for n in 0..2 {
        if n > 0 {
            let mut sources = state.sources.write().await;
            sources[0].health = "ready".into();
            sources[0].cooldown_until = None;
        }
        let response = relay::router(state.clone()).oneshot(make(n)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 10000).await.unwrap();
    }
    drop(state);
    let restored = AppState::new(dir.path().to_owned(), 0, "local-key".into()).unwrap();
    *restored.sources.write().await = originals.into_iter().map(SourceRuntime::new).collect();
    let response = relay::router(restored.clone())
        .oneshot(make(2))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    restored.bindings.lock().await.clear();
    let mut generic = make(3);
    generic
        .headers_mut()
        .insert("authorization", "Bearer local-key".parse().unwrap());
    let response = relay::router(restored).oneshot(generic).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(original_hits.load(Ordering::Relaxed), 1);
    assert_eq!(hits.load(Ordering::Relaxed), 4);
    a_task.abort();
    b_task.abort();
}

#[tokio::test]
async fn degraded_fallback_removes_only_wire_state_and_notifies_once_while_official_is_empty() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let copy = seen.clone();
    let (origin, task) = serve(Router::new().route(
        "/responses",
        post(move |req: Request| {
            let copy = copy.clone();
            async move {
                assert!(!req.headers().contains_key("x-codex-turn-state"));
                let body = to_bytes(req.into_body(), 10000).await.unwrap();
                let value: Value = serde_json::from_slice(&body).unwrap();
                assert!(value.get("previous_response_id").is_none());
                assert!(value.get("conversation").is_none());
                assert_eq!(value["input"].as_array().unwrap().len(), 2);
                assert_eq!(value["input"][0]["content"], "visible history");
                assert_eq!(value["input"][1]["output"], "tool output");
                assert_eq!(value["metadata"]["encrypted_content"], "unrelated-metadata");
                copy.lock().await.push(value);
                axum::Json(
                    json!({"id":"fresh-response","usage":{"input_tokens":30,"output_tokens":5}}),
                )
            }
        }),
    ))
    .await;
    let mut official = source("official", "https://chatgpt.com/backend-api/codex", 0);
    official.kind = "official".into();
    official.account_id = Some("fixture-account".into());
    let (_dir, state) = state(vec![official, source("api", &origin, 1)]).await;
    state.sources.write().await[0].quota =
        json!({"windows":[{"name":"secondary","used_percent":100}]});
    state.settings.write().await.degraded_context_fallback = true;
    let body=br#"{"model":"m","previous_response_id":"old-id","conversation":"old-conversation","input":[{"type":"message","role":"user","content":"visible history"},{"type":"reasoning","encrypted_content":"old-private-block","summary":[]},{"type":"compaction","encrypted_content":"old-compact-block"},{"type":"function_call_output","call_id":"call","output":"tool output"}],"metadata":{"encrypted_content":"unrelated-metadata"}}"#;
    for _ in 0..2 {
        let mut req = request(body);
        req.headers_mut()
            .insert("thread-id", "same-thread".parse().unwrap());
        req.headers_mut()
            .insert("x-codex-turn-state", "old-turn".parse().unwrap());
        let response = relay::router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 10000).await.unwrap();
    }
    assert_eq!(seen.lock().await.len(), 2);
    let events = codexpress_gateway::management::dispatch(&state, "notifications.list", json!({}))
        .await
        .unwrap();
    assert_eq!(events["events"].as_array().unwrap().len(), 1);
    for _ in 0..2 {
        codexpress_gateway::management::dispatch(&state, "status", json!({}))
            .await
            .unwrap();
    }
    let events = state.notifications.list();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1]["kind"], "official_exhausted");
    let ids: Vec<_> = events
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_owned())
        .collect();
    codexpress_gateway::management::dispatch(&state, "notifications.ack", json!({"ids":ids}))
        .await
        .unwrap();
    assert!(state.notifications.list().is_empty());
    state.sources.write().await[0].quota =
        json!({"windows":[{"name":"secondary","used_percent":10}]});
    codexpress_gateway::management::dispatch(&state, "status", json!({}))
        .await
        .unwrap();
    state.sources.write().await[0].quota =
        json!({"windows":[{"name":"secondary","used_percent":100}]});
    codexpress_gateway::management::dispatch(&state, "status", json!({}))
        .await
        .unwrap();
    assert_eq!(state.notifications.list().len(), 1);
    task.abort();
}

#[tokio::test]
async fn confirmed_legacy_origin_bootstraps_old_opaque_context_only_to_matching_identity() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let body = br#"{"model":"m","previous_response_id":"old-unknown-response","input":[{"type":"reasoning","encrypted_content":"old-private-state"},{"type":"function_call_output","call_id":"old-call","output":"unchanged"}]}"#;
    let (origin, task) = serve(Router::new().route("/responses", post(move |req: Request| {
        let copy = copy.clone();
        async move {
            assert_eq!(req.headers()["authorization"],"Bearer upstream-original");
            assert_eq!(to_bytes(req.into_body(),10000).await.unwrap().as_ref(),body);
            copy.fetch_add(1,Ordering::Relaxed);
            axum::Json(json!({"id":"accepted-old-context","usage":{"input_tokens":100,"output_tokens":20}}))
        }
    }))).await;
    let original = source("original", &origin, 1);
    let (_dir, state) = state(vec![
        source("priority-first", "http://127.0.0.1:1", 0),
        original.clone(),
    ])
    .await;
    let home = state.state_dir.join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(home.join("config.toml"),format!("model_provider=\"old\"\n[model_providers.old]\nname=\"Old\"\nbase_url=\"{origin}\"\nwire_api=\"responses\"\n")).unwrap();
    let payload = json!({"codex_home":home,"provider_origins":[{"provider_id":"old","source_id":"original","source_identity":original.identity(),"confirmed":true}]});
    codexpress_gateway::config::handle(&state, "config.attach", payload.clone())
        .await
        .unwrap();
    let doc = std::fs::read_to_string(home.join("config.toml"))
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    let credential = doc["model_providers"]["old"]["experimental_bearer_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let make_request = || {
        let mut req = request(body);
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {credential}").parse().unwrap(),
        );
        req
    };
    assert_eq!(
        relay::router(state.clone())
            .oneshot(request(body))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    for _ in 0..2 {
        state.bindings.lock().await.clear();
        let response = relay::router(state.clone())
            .oneshot(make_request())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 10000).await.unwrap();
    }
    assert_eq!(hits.load(Ordering::Relaxed), 2);
    let mut conflicting_turn = make_request();
    conflicting_turn
        .headers_mut()
        .insert("x-codex-turn-state", "known-other-turn".parse().unwrap());
    use sha2::Digest;
    let key = format!("turn:{:x}", sha2::Sha256::digest(b"known-other-turn"));
    state
        .bindings
        .lock()
        .await
        .insert(key, "priority-first".into());
    assert_eq!(
        relay::router(state.clone())
            .oneshot(conflicting_turn)
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    state.bindings.lock().await.clear();
    state
        .bindings
        .lock()
        .await
        .insert("old-unknown-response".into(), "priority-first".into());
    assert_eq!(
        relay::router(state.clone())
            .oneshot(make_request())
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    state.bindings.lock().await.clear();
    state.sources.write().await[1].source.secret = json!({"api_key":"changed-key"});
    assert_eq!(
        relay::router(state.clone())
            .oneshot(make_request())
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let invalid = codexpress_gateway::config::handle(&state, "config.preview", payload.clone())
        .await
        .unwrap();
    assert!(invalid["origin_error"].is_string());
    assert!(
        codexpress_gateway::config::handle(&state, "config.attach", payload.clone())
            .await
            .is_err()
    );
    codexpress_gateway::config::handle(&state, "config.restore.preview", payload.clone())
        .await
        .unwrap();
    codexpress_gateway::config::handle(&state, "config.restore", payload)
        .await
        .unwrap();
    assert_eq!(hits.load(Ordering::Relaxed), 2);
    task.abort();
}

#[tokio::test]
async fn official_sse_without_content_type_extracts_reported_details_without_rewriting_bytes() {
    let payload = concat!(
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"headless\",\"usage\":{\"input_tokens\":1234,\"output_tokens\":56,\"total_tokens\":1290,\"input_tokens_details\":{\"cached_tokens\":1000,\"cache_write_tokens\":20},\"output_tokens_details\":{\"reasoning_tokens\":12}}}}\n\n"
    );
    let (origin, task) = serve(Router::new().route(
        "/responses",
        post(move || async move { Response::builder().body(Body::from(payload)).unwrap() }),
    ))
    .await;
    let (_dir, state) = state(vec![source("a", &origin, 0)]).await;
    let response = relay::router(state.clone())
        .oneshot(request(br#"{"model":"m","stream":true,"input":"hello"}"#))
        .await
        .unwrap();
    assert!(!response.headers().contains_key("content-type"));
    assert_eq!(
        &to_bytes(response.into_body(), 10000).await.unwrap()[..],
        payload.as_bytes()
    );
    let usage = state.usage.summary(1).unwrap();
    let row = &usage["recent"][0];
    assert_eq!(row["input_tokens"], 1234);
    assert_eq!(row["output_tokens"], 56);
    assert_eq!(row["cached_input_tokens"], 1000);
    assert_eq!(row["token_details"]["cache_write_input_tokens"], 20);
    assert_eq!(row["token_details"]["reasoning_output_tokens"], 12);
    assert_eq!(row["token_details"]["reported_total_tokens"], 1290);
    assert_eq!(row["complete"], true);
    task.abort();
}

#[tokio::test]
async fn compact_and_models_keep_native_paths_and_compressed_bytes() {
    let compressed = vec![
        31, 139, 8, 0, 0, 0, 0, 0, 0, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    let copy = compressed.clone();
    let (origin, task) = serve(
        Router::new()
            .route(
                "/v1/responses/compact",
                post(move |request: Request| {
                    let copy = copy.clone();
                    async move {
                        assert_eq!(request.uri().query(), Some("test=1"));
                        Response::builder()
                            .header("content-encoding", "gzip")
                            .header("content-type", "application/json")
                            .body(Body::from(copy))
                            .unwrap()
                    }
                }),
            )
            .route(
                "/v1/models",
                axum::routing::get(|| async {
                    axum::Json(json!({"object":"list","data":[{"id":"native-model"}]}))
                }),
            ),
    )
    .await;
    let (_dir, state) = state(vec![source("a", &format!("{origin}/v1/responses"), 0)]).await;
    let app = relay::router(state.clone());
    let mut compact = request(br#"{"model":"m","input":[]}"#);
    *compact.uri_mut() = "/v1/responses/compact?test=1".parse().unwrap();
    let response = app.clone().oneshot(compact).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-encoding"], "gzip");
    assert_eq!(
        &to_bytes(response.into_body(), 10000).await.unwrap()[..],
        compressed
    );
    assert_eq!(
        state.usage.summary(1).unwrap()["recent"][0]["complete"],
        false
    );
    let models = Request::builder()
        .uri("/v1/models")
        .header("host", "localhost:0")
        .header("authorization", "Bearer local-key")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(models).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["data"][0]["id"],
        "native-model"
    );
    task.abort();
}

#[tokio::test]
async fn recognized_quota_and_unknown_payment_errors_have_distinct_health() {
    let (origin, task) = serve(Router::new().route(
        "/responses",
        post(|| async {
            (
                StatusCode::PAYMENT_REQUIRED,
                axum::Json(json!({"error":{"code":"insufficient_quota"}})),
            )
        }),
    ))
    .await;
    let (_dir, state) = state(vec![source("a", &origin, 0)]).await;
    let response = relay::router(state.clone())
        .oneshot(request(br#"{"model":"m"}"#))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &to_bytes(response.into_body(), 10000).await.unwrap()
        )
        .unwrap()["error"]["code"],
        "insufficient_quota"
    );
    assert_eq!(state.sources.read().await[0].health, "exhausted");
    routing::mark_failure(&state, "a", 402, None).await;
    assert_eq!(state.sources.read().await[0].health, "cooldown");
    task.abort();
}

#[tokio::test]
async fn response_stream_failure_never_replays_and_marks_quota() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let wire="event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"failed-response\",\"error\":{\"code\":\"insufficient_quota\"}}}\n\n";
    let (first, a) = serve(Router::new().route(
        "/responses",
        post(move || async move {
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from(wire))
                .unwrap()
        }),
    ))
    .await;
    let (second, b) = serve(Router::new().route(
        "/responses",
        post(move || {
            let copy = copy.clone();
            async move {
                copy.fetch_add(1, Ordering::Relaxed);
                "wrong"
            }
        }),
    ))
    .await;
    let (_dir, state) = state(vec![source("a", &first, 0), source("b", &second, 1)]).await;
    let response = relay::router(state.clone())
        .oneshot(request(br#"{"model":"m"}"#))
        .await
        .unwrap();
    assert_eq!(
        &to_bytes(response.into_body(), 10000).await.unwrap()[..],
        wire.as_bytes()
    );
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    assert_eq!(state.sources.read().await[0].health, "exhausted");
    let summary = state.usage.summary(1).unwrap();
    assert_eq!(summary["recent"][0]["status"], "failed");
    assert_eq!(summary["recent"][0]["complete"], false);
    a.abort();
    b.abort();
}

#[tokio::test]
async fn encrypted_reasoning_returns_to_origin_and_unknown_ciphertext_is_rejected() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let (first,a)=serve(Router::new().route("/responses",post(move ||{let copy=copy.clone();async move {copy.fetch_add(1,Ordering::Relaxed);axum::Json(json!({"id":"r-a","output":[{"type":"reasoning","encrypted_content":"opaque-from-a"}],"usage":{"input_tokens":1,"output_tokens":1}}))}}))).await;
    let (second, b) =
        serve(Router::new().route("/responses", post(|| async { StatusCode::IM_A_TEAPOT }))).await;
    let (_dir, state) = state(vec![source("a", &first, 0), source("b", &second, 1)]).await;
    let app = relay::router(state.clone());
    let response = app
        .clone()
        .oneshot(request(br#"{"model":"m"}"#))
        .await
        .unwrap();
    to_bytes(response.into_body(), 10000).await.unwrap();
    state.sources.write().await[1].source.priority = -1;
    let response = app
        .clone()
        .oneshot(request(
            br#"{"model":"m","input":[{"type":"reasoning","encrypted_content":"opaque-from-a"}]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(hits.load(Ordering::Relaxed), 2);
    let unknown = app
        .oneshot(request(
            br#"{"model":"m","input":[{"encrypted_content":"unknown"}]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::CONFLICT);
    a.abort();
    b.abort();
}

#[tokio::test]
async fn tool_continuation_keeps_long_http_and_stream_metadata_turn_state() {
    // Routing tokens are opaque upstream values, not short response IDs.
    let token = "fixture-routing-state-".repeat(100);
    let metadata_token = "fixture-metadata-state-".repeat(100);
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let response_token = token.clone();
    let stream_token = metadata_token.clone();
    let (first, a) = serve(Router::new().route(
        "/responses",
        post(move || {
            let hits = copy.clone();
            let token = response_token.clone();
            let metadata = json!({"type":"response.metadata", "headers":{"X-Codex-Turn-State":stream_token}});
            async move {
                hits.fetch_add(1, Ordering::Relaxed);
                let completed = json!({"type":"response.completed","response":{"id":"tool-response","output":[{"type":"function_call","call_id":"call-fixture","name":"local_tool","arguments":"{}"}],"usage":{"input_tokens":1,"output_tokens":1}}});
                let wire = format!("data: {metadata}\n\ndata: {completed}\n\n");
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .header("x-codex-turn-state", token)
                    .body(Body::from(wire))
                    .unwrap()
            }
        }),
    ))
    .await;
    let (second, b) =
        serve(Router::new().route("/responses", post(|| async { StatusCode::IM_A_TEAPOT }))).await;
    let (_dir, state) = state(vec![source("a", &first, 0), source("b", &second, 1)]).await;
    let app = relay::router(state.clone());
    let first = app
        .clone()
        .oneshot(request(br#"{"model":"m","input":"hello"}"#))
        .await
        .unwrap();
    assert_eq!(first.headers()["x-codex-turn-state"], token);
    let first_body = to_bytes(first.into_body(), 10000).await.unwrap();
    assert!(String::from_utf8_lossy(&first_body).contains(&metadata_token));
    state.sources.write().await[1].source.priority = -1;
    for turn_state in [&token, &metadata_token] {
        let mut next = request(br#"{"model":"m","input":[{"type":"function_call_output","call_id":"call-fixture","output":"ok"}]}"#);
        next.headers_mut()
            .insert("x-codex-turn-state", turn_state.parse().unwrap());
        let response = app.clone().oneshot(next).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 10000).await.unwrap();
    }
    let mut unknown = request(br#"{"model":"m","input":"hello"}"#);
    unknown.headers_mut().insert(
        "x-codex-turn-state",
        "unknown-routing-state".parse().unwrap(),
    );
    assert_eq!(
        app.oneshot(unknown).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    assert_eq!(hits.load(Ordering::Relaxed), 3);
    // In-memory keys retain hashes, never the opaque upstream routing tokens.
    assert!(!state
        .bindings
        .lock()
        .await
        .keys()
        .any(|key| key.contains(&token) || key.contains(&metadata_token)));
    a.abort();
    b.abort();
}

#[tokio::test]
async fn codex_thread_headers_bind_opaque_tool_continuations_without_crossing_threads() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let (first, a) = serve(Router::new().route(
        "/responses",
        post(move || {
            let hits = copy.clone();
            async move {
                hits.fetch_add(1, Ordering::Relaxed);
                axum::Json(json!({"id":"response-a","usage":{"input_tokens":1,"output_tokens":1}}))
            }
        }),
    ))
    .await;
    let (second, b) =
        serve(Router::new().route("/responses", post(|| async { StatusCode::IM_A_TEAPOT }))).await;
    let (_dir, state) = state(vec![source("a", &first, 0), source("b", &second, 1)]).await;
    let app = relay::router(state.clone());
    let mut first = request(br#"{"model":"m","input":"hello"}"#);
    first
        .headers_mut()
        .insert("session-id", "codex-process-session".parse().unwrap());
    first
        .headers_mut()
        .insert("thread-id", "thread-a".parse().unwrap());
    let response = app.clone().oneshot(first).await.unwrap();
    to_bytes(response.into_body(), 10000).await.unwrap();
    state.sources.write().await[1].source.priority = -1;
    for (thread, expected) in [
        ("thread-a", StatusCode::OK),
        ("thread-b", StatusCode::CONFLICT),
    ] {
        // An unobserved opaque item can be recovered via this thread's known
        // origin, e.g. when its upstream response was compressed or oversized.
        let mut next = request(br#"{"model":"m","input":[{"type":"reasoning","encrypted_content":"unobserved-reasoning"},{"type":"function_call_output","call_id":"call-fixture","output":"ok"}]}"#);
        next.headers_mut()
            .insert("session-id", "codex-process-session".parse().unwrap());
        next.headers_mut()
            .insert("thread-id", thread.parse().unwrap());
        let response = app.clone().oneshot(next).await.unwrap();
        assert_eq!(response.status(), expected);
        to_bytes(response.into_body(), 10000).await.unwrap();
    }
    assert_eq!(hits.load(Ordering::Relaxed), 2);
    a.abort();
    b.abort();
}

#[tokio::test]
async fn session_affinity_allows_opaque_context_only_after_known_response() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let (first, a) = serve(Router::new().route(
        "/responses",
        post(move || {
            let copy = copy.clone();
            async move {
                copy.fetch_add(1, Ordering::Relaxed);
                axum::Json(json!({"id":"r-a","usage":{"input_tokens":1,"output_tokens":1}}))
            }
        }),
    ))
    .await;
    let (second, b) =
        serve(Router::new().route("/responses", post(|| async { StatusCode::IM_A_TEAPOT }))).await;
    let (_dir, state) = state(vec![source("a", &first, 0), source("b", &second, 1)]).await;
    let app = relay::router(state.clone());
    let mut initial = request(br#"{"model":"m"}"#);
    initial
        .headers_mut()
        .insert("session-id", "my-session".parse().unwrap());
    let response = app.clone().oneshot(initial).await.unwrap();
    to_bytes(response.into_body(), 10000).await.unwrap();
    state.sources.write().await[1].source.priority = -1;
    let mut opaque = request(br#"{"model":"m","input":[{"encrypted_content":"new-opaque"}]}"#);
    opaque
        .headers_mut()
        .insert("session-id", "my-session".parse().unwrap());
    let response = app.clone().oneshot(opaque).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(hits.load(Ordering::Relaxed), 2);
    let mut unknown = request(br#"{"model":"m","input":[{"encrypted_content":"new-opaque"}]}"#);
    unknown
        .headers_mut()
        .insert("session_id", "unknown-session".parse().unwrap());
    assert_eq!(
        app.oneshot(unknown).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    assert_eq!(hits.load(Ordering::Relaxed), 2);
    a.abort();
    b.abort();
}

#[tokio::test]
async fn socks5h_keeps_dns_at_explicit_proxy() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let proxy = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut greeting = [0u8; 2];
        socket.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting[0], 5);
        let mut methods = vec![0; greeting[1] as usize];
        socket.read_exact(&mut methods).await.unwrap();
        assert!(methods.contains(&0));
        socket.write_all(&[5, 0]).await.unwrap();
        let mut command = [0u8; 4];
        socket.read_exact(&mut command).await.unwrap();
        assert_eq!(
            command,
            [5, 1, 0, 3],
            "SOCKS5h must send domain address to proxy"
        );
        let mut length = [0u8; 1];
        socket.read_exact(&mut length).await.unwrap();
        let mut hostname = vec![0; length[0] as usize];
        socket.read_exact(&mut hostname).await.unwrap();
        assert_eq!(hostname, b"remote-dns.invalid");
        let mut port = [0u8; 2];
        socket.read_exact(&mut port).await.unwrap();
        assert_eq!(u16::from_be_bytes(port), 80);
        socket
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
            .await
            .unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).await.unwrap();
            header.push(byte[0]);
            assert!(header.len() < 8192);
        }
        assert!(header.starts_with(b"GET /v1/models HTTP/1.1\r\n"));
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nsocks")
            .await
            .unwrap();
    });
    let mut target = source("socks", "http://remote-dns.invalid", 0);
    target.proxy = ProxyPolicy {
        mode: "explicit".into(),
        url: Some(format!("socks5h://{address}")),
    };
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        transport::client(&target)
            .unwrap()
            .get("http://remote-dns.invalid/v1/models")
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.text().await.unwrap(), "socks");
    proxy.await.unwrap();
    assert!(transport::client(&target)
        .unwrap()
        .get("http://remote-dns.invalid/v1/models")
        .send()
        .await
        .is_err());
}

#[tokio::test]
async fn portable_history_with_same_session_keeps_new_source_during_idle_protection() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let first_calls = calls.clone();
    let (first, a) = serve(Router::new().route(
        "/responses",
        post(move || {
            let calls = first_calls.clone();
            async move {
                calls.lock().await.push("a");
                axum::Json(json!({"id":"portable-a","usage":{"input_tokens":1,"output_tokens":1}}))
            }
        }),
    ))
    .await;
    let second_calls = calls.clone();
    let (second, b) = serve(Router::new().route(
        "/responses",
        post(move || {
            let calls = second_calls.clone();
            async move {
                calls.lock().await.push("b");
                axum::Json(json!({"id":"portable-b","usage":{"input_tokens":1,"output_tokens":1}}))
            }
        }),
    ))
    .await;
    let (_dir, state) = state(vec![source("a", &first, 0), source("b", &second, 1)]).await;
    let app = relay::router(state.clone());
    for enabled in [true, false, true] {
        state.sources.write().await[0].source.enabled = enabled;
        let mut portable = request(br#"{"model":"m","input":[{"role":"user","content":"complete portable input"}],"store":false}"#);
        portable
            .headers_mut()
            .insert("x-session-id", "same-local-session".parse().unwrap());
        let response = app.clone().oneshot(portable).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 10000).await.unwrap();
    }
    assert_eq!(*calls.lock().await, vec!["a", "b", "b"]);
    a.abort();
    b.abort();
}

fn expire_fixture_affinity(directory: &std::path::Path) {
    let db = rusqlite::Connection::open(directory.join("affinity.sqlite")).unwrap();
    db.execute(
        "UPDATE affinity SET last_seen=?1",
        [codexpress_gateway::model::now() - 3600],
    )
    .unwrap();
}

#[tokio::test]
async fn idle_return_rebuilds_cached_context_and_failed_return_keeps_working_source() {
    let preferred_calls = Arc::new(AtomicUsize::new(0));
    let copy = preferred_calls.clone();
    let (preferred, preferred_task) = serve(Router::new().route("/responses", post(move |req: Request| {
        let copy = copy.clone();
        async move {
            let body: Value = serde_json::from_slice(&to_bytes(req.into_body(), 10000).await.unwrap()).unwrap();
            let attempt = copy.fetch_add(1, Ordering::Relaxed);
            assert!(!body.to_string().contains("old-api-cipher"));
            if attempt < 2 {
                assert!(body.get("previous_response_id").is_none());
                assert!(body["input"].as_array().unwrap().iter().any(|item| item["content"] == "original prompt"));
                assert!(body["input"].as_array().unwrap().iter().any(|item| item["content"] == "api answer"));
            } else {
                assert_eq!(body["previous_response_id"], "preferred-response");
                assert!(body.to_string().contains("new-preferred-cipher"));
            }
            if attempt == 0 {
                (StatusCode::TOO_MANY_REQUESTS, axum::Json(json!({"error":{"code":"rate_limit_exceeded"}})))
            } else {
                (StatusCode::OK, axum::Json(json!({"id":"preferred-response","status":"completed","output":[{"type":"reasoning","encrypted_content":"new-preferred-cipher"}]})))
            }
        }
    }))).await;
    let api_calls = Arc::new(AtomicUsize::new(0));
    let copy = api_calls.clone();
    let (api, api_task) = serve(Router::new().route("/responses", post(move || {
        let copy = copy.clone();
        async move {
            copy.fetch_add(1, Ordering::Relaxed);
            axum::Json(json!({"id":"api-response","status":"completed","output":[{"role":"assistant","content":"api answer"},{"type":"reasoning","encrypted_content":"old-api-cipher"}]}))
        }
    }))).await;
    let mut first = source("preferred", &preferred, 0);
    first.enabled = false;
    let second = source("api", &api, 1);
    let (directory, initial) = state(vec![first.clone(), second.clone()]).await;
    initial.history.configure(true, 64_000_000).unwrap();
    let make = |body: Value| {
        let mut req = request(&serde_json::to_vec(&body).unwrap());
        req.headers_mut()
            .insert("thread-id", "idle-return-thread".parse().unwrap());
        req
    };
    let response = relay::router(initial.clone())
        .oneshot(make(
            json!({"model":"m","input":[{"role":"user","content":"original prompt"}]}),
        ))
        .await
        .unwrap();
    to_bytes(response.into_body(), 10000).await.unwrap();
    drop(initial);
    first.enabled = true;
    let next = json!({"model":"m","previous_response_id":"api-response","input":[{"role":"user","content":"next turn"}]});
    for attempt in 0..2 {
        expire_fixture_affinity(directory.path());
        let restored = AppState::new(directory.path().to_owned(), 0, "local-key".into()).unwrap();
        *restored.sources.write().await = vec![
            SourceRuntime::new(first.clone()),
            SourceRuntime::new(second.clone()),
        ];
        let app = relay::router(restored.clone());
        let response = app.clone().oneshot(make(next.clone())).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 10000).await.unwrap();
        let key = codexpress_gateway::affinity::key("thread:idle-return-thread", "m");
        assert_eq!(
            restored
                .affinity
                .preferred(&key, &[first.clone(), second.clone()])
                .as_deref(),
            Some(if attempt == 0 { "api" } else { "preferred" })
        );
        if attempt == 0 {
            // A failed reclaim and fresh API activity restart the idle window.
            let response = app.oneshot(make(next.clone())).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            to_bytes(response.into_body(), 10000).await.unwrap();
            assert_eq!(preferred_calls.load(Ordering::Relaxed), 1);
        } else {
            // Discard only old-source state; keep new-source ciphertext intact.
            let followup = json!({"model":"m","previous_response_id":"preferred-response","input":[{"type":"reasoning","encrypted_content":"old-api-cipher"},{"type":"reasoning","encrypted_content":"new-preferred-cipher"},{"role":"user","content":"continue"}]});
            let response = app.oneshot(make(followup)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            to_bytes(response.into_body(), 10000).await.unwrap();
        }
    }
    assert_eq!(preferred_calls.load(Ordering::Relaxed), 3);
    assert_eq!(api_calls.load(Ordering::Relaxed), 3);
    preferred_task.abort();
    api_task.abort();
}

#[tokio::test]
async fn idle_return_keeps_current_route_for_tool_results_disabled_rebuild_and_missing_history() {
    let higher_hits = Arc::new(AtomicUsize::new(0));
    let copy = higher_hits.clone();
    let (higher, a) = serve(Router::new().route(
        "/responses",
        post(move || {
            let copy = copy.clone();
            async move {
                copy.fetch_add(1, Ordering::Relaxed);
                StatusCode::OK
            }
        }),
    ))
    .await;
    let (api, b) = serve(Router::new().route(
        "/responses",
        post(|| async {
            axum::Json(json!({"id":"api-response","status":"completed","output":[]}))
        }),
    ))
    .await;
    let mut first = source("preferred", &higher, 0);
    first.enabled = false;
    let second = source("api", &api, 1);
    let (directory, initial) = state(vec![first.clone(), second.clone()]).await;
    initial.history.configure(true, 64_000_000).unwrap();
    let key = codexpress_gateway::affinity::key("thread:protected-thread", "m");
    let mut lease = routing::reserve(&initial, &second, Some(&key), false)
        .await
        .unwrap();
    lease.commit().unwrap();
    drop(lease);
    drop(initial);
    first.enabled = true;
    for case in 0..3 {
        expire_fixture_affinity(directory.path());
        let restored = AppState::new(directory.path().to_owned(), 0, "local-key".into()).unwrap();
        *restored.sources.write().await = vec![
            SourceRuntime::new(first.clone()),
            SourceRuntime::new(second.clone()),
        ];
        restored
            .bindings
            .lock()
            .await
            .insert("uncached-response".into(), "api".into());
        restored.settings.write().await.degraded_context_fallback = case != 1;
        let input = if case == 0 {
            json!([{"type":"function_call_output","call_id":"call","output":"tool result"}])
        } else {
            json!([{"role":"user","content":"next"}])
        };
        let mut req = request(
            &serde_json::to_vec(
                &json!({"model":"m","previous_response_id":"uncached-response","input":input}),
            )
            .unwrap(),
        );
        req.headers_mut()
            .insert("thread-id", "protected-thread".parse().unwrap());
        let response = relay::router(restored.clone()).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 10000).await.unwrap();
        assert_eq!(
            restored
                .affinity
                .preferred(&key, &[first.clone(), second.clone()])
                .as_deref(),
            Some("api")
        );
    }
    assert_eq!(higher_hits.load(Ordering::Relaxed), 0);
    a.abort();
    b.abort();
}

#[tokio::test]
async fn empty_official_quota_never_contacts_proxy_and_recovery_keeps_api_cache_affinity() {
    let proxy_hits = Arc::new(AtomicUsize::new(0));
    let copy = proxy_hits.clone();
    let (proxy, proxy_task) = serve(Router::new().fallback(move || {
        let copy = copy.clone();
        async move {
            copy.fetch_add(1, Ordering::Relaxed);
            StatusCode::BAD_GATEWAY
        }
    }))
    .await;
    let expected=br#"{ "model":"m", "prompt_cache_key":"stable-fixture-cache", "input":[{"role":"user","content":"complete portable history"}],"store":false }"#;
    let api_hits = Arc::new(AtomicUsize::new(0));
    let copy = api_hits.clone();
    let (api,api_task)=serve(Router::new().route("/responses",post(move |request:Request| {
        let copy=copy.clone();async move {
            assert_eq!(request.headers()["thread-id"],"long-fixture-thread");
            assert_eq!(request.headers()["session-id"],"shared-process");
            assert_eq!(&to_bytes(request.into_body(),10000).await.unwrap()[..],expected);
            copy.fetch_add(1,Ordering::Relaxed);
            axum::Json(json!({"id":"api-fixture-response","usage":{"input_tokens":100,"output_tokens":2}}))
        }
    }))).await;
    let mut official = source("official", "https://chatgpt.com/backend-api/codex", 0);
    official.kind = "official".into();
    official.account_id = Some("fixture-workspace".into());
    official.proxy = ProxyPolicy {
        mode: "explicit".into(),
        url: Some(proxy),
    };
    official.secret = json!({"access_token":"official-fixture","expires_at":4000000000_u64,"account_id":"fixture-workspace"});
    let (dir, state) = state(vec![official.clone(), source("api", &api, 1)]).await;
    state.sources.write().await[0].quota =
        json!({"windows":[{"name":"secondary","used_percent":100}]});
    let app = relay::router(state.clone());
    for _ in 0..2 {
        let mut req = request(expected);
        req.headers_mut()
            .insert("thread-id", "long-fixture-thread".parse().unwrap());
        req.headers_mut()
            .insert("session-id", "shared-process".parse().unwrap());
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 10000).await.unwrap();
        state.sources.write().await[0].quota =
            json!({"windows":[{"name":"secondary","used_percent":0}]});
    }
    assert_eq!(proxy_hits.load(Ordering::Relaxed), 0);
    assert_eq!(api_hits.load(Ordering::Relaxed), 2);
    // Restart the helper state, preserving only the app-owned affinity DB.
    drop(app);
    drop(state);
    let restored = AppState::new(dir.path().to_owned(), 0, "local-key".into()).unwrap();
    *restored.sources.write().await = vec![
        SourceRuntime::new(official),
        SourceRuntime::new(source("api", &api, 1)),
    ];
    let mut req = request(expected);
    req.headers_mut()
        .insert("thread-id", "long-fixture-thread".parse().unwrap());
    req.headers_mut()
        .insert("session-id", "shared-process".parse().unwrap());
    let response = relay::router(restored).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(proxy_hits.load(Ordering::Relaxed), 0);
    assert_eq!(api_hits.load(Ordering::Relaxed), 3);
    proxy_task.abort();
    api_task.abort();
}

#[tokio::test]
async fn content_length_json_over_real_tcp_finalizes_usage_before_last_chunk() {
    let wire = br#"{"id":"tcp-json","usage":{"input_tokens":17,"output_tokens":3}}"#;
    let (origin, origin_task) = serve(Router::new().route(
        "/responses",
        post(move || async move {
            Response::builder()
                .header("content-type", "application/json")
                .header("content-length", wire.len())
                .body(Body::from(wire.as_slice()))
                .unwrap()
        }),
    ))
    .await;
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = AppState::new(
        directory.path().to_owned(),
        address.port(),
        "local-key".into(),
    )
    .unwrap();
    state
        .sources
        .write()
        .await
        .push(SourceRuntime::new(source("a", &origin, 0)));
    let app = relay::router(state.clone());
    let gateway_task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/responses"))
        .bearer_auth("local-key")
        .json(&json!({"model":"m","input":"portable"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.content_length(), Some(wire.len() as u64));
    assert_eq!(&response.bytes().await.unwrap()[..], wire);
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.inflight.load(Ordering::Relaxed) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let usage = state.usage.summary(1).unwrap();
    assert_eq!(usage["recent"][0]["status"], "success");
    assert_eq!(usage["recent"][0]["input_tokens"], 17);
    assert_eq!(usage["recent"][0]["output_tokens"], 3);
    assert_eq!(usage["recent"][0]["complete"], true);
    assert_eq!(
        state
            .bindings
            .lock()
            .await
            .get("tcp-json")
            .map(String::as_str),
        Some("a")
    );
    origin_task.abort();
    gateway_task.abort();
}

#[tokio::test]
async fn old_inflight_response_cannot_rebind_after_source_identity_changes() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let upstream_entered = entered.clone();
    let upstream_release = release.clone();
    let (origin, task) = serve(Router::new().route("/responses", post(move || {
        let entered = upstream_entered.clone();
        let release = upstream_release.clone();
        async move {
            entered.notify_one();
            release.notified().await;
            axum::Json(json!({"id":"old-response","output":[{"type":"reasoning","encrypted_content":"old-account-opaque"}],"usage":{"input_tokens":1,"output_tokens":1}}))
        }
    }))).await;
    let (_dir, state) = state(vec![source("a", &origin, 0)]).await;
    let app = relay::router(state.clone());
    let mut initial = request(br#"{"model":"m"}"#);
    initial
        .headers_mut()
        .insert("x-session-id", "old-session".parse().unwrap());
    let pending_app = app.clone();
    let pending = tokio::spawn(async move {
        let response = pending_app.oneshot(initial).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 10000).await.unwrap();
    });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    {
        let mut sources = state.sources.write().await;
        sources[0].source.secret = json!({"api_key":"new-account-key"});
        state.bindings.lock().await.clear();
    }
    release.notify_one();
    pending.await.unwrap();
    assert!(
        state.bindings.lock().await.is_empty(),
        "old response, session and opaque bindings must remain invalidated"
    );
    assert!(routing::candidates(&state, "m", Some("old-response"))
        .await
        .is_empty());
    let response = app
        .oneshot(request(
            br#"{"model":"m","previous_response_id":"old-response"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    task.abort();
}

#[tokio::test]
async fn expired_official_auth_preflight_can_migrate_portable_session_without_proxy_submission() {
    let (api, task) = serve(Router::new().route(
        "/responses",
        post(|| async {
            axum::Json(json!({"id":"migrated","usage":{"input_tokens":10,"output_tokens":2}}))
        }),
    ))
    .await;
    let mut official = source("official", "https://chatgpt.com/backend-api/codex", 0);
    official.kind = "official".into();
    official.account_id = Some("fixture-account".into());
    official.proxy = ProxyPolicy {
        mode: "explicit".into(),
        url: Some("http://127.0.0.1:1".into()),
    };
    official.secret =
        json!({"access_token":"expired-fixture","expires_at":1,"account_id":"fixture-account"});
    let (_dir, state) = state(vec![official.clone(), source("api", &api, 1)]).await;
    let key = codexpress_gateway::affinity::key("thread:expired-auth-thread", "m");
    let mut previous = routing::reserve(&state, &official, Some(&key), false)
        .await
        .unwrap();
    previous.commit().unwrap();
    drop(previous);
    let mut req = request(br#"{"model":"m","input":"full portable input"}"#);
    req.headers_mut()
        .insert("thread-id", "expired-auth-thread".parse().unwrap());
    let response = relay::router(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(
        state.affinity.preferred(
            &key,
            &state
                .sources
                .read()
                .await
                .iter()
                .map(|r| r.source.clone())
                .collect::<Vec<_>>()
        ),
        Some("api".into())
    );
    task.abort();
}

#[tokio::test]
async fn cached_incremental_history_survives_restart_and_rebuilds_tool_pairs_for_new_source() {
    let first_wire = br#"{ "model":"m", "input":[{"role":"user","content":"remember original prompt"}], "store":false }"#;
    let output =
        json!({"type":"function_call","call_id":"cached-tool","name":"shell","arguments":"{}"});
    let output_copy = output.clone();
    let (a, task_a) = serve(Router::new().route(
        "/responses",
        post(move |req: Request| {
            let output = output_copy.clone();
            async move {
                assert_eq!(
                    &to_bytes(req.into_body(), 10000).await.unwrap()[..],
                    first_wire
                );
                axum::Json(json!({"id":"cached-parent","status":"completed","output":[output]}))
            }
        }),
    ))
    .await;
    let received = Arc::new(Mutex::new(Vec::new()));
    let copy = received.clone();
    let (b,task_b)=serve(Router::new().route("/responses",post(move |req:Request|{let copy=copy.clone();async move{
        let body:Value=serde_json::from_slice(&to_bytes(req.into_body(),10000).await.unwrap()).unwrap();
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(body["input"][0]["content"],"remember original prompt");
        assert_eq!(body["input"][1]["call_id"],body["input"][2]["call_id"]);
        assert_eq!(body["input"][2]["output"],"tool result");
        copy.lock().await.push(body);
        axum::Json(json!({"id":"new-source-response","status":"completed","output":[{"role":"assistant","content":"continued"}]}))
    }}))).await;
    let (dir, state) = state(vec![source("a", &a, 0)]).await;
    state.history.configure(true, 64_000_000).unwrap();
    let response = relay::router(state.clone())
        .oneshot(request(first_wire))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert!(state.history.status().unwrap()["blocks"].as_u64().unwrap() > 0);
    drop(state);
    let restored = AppState::new(dir.path().to_owned(), 0, "local-key".into()).unwrap();
    *restored.sources.write().await = vec![SourceRuntime::new(source("b", &b, 0))];
    let next=br#"{"model":"m","previous_response_id":"cached-parent","input":[{"type":"function_call_output","call_id":"cached-tool","output":"tool result"}]}"#;
    let response = relay::router(restored.clone())
        .oneshot(request(next))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 10000).await.unwrap();
    assert_eq!(received.lock().await.len(), 1);
    restored.history.clear().unwrap();
    restored.bindings.lock().await.clear();
    let response = relay::router(restored)
        .oneshot(request(next))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        received.lock().await.len(),
        1,
        "missing blocks must never submit partial history"
    );
    task_a.abort();
    task_b.abort();
}

#[tokio::test]
async fn sse_capture_keeps_completed_output_and_usage_never_returns_bodies() {
    let wire="data: {\"type\":\"response.completed\",\"response\":{\"id\":\"sse-cached\",\"status\":\"completed\",\"output\":[{\"role\":\"assistant\",\"content\":\"private-response-text\"}],\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\n";
    let (origin, task) = serve(Router::new().route(
        "/responses",
        post(move || async move {
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from(wire))
                .unwrap()
        }),
    ))
    .await;
    let (_dir, state) = state(vec![source("a", &origin, 0)]).await;
    state.history.configure(true, 64_000_000).unwrap();
    let response = relay::router(state.clone())
        .oneshot(request(
            br#"{"model":"m","stream":true,"input":"private-input-text"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(
        &to_bytes(response.into_body(), 10000).await.unwrap()[..],
        wire.as_bytes()
    );
    assert!(
        state.history.status().unwrap()["raw_bytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    for op in ["history.status", "status", "usage"] {
        let result = codexpress_gateway::management::dispatch(&state, op, json!({}))
            .await
            .unwrap()
            .to_string();
        assert!(
            !result.contains("private-input-text") && !result.contains("private-response-text")
        );
        assert!(!result.contains("upstream-a") && !result.contains("local-key"));
    }
    task.abort();
}
