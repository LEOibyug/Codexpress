use codexpress_gateway::{
    credentials::{CredentialStore, FileStore, ManagedCredentialStore},
    management,
    model::AppState,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    os::unix::fs::{symlink, PermissionsExt},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

#[derive(Default)]
struct MemoryStore {
    values: Mutex<HashMap<String, Value>>,
    reads: AtomicUsize,
    fail: AtomicBool,
}
impl CredentialStore for MemoryStore {
    fn load(&self, id: &str) -> anyhow::Result<Option<Value>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.values.lock().unwrap().get(id).cloned())
    }
    fn save(&self, id: &str, secret: &Value) -> anyhow::Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            anyhow::bail!("fixture write failed");
        }
        self.values
            .lock()
            .unwrap()
            .insert(id.into(), secret.clone());
        Ok(())
    }
    fn delete(&self, id: &str) -> anyhow::Result<()> {
        self.values.lock().unwrap().remove(id);
        Ok(())
    }
}

#[test]
fn cached_credentials_migrate_and_rotated_official_tokens_survive_restart_without_keychain_access()
{
    let dir = tempfile::tempdir().unwrap();
    let keychain = Arc::new(MemoryStore::default());
    let api = json!({"api_key":"fixture-api"});
    let oauth = json!({"access_token":"fixture-old","refresh_token":"fixture-refresh","account_id":"account"});
    keychain.save("api", &api).unwrap();
    keychain.save("official", &oauth).unwrap();
    let store = ManagedCredentialStore::open(dir.path(), keychain.clone()).unwrap();
    for _ in 0..5 {
        assert_eq!(store.load("api").unwrap(), Some(api.clone()));
        assert_eq!(store.load("missing").unwrap(), None);
    }
    assert_eq!(
        keychain.reads.load(Ordering::SeqCst),
        2,
        "present and absent items are read once per helper process"
    );
    let file = dir.path().join(".codexpress/credentials.json");
    let ids = vec!["api".into(), "official".into(), "missing".into()];
    store.switch_storage("file", Some(&file), &ids).unwrap();
    assert_eq!(
        std::fs::metadata(file.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let reads = keychain.reads.load(Ordering::SeqCst);
    let rotated = json!({"access_token":"fixture-new","refresh_token":"fixture-rotated","account_id":"account"});
    store.save("official", &rotated).unwrap();
    drop(store);
    let reopened = ManagedCredentialStore::open(dir.path(), keychain.clone()).unwrap();
    assert_eq!(reopened.storage_status().unwrap()["mode"], "file");
    assert_eq!(reopened.load("official").unwrap(), Some(rotated.clone()));
    assert_eq!(reopened.load("api").unwrap(), Some(api));
    assert_eq!(
        keychain.reads.load(Ordering::SeqCst),
        reads,
        "file mode must not touch keychain"
    );
    reopened.switch_storage("keychain", None, &ids).unwrap();
    assert_eq!(keychain.values.lock().unwrap()["official"], rotated);
    assert!(
        file.exists(),
        "migration explicitly retains original copies"
    );
    reopened.delete("api").unwrap();
    assert_eq!(reopened.load("api").unwrap(), None);
}

#[test]
fn migration_failure_keeps_active_store_and_bad_files_never_fall_back_or_get_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = Arc::new(MemoryStore::default());
    let store = ManagedCredentialStore::open(dir.path(), keychain.clone()).unwrap();
    store.save("api", &json!({"api_key":"fixture"})).unwrap();
    let file = dir.path().join(".codexpress/credentials.json");
    let ids = vec!["api".into()];
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::create_dir(&file).unwrap(); // Reliable failure even with admin permissions.
    assert!(store.switch_storage("file", Some(&file), &ids).is_err());
    assert_eq!(store.storage_status().unwrap()["mode"], "keychain");
    assert!(store.load("api").unwrap().is_some());
    std::fs::remove_dir(&file).unwrap();
    let policy = dir.path().join("credential-storage.json");
    std::fs::create_dir(&policy).unwrap();
    assert!(store.switch_storage("file", Some(&file), &ids).is_err());
    assert_eq!(
        store.storage_status().unwrap()["mode"],
        "keychain",
        "failed policy commit cannot publish the new backend"
    );
    std::fs::remove_dir(&policy).unwrap();
    store.switch_storage("file", Some(&file), &ids).unwrap();
    keychain.fail.store(true, Ordering::SeqCst);
    assert!(store.switch_storage("keychain", None, &ids).is_err());
    assert_eq!(store.storage_status().unwrap()["mode"], "file");
    let original = b"broken credential file";
    std::fs::write(&file, original).unwrap();
    let reopened = ManagedCredentialStore::open(dir.path(), keychain.clone()).unwrap();
    let reads = keychain.reads.load(Ordering::SeqCst);
    assert!(reopened.load("api").is_err());
    assert!(reopened
        .save("api", &json!({"api_key":"replacement"}))
        .is_err());
    assert_eq!(std::fs::read(&file).unwrap(), original);
    assert_eq!(keychain.reads.load(Ordering::SeqCst), reads);
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
async fn management_uses_private_store_for_api_and_official_and_migration_serializes_with_refresh()
{
    let dir = tempfile::tempdir().unwrap();
    let keychain = Arc::new(MemoryStore::default());
    let store = Arc::new(ManagedCredentialStore::open(dir.path(), keychain).unwrap());
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
    let path = dir.path().join(".codexpress/credentials.json");
    let held = state.sources.write().await;
    let task_state = state.clone();
    let task = tokio::spawn(async move {
        management::dispatch(
            &task_state,
            "credentials.storage.switch",
            json!({"mode":"file","file_path":path,"source_ids":["api"]}),
        )
        .await
    });
    tokio::task::yield_now().await;
    assert!(!task.is_finished());
    drop(held);
    assert_eq!(task.await.unwrap().unwrap()["mode"], "file");
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
