#[path = "common/source.rs"]
mod source_fixture;

use codexpress_gateway::{
    management,
    model::{AppState, SourceRuntime},
};
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::Path,
    sync::Arc,
};

async fn fixture() -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("state")).unwrap();
    let state = AppState::new(
        directory.path().join("state"),
        17863,
        "PRIVATE-NATIVE-KEY".into(),
    )
    .unwrap();
    // Bind only a fresh ephemeral localhost port. No upstream server is contacted.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    management::dispatch(&state, "chatbot.configure", json!({"port":port}))
        .await
        .unwrap();
    let mut source = source_fixture::api_source(
        "api",
        "API",
        "https://invalid.example/v1",
        "PRIVATE-UPSTREAM-KEY",
    );
    source.models = vec!["gpt-6-astra".into(), "source-model".into()];
    state.sources.write().await.push(SourceRuntime::new(source));
    (directory, state)
}
async fn call(state: &Arc<AppState>, op: &str, home: &Path, extra: Value) -> anyhow::Result<Value> {
    let mut payload = extra;
    payload["agent_dir"] = json!(home);
    management::dispatch(state, op, payload).await
}
fn yaml(path: &Path) -> Value {
    serde_yaml::from_slice(&fs::read(path).unwrap()).unwrap()
}
async fn apply(state: &Arc<AppState>, home: &Path) -> Value {
    let attached = call(state, "omp.attach", home, json!({"model_id":"gpt-6-astra"}))
        .await
        .unwrap();
    configure(state, home, &attached, json!([
        {"file":"config","path":["compaction","modelThresholds","codexpress/gpt-6-astra"],"value":"f180000"},
        {"file":"models","path":["providers","codexpress","modelOverrides","gpt-6-astra","contextWindow"],"value":300000}
    ])).await.unwrap()
}

async fn configure(
    state: &Arc<AppState>,
    home: &Path,
    status: &Value,
    edits: Value,
) -> anyhow::Result<Value> {
    call(
        state,
        "omp.configure",
        home,
        json!({
            "expected_revision":status["configuration_revision"], "edits":edits
        }),
    )
    .await
}

#[tokio::test]
async fn read_is_safe_readonly_without_attachment_or_chatbot_and_ignores_other_attachment() {
    let (directory, state) = fixture().await;
    management::dispatch(&state, "chatbot.configure", json!({"enabled":false})).await.unwrap();
    let home = directory.path().canonicalize().unwrap().join("local-settings");
    fs::create_dir(&home).unwrap();
    let models = "# private providers stay private\nproviders:\n  personal: {apiKey: PRIVATE-PERSONAL-KEY}\n  codexpress:\n    apiKey: PRIVATE-CONNECTION-KEY\n    headers: {Authorization: PRIVATE-HEADER}\n    models: [{id: gpt-6-astra, contextWindow: 300000, apiKey: PRIVATE-MODEL-KEY}]\n    modelOverrides: {gpt-6-astra: {maxTokens: 8192}}\n";
    let settings = "# local preferences\nmodelRoles: {default: 'codexpress/gpt-6-astra:xhigh'}\ncompaction: {enabled: false, reserveTokens: 456, apiKey: PRIVATE-COMPACTION-KEY}\ncustom: PRIVATE-CUSTOM\n";
    fs::write(home.join("models.yaml"), models).unwrap();
    fs::write(home.join("config.yaml"), settings).unwrap();
    // These are deliberately unreadable as configuration files. Only migration
    // metadata may be inspected; credentials, sessions and database bodies are not inputs.
    for name in ["auth.json", "sessions", "history.jsonl", "agent.db"] {
        fs::create_dir(home.join(name)).unwrap();
    }
    let first = call(&state, "omp.read", &home, json!({})).await.unwrap();
    assert_eq!(first["agent_dir"], json!(home));
    assert_eq!(first["model_id"], "gpt-6-astra:xhigh");
    assert_eq!(first["configuration"]["compaction"], json!({"enabled":false,"reserveTokens":456}));
    assert_eq!(first["configuration"]["models"], json!([
        {"id":"gpt-6-astra","base":{"contextWindow":300000},"overrides":{"maxTokens":8192}}
    ]));
    assert!(!first.to_string().contains("PRIVATE"));
    assert!(!directory.path().join("state/omp").exists());
    assert_eq!(fs::read_to_string(home.join("models.yaml")).unwrap(), models);
    assert_eq!(fs::read_to_string(home.join("config.yaml")).unwrap(), settings);
    let missing = directory.path().join("empty-not-created");
    let empty = call(&state, "omp.read", &missing, json!({})).await.unwrap();
    assert!(empty["model_id"].is_null());
    assert_eq!(empty["configuration"], json!({"compaction":{},"providerRemoteCompaction":{},"models":[]}));
    assert!(!missing.exists());
    let available = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = available.local_addr().unwrap().port();
    drop(available);
    management::dispatch(&state, "chatbot.configure", json!({"enabled":true,"port":port})).await.unwrap();
    apply(&state, &directory.path().join("attached-elsewhere")).await;
    let snapshot_path = directory.path().join("state/omp/attachment.json");
    let snapshot_before = fs::read(&snapshot_path).unwrap();
    management::dispatch(&state, "chatbot.configure", json!({"enabled":false})).await.unwrap();
    assert_eq!(call(&state, "omp.read", &home, json!({})).await.unwrap(), first);
    assert_eq!(fs::read(&snapshot_path).unwrap(), snapshot_before);
    assert_eq!(fs::read_to_string(home.join("models.yaml")).unwrap(), models);
    assert_eq!(fs::read_to_string(home.join("config.yaml")).unwrap(), settings);
}

#[tokio::test]
async fn merge_preserves_providers_defaults_and_compression_and_never_returns_keys() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    fs::create_dir(&home).unwrap();
    let original_models = "# saved comment\nproviders:\n  personal:\n    apiKey: PRIVATE-PERSONAL-KEY\n    models: [{id: keep}]\ncustom: [one, two]\n";
    let original_settings = "# settings\nmodelRoles:\n  default: personal/keep\n  smol: personal/fast\ncompaction:\n  enabled: false\n  reserveTokens: 1234\n  modelThresholds:\n    personal/keep: 80%\n    codexpress/gpt-6-astra: 75%\nunknown:\n  keep: true\n";
    fs::write(home.join("models.yml"), original_models).unwrap();
    fs::write(home.join("config.yml"), original_settings).unwrap();
    fs::write(home.join("auth.json"), "DO NOT OPEN AUTH").unwrap();
    fs::create_dir(home.join("sessions")).unwrap();
    fs::write(
        home.join("sessions").join("keep.jsonl"),
        "DO NOT OPEN SESSIONS",
    )
    .unwrap();
    let attached = apply(&state, &home).await;
    let models = yaml(&home.join("models.yml"));
    let settings = yaml(&home.join("config.yml"));
    assert_eq!(
        models["providers"]["personal"]["apiKey"],
        "PRIVATE-PERSONAL-KEY"
    );
    assert_eq!(models["providers"]["codexpress"]["api"], "openai-responses");
    assert_eq!(models["providers"]["codexpress"]["auth"], "apiKey");
    let key = management::dispatch(&state, "chatbot.key", json!({}))
        .await
        .unwrap();
    assert_eq!(models["providers"]["codexpress"]["apiKey"], key["key"]);
    assert_eq!(settings["modelRoles"]["default"], "codexpress/gpt-6-astra");
    assert_eq!(settings["modelRoles"]["smol"], "personal/fast");
    assert_eq!(settings["compaction"]["enabled"], false);
    assert_eq!(settings["compaction"]["reserveTokens"], 1234);
    assert_eq!(
        settings["compaction"]["modelThresholds"]["personal/keep"],
        "80%"
    );
    assert_eq!(
        settings["compaction"]["modelThresholds"]["codexpress/gpt-6-astra"],
        "f180000"
    );
    assert_eq!(attached["model_count"], 2);
    assert_eq!(attached["needs_update"], false);
    assert!(!attached.to_string().contains(key["key"].as_str().unwrap()));
    assert!(!attached.to_string().contains("PRIVATE"));
    let status = call(&state, "omp.status", &home, json!({})).await.unwrap();
    assert!(!status.to_string().contains(key["key"].as_str().unwrap()));
    assert!(!status.to_string().contains("PRIVATE"));
    assert_eq!(
        fs::metadata(home.join("models.yml"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(directory.path().join("state/omp"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(directory.path().join("state/omp/attachment.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    call(&state, "omp.restore", &home, json!({})).await.unwrap();
    assert_eq!(
        fs::read(home.join("models.yml")).unwrap(),
        original_models.as_bytes()
    );
    assert_eq!(
        fs::read(home.join("config.yml")).unwrap(),
        original_settings.as_bytes()
    );
    assert_eq!(
        fs::read_to_string(home.join("auth.json")).unwrap(),
        "DO NOT OPEN AUTH"
    );
    assert_eq!(
        fs::read_to_string(home.join("sessions/keep.jsonl")).unwrap(),
        "DO NOT OPEN SESSIONS"
    );
}

#[tokio::test]
async fn repeated_apply_updates_local_key_and_rules_but_restores_first_bytes() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    fs::create_dir(&home).unwrap();
    let original_models = b"# original\nproviders: {}\n";
    let original_settings = b"modelRoles: {default: prior/model}\ncompaction: {modelThresholds: {codexpress/gpt-6-astra: null}}\n";
    fs::write(home.join("models.yml"), original_models).unwrap();
    fs::write(home.join("config.yml"), original_settings).unwrap();
    apply(&state, &home).await;
    management::dispatch(&state, "chatbot.configure", json!({"regenerate_key":true}))
        .await
        .unwrap();
    assert_eq!(
        call(&state, "omp.status", &home, json!({})).await.unwrap()["needs_update"],
        true
    );
    let attached = call(
        &state,
        "omp.attach",
        &home,
        json!({"model_id":"explicit-model"}),
    )
    .await
    .unwrap();
    assert_eq!(attached["model_id"], "explicit-model");
    assert_eq!(attached["needs_update"], false);
    configure(&state, &home, &attached, json!([
        {"file":"config","path":["compaction","modelThresholds","codexpress/gpt-6-astra"],"remove":true},
        {"file":"config","path":["compaction","modelThresholds","codexpress/source-model"],"value":"f9999"}
    ])).await.unwrap();
    let settings = yaml(&home.join("config.yml"));
    assert!(settings["compaction"]["modelThresholds"]["codexpress/gpt-6-astra"].is_null());
    assert_eq!(
        settings["compaction"]["modelThresholds"]["codexpress/source-model"],
        "f9999"
    );
    call(&state, "omp.restore", &home, json!({})).await.unwrap();
    assert_eq!(fs::read(home.join("models.yml")).unwrap(), original_models);
    assert_eq!(
        fs::read(home.join("config.yml")).unwrap(),
        original_settings
    );
}

#[tokio::test]
async fn missing_files_and_read_only_legacy_bases_restore_missing_yaml() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("new").join("agent");
    assert_eq!(
        call(&state, "omp.status", &home, json!({})).await.unwrap()["attached"],
        false
    );
    assert!(!home.exists());
    apply(&state, &home).await;
    call(&state, "omp.restore", &home, json!({})).await.unwrap();
    assert!(!home.join("models.yml").exists());
    assert!(!home.join("config.yml").exists());
    let legacy_models = br#"{"providers":{"legacy":{"apiKey":"PRIVATE-LEGACY-KEY","models":[{"id":"legacy-model"}]}},"custom":true}"#;
    let legacy_settings = br#"{"modelRoles":{"default":"legacy/legacy-model"},"custom":[1,2,3]}"#;
    fs::write(home.join("models.json"), legacy_models).unwrap();
    fs::write(home.join("settings.json"), legacy_settings).unwrap();
    apply(&state, &home).await;
    assert_eq!(
        yaml(&home.join("models.yml"))["providers"]["legacy"]["apiKey"],
        "PRIVATE-LEGACY-KEY"
    );
    assert_eq!(yaml(&home.join("config.yml"))["custom"], json!([1, 2, 3]));
    call(&state, "omp.restore", &home, json!({})).await.unwrap();
    assert!(!home.join("models.yml").exists());
    assert!(!home.join("config.yml").exists());
    assert_eq!(fs::read(home.join("models.json")).unwrap(), legacy_models);
    assert_eq!(
        fs::read(home.join("settings.json")).unwrap(),
        legacy_settings
    );
}

#[tokio::test]
async fn existing_yaml_extension_is_respected_and_yml_takes_precedence() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("models.yaml"), "custom: yaml\n").unwrap();
    fs::write(home.join("config.yaml"), "custom: yaml\n").unwrap();
    let result = apply(&state, &home).await;
    assert!(result["models_file"]
        .as_str()
        .unwrap()
        .ends_with("models.yaml"));
    assert!(result["settings_file"]
        .as_str()
        .unwrap()
        .ends_with("config.yaml"));
    assert!(!home.join("models.yml").exists());
    call(&state, "omp.restore", &home, json!({})).await.unwrap();
    fs::write(home.join("models.yml"), "custom: yml\n").unwrap();
    fs::write(home.join("config.yml"), "custom: yml\n").unwrap();
    apply(&state, &home).await;
    assert_eq!(yaml(&home.join("models.yml"))["custom"], "yml");
    assert_eq!(
        fs::read_to_string(home.join("models.yaml")).unwrap(),
        "custom: yaml\n"
    );
}

#[tokio::test]
async fn external_unmanaged_changes_survive_restore_and_owned_changes_refuse() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    apply(&state, &home).await;
    let mut models = yaml(&home.join("models.yml"));
    models["providers"]["later"] = json!({"apiKey":"PRIVATE-ADDED-KEY"});
    models["later"] = json!(true);
    fs::write(
        home.join("models.yml"),
        serde_yaml::to_string(&models).unwrap(),
    )
    .unwrap();
    let mut settings = yaml(&home.join("config.yml"));
    settings["later"] = json!({"untouched":true});
    settings["compaction"]["modelThresholds"]["later/model"] = json!("60%");
    fs::write(
        home.join("config.yml"),
        serde_yaml::to_string(&settings).unwrap(),
    )
    .unwrap();
    call(&state, "omp.restore", &home, json!({})).await.unwrap();
    assert!(yaml(&home.join("models.yml"))["providers"]
        .get("codexpress")
        .is_none());
    assert_eq!(
        yaml(&home.join("models.yml"))["providers"]["later"]["apiKey"],
        "PRIVATE-ADDED-KEY"
    );
    assert_eq!(
        yaml(&home.join("config.yml"))["compaction"]["modelThresholds"]["later/model"],
        "60%"
    );
    apply(&state, &home).await;
    let mut models = yaml(&home.join("models.yml"));
    models["providers"]["codexpress"]["baseUrl"] = json!("http://127.0.0.1:1/v1");
    fs::write(
        home.join("models.yml"),
        serde_yaml::to_string(&models).unwrap(),
    )
    .unwrap();
    let before_models = fs::read(home.join("models.yml")).unwrap();
    let before_settings = fs::read(home.join("config.yml")).unwrap();
    assert!(call(&state, "omp.restore", &home, json!({})).await.is_err());
    assert!(call(&state, "omp.attach", &home, json!({"model_id":"m"}))
        .await
        .is_err());
    assert_eq!(fs::read(home.join("models.yml")).unwrap(), before_models);
    assert_eq!(fs::read(home.join("config.yml")).unwrap(), before_settings);
}

#[tokio::test]
async fn invalid_or_symlinked_files_and_foreign_provider_are_never_overwritten() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    fs::create_dir(&home).unwrap();
    let malformed = "providers: [PRIVATE-KEY-WITH-SENSITIVE-PARSER-LINE\n";
    fs::write(home.join("models.yml"), malformed).unwrap();
    let error = call(&state, "omp.attach", &home, json!({"model_id":"m"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("PRIVATE"));
    assert_eq!(
        fs::read_to_string(home.join("models.yml")).unwrap(),
        malformed
    );
    fs::remove_file(home.join("models.yml")).unwrap();
    let target = directory.path().join("target.yml");
    fs::write(&target, "providers: {}\n").unwrap();
    symlink(&target, home.join("models.yml")).unwrap();
    assert!(call(&state, "omp.attach", &home, json!({"model_id":"m"}))
        .await
        .is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "providers: {}\n");
    fs::remove_file(home.join("models.yml")).unwrap();
    fs::write(
        home.join("models.yml"),
        "providers: {codexpress: {apiKey: PRIVATE-USER-KEY}}\n",
    )
    .unwrap();
    assert!(call(&state, "omp.attach", &home, json!({"model_id":"m"}))
        .await
        .is_err());
    assert!(!home.join("config.yml").exists());
}

#[tokio::test]
async fn second_document_validation_happens_before_any_write_and_requires_running_ingress() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("models.yml"), "providers: {}\n").unwrap();
    fs::write(home.join("config.yml"), "modelRoles: invalid\n").unwrap();
    assert!(call(&state, "omp.attach", &home, json!({"model_id":"m"}))
        .await
        .is_err());
    assert_eq!(
        fs::read_to_string(home.join("models.yml")).unwrap(),
        "providers: {}\n"
    );
    assert!(!directory.path().join("state/omp/attachment.json").exists());
    fs::remove_file(home.join("config.yml")).unwrap();
    management::dispatch(&state, "chatbot.configure", json!({"enabled":false}))
        .await
        .unwrap();
    assert!(call(&state, "omp.attach", &home, json!({"model_id":"m"}))
        .await
        .is_err());
    assert!(!home.join("config.yml").exists());
}

#[tokio::test]
async fn unmanaged_changes_between_applications_survive_the_final_restore() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    apply(&state, &home).await;
    let mut models = yaml(&home.join("models.yml"));
    models["providers"]["added"] = json!({"apiKey":"PRIVATE-LATER-KEY","models":[{"id":"later"}]});
    fs::write(
        home.join("models.yml"),
        serde_yaml::to_string(&models).unwrap(),
    )
    .unwrap();
    let mut settings = yaml(&home.join("config.yml"));
    settings["added"] = json!({"keep":true});
    fs::write(
        home.join("config.yml"),
        serde_yaml::to_string(&settings).unwrap(),
    )
    .unwrap();
    apply(&state, &home).await;
    call(&state, "omp.restore", &home, json!({})).await.unwrap();
    let models = yaml(&home.join("models.yml"));
    assert!(models["providers"].get("codexpress").is_none());
    assert_eq!(models["providers"]["added"]["apiKey"], "PRIVATE-LATER-KEY");
    let settings = yaml(&home.join("config.yml"));
    assert_eq!(settings["added"]["keep"], true);
    assert!(settings.get("modelRoles").is_none());
    assert!(settings.get("compaction").is_none());
}

#[tokio::test]
async fn legacy_database_requires_omps_own_migration_before_creating_primary_yaml() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("agent.db"), "DO NOT OPEN DATABASE").unwrap();
    fs::write(home.join("settings.json"), "{\"custom\":true}").unwrap();
    assert_eq!(
        call(&state, "omp.status", &home, json!({})).await.unwrap()["attached"],
        false
    );
    let error = call(&state, "omp.attach", &home, json!({"model_id":"m"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("迁移"));
    let read_error = call(&state, "omp.read", &home, json!({}))
        .await.unwrap_err().to_string();
    assert!(read_error.contains("迁移"));
    assert!(!home.join("config.yml").exists());
    assert!(!home.join("models.yml").exists());
    assert_eq!(
        fs::read_to_string(home.join("agent.db")).unwrap(),
        "DO NOT OPEN DATABASE"
    );
    fs::write(home.join("config.yml"), "custom: migrated\n").unwrap();
    let migrated = call(&state, "omp.read", &home, json!({})).await.unwrap();
    assert_eq!(migrated["configuration"]["models"], json!([]));
    apply(&state, &home).await;
    assert_eq!(yaml(&home.join("config.yml"))["custom"], "migrated");
}

#[tokio::test]
async fn nullable_containers_restore_original_bytes() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    fs::create_dir(&home).unwrap();
    let models = b"# null providers\nproviders: null\n";
    let settings = b"# null containers\nmodelRoles: null\ncompaction: {modelThresholds: null}\n";
    fs::write(home.join("models.yml"), models).unwrap();
    fs::write(home.join("config.yml"), settings).unwrap();
    apply(&state, &home).await;
    call(&state, "omp.restore", &home, json!({})).await.unwrap();
    assert_eq!(fs::read(home.join("models.yml")).unwrap(), models);
    assert_eq!(fs::read(home.join("config.yml")).unwrap(), settings);
}

#[tokio::test]
async fn filtering_the_selected_provider_is_rejected_before_mutation() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    fs::create_dir(&home).unwrap();
    for settings in [
        "enabledModels: [openai/*]\n",
        "enabledModels: [gpt-6-astra]\n",
        "disabledProviders: [codexpress]\nenabledModels: [codexpress/*]\n",
    ] {
        fs::write(home.join("config.yml"), settings).unwrap();
        let error = call(
            &state,
            "omp.attach",
            &home,
            json!({"model_id":"gpt-6-astra"}),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("enabledModels") || error.contains("disabledProviders"));
        assert!(!home.join("models.yml").exists());
        assert_eq!(
            fs::read_to_string(home.join("config.yml")).unwrap(),
            settings
        );
    }
    fs::write(
        home.join("config.yml"),
        "enabledModels: [CODEXPRESS/GPT-6-ASTRA]\n",
    )
    .unwrap();
    apply(&state, &home).await;
    call(&state, "omp.restore", &home, json!({})).await.unwrap();
    fs::write(home.join("config.yml"), "enabledModels: [codexpress/*]\n").unwrap();
    assert!(call(
        &state,
        "omp.attach",
        &home,
        json!({"model_id":"vendor/model"})
    )
    .await
    .is_err());
    fs::write(home.join("config.yml"), "enabledModels: [codexpress/**]\n").unwrap();
    call(
        &state,
        "omp.attach",
        &home,
        json!({"model_id":"vendor/model"}),
    )
    .await
    .unwrap();
    assert_eq!(
        yaml(&home.join("config.yml"))["enabledModels"],
        json!(["codexpress/**"])
    );
}

#[tokio::test]
async fn attachment_leaves_native_footer_and_unmanaged_extensions_alone() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    let extensions = home.join("extensions");
    fs::create_dir_all(&extensions).unwrap();
    let plugin = extensions.join("codexpress-status.ts");
    fs::write(&plugin, "// independently installed extension\n").unwrap();
    let original = json!({"preset":"custom","contextLine":"embedded","leftSegments":["model","status"],"rightSegments":["time"],"segmentOptions":{"path":{"maxLength":19}}});
    fs::write(
        home.join("config.yml"),
        serde_yaml::to_string(&json!({"statusLine":original})).unwrap(),
    )
    .unwrap();
    apply(&state, &home).await;
    assert_eq!(yaml(&home.join("config.yml"))["statusLine"], original);
    call(&state, "omp.restore", &home, json!({})).await.unwrap();
    assert_eq!(yaml(&home.join("config.yml"))["statusLine"], original);
    assert_eq!(
        fs::read_to_string(plugin).unwrap(),
        "// independently installed extension\n"
    );
}

#[tokio::test]
async fn legacy_footer_retirement_restores_native_layout_and_protects_user_edits() {
    for plugin_missing in [false, true] {
        let (directory, state) = fixture().await;
        let home = directory.path().join("agent");
        fs::create_dir_all(home.join("extensions")).unwrap();
        let original =
            json!({"preset":"custom","leftSegments":["model","status"],"rightSegments":["time"]});
        fs::write(
            home.join("config.yml"),
            serde_yaml::to_string(&json!({"statusLine":original})).unwrap(),
        )
        .unwrap();
        let before = apply(&state, &home).await;
        let snapshot_path = directory.path().join("state/omp/attachment.json");
        let mut snapshot: Value =
            serde_json::from_slice(&fs::read(&snapshot_path).unwrap()).unwrap();
        let mut managed = yaml(&home.join("config.yml"));
        managed["statusLine"]["leftSegments"] = json!(["model", "path", "status"]);
        let managed_bytes = serde_yaml::to_string(&managed).unwrap().into_bytes();
        fs::write(home.join("config.yml"), &managed_bytes).unwrap();
        snapshot["settings"]["applied"] = json!(managed_bytes);
        snapshot["footer_before"] =
            json!({"leftSegments":serde_yaml::to_string(&original["leftSegments"]).unwrap()});
        let plugin_bytes = b"// formerly managed source footer\n";
        snapshot["status_plugin"] = json!(plugin_bytes.to_vec());
        fs::write(&snapshot_path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let plugin = home.join("extensions/codexpress-status.ts");
        if !plugin_missing {
            fs::write(&plugin, "// later user edit\n").unwrap();
            assert!(call(
                &state,
                "omp.attach",
                &home,
                json!({"model_id":"gpt-6-astra"})
            )
            .await
            .is_err());
            assert!(call(&state, "omp.restore", &home, json!({})).await.is_err());
            assert_eq!(fs::read(home.join("config.yml")).unwrap(), managed_bytes);
            assert_eq!(fs::read_to_string(&plugin).unwrap(), "// later user edit\n");
            fs::write(&plugin, plugin_bytes).unwrap();
        } else {
            // Already removed locally while the old gateway was still draining.
            let mut restored = yaml(&home.join("config.yml"));
            restored["statusLine"] = original.clone();
            fs::write(
                home.join("config.yml"),
                serde_yaml::to_string(&restored).unwrap(),
            )
            .unwrap();
        }
        let after = call(
            &state,
            "omp.attach",
            &home,
            json!({"model_id":"gpt-6-astra"}),
        )
        .await
        .unwrap();
        assert_eq!(after["needs_update"], false);
        assert_eq!(after["configuration"], before["configuration"]);
        assert!(!plugin.exists());
        assert_eq!(yaml(&home.join("config.yml"))["statusLine"], original);
        let saved: Value = serde_json::from_slice(&fs::read(snapshot_path).unwrap()).unwrap();
        assert!(saved.get("status_plugin").is_none());
        assert!(saved.get("footer_before").is_none());
        let mut changed = yaml(&home.join("config.yml"));
        changed["statusLine"]["leftSegments"] = json!(["status", "model"]);
        fs::write(
            home.join("config.yml"),
            serde_yaml::to_string(&changed).unwrap(),
        )
        .unwrap();
        apply(&state, &home).await;
        call(&state, "omp.restore", &home, json!({})).await.unwrap();
        assert_eq!(
            yaml(&home.join("config.yml"))["statusLine"]["leftSegments"],
            json!(["status", "model"])
        );
    }
}

#[tokio::test]
async fn saved_reasoning_preference_does_not_block_upgrade_or_become_an_upstream_model() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    apply(&state, &home).await;
    let mut settings = yaml(&home.join("config.yml"));
    settings["modelRoles"]["default"] = json!("codexpress/gpt-6-astra:xhigh");
    fs::write(
        home.join("config.yml"),
        serde_yaml::to_string(&settings).unwrap(),
    )
    .unwrap();
    assert_eq!(
        call(&state, "omp.status", &home, json!({})).await.unwrap()["needs_update"],
        false
    );
    let result = call(
        &state,
        "omp.attach",
        &home,
        json!({"model_id":"gpt-6-astra:xhigh"}),
    )
    .await
    .unwrap();
    assert_eq!(result["model_id"], "gpt-6-astra:xhigh");
    let ids: Vec<_> = yaml(&home.join("models.yml"))["providers"]["codexpress"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|model| model["id"].as_str().map(str::to_owned))
        .collect();
    assert!(ids.contains(&"gpt-6-astra".to_owned()));
    assert!(!ids.contains(&"gpt-6-astra:xhigh".to_owned()));
    assert_eq!(
        yaml(&home.join("config.yml"))["modelRoles"]["default"],
        "codexpress/gpt-6-astra:xhigh"
    );
    call(&state, "omp.restore", &home, json!({})).await.unwrap();
    assert!(!home.join("models.yml").exists());
    assert!(!home.join("config.yml").exists());
}

#[tokio::test]
async fn restoring_gateway_keeps_a_later_non_codexpress_default_model() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    apply(&state, &home).await;
    let mut settings = yaml(&home.join("config.yml"));
    settings["modelRoles"]["default"] = json!("personal/other-model:high");
    fs::write(
        home.join("config.yml"),
        serde_yaml::to_string(&settings).unwrap(),
    )
    .unwrap();
    call(&state, "omp.restore", &home, json!({})).await.unwrap();
    assert_eq!(
        yaml(&home.join("config.yml"))["modelRoles"]["default"],
        "personal/other-model:high"
    );
    assert!(!home.join("models.yml").exists());
    assert!(!home.join("extensions/codexpress-status.ts").exists());
}

#[tokio::test]
async fn precise_edits_reject_stale_views_and_preserve_unmanaged_provider_preferences() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    fs::create_dir(&home).unwrap();
    fs::write(
        home.join("config.yml"),
        "# retained\ncompaction:\n  modelThresholds:\n    codexpress/*: 80% # untouched selector\n",
    )
    .unwrap();
    let attached = call(
        &state,
        "omp.attach",
        &home,
        json!({"model_id":"gpt-6-astra"}),
    )
    .await
    .unwrap();
    let status = configure(&state, &home, &attached, json!([
        {"file":"config","path":["compaction","modelThresholds","codexpress/gpt-6-astra"],"value":null},
        {"file":"models","path":["providers","codexpress","modelOverrides","gpt-6-astra","contextWindow"],"value":300000}
    ])).await.unwrap();
    let mut models = yaml(&home.join("models.yml"));
    models["providers"]["codexpress"]["modelOverrides"]["gpt-6-astra"]["maxTokens"] = json!(8192);
    fs::write(
        home.join("models.yml"),
        serde_yaml::to_string(&models).unwrap(),
    )
    .unwrap();
    let before = fs::read(home.join("config.yml")).unwrap();
    let model_bytes = fs::read(home.join("models.yml")).unwrap();
    assert!(configure(
        &state,
        &home,
        &status,
        json!([
            {"file":"config","path":["compaction","enabled"],"value":false}
        ])
    )
    .await
    .is_err());
    assert_eq!(fs::read(home.join("config.yml")).unwrap(), before);
    assert_eq!(fs::read(home.join("models.yml")).unwrap(), model_bytes);
    let fresh = call(&state, "omp.status", &home, json!({})).await.unwrap();
    configure(&state, &home, &fresh, json!([
        {"file":"config","path":["compaction","modelThresholds","codexpress/gpt-6-astra"],"remove":true}
    ])).await.unwrap();
    let settings = yaml(&home.join("config.yml"));
    assert!(settings["compaction"]["modelThresholds"]
        .get("codexpress/gpt-6-astra")
        .is_none());
    assert_eq!(
        settings["compaction"]["modelThresholds"]["codexpress/*"],
        "80%"
    );
    call(
        &state,
        "omp.attach",
        &home,
        json!({"model_id":"gpt-6-astra"}),
    )
    .await
    .unwrap();
    assert_eq!(
        yaml(&home.join("models.yml"))["providers"]["codexpress"]["modelOverrides"]["gpt-6-astra"]
            ["maxTokens"],
        8192
    );
    call(&state, "omp.restore", &home, json!({})).await.unwrap();
    let provider = yaml(&home.join("models.yml"))["providers"]["codexpress"].clone();
    assert_eq!(provider["modelOverrides"]["gpt-6-astra"]["maxTokens"], 8192);
    assert!(provider.get("apiKey").is_none());
    assert!(provider["modelOverrides"]["gpt-6-astra"]
        .get("contextWindow")
        .is_none());
    assert!(fs::read_to_string(home.join("config.yml"))
        .unwrap()
        .contains("codexpress/*: 80% # untouched selector"));
}

fn remote_bundle() -> Value {
    json!({
        "agent_dir":"/remote-only/omp-agent", "agent_db_exists":false,
        "files":{"models.yml":null,"models.yaml":null,"models.json":null,
            "config.yml":null,"config.yaml":null,"settings.json":null},
        "snapshot":null,"host":"127.0.0.1","model_id":"gpt-6-astra"
    })
}

fn remote_bytes(bundle: &Value, name: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    bundle["files"][name].as_str().map(|value|
        base64::engine::general_purpose::STANDARD.decode(value).unwrap())
}

fn remote_text(bundle: &Value, name: &str) -> String {
    String::from_utf8(remote_bytes(bundle, name).unwrap()).unwrap()
}

fn remote_set(bundle: &mut Value, name: &str, text: &str) {
    use base64::Engine;
    bundle["files"][name] = json!(base64::engine::general_purpose::STANDARD.encode(text));
}

fn remote_next(request: &Value, output: &Value) -> Value {
    let mut next = request.clone();
    next["files"] = output["files"].clone();
    next["snapshot"] = output["snapshot"].clone();
    next
}

#[tokio::test]
async fn remote_read_needs_no_connection_settings_or_running_client_and_never_writes() {
    let (directory, state) = fixture().await;
    management::dispatch(&state, "chatbot.configure", json!({"enabled":false})).await.unwrap();
    let home = directory.path().join("remote-read-only");
    let mut request = remote_bundle();
    request["agent_dir"] = json!(home);
    let fields = request.as_object_mut().unwrap();
    fields.remove("model_id");
    fields.remove("host");
    fields.remove("snapshot");
    let empty = management::dispatch(&state, "omp.remote.read", request.clone()).await.unwrap();
    assert!(empty["model_id"].is_null());
    assert_eq!(empty["configuration"]["compaction"], json!({}));
    let ids: Vec<_> = empty["configuration"]["models"].as_array().unwrap().iter()
        .filter_map(|model| model["id"].as_str()).collect();
    assert!(ids.contains(&"gpt-6-astra") && ids.contains(&"source-model"));
    assert!(!home.exists());
    assert!(!directory.path().join("state/omp").exists());
    assert!(empty.get("key").is_none() && empty.get("base_url").is_none());
    remote_set(&mut request, "config.yml", "modelRoles: {default: 'codexpress/gpt-6-astra:xhigh'}\ncompaction: {enabled: false, reserveTokens: 456}\n");
    remote_set(&mut request, "models.yml", "providers:\n  personal: {apiKey: PRIVATE-OTHER}\n  codexpress:\n    apiKey: PRIVATE-REMOTE\n    headers: {Authorization: PRIVATE-HEADER}\n    models: [{id: gpt-6-astra, contextWindow: 300000}]\n");
    let existing = management::dispatch(&state, "omp.remote.read", request.clone()).await.unwrap();
    assert_eq!(existing["model_id"], "gpt-6-astra:xhigh");
    assert_eq!(existing["configuration"]["compaction"], json!({"enabled":false,"reserveTokens":456}));
    assert!(!existing.to_string().contains("PRIVATE"));
    assert_ne!(existing["configuration_revision"], empty["configuration_revision"]);
    request["files"]["config.yml"] = Value::Null;
    request["agent_db_exists"] = json!(true);
    assert!(management::dispatch(&state, "omp.remote.read", request).await.is_err());
    assert!(!home.exists());
}

#[tokio::test]
async fn remote_fresh_bundle_is_pure_and_restores_without_running_chatbot() {
    let (directory, state) = fixture().await;
    let mut request = remote_bundle();
    let home = directory.path().join("never-created").join("remote-agent");
    request["agent_dir"] = json!(home);
    request["model_id"] = json!("gpt-6-astra:xhigh");
    let output = management::dispatch(&state, "omp.remote.preview", request.clone()).await.unwrap();
    assert!(!home.exists());
    assert!(!directory.path().join("state/omp/attachment.json").exists());
    assert_eq!(output["files"].as_object().unwrap().len(), 6);
    let models: Value = serde_yaml::from_str(&remote_text(&output, "models.yml")).unwrap();
    assert_eq!(models["providers"]["codexpress"]["apiKey"], output["key"]);
    assert_ne!(output["key"], "PRIVATE-NATIVE-KEY");
    assert_eq!(models["providers"]["codexpress"]["baseUrl"], output["base_url"]);
    assert!(models["providers"]["codexpress"]["models"].as_array().unwrap().iter()
        .any(|model| model["id"] == "source-model"));
    let settings: Value = serde_yaml::from_str(&remote_text(&output, "config.yml")).unwrap();
    assert_eq!(settings["modelRoles"]["default"], "codexpress/gpt-6-astra:xhigh");
    assert!(settings.get("compaction").is_none());
    management::dispatch(&state, "chatbot.configure", json!({"enabled":false})).await.unwrap();
    let next = remote_next(&request, &output);
    assert!(management::dispatch(&state, "omp.remote.preview", next.clone()).await.is_err());
    let restored = management::dispatch(&state, "omp.remote.restore", next).await.unwrap();
    assert!(restored.get("key").is_none());
    assert!(restored.get("base_url").is_none());
    assert!(restored["snapshot"].is_null());
    assert_eq!(restored["files"], request["files"]);
    assert!(!home.exists());
}

#[tokio::test]
async fn remote_reapply_preserves_raw_user_edits_and_restores_first_connection_baseline() {
    let (directory, state) = fixture().await;
    let mut request = remote_bundle();
    request["agent_dir"] = json!(directory.path().join("remote-never-opened"));
    let models = "# provider header\nproviders:\n  personal:\n    apiKey: PRIVATE-REMOTE-KEY # keep secret comment\ncustom: original # custom comment\n";
    let settings = "# global settings\nmodelRoles:\n  default: personal/original\ncompaction:\n  enabled: false # untouched compaction\n  reserveTokens: 1234\ncustom: original\n";
    remote_set(&mut request, "models.yaml", models);
    remote_set(&mut request, "config.yaml", settings);
    remote_set(&mut request, "models.json", "{\"legacy\":true}");
    let first = management::dispatch(&state, "omp.remote.preview", request.clone()).await.unwrap();
    let mut next = remote_next(&request, &first);
    remote_set(&mut next, "models.yaml", &remote_text(&first, "models.yaml")
        .replace("custom: original", "custom: edited"));
    remote_set(&mut next, "config.yaml", &(remote_text(&first, "config.yaml") + "# later user comment\n"));
    next["host"] = json!("192.0.2.7");
    management::dispatch(&state, "chatbot.configure", json!({"regenerate_key":true})).await.unwrap();
    let second = management::dispatch(&state, "omp.remote.preview", next.clone()).await.unwrap();
    assert_ne!(second["key"], first["key"]);
    assert!(remote_text(&second, "config.yaml").contains("enabled: false # untouched compaction"));
    let restored = management::dispatch(&state, "omp.remote.restore", remote_next(&next, &second)).await.unwrap();
    let restored_models = remote_text(&restored, "models.yaml");
    assert!(restored_models.contains("# provider header\n"));
    assert!(restored_models.contains("  personal:\n    apiKey: PRIVATE-REMOTE-KEY # keep secret comment\n"));
    assert!(restored_models.contains("custom: edited # custom comment\n"));
    let restored_value: Value = serde_yaml::from_str(&restored_models).unwrap();
    assert!(restored_value["providers"].get("codexpress").is_none());
    let restored_settings = remote_text(&restored, "config.yaml");
    assert!(restored_settings.contains("# later user comment"));
    assert!(restored_settings.contains("default: personal/original"));
    assert!(restored_settings.contains("enabled: false # untouched compaction"));
    assert_eq!(restored["files"]["models.json"], request["files"]["models.json"]);
    assert!(restored["files"]["models.yml"].is_null());
    assert!(restored["snapshot"].is_null());
}

#[tokio::test]
async fn remote_native_edits_reapply_and_restore_first_baseline_preserving_user_changes() {
    let (directory, state) = fixture().await;
    let mut request = remote_bundle();
    let home = directory.path().canonicalize().unwrap().join("remote-config");
    request["agent_dir"] = json!(home);
    request["model_id"] = json!("gpt-6.1-sol");
    request["configuration_edits"] = json!([
        {"file":"config","path":["compaction","enabled"],"value":true},
        {"file":"config","path":["compaction","thresholdTokens"],"remove":true},
        {"file":"config","path":["compaction","modelThresholds","codexpress/old"],"remove":true},
        {"file":"config","path":["compaction","modelThresholds","codexpress/*"],"value":"f470000"},
        {"file":"models","path":["providers","codexpress","remoteCompaction","enabled"],"value":true},
        {"file":"models","path":["providers","codexpress","modelOverrides","gpt-6.1-sol","contextWindow"],"value":700000},
        {"file":"models","path":["providers","codexpress","modelOverrides","gpt-6.1-sol","maxContextWindow"],"value":950000}
    ]);
    let settings = "# original native settings\ncompaction:\n  enabled: false\n  thresholdTokens: 12345\n  reserveTokens: 99\n  modelThresholds:\n    codexpress/old: f20000\n    personal/keep: 65% # unrelated\n";
    remote_set(&mut request, "config.yml", settings);
    let first = management::dispatch(&state, "omp.remote.preview", request.clone()).await.unwrap();
    let config: Value = serde_yaml::from_str(&remote_text(&first, "config.yml")).unwrap();
    assert_eq!(config["compaction"]["enabled"], true);
    assert!(config["compaction"].get("thresholdTokens").is_none());
    assert!(config["compaction"]["modelThresholds"].get("codexpress/old").is_none());
    assert_eq!(config["compaction"]["modelThresholds"]["codexpress/*"], "f470000");
    assert_eq!(config["compaction"]["reserveTokens"], 99);
    let models: Value = serde_yaml::from_str(&remote_text(&first, "models.yml")).unwrap();
    assert_eq!(models["providers"]["codexpress"]["remoteCompaction"]["enabled"], true);
    assert_eq!(models["providers"]["codexpress"]["modelOverrides"]["gpt-6.1-sol"]["contextWindow"], 700000);
    assert!(first["summary"].as_str().unwrap().contains("配置 = 已更改"));
    assert!(!first["summary"].as_str().unwrap().contains(first["key"].as_str().unwrap()));
    assert!(!first["configuration"].to_string().contains(first["key"].as_str().unwrap()));
    // The preview revision must identify the prepared bytes, not the input bundle.
    fs::create_dir(&home).unwrap();
    for name in ["models.yml", "config.yml"] {
        fs::write(home.join(name), remote_bytes(&first, name).unwrap()).unwrap();
    }
    let reread = call(&state, "omp.read", &home, json!({})).await.unwrap();
    assert_eq!(reread["configuration_revision"], first["configuration_revision"]);
    assert_eq!(reread["configuration"], first["configuration"]);
    let mut next = remote_next(&request, &first);
    remote_set(&mut next, "config.yml", &(remote_text(&first, "config.yml") + "custom: later # user change\n"));
    next["configuration_edits"] = json!([
        {"file":"models","path":["providers","codexpress","modelOverrides","gpt-6.1-sol","contextWindow"],"value":820000}
    ]);
    let second = management::dispatch(&state, "omp.remote.preview", next.clone()).await.unwrap();
    let models: Value = serde_yaml::from_str(&remote_text(&second, "models.yml")).unwrap();
    assert_eq!(models["providers"]["codexpress"]["modelOverrides"]["gpt-6.1-sol"]["contextWindow"], 820000);
    let unchanged = management::dispatch(&state, "omp.remote.preview", remote_next(&next, &second)).await.unwrap();
    assert!(unchanged["summary"].as_str().unwrap().contains("配置 = 未更改"));
    assert_eq!(unchanged["files"], second["files"]);
    let restored = management::dispatch(&state, "omp.remote.restore", remote_next(&next, &unchanged)).await.unwrap();
    let restored_settings = remote_text(&restored, "config.yml");
    let restored_value: Value = serde_yaml::from_str(&restored_settings).unwrap();
    assert_eq!(restored_value["compaction"], serde_yaml::from_str::<Value>(settings).unwrap()["compaction"]);
    assert!(restored_settings.contains("# original native settings"));
    assert!(restored_settings.contains("personal/keep: 65% # unrelated"));
    assert!(restored_settings.contains("custom: later # user change"));
    assert!(restored["files"]["models.yml"].is_null());
}

#[tokio::test]
async fn remote_invalid_or_conflicting_bundles_fail_without_local_writes_or_secret_errors() {
    let (directory, state) = fixture().await;
    let mut valid = remote_bundle();
    let home = directory.path().join("remote-only");
    valid["agent_dir"] = json!(home);
    let applied = management::dispatch(&state, "omp.remote.preview", valid.clone()).await.unwrap();
    let mut invalid = Vec::new();
    let mut malformed = valid.clone();
    remote_set(&mut malformed, "config.yml", "secret: PRIVATE-YAML-SECRET\ninvalid: [\n");
    invalid.push(malformed);
    let mut foreign = valid.clone();
    remote_set(&mut foreign, "models.yml", "providers: {codexpress: {apiKey: PRIVATE-YAML-SECRET}}\n");
    invalid.push(foreign);
    let mut changed = remote_next(&valid, &applied);
    remote_set(&mut changed, "models.yml", &remote_text(&applied, "models.yml")
        .replace(applied["key"].as_str().unwrap(), "PRIVATE-YAML-SECRET"));
    invalid.push(changed);
    let mut migrating = valid.clone();
    migrating["agent_db_exists"] = json!(true);
    remote_set(&mut migrating, "models.yml", "{}\n");
    invalid.push(migrating);
    let mut unknown_file = valid.clone();
    unknown_file["files"]["auth.json"] = Value::Null;
    invalid.push(unknown_file);
    let mut missing_file = valid.clone();
    missing_file["files"].as_object_mut().unwrap().remove("settings.json");
    invalid.push(missing_file);
    let mut invalid_base64 = valid.clone();
    invalid_base64["files"]["models.yml"] = json!("PRIVATE-YAML-SECRET!");
    invalid.push(invalid_base64);
    let mut invalid_snapshot = valid.clone();
    invalid_snapshot["snapshot"] = json!("e30=");
    invalid.push(invalid_snapshot);
    let mut ipv6 = valid.clone();
    ipv6["host"] = json!("::1");
    invalid.push(ipv6);
    let mut invalid_path = valid.clone();
    invalid_path["agent_dir"] = json!("/remote/../private");
    invalid.push(invalid_path);
    for (edits, guard) in [
        (Value::Null, "修改列表无效"),
        (json!({}), "修改列表无效"),
        (json!([{"file":"config","path":["compaction","enabled"],"value":"PRIVATE-YAML-SECRET"}]), "配置值无效"),
        (json!([{"file":"config","path":["compaction","unknown"],"value":true}]), "不支持"),
        (json!([{"file":"models","path":["providers","codexpress","apiKey"],"value":"PRIVATE-YAML-SECRET"}]), "不支持"),
        (json!([{"file":"models","path":["providers","codexpress","modelOverrides","unknown-model","maxTokens"],"value":1024}]), "不支持"),
        (json!([
            {"file":"models","path":["providers","codexpress","modelOverrides","gpt-6-astra","contextWindow"],"value":900000},
            {"file":"models","path":["providers","codexpress","modelOverrides","gpt-6-astra","maxContextWindow"],"value":600000}
        ]), "maxContextWindow"),
    ] {
        let mut invalid_edits = valid.clone();
        invalid_edits["configuration_edits"] = edits;
        let error = management::dispatch(&state, "omp.remote.preview", invalid_edits).await.unwrap_err().to_string();
        assert!(error.contains(guard), "{error}");
        assert!(!error.contains("PRIVATE-YAML-SECRET"));
    }
    for request in invalid {
        let error = management::dispatch(&state, "omp.remote.preview", request).await.unwrap_err();
        assert!(!error.to_string().contains("PRIVATE-YAML-SECRET"));
    }
    let mut edited = valid.clone();
    edited["configuration_edits"] = json!([
        {"file":"config","path":["compaction","enabled"],"value":true}
    ]);
    let owned = management::dispatch(&state, "omp.remote.preview", edited.clone()).await.unwrap();
    let mut conflicting = remote_next(&edited, &owned);
    remote_set(&mut conflicting, "config.yml", &remote_text(&owned, "config.yml")
        .replace("enabled: true", "enabled: false"));
    for op in ["omp.remote.preview", "omp.remote.restore"] {
        assert!(management::dispatch(&state, op, conflicting.clone()).await.is_err());
    }
    assert!(!home.exists());
    assert!(!directory.path().join("state/omp/attachment.json").exists());
}
