use crate::model::AppState;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs::{self, OpenOptions}, io::Write, os::unix::fs::{OpenOptionsExt, PermissionsExt}, path::{Path, PathBuf}, sync::Arc};
use toml_edit::{value, DocumentMut, Item, Table};

const WARNING: &str = "接管仅修改连接设置，不修改认证、工具或历史。新会话使用本地网关；已有会话可能仍保存原连接，需要重新载入并验证。API 模式下的云功能由客户端及上游能力决定。";

#[derive(Serialize, Deserialize)]
struct Snapshot {
    codex_home: PathBuf,
    existed: bool,
    before: String,
    applied_table: String,
}

pub fn secure_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("无法确定配置目录")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".codexpress-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&temporary); }
    result
}

fn path_checked(home: &Path) -> Result<PathBuf> {
    if !home.is_absolute() { bail!("Codex 目录必须是绝对路径"); }
    if home.components().any(|c| matches!(c, std::path::Component::ParentDir)) { bail!("目录不能包含上级路径跳转"); }
    if home.exists() { Ok(fs::canonicalize(home)?) } else { Ok(home.to_path_buf()) }
}

fn read_document(path: &Path) -> Result<(String, DocumentMut)> {
    let content = match fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => bail!("无法读取 Codex 配置，请检查目录权限"),
    };
    let doc = content.parse::<DocumentMut>().map_err(|_| anyhow::anyhow!("Codex 配置不是有效的 TOML，未做任何修改"))?;
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
    let mut selected=value("codexpress");
    if let Some(old)=doc.get("model_provider").and_then(Item::as_value) {
        *selected.as_value_mut().expect("string value").decor_mut()=old.decor().clone();
    }
    doc["model_provider"]=selected;
}

fn table_text(doc: &DocumentMut) -> Option<String> {
    doc.get("model_providers")?.get("codexpress")?.as_table().map(ToString::to_string)
}

fn snapshot_path(state_dir: &Path) -> PathBuf { state_dir.join("config-attachment.json") }

fn snapshot_load(state_dir: &Path) -> Result<Option<Snapshot>> {
    match fs::read(snapshot_path(state_dir)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).context("连接快照损坏，停止覆盖配置")?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => bail!("无法读取连接快照"),
    }
}

fn verify_attachment(doc: &DocumentMut, snapshot: &Snapshot) -> Result<()> {
    if doc.get("model_provider").and_then(Item::as_str) != Some("codexpress")
        || table_text(doc).as_deref() != Some(snapshot.applied_table.as_str()) {
        bail!("连接设置已被其他程序修改；为保留你的更改，未覆盖或恢复。请先检查 model_provider 和 model_providers.codexpress。");
    }
    Ok(())
}

fn redact(doc: &DocumentMut) -> String {
    fn walk(table: &mut dyn toml_edit::TableLike) {
        for (key, item) in table.iter_mut() {
            let name = key.get().to_lowercase();
            if name.contains("key") || name.contains("token") || name.contains("password") || name == "env" || name == "http_headers" || name == "env_http_headers" {
                *item = value("•••• 已隐藏");
            } else if let Some(nested) = item.as_table_like_mut() { walk(nested); }
        }
    }
    let mut safe = doc.clone();
    walk(safe.as_table_mut());
    safe.to_string()
}

pub fn preview(home: &Path, state_dir: &Path, port: u16, key: &str) -> Result<Value> {
    let home = path_checked(home)?;
    let (_, before) = read_document(&home.join("config.toml"))?;
    if let Some(snapshot) = snapshot_load(state_dir)? {
        if snapshot.codex_home != home { bail!("当前已接管另一个 Codex 目录，请先恢复该目录"); }
        verify_attachment(&before, &snapshot)?;
        return Ok(json!({"before":redact(&before),"after":redact(&before),"attached":true,"warning":WARNING}));
    }
    let mut after = before.clone();
    if after.get("model_providers").and_then(|p| p.get("codexpress")).is_some() {
        bail!("已有名为 codexpress 的供应商且不属于本应用，未覆盖");
    }
    if let Some(item) = after.get("model_providers") {
        if !item.is_table() { bail!("model_providers 需为标准 TOML 表，未修改配置"); }
    }
    select_local(&mut after);
    if after.get("model_providers").is_none() { after["model_providers"] = Item::Table(Table::new()); }
    after["model_providers"]["codexpress"] = Item::Table(provider_table(port, key));
    Ok(json!({"before":redact(&before),"after":redact(&after),"attached":false,"warning":WARNING}))
}

pub fn attach(home: &Path, state_dir: &Path, port: u16, key: &str) -> Result<Value> {
    let home = path_checked(home)?;
    fs::create_dir_all(&home)?;
    let home = fs::canonicalize(&home)?;
    let config = home.join("config.toml");
    if fs::symlink_metadata(&config).is_ok_and(|m| m.file_type().is_symlink()) { bail!("配置文件是符号链接，未执行接管；请直接选择真实配置目录"); }
    let (before, mut doc) = read_document(&config)?;
    if let Some(snapshot) = snapshot_load(state_dir)? {
        if snapshot.codex_home != home { bail!("当前已接管其他 Codex 目录"); }
        verify_attachment(&doc, &snapshot)?;
        let expected = provider_table(port,key).to_string();
        if expected != snapshot.applied_table { bail!("本地连接端口或凭据已变化，请先恢复连接后重新接管"); }
        return Ok(json!({"attached":true,"message":"已连接本地网关"}));
    }
    preview(&home, state_dir, port, key)?;
    select_local(&mut doc);
    if doc.get("model_providers").is_none() { doc["model_providers"] = Item::Table(Table::new()); }
    doc["model_providers"]["codexpress"] = Item::Table(provider_table(port,key));
    let snapshot = Snapshot { codex_home:home.clone(), existed:config.exists(), before:before.clone(), applied_table:table_text(&doc).context("无法生成连接配置")? };
    let current = fs::read_to_string(&config).unwrap_or_default();
    if current != before { bail!("配置在预览后发生变化，未写入；请刷新后重试"); }
    secure_write(&snapshot_path(state_dir), &serde_json::to_vec(&snapshot)?)?;
    if fs::read_to_string(&config).unwrap_or_default() != before {
        let _=fs::remove_file(snapshot_path(state_dir));
        bail!("配置在写入前发生变化，未覆盖；请刷新后重试");
    }
    if let Err(error) = secure_write(&config,doc.to_string().as_bytes()) {
        let _ = fs::remove_file(snapshot_path(state_dir));
        return Err(error).context("连接写入失败");
    }
    Ok(json!({"attached":true,"message":"已接管新会话连接；已有会话需重新载入并验证"}))
}

pub fn restore(home: &Path, state_dir: &Path) -> Result<Value> {
    let home = path_checked(home)?;
    let Some(snapshot) = snapshot_load(state_dir)? else { return Ok(json!({"attached":false,"message":"没有需要恢复的连接"})); };
    if snapshot.codex_home != home { bail!("所选目录与原接管目录不一致"); }
    let config = home.join("config.toml");
    let (before, mut doc) = read_document(&config)?;
    verify_attachment(&doc, &snapshot)?;
    let original = snapshot.before.parse::<DocumentMut>()?;
    match original.get("model_provider") {
        Some(item) => { doc["model_provider"] = item.clone(); },
        None => { doc.remove("model_provider"); },
    }
    if let Some(providers) = doc.get_mut("model_providers").and_then(Item::as_table_mut) {
        providers.remove("codexpress");
        if providers.is_empty() && original.get("model_providers").is_none() { doc.remove("model_providers"); }
    }
    if fs::read_to_string(&config)? != before { bail!("配置在恢复期间发生变化，未覆盖"); }
    if !snapshot.existed && doc.as_table().is_empty() { fs::remove_file(&config)?; }
    else { secure_write(&config,doc.to_string().as_bytes())?; }
    fs::remove_file(snapshot_path(state_dir))?;
    Ok(json!({"attached":false,"message":"已恢复原连接，保留接管期间的其他配置更改"}))
}

pub async fn handle(state: &Arc<AppState>, op: &str, payload: Value) -> Result<Value> {
    let _guard=state.config_lock.lock().await;
    let home = payload.get("codex_home").and_then(Value::as_str).context("缺少 Codex 目录")?;
    let home = Path::new(home);
    match op {
        "config.preview" => preview(home,&state.state_dir,state.port,&state.gateway_key),
        "config.attach" => attach(home,&state.state_dir,state.port,&state.gateway_key),
        "config.restore" => restore(home,&state.state_dir),
        _ => bail!("未知连接配置操作"),
    }
}
