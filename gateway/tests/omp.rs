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
    assert!(!home.join("config.yml").exists());
    assert!(!home.join("models.yml").exists());
    assert_eq!(
        fs::read_to_string(home.join("agent.db")).unwrap(),
        "DO NOT OPEN DATABASE"
    );
    fs::write(home.join("config.yml"), "custom: migrated\n").unwrap();
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
async fn status_extension_upgrade_is_reversible_and_never_overwrites_user_edits() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    let extensions = home.join("extensions");
    fs::create_dir_all(&extensions).unwrap();
    let plugin = extensions.join("codexpress-status.ts");
    fs::write(&plugin, "// user extension\n").unwrap();
    assert!(call(&state, "omp.attach", &home, json!({"model_id":"m"}))
        .await
        .is_err());
    assert!(!home.join("models.yml").exists());
    assert!(!home.join("config.yml").exists());
    assert_eq!(fs::read_to_string(&plugin).unwrap(), "// user extension\n");
    fs::remove_file(&plugin).unwrap();

    apply(&state, &home).await;
    let snapshot_path = directory.path().join("state/omp/attachment.json");
    // Simulate an attachment created by v0.4.0 before extension installation.
    let mut snapshot: Value = serde_json::from_slice(&fs::read(&snapshot_path).unwrap()).unwrap();
    snapshot.as_object_mut().unwrap().remove("status_plugin");
    fs::write(&snapshot_path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    fs::remove_file(&plugin).unwrap();
    let status = call(&state, "omp.status", &home, json!({})).await.unwrap();
    assert_eq!(status["needs_update"], true);
    assert_eq!(status["status_plugin_installed"], false);
    let status = apply(&state, &home).await;
    assert_eq!(status["status_plugin_installed"], true);

    let models = fs::read(home.join("models.yml")).unwrap();
    let settings = fs::read(home.join("config.yml")).unwrap();
    let managed = fs::read(&plugin).unwrap();
    fs::write(&plugin, "// later user edit\n").unwrap();
    assert!(call(&state, "omp.restore", &home, json!({})).await.is_err());
    assert!(call(&state, "omp.attach", &home, json!({"model_id":"m"}))
        .await
        .is_err());
    assert_eq!(fs::read(home.join("models.yml")).unwrap(), models);
    assert_eq!(fs::read(home.join("config.yml")).unwrap(), settings);
    assert_eq!(fs::read_to_string(&plugin).unwrap(), "// later user edit\n");
    fs::write(&plugin, managed).unwrap();
    call(&state, "omp.restore", &home, json!({})).await.unwrap();
    assert!(!plugin.exists());
    assert!(!home.join("models.yml").exists());
    assert!(!home.join("config.yml").exists());
}

#[tokio::test]
async fn compact_context_preserves_user_segments_and_original_display_setting() {
    let (directory, state) = fixture().await;
    let home = directory.path().join("agent");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("config.yml"), "statusLine:\n  preset: custom\n  contextLine: embedded\n  leftSegments: [model, path, context_pct]\n  rightSegments: [tokens]\n").unwrap();
    apply(&state, &home).await;
    let settings = yaml(&home.join("config.yml"));
    assert_eq!(settings["statusLine"]["contextLine"], "off");
    assert_eq!(
        settings["statusLine"]["leftSegments"],
        json!(["model", "path", "context_pct", "status"])
    );
    let mut settings = settings;
    settings["theme"] = json!("dark");
    fs::write(
        home.join("config.yml"),
        serde_yaml::to_string(&settings).unwrap(),
    )
    .unwrap();
    apply(&state, &home).await;
    call(&state, "omp.restore", &home, json!({})).await.unwrap();
    let restored = yaml(&home.join("config.yml"));
    assert_eq!(restored["statusLine"]["contextLine"], "embedded");
    assert_eq!(restored["statusLine"]["rightSegments"], json!(["tokens"]));
    assert_eq!(
        restored["statusLine"]["leftSegments"],
        json!(["model", "path", "context_pct"])
    );
    assert_eq!(restored["theme"], "dark");
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
    assert_eq!(result["status_plugin_installed"], true);
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
