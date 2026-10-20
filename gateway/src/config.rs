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
    applied_table: String,
    #[serde(default)]
    applied_compaction: Option<String>,
    #[serde(default)]
    compaction_before: Option<String>,
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
    if doc.get("model_provider").and_then(Item::as_str) != Some("codexpress")
        || table_text(doc).as_deref() != Some(snapshot.applied_table.as_str())
    {
        bail!("连接设置已被其他程序修改；为保留你的更改，未覆盖或恢复。请先检查 model_provider 和 model_providers.codexpress。");
    }
    if snapshot
        .applied_compaction
        .as_ref()
        .is_some_and(|expected| compaction_text(doc) != *expected)
    {
        bail!("模型目录或全局上下文设置已被其他程序修改；未覆盖，请先检查模型压缩配置。");
    }
    Ok(())
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
    if snapshot.is_none()
        && after
            .get("model_providers")
            .and_then(|p| p.get("codexpress"))
            .is_some()
    {
        bail!("已有名为 codexpress 的供应商且不属于本应用，未覆盖");
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
    let catalog = prepare_compaction(&mut after, &home, state_dir, snapshot.as_ref(), rules)?;
    let model_limits = catalog.as_ref().map(|(_, bytes)| {
        let catalog: Value = serde_json::from_slice(bytes).expect("generated JSON");
        catalog["models"].as_array().unwrap().iter().filter(|m| rules.iter().any(|r| m["slug"] == r.model))
            .map(|m| json!({"model":m["slug"],"auto_compact_token_limit":m["auto_compact_token_limit"],"context_window":m["context_window"]})).collect::<Vec<_>>()
    }).unwrap_or_default();
    Ok(
        json!({"before":redact(&before),"after":redact(&after),"attached":snapshot.is_some(),"model_limits":model_limits,"warning":WARNING}),
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
        if expected != snapshot.applied_table {
            bail!("本地连接端口或凭据已变化，请先恢复连接后重新接管");
        }
    }
    preview_with_rules(&home, state_dir, port, key, rules)?;
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
    let snapshot = Snapshot {
        codex_home: home.clone(),
        existed: previous_snapshot
            .as_ref()
            .map_or(config.exists(), |s| s.existed),
        before: previous_snapshot
            .as_ref()
            .map_or_else(|| before.clone(), |s| s.before.clone()),
        applied_table: table_text(&doc).context("无法生成连接配置")?,
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
    Ok(
        json!({"attached":true,"message":"已应用连接与模型规则；请重启 Codex 并新建会话，已有会话需另行验证"}),
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
    if let Some(providers) = doc.get_mut("model_providers").and_then(Item::as_table_mut) {
        providers.remove("codexpress");
        if providers.is_empty() && original.get("model_providers").is_none() {
            doc.remove("model_providers");
        }
    }
    if fs::read_to_string(&config)? != before {
        bail!("配置在恢复期间发生变化，未覆盖");
    }
    if !snapshot.existed && doc.as_table().is_empty() {
        fs::remove_file(&config)?;
    } else {
        secure_write(&config, doc.to_string().as_bytes())?;
    }
    fs::remove_file(snapshot_path(state_dir))?;
    Ok(json!({"attached":false,"message":"已恢复原连接，保留接管期间的其他配置更改"}))
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
        _ => bail!("未知连接配置操作"),
    }
}
