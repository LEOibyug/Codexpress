use axum::{
    body::{to_bytes, Body},
    http::Request,
    routing::post,
    Router,
};
use codexpress_gateway::{
    management,
    model::{AppState, ProxyPolicy, Source, SourceRuntime},
    relay, remote,
};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn remote_compaction_uses_remote_paths_preserves_metadata_and_can_restore_prior_limits() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().to_path_buf(), 17863, "fixture-local".into()).unwrap();
    let original = "model='custom'\nmodel_auto_compact_token_limit=30000\nmodel_context_window=100000\nmodel_catalog_json='own-models.json'\n[mcp_servers.keep]\ncommand='keep-tool'\n";
    let catalog = json!({"models":[{"slug":"custom","context_window":120000,"max_context_window":200000,"custom_capability":{"keep":true},"model_messages":{"instructions_template":"fixture metadata"}}]});
    let mut payload = json!({"id":"remote-limits","name":"Remote","host":"192.0.2.10","config":original,"codex_home":"/home/fixture/.codex","catalog":catalog,"model_compaction":[{"model":"custom","auto_compact_token_limit":60000,"context_window":100000},{"model":"custom-v2","auto_compact_token_limit":40000,"context_window":80000}]});
    let prepared = management::dispatch(&state, "remote.preview", payload.clone())
        .await
        .unwrap();
    let doc = prepared["config"]
        .as_str()
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    assert_eq!(
        doc["model_catalog_json"].as_str(),
        Some("/home/fixture/.codex/.codexpress-model-catalog.json")
    );
    assert!(doc.get("model_auto_compact_token_limit").is_none());
    assert!(doc.get("model_context_window").is_none());
    assert_eq!(
        doc["mcp_servers"]["keep"]["command"].as_str(),
        Some("keep-tool")
    );
    let generated: serde_json::Value =
        serde_json::from_str(prepared["catalog"].as_str().unwrap()).unwrap();
    assert_eq!(prepared["catalog_action"], "write");
    assert_eq!(generated["models"][0]["auto_compact_token_limit"], 60000);
    assert_eq!(generated["models"][0]["custom_capability"]["keep"], true);
    assert_eq!(
        generated["models"][0]["base_instructions"],
        "fixture metadata"
    );
    assert_eq!(
        generated["models"][1]["base_instructions"],
        "fixture metadata"
    );
    assert_eq!(
        generated["models"][1]["model_messages"]["instructions_template"],
        "fixture metadata"
    );
    assert_eq!(generated["models"][1]["context_window"], 80000);
    payload["config"] = prepared["config"].clone();
    payload["compaction_base"] = json!(original);
    payload["model_compaction"] = json!([]);
    let reset = management::dispatch(&state, "remote.preview", payload.clone())
        .await
        .unwrap();
    let reset_doc = reset["config"]
        .as_str()
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    assert_eq!(reset["catalog_action"], "restore");
    assert_eq!(
        reset_doc["model_auto_compact_token_limit"].as_integer(),
        Some(30000)
    );
    assert_eq!(reset_doc["model_context_window"].as_integer(), Some(100000));
    assert_eq!(
        reset_doc["model_catalog_json"].as_str(),
        Some("own-models.json")
    );
    assert_eq!(reset_doc["model_provider"].as_str(), Some("codexpress"));
    payload["model_compaction"] =
        json!([{"model":"custom","auto_compact_token_limit":95000,"context_window":100000}]);
    assert!(management::dispatch(&state, "remote.preview", payload)
        .await
        .is_err());
    assert_eq!(
        remote::compaction_source(original).unwrap()["catalog_path"],
        "own-models.json"
    );
}

#[tokio::test]
async fn shared_listener_keeps_remote_credentials_host_restrictions_revoke_and_restart() {
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", upstream.local_addr().unwrap());
    let upstream_task = tokio::spawn(async move {
        axum::serve(upstream,Router::new().route("/responses",post(|request:Request<Body>|async move{
        assert_eq!(request.headers()["authorization"],"Bearer fixture-upstream");
        assert_eq!(&to_bytes(request.into_body(),1024).await.unwrap()[..],br#"{ "model":"m", "input":"keep bytes" }"#);
        axum::Json(json!({"id":"remote-response","usage":{"input_tokens":2,"output_tokens":1}}))
    }))).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let available = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = available.local_addr().unwrap().port();
    let state = AppState::new(dir.path().to_owned(), port, "fixture-local-key".into()).unwrap();
    *state.sources.write().await = vec![SourceRuntime::new(Source {
        id: "api".into(),
        name: "API".into(),
        kind: "api".into(),
        is_subscription: false,
        base_url: base,
        enabled: true,
        priority: 0,
        max_active_conversations: None,
        active_conversation_window_seconds: 15,
        proxy: ProxyPolicy {
            mode: "direct".into(),
            url: None,
        },
        models: vec![],
        account_id: None,
        secret: json!({"api_key":"fixture-upstream"}),
    })];
    assert_eq!(
        remote::router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/v1/remote-check")
                    .header("host", "127.0.0.1:17864")
                    .header("authorization", "Bearer fixture-local-key")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        401
    );
    let router =
        remote::router(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    let gateway_task = tokio::spawn(async move {
        axum::serve(available, router).await.unwrap();
    });
    let preview = management::dispatch(
        &state,
        "remote.preview",
        json!({"id":"fixture-peer","name":"Remote User","host":"127.0.0.1","config":"model='m'"}),
    )
    .await
    .unwrap();
    let key = preview["key"].as_str().unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://127.0.0.1:{port}/v1/remote-check");
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("fixture-local-key")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(key)
            .header("origin", "https://evil.example")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(key)
            .header("host", format!("localhost:{port}"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let check: serde_json::Value = client
        .get(&url)
        .bearer_auth(key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(check["mode"], "pool");
    let local = Request::builder()
        .uri("/v1/models")
        .header("host", "127.0.0.1:17863")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        relay::router(state.clone())
            .oneshot(local)
            .await
            .unwrap()
            .status(),
        401
    );
    let response = client
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .bearer_auth(key)
        .header("content-type", "application/json")
        .body(r#"{ "model":"m", "input":"keep bytes" }"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["id"],
        "remote-response"
    );
    let status = management::dispatch(&state, "remote.status", json!({}))
        .await
        .unwrap();
    assert!(!status.to_string().contains(key));
    assert!(!status.to_string().contains("fixture-upstream"));
    management::dispatch(
        &state,
        "remote.preview",
        json!({"id":"fixture-peer","name":"Remote User","host":"localhost","config":"model='m'"}),
    )
    .await
    .unwrap();
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(key)
            .send()
            .await
            .unwrap()
            .status(),
        200,
        "previewing another address must not break the old remote connection"
    );
    let loaded = remote::RemoteAccess::open(dir.path(), port).unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("host", format!("127.0.0.1:{port}").parse().unwrap());
    headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
    assert_eq!(loaded.peer(&headers).as_deref(), Some("fixture-peer"));
    management::dispatch(&state, "remote.revoke", json!({"id":"fixture-peer"}))
        .await
        .unwrap();
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(key)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    gateway_task.abort();
    upstream_task.abort();
}
