use codexpress_gateway::{management, model::AppState};
use serde_json::json;

#[tokio::test]
async fn management_sync_is_transactional_and_does_not_expose_secrets_in_status() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().to_owned(), 17863, "local-test-key".into()).unwrap();
    let valid = json!({"id":"source-a","name":"API A","kind":"api","base_url":"https://example.com/v1","enabled":true,"priority":0,"proxy":{"mode":"direct"},"secret":{"api_key":"PRIVATE-API-SECRET"}});
    management::dispatch(
        &state,
        "sync",
        json!({"sources":[valid.clone()],"mode":"auto"}),
    )
    .await
    .unwrap();
    let status = management::dispatch(&state, "status", json!({}))
        .await
        .unwrap();
    assert_eq!(status["sources"][0]["id"], "source-a");
    assert_eq!(status["official_low_quota_percent"], 10);
    assert_eq!(status["idle_return_minutes"], 30);
    for invalid in [json!(-1), json!(0.5), json!(10081), json!("30")] {
        assert!(management::dispatch(
            &state,
            "sync",
            json!({"sources":[],"idle_return_minutes":invalid})
        )
        .await
        .is_err());
        assert_eq!(state.sources.read().await.len(), 1);
        assert_eq!(state.settings.read().await.idle_return_minutes, 30);
    }
    assert!(management::dispatch(
        &state,
        "sync",
        json!({"sources":[],"official_low_quota_percent":0})
    )
    .await
    .is_err());
    assert_eq!(state.sources.read().await.len(), 1);
    assert!(!status.to_string().contains("PRIVATE-API-SECRET"));
    let mut invalid = valid;
    invalid["proxy"] = json!({"mode":"explicit","url":"http://user:PRIVATE-PROXY@localhost:1"});
    assert!(
        management::dispatch(&state, "sync", json!({"sources":[invalid],"mode":"auto"}))
            .await
            .is_err()
    );
    assert_eq!(state.sources.read().await[0].source.proxy.mode, "direct");
    assert!(!management::dispatch(&state, "credentials", json!({}))
        .await
        .unwrap()
        .to_string()
        .contains("PRIVATE-API-SECRET"));
}

#[tokio::test]
async fn official_refresh_generation_cannot_be_overwritten_by_stale_ui_sync() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().to_owned(), 17863, "local-test-key".into()).unwrap();
    let source = json!({"id":"official-a","name":"Official","kind":"official","base_url":"https://chatgpt.com/backend-api/codex","enabled":true,"priority":0,"proxy":{"mode":"explicit","url":"http://127.0.0.1:7890"},"secret":{"access_token":"old-access","refresh_token":"old-refresh"}});
    management::dispatch(
        &state,
        "sync",
        json!({"sources":[source.clone()],"mode":"auto"}),
    )
    .await
    .unwrap();
    state.sources.write().await[0].source.secret =
        json!({"access_token":"new-access","refresh_token":"new-refresh","generation":2});
    let response = management::dispatch(&state, "sync", json!({"sources":[source],"mode":"auto"}))
        .await
        .unwrap();
    assert_eq!(
        response["updated_secrets"][0]["secret"]["refresh_token"],
        "new-refresh"
    );
    assert_eq!(state.sources.read().await[0].source.secret["generation"], 2);
    let stored = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|p| std::fs::read(p.unwrap().path()).unwrap())
        .collect::<Vec<_>>();
    for data in stored {
        assert!(!data.windows(11).any(|s| s == b"new-refresh"));
    }
}

#[tokio::test]
async fn changing_api_identity_invalidates_prior_server_state_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().to_owned(), 17863, "local-test-key".into()).unwrap();
    let mut source = json!({"id":"api-a","name":"API","kind":"api","base_url":"https://a.example/v1","enabled":true,"priority":0,"proxy":{"mode":"direct"},"secret":{"api_key":"key-a"}});
    management::dispatch(
        &state,
        "sync",
        json!({"sources":[source.clone()],"mode":"auto"}),
    )
    .await
    .unwrap();
    state
        .bindings
        .lock()
        .await
        .insert("old-response".into(), "api-a".into());
    source["base_url"] = json!("https://b.example/v1");
    management::dispatch(&state, "sync", json!({"sources":[source],"mode":"auto"}))
        .await
        .unwrap();
    assert!(state.bindings.lock().await.get("old-response").is_none());
}
