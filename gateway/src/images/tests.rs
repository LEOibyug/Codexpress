use crate::{
    images, management,
    model::{AppState, SourceRuntime},
};
use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::StatusCode,
    response::Response,
    routing::post,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tower::ServiceExt;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfixture";
async fn setup(
    router: Router,
) -> (
    tempfile::TempDir,
    Arc<AppState>,
    String,
    u16,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().into(), 17863, "PRIVATE-NATIVE-KEY".into()).unwrap();
    let source=serde_json::from_value(json!({"id":"official","name":"Official","kind":"official","base_url":"https://chatgpt.com/backend-api/codex","enabled":true,"priority":1,"proxy":{"mode":"explicit","url":proxy},"account_id":"account-fixture","models":["text-model"],"secret":{"access_token":"PRIVATE-OAUTH","expires_at":crate::model::now()+3600}})).unwrap();
    state.sources.write().await.push(SourceRuntime::new(source));
    let status = management::dispatch(&state, "images.status", json!({}))
        .await
        .unwrap();
    let key = management::dispatch(&state, "images.key", json!({}))
        .await
        .unwrap()["key"]
        .as_str()
        .unwrap()
        .to_owned();
    (
        dir,
        state,
        key,
        status["port"].as_u64().unwrap() as u16,
        task,
    )
}
fn request(port: u16, key: &str, path: &str, body: Option<Value>) -> Request {
    let mut request = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("host", format!("127.0.0.1:{port}"))
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(
            body.map(|v| Body::from(v.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    request
        .extensions_mut()
        .insert(super::TestTarget("http://images.example/codex".into()));
    request
}
async fn output(response: Response) -> Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), 128 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}
#[tokio::test]
async fn image_gateway_uses_official_oauth_proxy_and_independent_key_without_text_model_filter() {
    let (_dir,state,key,port,task)=setup(Router::new().route("/codex/images/generations",post(|request:Request|async move{
        assert_eq!(request.headers()["authorization"],"Bearer PRIVATE-OAUTH");assert_eq!(request.headers()["chatgpt-account-id"],"account-fixture");assert!(request.headers().contains_key("x-codex-image-turn-id"));
        let value:Value=serde_json::from_slice(&to_bytes(request.into_body(),65536).await.unwrap()).unwrap();assert_eq!(value["model"],"gpt-image-2");assert_eq!(value["prompt"],"fixture");assert!(value.get("output_format").is_none());
        Json(json!({"created":1,"data":[{"b64_json":STANDARD.encode(PNG)}],"private_field":"PRIVATE-OAUTH"}))
    }))).await;
    let api=serde_json::from_value(json!({"id":"api","name":"API","kind":"api","base_url":"http://127.0.0.1:1","enabled":true,"priority":0,"proxy":{"mode":"direct"},"secret":{"api_key":"PRIVATE-UPSTREAM"}})).unwrap();
    state.sources.write().await.push(SourceRuntime::new(api));
    state.settings.write().await.mode = "fixed".into();
    state.settings.write().await.fixed_source_id = Some("api".into());
    let native = images::router(state.clone())
        .oneshot(request(port, "PRIVATE-NATIVE-KEY", "/v1/models", None))
        .await
        .unwrap();
    assert_eq!(native.status(), 401);
    let models = images::router(state.clone())
        .oneshot(request(port, &key, "/v1/models", None))
        .await
        .unwrap();
    assert_eq!(output(models).await["data"][0]["id"], "gpt-image-2");
    let response = images::router(state.clone())
        .oneshot(request(
            port,
            &key,
            "/v1/images/generations",
            Some(json!({"prompt":"fixture","output_format":"png","response_format":"b64_json"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let result = output(response).await;
    assert_eq!(result["data"][0]["b64_json"], STANDARD.encode(PNG));
    assert!(!result.to_string().contains("PRIVATE"));
    assert_eq!(state.inflight.load(Ordering::Relaxed), 0);
    let usage = state.usage.summary(1).unwrap();
    assert_eq!(usage["recent"][0]["model"], "gpt-image-2");
    assert_eq!(usage["recent"][0]["status"], "success");
    assert_eq!(usage["recent"][0]["output_tokens"], 0);
    task.abort();
}
#[tokio::test]
async fn edits_accept_sdk_multipart_over_two_megabytes_and_send_inline_codex_json() {
    let (_dir, state, key, port, task) = setup(Router::new().route(
        "/codex/images/edits",
        post(|request: Request| async move {
            assert_eq!(request.headers()["content-type"], "application/json");
            let value: Value = serde_json::from_slice(
                &to_bytes(request.into_body(), 8 * 1024 * 1024)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert!(value["images"][0]["image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,"));
            assert_eq!(value["prompt"], "edit fixture");
            Json(json!({"created":1,"data":[{"b64_json":STANDARD.encode(PNG)}]}))
        }),
    ))
    .await;
    let mut body=b"--fixture\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nedit fixture\r\n--fixture\r\nContent-Disposition: form-data; name=\"image[]\"; filename=\"in.png\"\r\nContent-Type: image/png\r\n\r\n".to_vec();
    body.extend_from_slice(PNG);
    body.resize(body.len() + 3 * 1024 * 1024, 0);
    body.extend_from_slice(b"\r\n--fixture--\r\n");
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/images/edits")
        .header("host", format!("127.0.0.1:{port}"))
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "multipart/form-data; boundary=fixture")
        .body(Body::from(body))
        .unwrap();
    req.extensions_mut()
        .insert(super::TestTarget("http://images.example/codex".into()));
    let response = images::router(state).oneshot(req).await.unwrap();
    assert_eq!(response.status(), 200);
    task.abort();
}
#[tokio::test]
async fn invalid_parameters_masks_urls_and_unavailable_accounts_never_generate() {
    let calls = Arc::new(AtomicUsize::new(0));
    let copy = calls.clone();
    let (_dir, state, key, port, task) = setup(Router::new().fallback(move || {
        let count = copy.clone();
        async move {
            count.fetch_add(1, Ordering::Relaxed);
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }))
    .await;
    for body in [
        json!({"prompt":"x","stream":true}),
        json!({"prompt":"x","output_format":"jpeg"}),
        json!({"prompt":"x","mask":"a"}),
        json!({"prompt":"x","n":0}),
        json!({"prompt":"x","size":"1x1"}),
        json!({"prompt":"x","model":"other"}),
        json!({"prompt":"x","model":42}),
    ] {
        let response = images::router(state.clone())
            .oneshot(request(port, &key, "/v1/images/generations", Some(body)))
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }
    let response = images::router(state.clone())
        .oneshot(request(
            port,
            &key,
            "/v1/images/edits",
            Some(json!({"prompt":"x","images":[{"image_url":"http://127.0.0.1/private"}]})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let response = images::router(state.clone()).oneshot(request(port, &key, "/v1/images/edits", Some(json!({"prompt":"x","images":[{"image_url":format!("data:image/png;base64,{}",STANDARD.encode(PNG)),"mask":"unsupported"}]})))).await.unwrap();
    assert_eq!(response.status(), 400);
    state.sources.write().await[0].source.enabled = false;
    let response = images::router(state.clone())
        .oneshot(request(
            port,
            &key,
            "/v1/images/generations",
            Some(json!({"prompt":"x"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    for health in ["auth_required", "cooldown", "proxy_error"] {
        {
            let mut sources = state.sources.write().await;
            sources[0].source.enabled = true;
            sources[0].health = health.into();
            sources[0].cooldown_until = Some(crate::model::now() + 3600);
            sources[0].quota = json!({"windows":[{"used_percent":100}]});
            sources[0].quota_exhausted = true;
        }
        let response = images::router(state.clone())
            .oneshot(request(
                port,
                &key,
                "/v1/images/generations",
                Some(json!({"prompt":"x"})),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    task.abort();
}

#[tokio::test]
async fn exhausted_chat_quota_allows_images_without_reenabling_chat_or_bypassing_capacity() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let (_dir, state, key, port, task) = setup(Router::new().fallback(move || {
        seen.fetch_add(1, Ordering::Relaxed);
        async { Json(json!({"data":[{"b64_json":STANDARD.encode(PNG)}]})) }
    }))
    .await;
    let reset_at = crate::model::now() + 3600;
    for window in ["primary", "secondary"] {
        {
            let mut sources = state.sources.write().await;
            sources[0].quota = json!({"windows":[{"name":window,"used_percent":100}]});
            sources[0].health = "exhausted".into();
            sources[0].quota_exhausted = true;
            sources[0].cooldown_until = Some(reset_at);
        }
        for (path, body) in [
            ("/v1/images/generations", json!({"prompt":"x"})),
            (
                "/v1/images/edits",
                json!({"prompt":"x","images":[{"image_url":format!("data:image/png;base64,{}",STANDARD.encode(PNG))}]}),
            ),
        ] {
            let response = images::router(state.clone())
                .oneshot(request(port, &key, path, Some(body)))
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
        }
    }
    assert_eq!(calls.load(Ordering::Relaxed), 4);
    let sources = state.sources.read().await;
    assert!(crate::routing::official_exhausted(&sources[0], &sources));
    assert_eq!(sources[0].health, "exhausted");
    assert_eq!(sources[0].cooldown_until, Some(reset_at));
    drop(sources);
    let source = state.sources.read().await[0].source.clone();
    let _text = state
        .affinity
        .reserve(Some("active-text"), &source, false, false, None)
        .unwrap();
    state.sources.write().await[0]
        .source
        .max_active_conversations = Some(1);
    let response = images::router(state.clone())
        .oneshot(request(
            port,
            &key,
            "/v1/images/generations",
            Some(json!({"prompt":"x"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(calls.load(Ordering::Relaxed), 4);
    task.abort();
}

#[tokio::test]
async fn image_rate_limits_do_not_exhaust_chat_quota_or_retry_the_same_account() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let (_dir, state, key, port, task) = setup(Router::new().fallback(move || {
        seen.fetch_add(1, Ordering::Relaxed);
        async {
            (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error":{"code":"usage_limit_reached","message":"Image generation limit reached"}})))
        }
    }))
    .await;
    let response = images::router(state.clone())
        .oneshot(request(
            port,
            &key,
            "/v1/images/generations",
            Some(json!({"prompt":"x"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let sources = state.sources.read().await;
    assert_eq!(sources[0].health, "ready");
    assert!(!crate::routing::official_exhausted(&sources[0], &sources));
    task.abort();
}
#[tokio::test]
async fn server_errors_are_not_retried_and_access_configuration_is_isolated() {
    let calls = Arc::new(AtomicUsize::new(0));
    let copy = calls.clone();
    let (dir, state, key, port, task) = setup(Router::new().route(
        "/codex/images/generations",
        post(move || {
            let count = copy.clone();
            async move {
                count.fetch_add(1, Ordering::Relaxed);
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }),
    ))
    .await;
    let response = images::router(state.clone())
        .oneshot(request(
            port,
            &key,
            "/v1/images/generations",
            Some(json!({"prompt":"x"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let chatbot = management::dispatch(&state, "chatbot.key", json!({}))
        .await
        .unwrap()["key"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        management::dispatch(&state, "images.configure", json!({"key":chatbot}))
            .await
            .is_err()
    );
    assert!(
        management::dispatch(&state, "chatbot.configure", json!({"key":key}))
            .await
            .is_err()
    );
    let other = management::dispatch(&state, "chatbot.status", json!({}))
        .await
        .unwrap()["port"]
        .as_u64()
        .unwrap();
    assert!(
        management::dispatch(&state, "images.configure", json!({"port":other}))
            .await
            .is_err()
    );
    assert!(
        management::dispatch(&state, "chatbot.configure", json!({"port":port}))
            .await
            .is_err()
    );
    assert!(management::dispatch(
        &state,
        "images.configure",
        json!({"source_id":"not-official"})
    )
    .await
    .is_err());
    assert!(
        !std::fs::read_to_string(dir.path().join("image-access.json"))
            .unwrap()
            .contains("PRIVATE")
    );
    task.abort();
}

#[tokio::test]
async fn official_priority_selection_rate_limit_fallback_and_capacity_reservations() {
    let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let limited = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = calls.clone();
    let limit = limited.clone();
    let (_dir, state, key, port, task) = setup(Router::new().route(
        "/codex/images/generations",
        post(move |request: Request| {
            let seen = seen.clone();
            let limit = limit.clone();
            async move {
                let account = request.headers()["chatgpt-account-id"]
                    .to_str()
                    .unwrap()
                    .to_owned();
                seen.lock().unwrap().push(account.clone());
                if account == "fast" && limit.load(Ordering::Relaxed) {
                    return StatusCode::TOO_MANY_REQUESTS.into_response();
                }
                Json(json!({"created":1,"data":[{"b64_json":STANDARD.encode(PNG)}]}))
                    .into_response()
            }
        }),
    ))
    .await;
    let mut fast = state.sources.read().await[0].source.clone();
    fast.id = "fast".into();
    fast.account_id = Some("fast".into());
    fast.priority = 0;
    state.sources.write().await.push(SourceRuntime::new(fast));
    let run = || {
        images::router(state.clone()).oneshot(request(
            port,
            &key,
            "/v1/images/generations",
            Some(json!({"prompt":"x"})),
        ))
    };
    assert_eq!(run().await.unwrap().status(), 200);
    assert_eq!(calls.lock().unwrap().as_slice(), ["fast"]);
    state.images.settings.lock().unwrap().source_id = Some("official".into());
    assert_eq!(run().await.unwrap().status(), 200);
    assert_eq!(calls.lock().unwrap().last().unwrap(), "account-fixture");
    state.images.settings.lock().unwrap().source_id = None;
    limited.store(true, Ordering::Relaxed);
    let start = calls.lock().unwrap().len();
    assert_eq!(run().await.unwrap().status(), 200);
    assert_eq!(&calls.lock().unwrap()[start..], ["fast", "account-fixture"]);
    state.sources.write().await[1].source.enabled = false;
    state.sources.write().await[0].quota =
        json!({"windows":[{"name":"primary","used_percent":95}]});
    // Chat quota protection does not gate image generation.
    assert_eq!(run().await.unwrap().status(), 200);
    let source = state.sources.read().await[0].source.clone();
    let _text = state
        .affinity
        .reserve(Some("active-text"), &source, false, false, None)
        .unwrap();
    assert_eq!(run().await.unwrap().status(), 200);
    state.sources.write().await[0]
        .source
        .max_active_conversations = Some(1);
    let start = calls.lock().unwrap().len();
    assert_eq!(run().await.unwrap().status(), 503);
    assert_eq!(calls.lock().unwrap().len(), start);
    task.abort();
}
use axum::response::IntoResponse;

#[tokio::test]
async fn image_error_feedback_is_saved_without_oauth_or_request_content() {
    let (_dir,state,key,port,task)=setup(Router::new().route("/codex/images/generations",post(||async {
        (StatusCode::BAD_REQUEST,Json(json!({"error":{"message":"invalid request","code":"invalid_image","details":{"reason":"unsupported size","access_token":"PRIVATE-OAUTH"}}})))
    }))).await;
    let response = images::router(state.clone())
        .oneshot(request(
            port,
            &key,
            "/v1/images/generations",
            Some(json!({"prompt":"PRIVATE-IMAGE-PROMPT"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let record = &state.usage.summary(1).unwrap()["recent"][0];
    let detail = state
        .usage
        .detail(&json!({"attempt_id":record["attempt_id"]}))
        .unwrap();
    assert_eq!(detail["diagnostic"]["ingress"], "image");
    assert_eq!(detail["diagnostic"]["http_status"], 400);
    assert_eq!(detail["diagnostic"]["body_complete"], true);
    assert!(detail["diagnostic"]["body_text"]
        .as_str()
        .unwrap()
        .contains("unsupported size"));
    for private in [
        "PRIVATE-OAUTH",
        "PRIVATE-NATIVE-KEY",
        "PRIVATE-IMAGE-PROMPT",
        &key,
    ] {
        assert!(!detail.to_string().contains(private));
    }
    task.abort();
}
