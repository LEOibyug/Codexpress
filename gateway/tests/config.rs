use codexpress_gateway::config;
use std::fs;

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
