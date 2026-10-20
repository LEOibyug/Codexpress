use codexpress_gateway::compaction::ModelCompactionRule;
use codexpress_gateway::config;
use serde_json::Value;
use std::fs;

fn rule(model: &str, limit: i64, window: Option<i64>) -> ModelCompactionRule {
    ModelCompactionRule {
        model: model.into(),
        auto_compact_token_limit: limit,
        context_window: window,
    }
}

fn active_catalog(home: &std::path::Path) -> Value {
    let doc = fs::read_to_string(home.join("config.toml"))
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    serde_json::from_slice(&fs::read(doc["model_catalog_json"].as_str().unwrap()).unwrap()).unwrap()
}

#[test]
fn per_model_limits_apply_update_disable_and_restore_without_losing_other_settings() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("codex");
    let state = root.path().join("app");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&state).unwrap();
    let original = "model = \"gpt-6.1-sol\"\nmodel_auto_compact_token_limit = 160000\nmodel_context_window = 250000\n[mcp_servers.keep]\ncommand = \"keep-tool\"\n";
    fs::write(home.join("config.toml"), original).unwrap();
    let rules = [
        rule("gpt-6.1-sol", 180000, Some(300000)),
        rule("custom-model", 45000, Some(64000)),
    ];
    let preview =
        config::preview_with_rules(&home, &state, 17863, "private-local-key", &rules).unwrap();
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        original
    );
    assert!(!preview["after"]
        .as_str()
        .unwrap()
        .contains("private-local-key"));
    assert_eq!(preview["model_limits"].as_array().unwrap().len(), 2);
    config::attach_with_rules(&home, &state, 17863, "private-local-key", &rules).unwrap();
    let catalog = active_catalog(&home);
    let models = catalog["models"].as_array().unwrap();
    let configured = models.iter().find(|m| m["slug"] == "gpt-6.1-sol").unwrap();
    assert_eq!(configured["auto_compact_token_limit"], 180000);
    assert_eq!(configured["context_window"], 300000);
    let untouched = models.iter().find(|m| m["slug"] == "gpt-6-astra").unwrap();
    assert_eq!(untouched["auto_compact_token_limit"], 160000);
    assert_eq!(untouched["context_window"], 250000);
    let active = fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(!active.contains("model_auto_compact_token_limit"));
    fs::write(
        home.join("config.toml"),
        format!("{active}\n[mcp_servers.new]\ncommand = \"new-tool\"\n"),
    )
    .unwrap();
    config::attach_with_rules(
        &home,
        &state,
        17863,
        "private-local-key",
        &[rule("custom-model", 40000, Some(64000))],
    )
    .unwrap();
    let updated = active_catalog(&home);
    assert_eq!(
        updated["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["slug"] == "gpt-6.1-sol")
            .unwrap()["auto_compact_token_limit"],
        160000
    );
    config::attach_with_rules(&home, &state, 17863, "private-local-key", &[]).unwrap();
    let disabled = fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(disabled.contains("model_auto_compact_token_limit = 160000"));
    assert!(!disabled.contains("model_catalog_json"));
    config::attach_with_rules(&home, &state, 17863, "private-local-key", &rules).unwrap();
    config::restore(&home, &state).unwrap();
    let restored = fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(restored.contains("model_auto_compact_token_limit = 160000"));
    assert!(restored.contains("model_context_window = 250000"));
    assert!(restored.contains("new-tool"));
    assert!(restored.contains("keep-tool"));
    assert!(!restored.contains("model_catalog_json"));
}

#[test]
fn custom_catalog_preserves_model_capabilities_and_conflicts_are_protected() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("codex");
    let state = root.path().join("app");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&state).unwrap();
    let custom = serde_json::json!({"models":[{"slug":"custom","context_window":100000,"unknown_capability":{"keep":true},"model_messages":{"instructions_template":"preserve me"}}]});
    fs::write(home.join("own.json"), custom.to_string()).unwrap();
    let original = "model_catalog_json = \"own.json\"\n";
    fs::write(home.join("config.toml"), original).unwrap();
    config::attach(&home, &state, 17863, "key").unwrap();
    // Compaction starts owning these fields only when rules are first applied.
    let connected = fs::read_to_string(home.join("config.toml")).unwrap();
    fs::write(
        home.join("config.toml"),
        format!("model_auto_compact_token_limit = 30000\n{connected}"),
    )
    .unwrap();
    let rules = [rule("custom", 80000, None), rule("custom-v2", 70000, None)];
    config::attach_with_rules(&home, &state, 17863, "key", &rules).unwrap();
    let catalog = active_catalog(&home);
    assert_eq!(catalog["models"][0]["unknown_capability"]["keep"], true);
    assert_eq!(
        catalog["models"][0]["model_messages"]["instructions_template"],
        "preserve me"
    );
    assert_eq!(catalog["models"][1]["unknown_capability"]["keep"], true);
    let active = fs::read_to_string(home.join("config.toml")).unwrap();
    fs::write(
        home.join("config.toml"),
        format!("model_auto_compact_token_limit = 1\n{active}"),
    )
    .unwrap();
    assert!(config::attach_with_rules(&home, &state, 17863, "key", &rules).is_err());
    assert!(config::restore(&home, &state).is_err());
    fs::write(home.join("config.toml"), active).unwrap();
    config::restore(&home, &state).unwrap();
    let restored = fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(restored.contains("model_auto_compact_token_limit = 30000"));
    assert!(restored.contains(original.trim()));
    assert!(!restored.contains("model_providers.codexpress"));
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(home.join("own.json")).unwrap()).unwrap(),
        custom
    );
}

#[test]
fn invalid_rules_never_change_config_or_create_attachment() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("app");
    let home = root.path().join("codex");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&state).unwrap();
    let original = "model = \"keep\"\n";
    fs::write(home.join("config.toml"), original).unwrap();
    for rules in [
        vec![rule("same", 1, None), rule("same", 2, None)],
        vec![rule("too-large", 250000, Some(100000))],
        vec![rule("bad", 0, None)],
    ] {
        assert!(config::attach_with_rules(&home, &state, 17863, "key", &rules).is_err());
        assert_eq!(
            fs::read_to_string(home.join("config.toml")).unwrap(),
            original
        );
        assert!(!state.join("config-attachment.json").exists());
    }
}

#[test]
fn attachment_preserves_history_auth_and_unrelated_live_edits() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("codex");
    let state = root.path().join("app");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&state).unwrap();
    let original="# my setup\nmodel = \"keep-model\"\nmodel_provider = \"old\" # chosen\n\n[mcp_servers.custom]\ncommand = \"a tool\"\nargs = [\"--keep\"]\n\n[unknown.nested]\nx = 27\n";
    fs::write(home.join("config.toml"), original).unwrap();
    fs::write(home.join("auth.json"), "untouched").unwrap();
    fs::create_dir_all(home.join("sessions")).unwrap();
    fs::write(home.join("sessions/old.jsonl"), "history").unwrap();
    let preview = config::preview(&home, &state, 17863, "LOCAL-TEST-KEY").unwrap();
    assert!(!preview["after"]
        .as_str()
        .unwrap()
        .contains("LOCAL-TEST-KEY"));
    config::attach(&home, &state, 17863, "LOCAL-TEST-KEY").unwrap();
    let active = fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(active.contains("command = \"a tool\""));
    assert!(active.contains("# chosen"));
    fs::write(
        home.join("config.toml"),
        format!("{active}\n[mcp_servers.new]\ncommand = \"new-tool\"\n"),
    )
    .unwrap();
    config::restore(&home, &state).unwrap();
    let restored = fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(restored.contains("model_provider = \"old\" # chosen"));
    assert!(restored.contains("command = \"new-tool\""));
    assert!(restored.contains("[unknown.nested]"));
    assert!(!restored.contains("[model_providers.codexpress]"));
    assert_eq!(
        fs::read_to_string(home.join("auth.json")).unwrap(),
        "untouched"
    );
    assert_eq!(
        fs::read_to_string(home.join("sessions/old.jsonl")).unwrap(),
        "history"
    );
}

#[test]
fn restore_refuses_external_edits_to_managed_connection() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("codex");
    let state = root.path().join("app");
    fs::create_dir_all(&state).unwrap();
    config::attach(&home, &state, 17863, "test").unwrap();
    let changed = fs::read_to_string(home.join("config.toml"))
        .unwrap()
        .replace("17863", "19999");
    fs::write(home.join("config.toml"), &changed).unwrap();
    assert!(config::restore(&home, &state).is_err());
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        changed
    );
}

#[test]
fn official_gateway_roundtrip_preserves_compaction_tools_auth_and_history() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("codex");
    let state = root.path().join("app");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&state).unwrap();
    let original = "# keep comments\nmodel = \"gpt-6.1-sol\"\nmodel_provider = \"custom\" # selected\nopenai_base_url = \"https://custom.example/v1\"\nchatgpt_base_url = \"https://custom.example/backend\"\nforced_login_method = \"api\"\n[mcp_servers.keep]\ncommand = \"keep-tool\"\n";
    fs::write(home.join("config.toml"), original).unwrap();
    fs::write(home.join("auth.json"), "private-native-auth-sentinel").unwrap();
    fs::write(home.join("history.jsonl"), "history-sentinel").unwrap();
    let rules = [rule("gpt-6.1-sol", 180000, Some(300000))];
    config::attach_with_rules(&home, &state, 17863, "local-private-key", &rules).unwrap();
    assert_eq!(config::connection_mode(&state).unwrap(), "gateway");
    let catalog = active_catalog(&home);
    let before = fs::read_to_string(home.join("config.toml")).unwrap();
    let preview = config::official_preview(&home, &state).unwrap();
    assert!(!preview.to_string().contains("local-private-key"));
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        before
    );
    config::use_official(&home, &state).unwrap();
    let official = fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(official.contains("model_provider = \"openai\" # selected"));
    assert!(official.contains("forced_login_method = \"chatgpt\""));
    assert!(official.contains("openai_base_url = \"https://chatgpt.com/backend-api/codex\""));
    assert!(official.contains("chatgpt_base_url = \"https://chatgpt.com/backend-api/\""));
    assert_eq!(active_catalog(&home), catalog);
    assert_eq!(config::connection_mode(&state).unwrap(), "official");
    // A second click is idempotent and does not replace the original snapshot.
    config::use_official(&home, &state).unwrap();
    fs::write(
        home.join("config.toml"),
        format!("{official}\n[mcp_servers.new]\ncommand = \"new-tool\"\n"),
    )
    .unwrap();
    config::attach_with_rules(&home, &state, 17863, "local-private-key", &rules).unwrap();
    let gateway = fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(gateway.contains("forced_login_method = \"api\""));
    assert!(gateway.contains("openai_base_url = \"https://custom.example/v1\""));
    assert!(gateway.contains("model_provider = \"codexpress\""));
    config::use_official(&home, &state).unwrap();
    config::restore(&home, &state).unwrap();
    let restored = fs::read_to_string(home.join("config.toml")).unwrap();
    for field in [
        "model_provider = \"custom\" # selected",
        "forced_login_method = \"api\"",
        "https://custom.example/backend",
        "keep-tool",
        "new-tool",
    ] {
        assert!(restored.contains(field));
    }
    assert!(!restored.contains("model_catalog_json"));
    assert!(!restored.contains("model_providers.codexpress"));
    assert_eq!(
        fs::read_to_string(home.join("auth.json")).unwrap(),
        "private-native-auth-sentinel"
    );
    assert_eq!(
        fs::read_to_string(home.join("history.jsonl")).unwrap(),
        "history-sentinel"
    );
    assert_eq!(config::connection_mode(&state).unwrap(), "unmanaged");
}

#[test]
fn direct_official_without_gateway_preserves_unowned_provider_and_restores() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("codex");
    let state = root.path().join("app");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&state).unwrap();
    let original = "model_provider = \"other\"\n[model_providers.codexpress]\nname = \"unowned\"\nbase_url = \"https://unowned.example/v1\"\n";
    fs::write(home.join("config.toml"), original).unwrap();
    config::use_official(&home, &state).unwrap();
    assert!(config::attach(&home, &state, 17863, "local").is_err());
    config::restore(&home, &state).unwrap();
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        original
    );
    fs::remove_file(home.join("config.toml")).unwrap();
    config::use_official(&home, &state).unwrap();
    config::attach(&home, &state, 17863, "local").unwrap();
    config::use_official(&home, &state).unwrap();
    config::restore(&home, &state).unwrap();
    assert!(!home.join("config.toml").exists());
}

#[test]
fn native_switch_conflicts_and_profile_overrides_never_modify_files() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("codex");
    let state = root.path().join("app");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&state).unwrap();
    for original in [
        "profile = \"work\"\n[profiles.work]\nmodel_provider = \"custom\"\n",
        "profile = \"../outside\"\n",
    ] {
        fs::write(home.join("config.toml"), original).unwrap();
        assert!(config::use_official(&home, &state).is_err());
        assert_eq!(
            fs::read_to_string(home.join("config.toml")).unwrap(),
            original
        );
        assert!(!state.join("config-attachment.json").exists());
    }
    let original = "profile = \"work\"\n";
    fs::write(home.join("config.toml"), original).unwrap();
    fs::write(
        home.join("work.config.toml"),
        "forced_login_method = \"api\"\n",
    )
    .unwrap();
    assert!(config::use_official(&home, &state).is_err());
    fs::remove_file(home.join("work.config.toml")).unwrap();
    config::use_official(&home, &state).unwrap();
    let changed = fs::read_to_string(home.join("config.toml"))
        .unwrap()
        .replace("\"chatgpt\"", "\"api\"");
    fs::write(home.join("config.toml"), &changed).unwrap();
    assert!(config::use_official(&home, &state).is_err());
    assert!(config::attach(&home, &state, 17863, "key").is_err());
    assert!(config::restore(&home, &state).is_err());
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        changed
    );
}

#[test]
fn pool_aliases_resume_provider_ids_and_restore_original_tables_without_history_edits() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("codex");
    let state = root.path().join("app");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&state).unwrap();
    let original = "model_provider = \"custom\"\n[model_providers.custom]\nname = \"Original\"\nbase_url = \"https://custom.invalid/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true\n[model_providers.second]\nname = \"Second\"\nbase_url = \"https://second.invalid/v1\"\nwire_api = \"responses\"\n[mcp_servers.keep]\ncommand = \"tool\"\n";
    fs::write(home.join("config.toml"), original).unwrap();
    fs::write(home.join("history.jsonl"), "unchanged-conversation").unwrap();
    let preview = config::preview(&home, &state, 17863, "private-local").unwrap();
    assert_eq!(
        preview["compatibility_provider_ids"],
        serde_json::json!(["custom", "second"])
    );
    assert!(!preview.to_string().contains("private-local"));
    config::attach(&home, &state, 17863, "private-local").unwrap();
    let active = fs::read_to_string(home.join("config.toml")).unwrap();
    let doc = active.parse::<toml_edit::DocumentMut>().unwrap();
    for id in ["custom", "second", "codexpress"] {
        assert_eq!(
            doc["model_providers"][id]["base_url"].as_str(),
            Some("http://127.0.0.1:17863/v1")
        );
        assert_eq!(
            doc["model_providers"][id]["requires_openai_auth"].as_bool(),
            Some(false)
        );
        assert_eq!(
            doc["model_providers"][id]["experimental_bearer_token"].as_str(),
            Some("private-local")
        );
    }
    // Idempotent application and official roundtrips keep the real originals.
    config::attach(&home, &state, 17863, "private-local").unwrap();
    config::use_official(&home, &state).unwrap();
    config::attach(&home, &state, 17863, "private-local").unwrap();
    let active = fs::read_to_string(home.join("config.toml")).unwrap();
    fs::write(home.join("config.toml"), format!("{active}\n[model_providers.new]\nname = \"Added later\"\nbase_url = \"https://new.invalid/v1\"\nwire_api = \"responses\"\n")).unwrap();
    config::restore(&home, &state).unwrap();
    let restored = fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(restored.contains("https://custom.invalid/v1"));
    assert!(restored.contains("https://second.invalid/v1"));
    assert!(restored.contains("https://new.invalid/v1"));
    assert!(restored.contains("command = \"tool\""));
    assert!(!restored.contains("private-local"));
    assert_eq!(
        fs::read_to_string(home.join("history.jsonl")).unwrap(),
        "unchanged-conversation"
    );
    config::attach(&home, &state, 17863, "private-local").unwrap();
    let active = fs::read_to_string(home.join("config.toml")).unwrap();
    let changed = active.replacen("name = \"Codexpress\"", "name = \"External edit\"", 1);
    fs::write(home.join("config.toml"), &changed).unwrap();
    assert!(config::restore(&home, &state).is_err());
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        changed
    );
}

#[test]
fn original_gateway_snapshot_format_remains_compatible() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("codex");
    let state = root.path().join("app");
    fs::create_dir_all(&state).unwrap();
    config::attach(&home, &state, 17863, "key").unwrap();
    let file = state.join("config-attachment.json");
    let mut snapshot: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    for key in [
        "applied_provider",
        "owns_table",
        "official_before",
        "applied_official",
    ] {
        snapshot.as_object_mut().unwrap().remove(key);
    }
    assert!(snapshot["applied_table"].is_string());
    fs::write(&file, snapshot.to_string()).unwrap();
    config::use_official(&home, &state).unwrap();
    config::restore(&home, &state).unwrap();
    assert!(!home.join("config.toml").exists());
}
