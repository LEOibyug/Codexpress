use codexpress_gateway::{
    credentials::{CredentialStore, FileStore, ManagedCredentialStore},
    management,
    model::AppState,
};
use serde_json::{json, Value};
use std::{fs, os::unix::fs::{symlink, PermissionsExt}, path::Path, sync::Arc};

fn state_directory() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn write_policy(dir: &Path, policy: Value) {
    let path = dir.join("credential-storage.json");
    fs::write(&path, serde_json::to_vec(&policy).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn file_only_restart_preserves_rotations_without_reviving_inactive_legacy_files() {
    let dir = state_directory();
    let path = dir.path().join("credentials.json");
    let old = dir.path().join(".codexpress/credentials.json");
    let stale = json!({"api_key":"inactive-fixture"});
    FileStore::new(old.clone()).unwrap().save("api", &stale).unwrap();
    let api = json!({"api_key":"new-fixture"});
    let oauth = json!({"access_token":"renewed", "refresh_token":"rotated"});

    let store = ManagedCredentialStore::open(dir.path()).unwrap();
    assert_eq!(store.storage_status().unwrap(), json!({"mode":"file","file_path":path}));
    assert_eq!(store.load("api").unwrap(), None);
    // Previous releases left a private file behind when switching back to Keychain.
    write_policy(dir.path(), json!({"mode":"keychain","file_path":null}));
    let store = ManagedCredentialStore::open(dir.path()).unwrap();
    assert_eq!(store.load("api").unwrap(), None);
    store.save("api", &api).unwrap();
    store.save("official", &json!({"refresh_token":"before-rotation"})).unwrap();
    store.save("official", &oauth).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777, 0o700);
    drop(store);
    let reopened = ManagedCredentialStore::open(dir.path()).unwrap();
    assert_eq!(reopened.load("official").unwrap(), Some(oauth));
    assert_eq!(reopened.load("api").unwrap(), Some(api));

    write_policy(dir.path(), json!({"mode":"file","file_path":old}));
    let previous_file_user = ManagedCredentialStore::open(dir.path()).unwrap();
    assert_eq!(previous_file_user.storage_status().unwrap()["file_path"], json!(old));
    assert_eq!(previous_file_user.load("api").unwrap(), Some(stale));
    previous_file_user.delete("api").unwrap();
    assert_eq!(FileStore::new(old).unwrap().load("api").unwrap(), None);
    assert_eq!(FileStore::new(path).unwrap().load("api").unwrap(), Some(json!({"api_key":"new-fixture"})));
}

#[test]
fn unsafe_or_corrupt_files_cannot_be_used_or_overwritten() {
    let dir = state_directory();
    let file = dir.path().join("credentials.json");
    let policy = dir.path().join("credential-storage.json");
    write_policy(dir.path(), json!({"mode":"unknown"}));
    assert!(ManagedCredentialStore::open(dir.path()).is_err());
    fs::write(&policy, b"broken policy").unwrap();
    assert!(ManagedCredentialStore::open(dir.path()).is_err());
    assert_eq!(fs::read(&policy).unwrap(), b"broken policy");
    let original = b"broken credential file";
    fs::write(&file, original).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let store = FileStore::new(file.clone()).unwrap();
    assert!(store.load("api").is_err());
    assert!(store.save("api", &json!({"api_key":"replacement"})).is_err());
    assert_eq!(fs::read(&file).unwrap(), original);
    std::fs::remove_file(&file).unwrap();
    let outside = dir.path().join("outside");
    std::fs::write(&outside, b"leave alone").unwrap();
    symlink(&outside, &file).unwrap();
    assert!(FileStore::new(file.clone())
        .unwrap()
        .save("api", &json!({}))
        .is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"leave alone");
    std::fs::remove_file(&file).unwrap();
    FileStore::new(file.clone())
        .unwrap()
        .save("api", &json!({"api_key":"fixture"}))
        .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(FileStore::new(file).unwrap().load("api").is_err());
}

#[tokio::test]
async fn management_uses_private_file_for_api_and_protects_live_official_accounts() {
    let dir = state_directory();
    let store = Arc::new(ManagedCredentialStore::open(dir.path()).unwrap());
    let state = AppState::new_with_credential_store(
        dir.path().to_path_buf(),
        0,
        "fixture".into(),
        Some(store),
    )
    .unwrap();
    management::dispatch(
        &state,
        "credentials.save_api",
        json!({"source_id":"api","api_key":"fixture-api"}),
    )
    .await
    .unwrap();
    let before = management::dispatch(&state, "credentials.read", json!({"source_ids":["api"]}))
        .await
        .unwrap();
    assert_eq!(before["items"][0]["secret"]["api_key"], "fixture-api");
    assert!(management::dispatch(&state, "credentials.storage.switch", json!({"mode":"keychain","source_ids":["api"]})).await.is_err());
    let oauth = json!({"access_token":"fixture-new","refresh_token":"fixture-rotated","account_id":"account"});
    state.persist_credential("official", &oauth).unwrap();
    let read = management::dispatch(
        &state,
        "credentials.read",
        json!({"source_ids":["official"]}),
    )
    .await
    .unwrap();
    assert_eq!(read["items"][0]["secret"], oauth);
    let source = serde_json::from_value(json!({"id":"official","name":"Official fixture","kind":"official","base_url":"https://chatgpt.com/backend-api/codex","enabled":false,"priority":0,"proxy":{"mode":"explicit","url":"http://127.0.0.1:9"},"secret":oauth})).unwrap();
    state
        .sources
        .write()
        .await
        .push(codexpress_gateway::model::SourceRuntime::new(source));
    assert!(management::dispatch(
        &state,
        "credentials.save_api",
        json!({"source_id":"official","api_key":"must-not-replace-oauth"})
    )
    .await
    .is_err());
    assert!(management::dispatch(
        &state,
        "credentials.delete",
        json!({"source_id":"official"})
    )
    .await
    .is_err());
    assert_eq!(
        management::dispatch(
            &state,
            "credentials.read",
            json!({"source_ids":["official"]})
        )
        .await
        .unwrap()["items"][0]["secret"],
        oauth
    );
    let status = management::dispatch(&state, "credentials.storage.status", json!({}))
        .await
        .unwrap();
    assert!(!status.to_string().contains("fixture-api"));
    assert!(!status.to_string().contains("fixture-new"));
    management::dispatch(&state, "credentials.delete", json!({"source_id":"api"}))
        .await
        .unwrap();
    assert_eq!(
        management::dispatch(&state, "credentials.read", json!({"source_ids":["api"]}))
            .await
            .unwrap()["items"][0]["secret"],
        Value::Null
    );
}

#[tokio::test]
async fn missing_api_key_blocks_only_its_source_and_preserves_fixed_routing_until_repaired() {
    let dir = state_directory();
    let store = Arc::new(ManagedCredentialStore::open(dir.path()).unwrap());
    let state = AppState::new_with_credential_store(dir.path().into(), 0, "fixture".into(), Some(store)).unwrap();
    let missing = json!({"id":"missing","name":"Legacy API","kind":"api","enabled":true,
        "priority":0,"proxy":{"mode":"direct"},"base_url":"https://unused.invalid/v1","secret":{}});
    let mut ready = missing.clone();
    ready["id"] = json!("ready");
    ready["secret"] = json!({"api_key":"usable-fixture"});
    let mut config = json!({"sources":[missing,ready],"mode":"auto"});
    management::dispatch(&state, "sync", config.clone()).await.unwrap();
    let status = management::dispatch(&state, "status", json!({})).await.unwrap();
    assert!(management::dispatch(&state, "routing.try", json!({"source_id":"missing"})).await.is_err());
    let row = status["sources"].as_array().unwrap().iter().find(|row| row["id"] == "missing").unwrap();
    assert_eq!(row["health"], "auth_required");
    let candidates = codexpress_gateway::routing::candidates(&state, "fixture", None).await;
    assert_eq!(candidates.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["ready"]);

    config["mode"] = json!("fixed");
    config["fixed_source_id"] = json!("missing");
    management::dispatch(&state, "sync", config.clone()).await.unwrap();
    let mut malformed = config.clone();
    malformed["sources"][0]["secret"] = json!({"api_key":""});
    assert!(management::dispatch(&state, "sync", malformed).await.is_err());
    assert!(codexpress_gateway::routing::candidates(&state, "fixture", None).await.is_empty());
    management::dispatch(&state, "credentials.save_api", json!({"source_id":"missing","api_key":"reentered"})).await.unwrap();
    let saved = management::dispatch(&state, "credentials.read", json!({"source_ids":["missing"]})).await.unwrap();
    config["sources"][0]["secret"] = saved["items"][0]["secret"].clone();
    management::dispatch(&state, "sync", config).await.unwrap();
    let restored = codexpress_gateway::routing::candidates(&state, "fixture", None).await;
    assert_eq!(restored.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["missing"]);
}
