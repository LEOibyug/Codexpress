use axum::{
    body::{to_bytes, Body},
    extract::{ConnectInfo, Request},
    http::StatusCode,
    routing::post,
    Router,
};
use codexpress_gateway::{
    management,
    model::{AppState, SourceRuntime},
    remote,
};
use serde_json::json;
use std::net::SocketAddr;
use tower::ServiceExt;

#[tokio::test]
async fn local_and_remote_share_one_port_without_activation_and_keep_credentials_separate() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/responses",
                post(|request: Request| async {
                    assert_eq!(
                        &to_bytes(request.into_body(), 1024).await.unwrap()[..],
                        br#"{ "model":"m", "input":"native bytes" }"#
                    );
                    axum::Json(json!({"id":"r","output":[]}))
                }),
            ),
        )
        .await
        .unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().into(), 17863, "local-only".into()).unwrap();
    state.sources.write().await.push(SourceRuntime::new(serde_json::from_value(json!({"id":"api","name":"API","kind":"api","base_url":base,"enabled":true,"priority":0,"proxy":{"mode":"direct"},"secret":{"api_key":"fixture"}})).unwrap()));
    let preview = management::dispatch(
        &state,
        "remote.preview",
        json!({"id":"peer","name":"Peer","host":"192.0.2.1","config":"model='m'"}),
    )
    .await
    .unwrap();
    assert_eq!(preview["base_url"], "http://192.0.2.1:17863/v1");
    let key = preview["key"].as_str().unwrap();
    let status = management::dispatch(&state, "remote.status", json!({}))
        .await
        .unwrap();
    assert_eq!(status["running"], true);
    let router = remote::router(state);
    for (ip, host, token, expected) in [
        ("127.0.0.1:4444", "127.0.0.1:17863", "local-only", 200),
        ("192.0.2.2:4444", "192.0.2.1:17863", key, 200),
        ("192.0.2.2:4444", "127.0.0.1:17863", "local-only", 401),
        ("192.0.2.2:4444", "192.0.2.9:17863", key, 401),
    ] {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("host", host)
            .header("authorization", format!("Bearer {token}"))
            .extension(ConnectInfo(ip.parse::<SocketAddr>().unwrap()))
            .body(Body::from(r#"{ "model":"m", "input":"native bytes" }"#))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status().as_u16(), expected);
        let _ = to_bytes(response.into_body(), 1024).await;
    }
    let response = router
        .oneshot(
            Request::builder()
                .uri("/v1/remote-check")
                .header("host", "192.0.2.1:17863")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    task.abort();
}
