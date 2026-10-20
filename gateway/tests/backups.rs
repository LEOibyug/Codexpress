use codexpress_gateway::{backups, config};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
};

#[test]
fn native_login_overwrites_auth_but_pool_and_official_sessions_each_return() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("codex");
    let state = temp.path().join("app");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::write(home.join("config.toml"),"model_provider=\"custom\"\nopenai_base_url=\"https://third.example/v1\"\nrespect_system_proxy=false\n").unwrap();
    let api = json!({"OPENAI_API_KEY":"PRIVATE-ORIGINAL-KEY"}).to_string();
    let official = json!({"tokens":{"access_token":"PRIVATE-OFFICIAL-TOKEN"}}).to_string();
    fs::write(home.join("auth.json"), &api).unwrap();
    config::attach(&home, &state, 17863, "local").unwrap();
    config::use_official(&home, &state).unwrap();
    // Simulates the native ChatGPT login replacing the original auth.json.
    fs::write(home.join("auth.json"), &official).unwrap();
    config::use_official(&home, &state).unwrap();
    config::attach(&home, &state, 17863, "local").unwrap();
    assert_eq!(fs::read_to_string(home.join("auth.json")).unwrap(), api);
    config::use_official(&home, &state).unwrap();
    assert_eq!(
        fs::read_to_string(home.join("auth.json")).unwrap(),
        official
    );
    config::restore(&home, &state).unwrap();
    assert_eq!(fs::read_to_string(home.join("auth.json")).unwrap(), api);
    assert!(fs::read_to_string(home.join("config.toml"))
        .unwrap()
        .contains("respect_system_proxy=false"));
}

#[test]
fn full_backup_restores_exact_config_auth_profiles_and_can_undo_restore() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("codex");
    let state = temp.path().join("app");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&state).unwrap();
    let original = "# comments\nmodel_provider=\"old\"\n[mcp_servers.keep]\ncommand=\"keep\"\n";
    fs::write(home.join("config.toml"), original).unwrap();
    fs::write(home.join("auth.json"), "PRIVATE-API-KEY").unwrap();
    fs::write(home.join("auth.json.bak"), "PRIVATE-AUTH-BAK").unwrap();
    fs::write(home.join("work.config.toml"), "model=\"keep-model\"\n").unwrap();
    fs::write(home.join("history.jsonl"), "history").unwrap();
    // Applying, rather than an explicit backup call, must create the checkpoint.
    config::attach(&home, &state, 17863, "local").unwrap();
    let list = backups::list(&home, &state).unwrap();
    let id = list["backups"][0]["id"].as_str().unwrap();
    assert!(!list.to_string().contains("PRIVATE"));
    assert_eq!(list["backups"][0]["has_auth"], true);
    let backup_dir = state.join("configuration-backups").join(id);
    assert_eq!(
        fs::metadata(&backup_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(backup_dir.join("files/auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let connected = fs::read(home.join("config.toml")).unwrap();
    fs::write(home.join("auth.json"), "PRIVATE-NEW-LOGIN").unwrap();
    fs::write(home.join("work.config.toml"), "model=\"changed\"\n").unwrap();
    fs::write(home.join("new.config.toml"), "model=\"new\"\n").unwrap();
    backups::preflight(&home, &state, id).unwrap();
    let restored = backups::restore(&home, &state, id).unwrap();
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        original
    );
    assert_eq!(
        fs::read_to_string(home.join("auth.json")).unwrap(),
        "PRIVATE-API-KEY"
    );
    assert_eq!(
        fs::read_to_string(home.join("auth.json.bak")).unwrap(),
        "PRIVATE-AUTH-BAK"
    );
    assert_eq!(
        fs::read_to_string(home.join("work.config.toml")).unwrap(),
        "model=\"keep-model\"\n"
    );
    assert!(!home.join("new.config.toml").exists());
    assert!(!state.join("config-attachment.json").exists());
    assert_eq!(
        fs::read_to_string(home.join("history.jsonl")).unwrap(),
        "history"
    );
    backups::restore(
        &home,
        &state,
        restored["recovery_backup_id"].as_str().unwrap(),
    )
    .unwrap();
    assert_eq!(fs::read(home.join("config.toml")).unwrap(), connected);
    assert_eq!(
        fs::read_to_string(home.join("auth.json")).unwrap(),
        "PRIVATE-NEW-LOGIN"
    );
    assert_eq!(config::connection_mode(&state).unwrap(), "gateway");
}

#[test]
fn corrupt_backup_symlink_and_wrong_home_never_overwrite_current_auth() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("codex");
    let state = temp.path().join("app");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::write(home.join("auth.json"), "PRIVATE-ORIGINAL").unwrap();
    let id = backups::create(&home, &state, "before_apply").unwrap();
    fs::write(home.join("auth.json"), "PRIVATE-CURRENT").unwrap();
    let file = state
        .join("configuration-backups")
        .join(&id)
        .join("files/auth.json");
    fs::write(file, "corrupt").unwrap();
    assert!(backups::restore(&home, &state, &id).is_err());
    assert_eq!(
        fs::read_to_string(home.join("auth.json")).unwrap(),
        "PRIVATE-CURRENT"
    );
    assert!(backups::preflight(&temp.path().join("other"), &state, &id).is_err());
    assert!(backups::restore(&home, &state, "../../bad").is_err());
    fs::remove_file(home.join("auth.json")).unwrap();
    let outside = temp.path().join("outside");
    fs::write(&outside, "private").unwrap();
    symlink(&outside, home.join("auth.json")).unwrap();
    assert!(backups::create(&home, &state, "before_apply").is_err());
    assert!(config::attach(&home, &state, 17863, "local").is_err());
    assert!(!home.join("config.toml").exists());
}

#[test]
fn official_pins_service_urls_and_removes_legacy_openai_provider_override() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("codex");
    let state = temp.path().join("app");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&state).unwrap();
    let original="model_provider=\"openai\"\n[model_providers.openai]\nname=\"legacy\"\nbase_url=\"https://third.example/v1\"\n[model_providers.old]\nname=\"old\"\nbase_url=\"https://old.example/v1\"\nenv_key=\"PRIVATE-ENV-KEY\"\n";
    fs::write(home.join("config.toml"), original).unwrap();
    config::use_official(&home, &state).unwrap();
    let doc = fs::read_to_string(home.join("config.toml"))
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    assert_eq!(
        doc["openai_base_url"].as_str(),
        Some(config::OFFICIAL_RESPONSES_URL)
    );
    assert_eq!(
        doc["chatgpt_base_url"].as_str(),
        Some(config::OFFICIAL_SERVICE_URL)
    );
    assert_eq!(doc["respect_system_proxy"].as_bool(), Some(true));
    assert_eq!(
        doc["model_providers"]["old"]["base_url"].as_str(),
        Some(config::OFFICIAL_RESPONSES_URL)
    );
    assert_eq!(
        doc["model_providers"]["old"]["requires_openai_auth"].as_bool(),
        Some(true)
    );
    assert!(doc["model_providers"]["old"].get("env_key").is_none());
    assert!(doc
        .get("model_providers")
        .and_then(|p| p.get("openai"))
        .is_none());
    fs::write(home.join("auth.json"), "new official session").unwrap();
    config::restore(&home, &state).unwrap();
    assert!(!home.join("auth.json").exists());
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        original
    );
}
