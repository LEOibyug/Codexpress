use axum::{body::Body, extract::Request, http::StatusCode, response::Response, Router};
use codexpress_gateway::{
    management,
    model::{now, AppState, SourceRuntime},
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (address, task)
}
fn source(base: &str) -> Value {
    json!({"id":"api-a","name":"API","kind":"api","base_url":base,"enabled":true,"priority":0,"proxy":{"mode":"direct"},"secret":{"api_key":"PRIVATE-TEST-KEY"}})
}

#[tokio::test]
async fn drafts_discover_at_responses_sibling_without_changing_saved_sources() {
    let (address, task) = serve(Router::new().fallback(|request: Request| async move {
        assert_eq!(request.method(), "GET");
        assert_eq!(request.uri().path(), "/custom/v1/models");
        assert_eq!(
            request.headers()["authorization"],
            "Bearer PRIVATE-TEST-KEY"
        );
        assert_eq!(request.headers()["accept-encoding"], "identity");
        axum::Json(json!({"data":[{"id":"model-z"},{"id":"model-a"},{"id":"model-z"}]}))
    }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().to_owned(), 0, "local".into()).unwrap();
    let draft = source(&format!("{address}/custom/v1/responses/"));
    let result = management::dispatch(&state, "models.list", json!({"source":draft}))
        .await
        .unwrap();
    assert_eq!(result, json!({"models":["model-a","model-z"]}));
    assert!(state.sources.read().await.is_empty());
    assert!(!result.to_string().contains("PRIVATE"));
    assert!(state.usage.summary(1).unwrap()["recent"]
        .as_array()
        .unwrap()
        .is_empty());
    task.abort();
}

#[tokio::test]
async fn model_discovery_uses_explicit_proxy_and_never_falls_back() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let (address, upstream) = serve(Router::new().fallback(move || {
        let copy = copy.clone();
        async move {
            copy.fetch_add(1, Ordering::SeqCst);
            axum::Json(json!({"data":[{"id":"direct"}]}))
        }
    }))
    .await;
    let expected = format!("{address}/v1/models");
    let (proxy, proxy_task) = serve(Router::new().fallback(move |request: Request| {
        assert_eq!(request.uri().to_string(), expected);
        async { axum::Json(json!({"data":[{"id":"proxied"}]})) }
    }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().to_owned(), 0, "local".into()).unwrap();
    let mut draft = source(&format!("{address}/v1"));
    draft["proxy"] = json!({"mode":"explicit","url":proxy});
    let result = management::dispatch(&state, "models.list", json!({"source":draft}))
        .await
        .unwrap();
    assert_eq!(result["models"], json!(["proxied"]));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    proxy_task.abort();
    let unavailable = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    draft["proxy"]["url"] = json!(format!("http://{}", unavailable.local_addr().unwrap()));
    drop(unavailable);
    assert!(
        management::dispatch(&state, "models.list", json!({"source":draft}))
            .await
            .is_err()
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    // Direct mode ignores even a stale explicit proxy URL.
    draft["proxy"]["mode"] = json!("direct");
    let result = management::dispatch(&state, "models.list", json!({"source":draft}))
        .await
        .unwrap();
    assert_eq!(result["models"], json!(["direct"]));
    upstream.abort();
}

#[tokio::test]
async fn redirects_errors_and_large_catalogs_are_safe_failures() {
    let (address, task) = serve(Router::new().fallback(|request: Request| async move {
        match request.uri().path() {
            "/redirect/models" => Response::builder()
                .status(302)
                .header("location", "/leak/models")
                .body(Body::from("PRIVATE-TEST-KEY"))
                .unwrap(),
            "/large/models" => Response::builder()
                .header("content-length", "5000000")
                .body(Body::from("a".repeat(5_000_000)))
                .unwrap(),
            "/empty/models" => Response::new(Body::from(r#"{"data":[]}"#)),
            "/error/models" => Response::builder()
                .status(401)
                .body(Body::from("PRIVATE-TEST-KEY"))
                .unwrap(),
            _ => panic!("redirect must not be followed"),
        }
    }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().to_owned(), 0, "local".into()).unwrap();
    for path in ["redirect", "large", "empty", "error"] {
        let error = management::dispatch(
            &state,
            "models.list",
            json!({"source":source(&format!("{address}/{path}"))}),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(!error.contains("PRIVATE"));
    }
    task.abort();
}

#[tokio::test]
async fn official_discovery_uses_saved_identity_and_required_proxy() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let (proxy, task) = serve(Router::new().fallback(move |request: Request| {
        assert_eq!(request.method(), "CONNECT");
        assert_eq!(request.uri().to_string(), "chatgpt.com:443");
        assert!(!request.headers().contains_key("authorization"));
        copy.fetch_add(1, Ordering::SeqCst);
        async { StatusCode::BAD_GATEWAY }
    }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().to_owned(), 0, "local".into()).unwrap();
    let official = json!({"id":"official-a","name":"Official","kind":"official","base_url":"https://chatgpt.com/backend-api/codex","enabled":true,"priority":0,"proxy":{"mode":"explicit","url":proxy},"account_id":"test-account","secret":{"access_token":"PRIVATE-OAUTH","expires_at":now()+3600}});
    state.sources.write().await.push(SourceRuntime::new(
        serde_json::from_value(official.clone()).unwrap(),
    ));
    assert!(
        management::dispatch(&state, "models.list", json!({"source":official}))
            .await
            .is_err()
    );
    let error = management::dispatch(&state, "models.list", json!({"source_id":"official-a"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("PRIVATE"));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    task.abort();
}
