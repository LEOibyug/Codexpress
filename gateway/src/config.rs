use crate::{
    compaction::{self, ModelCompactionRule},
    model::AppState,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};
use toml_edit::{value, DocumentMut, Item, Table};

const WARNING: &str = "接管仅修改连接及已配置的模型压缩规则，不修改认证、工具或历史。模型规则由 Codex 启动时加载，应用后需重启 Codex 并新建会话。已有会话可能仍保存原连接，需要重新载入并验证。";

#[derive(Serialize, Deserialize)]
struct Snapshot {
    codex_home: PathBuf,
    existed: bool,
    before: String,
    applied_table: Option<String>,
    #[serde(default = "local_provider")]
    applied_provider: String,
    #[serde(default = "yes")]
    owns_table: bool,
    #[serde(default)]
    official_before: Option<String>,
    #[serde(default)]
    applied_official: Option<String>,
    #[serde(default = "first_schema")]
    official_schema: u32,
    #[serde(default)]
    gateway_auth_backup_id: Option<String>,
    #[serde(default)]
    official_auth_backup_id: Option<String>,
    #[serde(default)]
    applied_compaction: Option<String>,
    #[serde(default)]
    compaction_before: Option<String>,
    #[serde(default)]
    gateway_aliases: Option<ProviderAliases>,
}

#[derive(Serialize, Deserialize)]
struct ProviderAliases {
    before: String,
    applied: String,
}

fn local_provider() -> String {
    "codexpress".into()
}
fn yes() -> bool {
    true
}
fn first_schema() -> u32 {
    1
}
const OFFICIAL_KEYS: [&str; 4] = [
    "openai_base_url",
    "chatgpt_base_url",
    "forced_login_method",
    "respect_system_proxy",
];
pub const OFFICIAL_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const OFFICIAL_SERVICE_URL: &str = "https://chatgpt.com/backend-api/";

pub fn connection_mode(state_dir: &Path) -> Result<&'static str> {
    Ok(match snapshot_load(state_dir)? {
        Some(snapshot) if snapshot.applied_provider == "openai" => "official",
        Some(_) => "gateway",
        None => "unmanaged",
    })
}

fn official_fields_version(doc: &DocumentMut, schema: u32) -> String {
    let mut fields = DocumentMut::new();
    for key in OFFICIAL_KEYS {
        if schema < 2 && key == "respect_system_proxy" {
            continue;
        }
        if let Some(item) = doc.get(key) {
            fields[key] = item.clone();
        }
    }
    if schema >= 3 {
        if let Some(item) = doc.get("model_providers") {
            fields["model_providers"] = item.clone();
        }
    } else if schema >= 2 {
        if let Some(item) = doc.get("model_providers").and_then(|p| p.get("openai")) {
            fields["model_providers"] = Item::Table(Table::new());
            fields["model_providers"]["openai"] = item.clone();
        }
    }
    fields.to_string()
}

fn official_fields(doc: &DocumentMut) -> String {
    official_fields_version(doc, 3)
}

fn restore_official_fields(doc: &mut DocumentMut, snapshot: &Snapshot) -> Result<()> {
    if let Some(before) = &snapshot.official_before {
        let original = before.parse::<DocumentMut>()?;
        for key in OFFICIAL_KEYS {
            if snapshot.official_schema < 2 && key == "respect_system_proxy" {
                continue;
            }
            match original.get(key) {
                Some(item) => {
                    doc[key] = item.clone();
                }
                None => {
                    doc.remove(key);
                }
            }
        }
        if snapshot.official_schema >= 3 {
            if let Some(item) = original.get("model_providers") {
                doc["model_providers"] = item.clone();
            } else {
                doc.remove("model_providers");
            }
        } else if snapshot.official_schema >= 2 {
            if let Some(item) = original
                .get("model_providers")
                .and_then(|p| p.get("openai"))
            {
                if doc.get("model_providers").is_none() {
                    let mut providers = Table::new();
                    providers.set_implicit(true);
                    doc["model_providers"] = Item::Table(providers);
                }
                doc["model_providers"]["openai"] = item.clone();
            } else if let Some(table) = doc
                .get_mut("model_providers")
                .and_then(Item::as_table_like_mut)
            {
                table.remove("openai");
            }
        }
    }
    Ok(())
}

fn alias_tables(doc: &DocumentMut) -> Option<&dyn toml_edit::TableLike> {
    doc.get("model_providers").and_then(Item::as_table_like)
}

fn verify_gateway_aliases(doc: &DocumentMut, snapshot: &Snapshot) -> Result<()> {
    if let Some(aliases) = &snapshot.gateway_aliases {
        let applied = aliases.applied.parse::<DocumentMut>()?;
        if let Some(providers) = alias_tables(&applied) {
            for (id, expected) in providers.iter() {
                if alias_tables(doc)
                    .and_then(|p| p.get(id))
                    .map(ToString::to_string)
                    != Some(expected.to_string())
                {
                    bail!("已有对话的供应商连接已被其他程序修改，未覆盖；请先检查供应商配置。");
                }
            }
        }
    }
    Ok(())
}

fn restore_gateway_aliases(doc: &mut DocumentMut, snapshot: &Snapshot) -> Result<()> {
    if let Some(aliases) = &snapshot.gateway_aliases {
        let before = aliases.before.parse::<DocumentMut>()?;
        if let Some(providers) = alias_tables(&before) {
            for (id, item) in providers.iter() {
                doc["model_providers"][id] = item.clone();
            }
        }
    }
    Ok(())
}

/// Resume can retain a custom provider ID. Redirect its current definition,
/// keeping the conversation itself untouched and the original table restorable.
fn prepare_gateway_aliases(
    doc: &mut DocumentMut,
    port: u16,
    key: &str,
) -> Result<Option<ProviderAliases>> {
    let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
    else {
        return Ok(None);
    };
    let mut before = DocumentMut::new();
    let mut applied = DocumentMut::new();
    before["model_providers"] = Item::Table(Table::new());
    applied["model_providers"] = Item::Table(Table::new());
    for (id, item) in providers.iter_mut() {
        let id = id.get();
        if matches!(id, "codexpress" | "openai" | "ollama" | "lmstudio") {
            continue;
        }
        before["model_providers"][id] = item.clone();
        *item = Item::Table(provider_table(port, key));
        applied["model_providers"][id] = item.clone();
    }
    Ok(alias_tables(&before)
        .filter(|p| !p.is_empty())
        .map(|_| ProviderAliases {
            before: before.to_string(),
            applied: applied.to_string(),
        }))
}

pub fn secure_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("无法确定配置目录")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".codexpress-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn path_checked(home: &Path) -> Result<PathBuf> {
    if !home.is_absolute() {
        bail!("Codex 目录必须是绝对路径");
    }
    if home
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("目录不能包含上级路径跳转");
    }
    if home.exists() {
        Ok(fs::canonicalize(home)?)
    } else {
        Ok(home.to_path_buf())
    }
}

fn read_document(path: &Path) -> Result<(String, DocumentMut)> {
    let content = match fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => bail!("无法读取 Codex 配置，请检查目录权限"),
    };
    let doc = content
        .parse::<DocumentMut>()
        .map_err(|_| anyhow::anyhow!("Codex 配置不是有效的 TOML，未做任何修改"))?;
    Ok((content, doc))
}

fn provider_table(port: u16, key: &str) -> Table {
    let mut table = Table::new();
    table["name"] = value("Codexpress");
    table["base_url"] = value(format!("http://127.0.0.1:{port}/v1"));
    table["wire_api"] = value("responses");
    table["requires_openai_auth"] = value(false);
    table["experimental_bearer_token"] = value(key);
    table["supports_websockets"] = value(false);
    table
}

fn select_local(doc: &mut DocumentMut) {
    let mut selected = value("codexpress");
    if let Some(old) = doc.get("model_provider").and_then(Item::as_value) {
        *selected.as_value_mut().expect("string value").decor_mut() = old.decor().clone();
    }
    doc["model_provider"] = selected;
}

fn table_text(doc: &DocumentMut) -> Option<String> {
    doc.get("model_providers")?
        .get("codexpress")?
        .as_table()
        .map(ToString::to_string)
}

fn snapshot_path(state_dir: &Path) -> PathBuf {
    state_dir.join("config-attachment.json")
}

fn snapshot_load(state_dir: &Path) -> Result<Option<Snapshot>> {
    match fs::read(snapshot_path(state_dir)) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("连接快照损坏，停止覆盖配置")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => bail!("无法读取连接快照"),
    }
}

fn verify_attachment(doc: &DocumentMut, snapshot: &Snapshot) -> Result<()> {
    if doc.get("model_provider").and_then(Item::as_str) != Some(snapshot.applied_provider.as_str())
        || (snapshot.owns_table && table_text(doc) != snapshot.applied_table)
    {
        bail!("连接设置已被其他程序修改；为保留你的更改，未覆盖或恢复。请先检查 model_provider 和 model_providers.codexpress。");
    }
    if snapshot
        .applied_official
        .as_ref()
        .is_some_and(|expected| official_fields_version(doc, snapshot.official_schema) != *expected)
    {
        bail!("官方连接或登录设置已被其他程序修改，未覆盖；请先检查配置。");
    }
    if snapshot
        .applied_compaction
        .as_ref()
        .is_some_and(|expected| compaction_text(doc) != *expected)
    {
        bail!("模型目录或全局上下文设置已被其他程序修改；未覆盖，请先检查模型压缩配置。");
    }
    verify_gateway_aliases(doc, snapshot)
}

fn compaction_text(doc: &DocumentMut) -> String {
    let mut fields = DocumentMut::new();
    for key in compaction::MANAGED_KEYS {
        if let Some(item) = doc.get(key) {
            fields[key] = item.clone();
        }
    }
    fields.to_string()
}

fn restore_compaction(doc: &mut DocumentMut, original: &DocumentMut) {
    for key in compaction::MANAGED_KEYS {
        match original.get(key) {
            Some(item) => {
                doc[key] = item.clone();
            }
            None => {
                doc.remove(key);
            }
        }
    }
}

fn prepare_compaction(
    doc: &mut DocumentMut,
    home: &Path,
    state_dir: &Path,
    snapshot: Option<&Snapshot>,
    rules: &[ModelCompactionRule],
) -> Result<Option<(PathBuf, Vec<u8>)>> {
    compaction::validate(rules)?;
    let original = match snapshot.filter(|s| s.applied_compaction.is_some()) {
        Some(snapshot) => snapshot
            .compaction_before
            .as_ref()
            .unwrap_or(&snapshot.before)
            .parse::<DocumentMut>()?,
        None => doc.clone(),
    };
    if rules.is_empty() {
        if snapshot.is_some_and(|s| s.applied_compaction.is_some()) {
            restore_compaction(doc, &original);
        }
        return Ok(None);
    }
    let bytes = serde_json::to_vec_pretty(&compaction::catalog(home, &original, rules)?)?;
    let path = state_dir
        .join("model-catalogs")
        .join(format!("{:x}.json", Sha256::digest(&bytes)));
    doc["model_catalog_json"] = value(path.to_str().context("模型目录路径无效")?);
    doc.remove("model_auto_compact_token_limit");
    doc.remove("model_context_window");
    Ok(Some((path, bytes)))
}

fn redact(doc: &DocumentMut) -> String {
    fn walk(table: &mut dyn toml_edit::TableLike) {
        for (key, item) in table.iter_mut() {
            let name = key.get().to_lowercase();
            if name.contains("key")
                || (name.contains("token") && name != "model_auto_compact_token_limit")
                || name.contains("password")
                || name == "env"
                || name == "http_headers"
                || name == "env_http_headers"
            {
                *item = value("•••• 已隐藏");
            } else if let Some(nested) = item.as_table_like_mut() {
                walk(nested);
            }
        }
    }
    let mut safe = doc.clone();
    walk(safe.as_table_mut());
    safe.to_string()
}

pub fn preview(home: &Path, state_dir: &Path, port: u16, key: &str) -> Result<Value> {
    preview_with_rules(home, state_dir, port, key, &[])
}

pub fn preview_with_rules(
    home: &Path,
    state_dir: &Path,
    port: u16,
    key: &str,
    rules: &[ModelCompactionRule],
) -> Result<Value> {
    let home = path_checked(home)?;
    let (_, before) = read_document(&home.join("config.toml"))?;
    let snapshot = snapshot_load(state_dir)?;
    if let Some(snapshot) = snapshot.as_ref() {
        if snapshot.codex_home != home {
            bail!("当前已接管另一个 Codex 目录，请先恢复该目录");
        }
        verify_attachment(&before, snapshot)?;
    }
    let mut after = before.clone();
    if snapshot.as_ref().is_none_or(|s| !s.owns_table)
        && after
            .get("model_providers")
            .and_then(|p| p.get("codexpress"))
            .is_some()
    {
        bail!("已有名为 codexpress 的供应商且不属于本应用，未覆盖");
    }
    if let Some(snapshot) = &snapshot {
        restore_official_fields(&mut after, snapshot)?;
        restore_gateway_aliases(&mut after, snapshot)?;
    }
    if let Some(item) = after.get("model_providers") {
        if !item.is_table() {
            bail!("model_providers 需为标准 TOML 表，未修改配置");
        }
    }
    select_local(&mut after);
    if after.get("model_providers").is_none() {
        after["model_providers"] = Item::Table(Table::new());
    }
    after["model_providers"]["codexpress"] = Item::Table(provider_table(port, key));
    let aliases = prepare_gateway_aliases(&mut after, port, key)?;
    let compatibility_provider_ids = aliases
        .as_ref()
        .map(|a| a.applied.parse::<DocumentMut>().expect("generated TOML"))
        .and_then(|doc| {
            alias_tables(&doc).map(|p| p.iter().map(|(id, _)| id.to_owned()).collect::<Vec<_>>())
        })
        .unwrap_or_default();
    let catalog = prepare_compaction(&mut after, &home, state_dir, snapshot.as_ref(), rules)?;
    let model_limits = catalog.as_ref().map(|(_, bytes)| {
        let catalog: Value = serde_json::from_slice(bytes).expect("generated JSON");
        catalog["models"].as_array().unwrap().iter().filter(|m| rules.iter().any(|r| m["slug"] == r.model))
            .map(|m| json!({"model":m["slug"],"auto_compact_token_limit":m["auto_compact_token_limit"],"context_window":m["context_window"]})).collect::<Vec<_>>()
    }).unwrap_or_default();
    Ok(
        json!({"before":redact(&before),"after":redact(&after),"attached":snapshot.is_some(),"model_limits":model_limits,"compatibility_provider_ids":compatibility_provider_ids,"warning":WARNING}),
    )
}

pub fn attach(home: &Path, state_dir: &Path, port: u16, key: &str) -> Result<Value> {
    attach_with_rules(home, state_dir, port, key, &[])
}

pub fn attach_with_rules(
    home: &Path,
    state_dir: &Path,
    port: u16,
    key: &str,
    rules: &[ModelCompactionRule],
) -> Result<Value> {
    let home = path_checked(home)?;
    fs::create_dir_all(&home)?;
    let home = fs::canonicalize(&home)?;
    let config = home.join("config.toml");
    if fs::symlink_metadata(&config).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("配置文件是符号链接，未执行接管；请直接选择真实配置目录");
    }
    let (before, mut doc) = read_document(&config)?;
    let previous_snapshot = snapshot_load(state_dir)?;
    if let Some(snapshot) = previous_snapshot.as_ref() {
        if snapshot.codex_home != home {
            bail!("当前已接管其他 Codex 目录");
        }
        verify_attachment(&doc, snapshot)?;
        let expected = provider_table(port, key).to_string();
        if snapshot.owns_table
            && snapshot.applied_provider == "codexpress"
            && Some(expected) != snapshot.applied_table
        {
            bail!("本地连接端口或凭据已变化，请先恢复连接后重新接管");
        }
    }
    preview_with_rules(&home, state_dir, port, key, rules)?;
    if let Some(snapshot) = &previous_snapshot {
        restore_official_fields(&mut doc, snapshot)?;
        restore_gateway_aliases(&mut doc, snapshot)?;
    }
    let catalog = prepare_compaction(
        &mut doc,
        &home,
        state_dir,
        previous_snapshot.as_ref(),
        rules,
    )?;
    select_local(&mut doc);
    if doc.get("model_providers").is_none() {
        doc["model_providers"] = Item::Table(Table::new());
    }
    doc["model_providers"]["codexpress"] = Item::Table(provider_table(port, key));
    let gateway_aliases = prepare_gateway_aliases(&mut doc, port, key)?;
    let mut snapshot = Snapshot {
        codex_home: home.clone(),
        existed: previous_snapshot
            .as_ref()
            .map_or(config.exists(), |s| s.existed),
        before: previous_snapshot
            .as_ref()
            .map_or_else(|| before.clone(), |s| s.before.clone()),
        applied_table: Some(table_text(&doc).context("无法生成连接配置")?),
        applied_provider: local_provider(),
        gateway_aliases,
        owns_table: true,
        official_before: None,
        applied_official: None,
        official_schema: 3,
        gateway_auth_backup_id: previous_snapshot
            .as_ref()
            .and_then(|s| s.gateway_auth_backup_id.clone()),
        official_auth_backup_id: previous_snapshot
            .as_ref()
            .and_then(|s| s.official_auth_backup_id.clone()),
        compaction_before: if rules.is_empty() {
            None
        } else {
            Some(
                previous_snapshot
                    .as_ref()
                    .filter(|s| s.applied_compaction.is_some())
                    .map(|s| {
                        s.compaction_before
                            .clone()
                            .unwrap_or_else(|| s.before.clone())
                    })
                    .unwrap_or_else(|| before.clone()),
            )
        },
        applied_compaction: if rules.is_empty() {
            None
        } else {
            Some(compaction_text(&doc))
        },
    };
    let current = fs::read_to_string(&config).unwrap_or_default();
    if current != before {
        bail!("配置在预览后发生变化，未写入；请刷新后重试");
    }
    let backup_id = crate::backups::create(&home, state_dir, "before_apply")?;
    let restore_auth = previous_snapshot
        .as_ref()
        .filter(|s| s.applied_provider == "openai")
        .and_then(|s| s.gateway_auth_backup_id.as_deref());
    if previous_snapshot
        .as_ref()
        .is_some_and(|s| s.applied_provider == "openai")
    {
        snapshot.official_auth_backup_id = Some(backup_id);
    }
    if let Some((path, bytes)) = &catalog {
        secure_write(path, bytes)?;
    }
    let previous_snapshot_bytes = fs::read(snapshot_path(state_dir)).ok();
    secure_write(&snapshot_path(state_dir), &serde_json::to_vec(&snapshot)?)?;
    if fs::read_to_string(&config).unwrap_or_default() != before {
        rollback_snapshot(state_dir, previous_snapshot_bytes.as_deref());
        bail!("配置在写入前发生变化，未覆盖；请刷新后重试");
    }
    if let Err(error) = secure_write(&config, doc.to_string().as_bytes()) {
        rollback_snapshot(state_dir, previous_snapshot_bytes.as_deref());
        return Err(error).context("连接写入失败");
    }
    if let Some(id) = restore_auth {
        if let Err(error) = crate::backups::restore_auth(&home, state_dir, id) {
            let _ = secure_write(&config, before.as_bytes());
            rollback_snapshot(state_dir, previous_snapshot_bytes.as_deref());
            return Err(error);
        }
    }
    let warning = previous_snapshot.as_ref().filter(|s|s.applied_provider=="openai" && s.gateway_auth_backup_id.is_none())
        .map(|_| "旧版切换前的 auth.json 没有备份，无法恢复其中的旧 API Key；来源池自身保存的密钥不受影响。");
    Ok(
        json!({"attached":true,"message":"已应用连接与模型规则；请重启 Codex 并新建会话，已有会话需另行验证","warning":warning}),
    )
}

fn rollback_snapshot(state_dir: &Path, previous: Option<&[u8]>) {
    if let Some(bytes) = previous {
        let _ = secure_write(&snapshot_path(state_dir), bytes);
    } else {
        let _ = fs::remove_file(snapshot_path(state_dir));
    }
}

pub fn restore(home: &Path, state_dir: &Path) -> Result<Value> {
    let home = path_checked(home)?;
    let Some(snapshot) = snapshot_load(state_dir)? else {
        return Ok(json!({"attached":false,"message":"没有需要恢复的连接"}));
    };
    if snapshot.codex_home != home {
        bail!("所选目录与原接管目录不一致");
    }
    let config = home.join("config.toml");
    let (before, mut doc) = read_document(&config)?;
    verify_attachment(&doc, &snapshot)?;
    let original = snapshot.before.parse::<DocumentMut>()?;
    restore_official_fields(&mut doc, &snapshot)?;
    restore_gateway_aliases(&mut doc, &snapshot)?;
    if snapshot.applied_compaction.is_some() {
        let original_compaction = snapshot
            .compaction_before
            .as_ref()
            .unwrap_or(&snapshot.before)
            .parse::<DocumentMut>()?;
        restore_compaction(&mut doc, &original_compaction);
    }
    match original.get("model_provider") {
        Some(item) => {
            doc["model_provider"] = item.clone();
        }
        None => {
            doc.remove("model_provider");
        }
    }
    if let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
        .filter(|_| snapshot.owns_table)
    {
        providers.remove("codexpress");
        if providers.is_empty() && original.get("model_providers").is_none() {
            doc.remove("model_providers");
        }
    }
    if fs::read_to_string(&config)? != before {
        bail!("配置在恢复期间发生变化，未覆盖");
    }
    crate::backups::create(&home, state_dir, "before_connection_restore")?;
    if !snapshot.existed && doc.as_table().is_empty() {
        fs::remove_file(&config)?;
    } else {
        secure_write(&config, doc.to_string().as_bytes())?;
    }
    if snapshot.applied_provider == "openai" {
        if let Some(id) = &snapshot.gateway_auth_backup_id {
            if let Err(error) = crate::backups::restore_auth(&home, state_dir, id) {
                let _ = secure_write(&config, before.as_bytes());
                return Err(error);
            }
        }
    }
    fs::remove_file(snapshot_path(state_dir))?;
    Ok(json!({"attached":false,"message":"已恢复原连接，保留接管期间的其他配置更改"}))
}

pub fn restore_preview(home: &Path, state_dir: &Path) -> Result<Value> {
    let home = path_checked(home)?;
    let snapshot = snapshot_load(state_dir)?.context("没有需要恢复的连接")?;
    if snapshot.codex_home != home {
        bail!("所选目录与原接管目录不一致");
    }
    let (_, doc) = read_document(&home.join("config.toml"))?;
    verify_attachment(&doc, &snapshot)?;
    Ok(json!({"restorable":true}))
}

fn official_document(home: &Path, state_dir: &Path) -> Result<(String, DocumentMut, Snapshot)> {
    let config = home.join("config.toml");
    if fs::symlink_metadata(&config).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("配置文件是符号链接，请先选择真实配置目录。");
    }
    let (before, mut doc) = read_document(&config)?;
    let previous = snapshot_load(state_dir)?;
    if let Some(snapshot) = &previous {
        if snapshot.codex_home != home {
            bail!("当前已接管其他 Codex 目录，请先恢复该目录。");
        }
        verify_attachment(&doc, snapshot)?;
        restore_gateway_aliases(&mut doc, snapshot)?;
    }
    // A selected profile can take precedence over root settings. Refuse a
    // misleading switch rather than dropping its unrelated tools/permissions.
    if let Some(profile) = doc.get("profile").and_then(Item::as_str) {
        if profile.is_empty() || profile.contains(['/', '\\']) || profile == "." || profile == ".."
        {
            bail!("配置档案名称无效，请先检查 profile。");
        }
        let overrides = |table: &dyn toml_edit::TableLike| {
            table.contains_key("model_provider")
                || OFFICIAL_KEYS.iter().any(|key| table.contains_key(key))
        };
        let inline = doc
            .get("profiles")
            .and_then(|p| p.get(profile))
            .and_then(Item::as_table_like)
            .is_some_and(overrides);
        let (_, external) = read_document(&home.join(format!("{profile}.config.toml")))?;
        if inline || overrides(external.as_table()) {
            bail!("当前配置档案覆盖了连接或登录方式，请在 Codex 中取消该档案后重试。");
        }
    }
    let official_before =
        if let Some(snapshot) = previous.as_ref().filter(|s| s.official_before.is_some()) {
            let mut original = doc.clone();
            restore_official_fields(&mut original, snapshot)?;
            official_fields(&original)
        } else {
            official_fields(&doc)
        };
    let mut selected = value("openai");
    if let Some(old) = doc.get("model_provider").and_then(Item::as_value) {
        *selected.as_value_mut().expect("string value").decor_mut() = old.decor().clone();
    }
    doc["model_provider"] = selected;
    // Pin both official services instead of letting a lower config/environment
    // layer supply the custom endpoint again. Launch-side overrides match these.
    doc["openai_base_url"] = value(OFFICIAL_RESPONSES_URL);
    doc["chatgpt_base_url"] = value(OFFICIAL_SERVICE_URL);
    doc["forced_login_method"] = value("chatgpt");
    doc["respect_system_proxy"] = value(true);
    if let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
    {
        providers.remove("openai");
        // A resumed chat can still select its previous provider ID. Keep that
        // ID resolvable while pinning its URL/auth to official for this mode.
        // The complete original table is private-snapshotted and restored later.
        for (_, item) in providers.iter_mut() {
            let mut alias = Table::new();
            alias["name"] = value("OpenAI");
            alias["base_url"] = value(OFFICIAL_RESPONSES_URL);
            alias["wire_api"] = value("responses");
            alias["requires_openai_auth"] = value(true);
            alias["supports_websockets"] = value(true);
            *item = Item::Table(alias);
        }
    }
    let mut snapshot = previous.unwrap_or_else(|| Snapshot {
        codex_home: home.to_owned(),
        existed: config.exists(),
        before: before.clone(),
        applied_table: None,
        applied_provider: local_provider(),
        owns_table: false,
        official_before: None,
        applied_official: None,
        official_schema: 3,
        gateway_auth_backup_id: None,
        official_auth_backup_id: None,
        applied_compaction: None,
        compaction_before: None,
        gateway_aliases: None,
    });
    snapshot.applied_provider = "openai".into();
    snapshot.gateway_aliases = None;
    snapshot.official_schema = 3;
    if snapshot.owns_table {
        snapshot.applied_table = table_text(&doc);
    }
    snapshot.official_before = Some(official_before);
    snapshot.applied_official = Some(official_fields(&doc));
    Ok((before, doc, snapshot))
}

pub fn official_preview(home: &Path, state_dir: &Path) -> Result<Value> {
    let home = path_checked(home)?;
    let (before, after, _) = official_document(&home, state_dir)?;
    Ok(
        json!({"before":redact(&before.parse()?),"after":redact(&after),"connection_mode":"official"}),
    )
}

pub fn use_official(home: &Path, state_dir: &Path) -> Result<Value> {
    let home = path_checked(home)?;
    // Preparing the document is read-only; a failed preflight creates nothing.
    let previous_snapshot = snapshot_load(state_dir)?;
    let (before, doc, mut snapshot) = official_document(&home, state_dir)?;
    fs::create_dir_all(&home)?;
    let home = fs::canonicalize(home)?;
    snapshot.codex_home = home.clone();
    let config = home.join("config.toml");
    if fs::read_to_string(&config).unwrap_or_default() != before {
        bail!("配置在切换期间发生变化，未覆盖。");
    }
    let backup_id = crate::backups::create(&home, state_dir, "before_official")?;
    let entering = previous_snapshot
        .as_ref()
        .is_none_or(|s| s.applied_provider != "openai");
    if entering {
        snapshot.gateway_auth_backup_id = Some(backup_id);
    }
    let restore_auth = entering
        .then(|| snapshot.official_auth_backup_id.clone())
        .flatten();
    let previous = fs::read(snapshot_path(state_dir)).ok();
    secure_write(&snapshot_path(state_dir), &serde_json::to_vec(&snapshot)?)?;
    if fs::read_to_string(&config).unwrap_or_default() != before {
        rollback_snapshot(state_dir, previous.as_deref());
        bail!("配置在写入前发生变化，未覆盖。");
    }
    if let Err(error) = secure_write(&config, doc.to_string().as_bytes()) {
        rollback_snapshot(state_dir, previous.as_deref());
        return Err(error).context("官方连接设置写入失败");
    }
    if let Some(id) = restore_auth {
        if let Err(error) = crate::backups::restore_auth(&home, state_dir, &id) {
            let _ = secure_write(&config, before.as_bytes());
            rollback_snapshot(state_dir, previous.as_deref());
            return Err(error);
        }
    }
    Ok(json!({"connection_mode":"official","attached":true}))
}

pub async fn handle(state: &Arc<AppState>, op: &str, payload: Value) -> Result<Value> {
    let _guard = state.config_lock.lock().await;
    let home = payload
        .get("codex_home")
        .and_then(Value::as_str)
        .context("缺少 Codex 目录")?;
    let home = Path::new(home);
    let rules: Vec<ModelCompactionRule> = serde_json::from_value(
        payload
            .get("model_compaction")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .context("模型压缩规则格式无效")?;
    match op {
        "config.preview" => preview_with_rules(
            home,
            &state.state_dir,
            state.port,
            &state.gateway_key,
            &rules,
        ),
        "config.attach" => attach_with_rules(
            home,
            &state.state_dir,
            state.port,
            &state.gateway_key,
            &rules,
        ),
        "config.restore" => restore(home, &state.state_dir),
        "config.restore.preview" => restore_preview(home, &state.state_dir),
        "config.official.preview" => official_preview(home, &state.state_dir),
        "config.official" => use_official(home, &state.state_dir),
        "config.backups" => crate::backups::list(&path_checked(home)?, &state.state_dir),
        "config.backup.preview" => crate::backups::preflight(
            &path_checked(home)?,
            &state.state_dir,
            payload
                .get("backup_id")
                .and_then(Value::as_str)
                .context("请选择备份")?,
        ),
        "config.backup.restore" => crate::backups::restore(
            &path_checked(home)?,
            &state.state_dir,
            payload
                .get("backup_id")
                .and_then(Value::as_str)
                .context("请选择备份")?,
        ),
        _ => bail!("未知连接配置操作"),
    }
}
