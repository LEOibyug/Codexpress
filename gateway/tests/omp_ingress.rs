#[path = "common/server.rs"]
mod server;
#[path = "common/source.rs"]
mod source_fixture;
use axum::{body::{to_bytes, Body}, extract::Request, http::StatusCode, routing::{get, post}, Router};
use codexpress_gateway::{chatbot, management, model::{AppState, SourceRuntime}};
use serde_json::{json, Value};
use tower::ServiceExt;

#[tokio::test]
async fn budget_revision_tracks_committed_rules_without_discovery_and_shares_the_omp_gate() {
    use std::{path::Path, sync::{Arc, atomic::{AtomicUsize, Ordering}}, time::Duration};
    use tokio::{io::{AsyncBufReadExt, AsyncWriteExt, BufReader}, net::UnixStream};

    async fn ipc_revision(socket: &Path) -> Value {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        stream.write_all(b"{\"id\":\"budget-probe\",\"op\":\"omp.budgets.revision\",\"payload\":{}}\n").await.unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], "budget-probe");
        assert_eq!(response["ok"], true);
        response["data"].clone()
    }

    async fn revision(router: &Router, socket: &Path, key: &str) -> Value {
        let ipc = ipc_revision(socket).await;
        assert_eq!(ipc.as_object().unwrap().len(), 1);
        assert!(!ipc["revision"].as_str().unwrap().is_empty());
        let response = router.clone().oneshot(Request::builder().uri("/omp/v1/budgets/revision")
            .header("host", "127.0.0.1:17865").header("authorization", format!("Bearer {key}"))
            .body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let http: Value = serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(http, ipc);
        ipc
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let upstream_calls = calls.clone();
    let (base, upstream_task) = server::serve(Router::new().fallback(move || {
        upstream_calls.fetch_add(1, Ordering::SeqCst);
        async { axum::Json(json!({"data":[{"id":"m"}]})) }
    })).await;
    std::fs::create_dir_all(".runtime").unwrap();
    let dir = tempfile::tempdir_in(".runtime").unwrap();
    let state = AppState::new(dir.path().into(), 17863, "native-secret".into()).unwrap();
    let source = source_fixture::api_source("budget-api", "API", &base, "upstream-secret");
    let mut source_json = serde_json::to_value(&source).unwrap();
    source_json["secret"] = source.secret.clone();
    let mut payload = json!({"sources":[source_json]});
    management::dispatch(&state, "sync", payload.clone()).await.unwrap();
    // A real directory read reaches the fixture, so accidental discovery by a
    // probe is observable even though the source has no configured model list.
    let binding = management::dispatch(&state, "omp.binding", json!({})).await.unwrap();
    assert!(binding["models"].as_array().unwrap().iter().any(|model| model["id"] == "m"));
    let discovery_calls = calls.load(Ordering::SeqCst);
    assert!(discovery_calls > 0);
    let key = state.chatbot.key().unwrap()["key"].as_str().unwrap().to_owned();
    let router = chatbot::router(state.clone());
    let socket = dir.path().join("control.sock");
    let control_state = state.clone();
    let control_task = tokio::spawn(async move { management::serve(control_state).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !socket.exists() { tokio::task::yield_now().await; }
    }).await.unwrap();
    let empty = revision(&router, &socket, &key).await;
    let original_rules = json!([
        {"model":"m","auto_compact_token_limit":100,"context_window":1000},
        {"model":"other","auto_compact_token_limit":200,"context_window":2000}
    ]);
    payload["model_compaction"] = original_rules.clone();
    management::dispatch(&state, "sync", payload.clone()).await.unwrap();
    let original = revision(&router, &socket, &key).await;
    assert_ne!(original, empty);
    payload["load_balance"] = json!(true);
    management::dispatch(&state, "sync", payload.clone()).await.unwrap();
    assert_eq!(revision(&router, &socket, &key).await, original);
    payload["model_compaction"].as_array_mut().unwrap().reverse();
    management::dispatch(&state, "sync", payload.clone()).await.unwrap();
    assert_eq!(revision(&router, &socket, &key).await, original);
    payload["model_compaction"][0]["auto_compact_token_limit"] = json!(210);
    management::dispatch(&state, "sync", payload.clone()).await.unwrap();
    let threshold = revision(&router, &socket, &key).await;
    assert_ne!(threshold, original);
    payload["model_compaction"][0]["context_window"] = json!(2200);
    management::dispatch(&state, "sync", payload.clone()).await.unwrap();
    let window = revision(&router, &socket, &key).await;
    assert_ne!(window, threshold);
    let mut invalid = payload.clone();
    invalid["model_compaction"][0]["auto_compact_token_limit"] = json!(0);
    assert!(management::dispatch(&state, "sync", invalid).await.is_err());
    assert_eq!(revision(&router, &socket, &key).await, window);
    payload["model_compaction"].as_array_mut().unwrap().remove(0);
    management::dispatch(&state, "sync", payload.clone()).await.unwrap();
    assert_ne!(revision(&router, &socket, &key).await, window);
    payload["model_compaction"] = json!([]);
    management::dispatch(&state, "sync", payload.clone()).await.unwrap();
    assert_eq!(revision(&router, &socket, &key).await, empty);
    let restarted = AppState::new(dir.path().into(), 17863, "native-secret".into()).unwrap();
    assert_eq!(management::dispatch(&restarted, "omp.budgets.revision", json!({})).await.unwrap(), empty);
    payload["model_compaction"] = original_rules;
    management::dispatch(&restarted, "sync", payload).await.unwrap();
    assert_eq!(management::dispatch(&restarted, "omp.budgets.revision", json!({})).await.unwrap(), original);

    for (method, host, token, expected) in [
        ("GET", "127.0.0.1:17865", None, StatusCode::UNAUTHORIZED),
        ("GET", "127.0.0.1:17865", Some("wrong-key"), StatusCode::UNAUTHORIZED),
        ("GET", "127.0.0.1:17865", Some("native-secret"), StatusCode::UNAUTHORIZED),
        ("GET", "127.0.0.1:17863", Some(key.as_str()), StatusCode::MISDIRECTED_REQUEST),
        ("POST", "127.0.0.1:17865", Some(key.as_str()), StatusCode::METHOD_NOT_ALLOWED),
    ] {
        let mut request = Request::builder().method(method).uri("/omp/v1/budgets/revision").header("host", host);
        if let Some(token) = token { request = request.header("authorization", format!("Bearer {token}")); }
        let response = router.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), expected);
    }
    management::dispatch(&state, "chatbot.configure", json!({"enabled":false})).await.unwrap();
    assert_eq!(ipc_revision(&socket).await, empty);
    let disabled = router.oneshot(Request::builder().uri("/omp/v1/budgets/revision")
        .header("host", "127.0.0.1:17865").header("authorization", format!("Bearer {key}"))
        .body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(disabled.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(calls.load(Ordering::SeqCst), discovery_calls, "budget probes must not contact upstream");
    control_task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn native_omp_authentication_bytes_events_and_source_identity() {
    let body = r#"{ "model":"m", "stream":true, "tools":[{"type":"image_generation"}], "tool_choice":"auto", "input":[{"type":"function_call_output","call_id":"call","output":"ok"},{"type":"reasoning","encrypted_content":"opaque"}], "unknown":{"kept":true} }"#;
    let events = "event: future.event\ndata: {\"type\":\"future.event\",\"opaque\":true}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[{\"type\":\"reasoning\",\"encrypted_content\":\"opaque\"}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
    let (base, task) = server::serve(Router::new().route("/responses", post(move |request: Request| async move {
        let bytes = to_bytes(request.into_body(), 8192).await.unwrap();
        if bytes.as_ref() != br#"{"model":"m","input":"warmup"}"# {
            assert_eq!(bytes.as_ref(), body.as_bytes());
        }
        ([("content-type", "text/event-stream")], events)
    })).route("/models", get(|| async { axum::Json(json!({"data":[{"id":"discovered-api-model"}]})) }))).await;
    let dir = tempfile::tempdir_in(".").unwrap();
    let state = AppState::new(dir.path().into(), 17863, "native-secret".into()).unwrap();
    let mut source = source_fixture::api_source("api", "API", &base, "upstream-secret");
    source.models = vec!["m".into()];
    let identity = source.identity();
    state.sources.write().await.push(SourceRuntime::new(source));
    state.settings.write().await.strict_priority = false;
    let binding = management::dispatch(&state, "omp.binding", json!({})).await.unwrap();
    assert_eq!(binding["api"], "openai-codex-responses");
    assert!(binding["base_url"].as_str().unwrap().ends_with("/omp/v1"));
    assert!(!binding["models"].as_array().unwrap().iter().any(|model| model["id"] == "gpt-image-2"));
    let key = binding["key"].as_str().unwrap();
    let router = chatbot::router(state.clone());
    let warmup = router.clone().oneshot(Request::builder().method("POST").uri("/omp/v1/responses")
        .header("host", "127.0.0.1:17865").header("authorization", format!("Bearer {key}"))
        .header("x-session-id", "real-session").body(Body::from(r#"{"model":"m","input":"warmup"}"#)).unwrap()).await.unwrap();
    assert_eq!(warmup.status(), StatusCode::OK);
    to_bytes(warmup.into_body(), 8192).await.unwrap();
    for (path, token, status) in [
        ("/omp/v1/responses", "native-secret", StatusCode::UNAUTHORIZED),
        ("/omp/v1/responses", key, StatusCode::OK),
        ("/omp/v1/codex/responses", key, StatusCode::OK),
    ] {
        let response = router.clone().oneshot(Request::builder().method("POST").uri(path)
            .header("host", "127.0.0.1:17865").header("authorization", format!("Bearer {token}"))
            .header("x-session-id", "real-session").body(Body::from(body)).unwrap()).await.unwrap();
        assert_eq!(response.status(), status);
        if status == StatusCode::OK {
            assert_eq!(response.headers()["x-codexpress-source-id"], "api");
            assert_eq!(response.headers()["x-codexpress-source-identity"], identity);
            assert_eq!(to_bytes(response.into_body(), 8192).await.unwrap().as_ref(), events.as_bytes());
        }
    }
    for (path, expected) in [("/omp/v1/responses", StatusCode::NOT_IMPLEMENTED), ("/omp/v1/codex/responses", StatusCode::NOT_IMPLEMENTED), ("/omp/v1/capabilities?model=m&session_id=real-session", StatusCode::OK), ("/omp/v1/status", StatusCode::OK)] {
        let response = router.clone().oneshot(Request::builder().uri(path).header("host", "127.0.0.1:17865")
            .header("authorization", format!("Bearer {key}")).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), expected);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        if path.contains("capabilities") {
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["source_identity"], identity);
            assert_eq!(value["websocket"], false);
            assert_eq!(value["remote_compaction_v2"], Value::Null);
        }
        if path.ends_with("status") {
            let text = String::from_utf8(bytes.to_vec()).unwrap();
            for secret in ["native-secret", "upstream-secret", key] { assert!(!text.contains(secret)); }
        }
    }
    for path in ["/omp/v1/status?include_usage=false", "/omp/v1/models"] {
        let response = router.clone().oneshot(Request::builder().uri(path)
            .header("host", "127.0.0.1:17865").header("authorization", format!("Bearer {key}"))
            .body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap();
        if path.contains("include_usage") {
            assert!(value.get("usage").is_none());
            assert!(value["sources"].is_array());
        } else {
            assert_eq!(value["data"], binding["models"]);
        }
    }
    let mut official = source_fixture::api_source("official", "官方", &base, "fixture-key");
    official.kind = "official".into();
    official.base_url = "https://chatgpt.com/backend-api/codex".into();
    official.proxy.mode = "explicit".into();
    official.proxy.url = Some("http://127.0.0.1:1".into());
    state.sources.write().await.push(SourceRuntime::new(official));
    std::fs::create_dir(dir.path().join("model-catalogs")).unwrap();
    std::fs::write(dir.path().join("model-catalogs/official-latest.json"),
        serde_json::to_vec(&json!({"models":[{"slug":"cached-model","context_window":700000,"max_context_window":900000,
            "kind":"chat","input_modalities":["text","image"],"supported_reasoning_levels":[{"effort":"high"}],
            "thinking":{"high":1000},"cost":{"input":2,"output":10}}]})).unwrap()).unwrap();
    let expanded = management::dispatch(&state, "omp.binding", json!({})).await.unwrap();
    assert_eq!(expanded["models"].as_array().unwrap().iter().find(|model| model["id"] == "cached-model").unwrap()["context_window"], 700000);
    let cached = expanded["models"].as_array().unwrap().iter().find(|model| model["id"] == "cached-model").unwrap();
    assert_eq!(cached["kind"], "chat");
    assert_eq!(cached["max_context_window"], 900000);
    assert_eq!(cached["input"], json!(["text", "image"]));
    assert_eq!(cached["reasoning"], true);
    assert_eq!(cached["thinking"], json!({"high":1000}));
    assert_eq!(cached["cost"], json!({"input":2,"output":10}));
    assert!(cached.get("max_output_tokens").is_none());
    assert!(expanded["models"].as_array().unwrap().iter().any(|model| model["id"] == "m"));
    let image = expanded["models"].as_array().unwrap().iter().find(|model| model["id"] == "gpt-image-2").unwrap();
    assert_eq!(image["kind"], "image");
    assert_eq!(image["input"], json!(["text", "image"]));
    let sources: Vec<_> = state.sources.read().await.iter().map(|runtime| {
        let mut source = serde_json::to_value(&runtime.source).unwrap();
        source["secret"] = runtime.source.secret.clone();
        source
    }).collect();
    for (rules, window, maximum, limit) in [
        (json!([{"model":"cached-model","context_window":1000000,"auto_compact_token_limit":600000},
            {"model":"gpt-image-2","context_window":1000000,"auto_compact_token_limit":600000}]), 1000000, 1000000, json!(600000)),
        (json!([{"model":"cached-model","context_window":800000,"auto_compact_token_limit":250000}]), 800000, 900000, json!(250000)),
        (json!([{"model":"cached-model","auto_compact_token_limit":250000}]), 700000, 900000, json!(250000)),
        (json!([]), 700000, 900000, Value::Null),
    ] {
        management::dispatch(&state, "sync", json!({"sources":sources,"model_compaction":rules})).await.unwrap();
        if window == 1000000 {
            for invalid in [json!({}), json!([
                {"model":"cached-model","context_window":2000000,"auto_compact_token_limit":1000000},
                {"model":"invalid","auto_compact_token_limit":0},
            ])] {
                assert!(management::dispatch(&state, "sync", json!({"sources":[],"load_balance":true,
                    "model_compaction":invalid})).await.is_err());
                let status = management::dispatch(&state, "status", json!({})).await.unwrap();
                assert_eq!(status["sources"].as_array().unwrap().len(), 2);
                assert_eq!(status["load_balance"], false);
            }
            let binding = management::dispatch(&state, "omp.binding", json!({})).await.unwrap();
            let model = binding["models"].as_array().unwrap().iter().find(|model| model["id"] == "cached-model").unwrap();
            assert_eq!(model["context_window"], 1000000);
            assert_eq!(model["auto_compact_token_limit"], 600000);
        }
        let response = router.clone().oneshot(Request::builder().uri("/omp/v1/models")
            .header("host", "127.0.0.1:17865").header("authorization", format!("Bearer {key}"))
            .body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap();
        let models = value["data"].as_array().unwrap();
        let cached = models.iter().find(|model| model["id"] == "cached-model").unwrap();
        assert_eq!(cached["context_window"], window);
        assert_eq!(cached["max_context_window"], maximum);
        assert_eq!(cached["auto_compact_token_limit"], limit);
        let unconfigured = models.iter().find(|model| model["id"] == "m").unwrap();
        assert!(unconfigured.get("context_window").is_none());
        assert_eq!(unconfigured["auto_compact_token_limit"], Value::Null);
        let image = models.iter().find(|model| model["id"] == "gpt-image-2").unwrap();
        assert_eq!(image["kind"], "image");
        assert!(image.get("context_window").is_none());
        assert_eq!(image["auto_compact_token_limit"], Value::Null);
    }
    management::dispatch(&state, "sync", json!({"sources":sources})).await.unwrap();
    state.sources.write().await.iter_mut().find(|runtime| runtime.source.id == "official").unwrap().source.enabled = false;
    let disabled = management::dispatch(&state, "omp.binding", json!({})).await.unwrap();
    assert!(!disabled["models"].as_array().unwrap().iter().any(|model| model["id"] == "gpt-image-2"));
    state.sources.write().await.iter_mut().find(|runtime| runtime.source.id == "official").unwrap().source.enabled = true;
    state.sources.write().await.iter_mut().find(|runtime| runtime.source.id == "api").unwrap().source.models.clear();
    let discovered = management::dispatch(&state, "omp.binding", json!({})).await.unwrap();
    assert!(discovered["models"].as_array().unwrap().iter().any(|model| model["id"] == "discovered-api-model"));
    task.abort();
}

#[tokio::test]
async fn compaction_rejections_are_scoped_to_the_requested_protocol() {
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
    let calls = Arc::new(AtomicUsize::new(0));
    let upstream_calls = calls.clone();
    let temporary_calls = Arc::new(AtomicUsize::new(0));
    let upstream_temporary_calls = temporary_calls.clone();
    let (base, task) = server::serve(Router::new()
        .route("/responses/compact", post(|| async { (StatusCode::BAD_REQUEST, axum::Json(json!({"error":{"code":"unsupported_compaction"}}))) }))
        .route("/responses", post(move |request: Request| {
            upstream_calls.fetch_add(1, Ordering::SeqCst);
            let (status, body) = if request.headers()["authorization"] == "Bearer temporary-key" {
                match upstream_temporary_calls.fetch_add(1, Ordering::SeqCst) {
                    0 => (StatusCode::SERVICE_UNAVAILABLE, json!({"error":{"code":"unsupported_compaction"}})),
                    1 => (StatusCode::TOO_MANY_REQUESTS, json!({"error":{"code":"unsupported_compaction"}})),
                    _ => (StatusCode::OK, json!({"id":"compacted","output":[{"type":"compaction","encrypted_content":"new-state"}]})),
                }
            } else {
                (StatusCode::BAD_REQUEST, json!({"error":{"code":"unsupported_compaction"}}))
            };
            async move { (status, axum::Json(body)) }
        }))).await;
    let dir = tempfile::tempdir_in(".").unwrap();
    let state = AppState::new(dir.path().into(), 17863, "native-secret".into()).unwrap();
    state.sources.write().await.push(SourceRuntime::new(source_fixture::api_source("protocol", "API", &base, "protocol-key")));
    let key = state.chatbot.key().unwrap()["key"].as_str().unwrap().to_owned();
    let identity = state.sources.read().await[0].source.identity();
    let router = chatbot::router(state.clone());
    for (path, input, supported) in [
        ("/omp/v1/responses/compact", json!([{"role":"user","content":"keep history"}]), Value::Null),
        ("/omp/v1/responses", json!([{"role":"user","content":"keep history"},{"type":"compaction_trigger"}]), json!(false)),
        ("/omp/v1/codex/responses", json!([{"role":"user","content":"keep history"},{"type":"compaction_trigger"}]), json!(false)),
        ("/omp/v1/codex/responses/compact", json!([{"role":"user","content":"keep history"}]), json!(false)),
    ] {
        let response = router.clone().oneshot(Request::builder().method("POST").uri(path)
            .header("host", "127.0.0.1:17865").header("authorization", format!("Bearer {key}"))
            .body(Body::from(json!({"model":"m","input":input,"stream":true}).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error: Value = serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(error["error"]["code"], "unsupported_compaction");
        let response = router.clone().oneshot(Request::builder().uri("/omp/v1/capabilities?model=m")
            .header("host", "127.0.0.1:17865").header("authorization", format!("Bearer {key}"))
            .body(Body::empty()).unwrap()).await.unwrap();
        let capability: Value = serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(capability["remote_compaction_v2"], supported);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "known V2 rejection must not reach upstream again");
    state.sources.write().await[0].source.secret = json!({"api_key":"changed-protocol-key"});
    let response = router.clone().oneshot(Request::builder().method("POST").uri("/omp/v1/codex/responses")
        .header("host", "127.0.0.1:17865").header("authorization", format!("Bearer {key}"))
        .body(Body::from(json!({"model":"m","input":[{"type":"compaction_trigger"}]}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_ne!(response.headers()["x-codexpress-source-identity"], identity);
    to_bytes(response.into_body(), 8192).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2, "changed source identity must relearn support");
    state.sources.write().await[0].source.secret = json!({"api_key":"temporary-key"});
    state.settings.write().await.upstream_attempts = 1;
    state.settings.write().await.upstream_retry_delay_seconds = 0;
    for expected in [StatusCode::SERVICE_UNAVAILABLE, StatusCode::TOO_MANY_REQUESTS, StatusCode::OK] {
        let response = router.clone().oneshot(Request::builder().method("POST").uri("/omp/v1/codex/responses")
            .header("host", "127.0.0.1:17865").header("authorization", format!("Bearer {key}"))
            .body(Body::from(json!({"model":"m","input":[{"type":"compaction_trigger"}]}).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), expected);
        to_bytes(response.into_body(), 8192).await.unwrap();
        let response = router.clone().oneshot(Request::builder().uri("/omp/v1/capabilities?model=m")
            .header("host", "127.0.0.1:17865").header("authorization", format!("Bearer {key}"))
            .body(Body::empty()).unwrap()).await.unwrap();
        let capability: Value = serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(capability["remote_compaction_v2"], Value::Null);
    }
    assert_eq!(temporary_calls.load(Ordering::SeqCst), 3);
    task.abort();
}

#[tokio::test]
async fn observing_compaction_error_does_not_consume_context_recovery_evidence() {
    use axum::response::IntoResponse;
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
    let attempts = Arc::new(AtomicUsize::new(0));
    let upstream_attempts = attempts.clone();
    let (first, first_task) = server::serve(Router::new().route("/responses", post(move || {
        let attempts = upstream_attempts.clone();
        async move {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                axum::Json(json!({"id":"seed-response","output":[{"type":"reasoning","encrypted_content":"known-context"}],"usage":{"input_tokens":2,"output_tokens":1}})).into_response()
            } else {
                (StatusCode::BAD_REQUEST, axum::Json(json!({"error":{"code":"invalid_encrypted_content","message":"encrypted content rejected"}}))).into_response()
            }
        }
    }))).await;
    let recovered = Arc::new(AtomicUsize::new(0));
    let recovered_requests = recovered.clone();
    let (second, second_task) = server::serve(Router::new().route("/responses", post(move |request: Request| {
        let recovered = recovered_requests.clone();
        async move {
            assert_eq!(request.headers()["session-id"], "context-recovery-session");
            let body: Value = serde_json::from_slice(&to_bytes(request.into_body(), 8192).await.unwrap()).unwrap();
            assert!(body["client_metadata"].get("codexpress_cache_group").is_none());
            assert_eq!(body["prompt_cache_key"], "original-recovery-cache");
            assert!(body["input"].as_array().unwrap().iter().any(|item| item["role"] == "user" && item["content"] == "keep complete visible history"));
            assert!(body["input"].as_array().unwrap().iter().all(|item| item.get("encrypted_content").is_none()));
            recovered.fetch_add(1, Ordering::SeqCst);
            axum::Json(json!({"id":"recovered","output":[{"type":"compaction","encrypted_content":"recovered-context"}],"usage":{"input_tokens":2,"output_tokens":1}}))
        }
    }))).await;
    let dir = tempfile::tempdir_in(".").unwrap();
    let state = AppState::new(dir.path().into(), 17863, "native-secret".into()).unwrap();
    let original = source_fixture::api_source("original", "API", &first, "original-key");
    let mut fallback = source_fixture::api_source("fallback", "API", &second, "fallback-key");
    fallback.priority = 1;
    state.sources.write().await.extend([SourceRuntime::new(original), SourceRuntime::new(fallback)]);
    state.settings.write().await.strict_priority = false;
    let key = state.chatbot.key().unwrap()["key"].as_str().unwrap().to_owned();
    let router = chatbot::router(state);
    for input in [json!([{"role":"user","content":"keep complete visible history"}]), json!([
        {"role":"user","content":"keep complete visible history"}, {"type":"reasoning","encrypted_content":"known-context"}, {"type":"compaction_trigger"}
    ])] {
        let response = router.clone().oneshot(Request::builder().method("POST").uri("/omp/v1/responses")
            .header("host", "127.0.0.1:17865").header("authorization", format!("Bearer {key}"))
            .header("thread-id", "context-recovery-thread")
            .header("session-id", "context-recovery-session")
            .body(Body::from(json!({"model":"m","input":input,"prompt_cache_key":"original-recovery-cache",
                "client_metadata":{"codexpress_cache_group":"a".repeat(64)}}).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 8192).await.unwrap();
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(recovered.load(Ordering::SeqCst), 1);
    first_task.abort();
    second_task.abort();
}

#[tokio::test]
async fn native_metadata_is_consumed_without_merging_child_sessions_on_a_parent_cache() {
    use std::sync::{Arc, Mutex};
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream_seen = seen.clone();
    let handler = post(move |request: Request| {
        let seen = upstream_seen.clone();
        async move {
            let thread = request.headers()["thread-id"].to_str().unwrap().to_owned();
            assert_eq!(request.headers()["session_id"], "shared-parent-cache");
            assert_eq!(request.headers()["session-id"], "original-api-affinity");
            let value: Value = serde_json::from_slice(&to_bytes(request.into_body(), 8192).await.unwrap()).unwrap();
            if let Some(metadata) = value.get("client_metadata") {
                assert!(metadata.get("codexpress_session_id").is_none());
                assert!(metadata.get("codexpress_cache_group").is_none());
                assert_eq!(metadata["real_field"], "preserved");
                assert!(metadata["session_id"].is_string());
                assert_eq!(metadata["thread_id"], "transport-thread");
            }
            assert_eq!(value["prompt_cache_key"], "shared-parent-cache");
            seen.lock().unwrap().push(thread);
            axum::Json(json!({"id":"metadata-response","output":[],"usage":{"input_tokens":1,"output_tokens":1}}))
        }
    });
    let (base, task) = server::serve(Router::new().route("/responses", handler.clone())
        .route("/responses/compact", handler)).await;
    let dir = tempfile::tempdir_in(".").unwrap();
    let state = AppState::new(dir.path().into(), 17863, "native-secret".into()).unwrap();
    state.sources.write().await.push(SourceRuntime::new(source_fixture::api_source("metadata", "API", &base, "metadata-key")));
    state.settings.write().await.strict_priority = false;
    let key = state.chatbot.key().unwrap()["key"].as_str().unwrap().to_owned();
    let router = chatbot::router(state);
    for (path, session, private) in [
        ("/omp/v1/responses", "child-a", true),
        ("/omp/v1/codex/responses", "child-a", true),
        ("/omp/v1/codex/responses", "child-b", true),
        ("/omp/v1/codex/responses", "fork-a", true),
        ("/omp/v1/codex/responses/compact", "child-a", false),
        ("/omp/v1/codex/responses", "child-a", false),
        ("/omp/v1/codex/responses/compact", "child-b", false),
    ] {
        let mut body = json!({"model":"m","input":"hello","prompt_cache_key":"shared-parent-cache",
            "client_metadata":{"session_id":session,"thread_id":"transport-thread","real_field":"preserved"}});
        body["client_metadata"]["codexpress_cache_group"] = json!("a".repeat(64));
        if private {
            body["client_metadata"]["codexpress_session_id"] = json!(session);
            body["client_metadata"]["session_id"] = json!("normalized-or-parent-cache");
        }
        let response = router.clone().oneshot(Request::builder().method("POST").uri(path)
            .header("host", "127.0.0.1:17865").header("authorization", format!("Bearer {key}"))
            .header("session_id", "shared-parent-cache").header("thread-id", "stale-parent")
            .header("session-id", "original-api-affinity")
            .body(Body::from(body.to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 8192).await.unwrap();
    }
    let body = json!({"model":"m","input":"compact child history","prompt_cache_key":"shared-parent-cache"});
    let response = router.oneshot(Request::builder().method("POST").uri("/omp/v1/responses/compact")
        .header("host", "127.0.0.1:17865").header("authorization", format!("Bearer {key}"))
        .header("session_id", "shared-parent-cache").header("session-id", "original-api-affinity")
        .header("x-codex-turn-metadata", json!({"session_id":"child-b","thread_id":"transport-thread"}).to_string())
        .body(Body::from(body.to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 8192).await.unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 8);
    assert_eq!(seen[0], seen[1]);
    assert_ne!(seen[0], seen[2]);
    assert_ne!(seen[0], seen[3]);
    assert_ne!(seen[2], seen[3]);
    assert_eq!(seen[0], seen[4]);
    assert_eq!(seen[0], seen[5]);
    assert_eq!(seen[2], seen[6]);
    assert_eq!(seen[2], seen[7]);
    task.abort();
}

