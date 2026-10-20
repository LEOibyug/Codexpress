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
use serde_json::json;
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
    (directory, state)
}

#[tokio::test]
async fn raw_json_headers_query_and_usage_survive_relay() {
    let seen = Arc::new(Mutex::new(None));
    let copy = seen.clone();
    let response_body = br#"{ "id":"resp-one", "usage":{"input_tokens":100,"output_tokens":9,"input_tokens_details":{"cached_tokens":40}}, "unknown":true }"#;
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
    let (_directory, state) = state(vec![source("a", &format!("{address}/v1"), 0)]).await;
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
async fn fallback_is_ordered_fixed_mode_and_previous_id_never_cross_sources() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_copy = calls.clone();
    let (first, a) = serve(Router::new().route(
        "/responses",
        post(|| async { StatusCode::TOO_MANY_REQUESTS }),
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
async fn sse_bytes_usage_sticky_binding_and_cancellation() {
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
    assert_eq!(summary["recent"][0]["status"], "interrupted");
    assert_eq!(summary["rows"][0]["input_tokens"], 30);
    assert_eq!(summary["rows"][0]["cached_input_tokens"], 12);
    task.abort();
    gateway_task.abort();
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
        .insert("x-session-id", "my-session".parse().unwrap());
    let response = app.clone().oneshot(initial).await.unwrap();
    to_bytes(response.into_body(), 10000).await.unwrap();
    state.sources.write().await[1].source.priority = -1;
    let mut opaque = request(br#"{"model":"m","input":[{"encrypted_content":"new-opaque"}]}"#);
    opaque
        .headers_mut()
        .insert("x-session-id", "my-session".parse().unwrap());
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
async fn portable_history_with_same_session_can_move_and_restore_priority() {
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
    assert_eq!(*calls.lock().await, vec!["a", "b", "a"]);
    a.abort();
    b.abort();
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
