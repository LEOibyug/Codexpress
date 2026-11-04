//! Reversible OMP connection setup. Authentication stores and sessions are never opened.
mod configuration;
mod document;
mod preferences;
mod remote;
use crate::{config::secure_write, model::AppState};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};
use serde_yaml::{Mapping, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

const FILE_LIMIT: usize = 4 * 1024 * 1024;
const SNAPSHOT_LIMIT: usize = 24 * 1024 * 1024;
const PROVIDER: &[&str] = &["providers", "codexpress"];
const DEFAULT_MODEL: &[&str] = &["modelRoles", "default"];
const THRESHOLDS: &[&str] = &["compaction", "modelThresholds"];

#[derive(Clone, Serialize, Deserialize)]
struct AttachedFile {
    name: String,
    // Original bytes restore comments/formatting exactly when no later edits exist.
    original: Option<Vec<u8>>,
    // YAML or legacy JSON used as the original semantic base.
    baseline: String,
    applied: Vec<u8>,
    #[serde(default)]
    preserve_raw: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    agent_dir: PathBuf,
    models: AttachedFile,
    settings: AttachedFile,
    // Read once when migrating the original connection-only snapshot.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    threshold_before: BTreeMap<String, Option<String>>,
    #[serde(default)]
    native_before: Vec<configuration::Original>,
    #[serde(default)]
    added_models: Vec<Value>,
    // Legacy fields are consumed once when retiring an older managed footer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status_plugin: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    footer_before: BTreeMap<String, Option<String>>,
}
struct Document {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
    value: Value,
}

fn mapping() -> Value {
    Value::Mapping(Mapping::new())
}
fn key(value: &str) -> Value {
    Value::String(value.into())
}
fn at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |value, field| value.as_mapping()?.get(key(field)))
}
fn put(value: &mut Value, path: &[&str], replacement: Option<Value>) -> Result<()> {
    let (field, rest) = path.split_first().context("OMP 配置字段无效")?;
    if value.is_null() {
        if replacement.is_none() {
            return Ok(());
        }
        *value = mapping();
    }
    let table = value
        .as_mapping_mut()
        .context("OMP 配置字段必须为对象，未修改文件")?;
    if rest.is_empty() {
        if let Some(replacement) = replacement {
            table.insert(key(field), replacement);
        } else {
            table.remove(key(field));
        }
    } else if replacement.is_some() || table.contains_key(key(field)) {
        put(
            table.entry(key(field)).or_insert_with(mapping),
            rest,
            replacement,
        )?;
    }
    Ok(())
}
fn prune(value: &mut Value, baseline: &Value, paths: &[&[&str]]) -> Result<()> {
    for path in paths {
        if at(value, path).is_some_and(|v| v.as_mapping().is_some_and(Mapping::is_empty)) {
            match at(baseline, path) {
                None => put(value, path, None)?,
                Some(Value::Null) => put(value, path, Some(Value::Null))?,
                _ => {}
            }
        }
    }
    Ok(())
}
fn parse(bytes: &[u8]) -> Result<Value> {
    // Parser diagnostics can contain the source line, including local/user keys.
    let mut value: Value = serde_yaml::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("OMP 配置不是有效的 YAML／JSON，未修改文件"))?;
    if value.is_null() {
        value = mapping();
    }
    if !value.is_mapping() {
        bail!("OMP 配置根节点必须为对象，未修改文件");
    }
    Ok(value)
}
fn encode(value: &Value) -> Result<Vec<u8>> {
    let bytes = serde_yaml::to_string(value)
        .map_err(|_| anyhow::anyhow!("无法生成 OMP 配置"))?
        .into_bytes();
    if bytes.len() > FILE_LIMIT {
        bail!("OMP 配置超过 4 MiB，未修改文件");
    }
    Ok(bytes)
}
fn read(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("无法读取 OMP 配置，请检查权限及符号链接"),
    };
    let meta = file.metadata().context("无法检查 OMP 配置文件")?;
    if !meta.is_file() || meta.len() > limit as u64 {
        bail!("OMP 配置不是普通文件或超过大小限制");
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .context("无法读取 OMP 配置")?;
    if bytes.len() > limit {
        bail!("OMP 配置超过大小限制");
    }
    Ok(Some(bytes))
}
fn normalized_dir(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        bail!("OMP 目录须为不含上级路径跳转的绝对路径");
    }
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.is_dir() || meta.file_type().is_symlink() {
                bail!("OMP 目录必须是普通目录，不能是符号链接");
            }
            fs::canonicalize(path).context("无法读取 OMP 目录")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = normalized_dir(path.parent().context("OMP 目录无效")?)?;
            Ok(parent.join(path.file_name().context("OMP 目录无效")?))
        }
        Err(_) => bail!("无法检查 OMP 目录"),
    }
}
fn choose(home: &Path, stem: &str, legacy: &str) -> Result<Document> {
    for extension in ["yml", "yaml"] {
        let path = home.join(format!("{stem}.{extension}"));
        if let Some(bytes) = read(&path, FILE_LIMIT)? {
            let value = parse(&bytes)?;
            return Ok(Document {
                path,
                bytes: Some(bytes),
                value,
            });
        }
    }
    let value = match read(&home.join(legacy), FILE_LIMIT)? {
        Some(bytes) => parse(&bytes)?,
        None => mapping(),
    };
    Ok(Document {
        path: home.join(format!("{stem}.yml")),
        bytes: None,
        value,
    })
}

fn validate_migration(home: &Path, settings: &Document) -> Result<()> {
    if settings.bytes.is_none() {
        match fs::symlink_metadata(home.join("agent.db")) {
            Ok(_) => {
                bail!("OMP 尚有待迁移的旧配置；请先启动 OMP 完成配置迁移，生成 config.yml 后重试")
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => bail!("无法检查 OMP 配置迁移状态"),
        }
    }
    Ok(())
}
fn snapshot_path(state_dir: &Path) -> PathBuf {
    state_dir.join("omp").join("attachment.json")
}
fn load_snapshot(state_dir: &Path) -> Result<Option<Snapshot>> {
    let path = snapshot_path(state_dir);
    if let Some(parent) = path.parent() {
        if fs::symlink_metadata(parent)
            .is_ok_and(|meta| !meta.is_dir() || meta.file_type().is_symlink())
        {
            bail!("OMP 连接快照目录无效");
        }
    }
    let Some(bytes) = read(&path, SNAPSHOT_LIMIT)? else {
        return Ok(None);
    };
    let mut snapshot: Snapshot = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("OMP 连接快照损坏，停止覆盖配置"))?;
    if ![1, 2].contains(&snapshot.version)
        || !["models.yml", "models.yaml"].contains(&snapshot.models.name.as_str())
        || !["config.yml", "config.yaml"].contains(&snapshot.settings.name.as_str())
        || snapshot.threshold_before.len() > 200
    {
        bail!("OMP 连接快照无效，停止覆盖配置");
    }
    configuration::migrate(&mut snapshot)?;
    Ok(Some(snapshot))
}
fn save_snapshot(state_dir: &Path, snapshot: &Snapshot) -> Result<()> {
    let path = snapshot_path(state_dir);
    let directory = path.parent().context("OMP 连接快照目录无效")?;
    if fs::symlink_metadata(directory)
        .is_ok_and(|meta| !meta.is_dir() || meta.file_type().is_symlink())
    {
        bail!("OMP 连接快照目录无效");
    }
    fs::create_dir_all(directory).context("无法创建 OMP 连接快照目录")?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    let bytes = serde_json::to_vec(snapshot)?;
    if bytes.len() > SNAPSHOT_LIMIT {
        bail!("OMP 连接快照超过大小限制，未修改配置");
    }
    secure_write(&path, &bytes).map_err(|_| anyhow::anyhow!("无法保存 OMP 连接快照，未修改配置"))
}
fn same_home(
    snapshot: &Snapshot,
    home: &Path,
    models: &Document,
    settings: &Document,
) -> Result<()> {
    if snapshot.agent_dir != home {
        bail!("当前已接管其他 OMP 目录，请先恢复该目录");
    }
    if models.path.file_name().and_then(|name| name.to_str()) != Some(&snapshot.models.name)
        || settings.path.file_name().and_then(|name| name.to_str()) != Some(&snapshot.settings.name)
    {
        bail!("OMP 使用的配置文件已变化，未覆盖或恢复");
    }
    Ok(())
}
fn owned_matches(snapshot: &Snapshot, models: &Value, settings: &Value) -> Result<bool> {
    let applied_models = parse(&snapshot.models.applied)?;
    let applied_settings = parse(&snapshot.settings.applied)?;
    // OMP saves model/thinking choices during normal use. They are user
    // preferences, not a connection or credential integrity boundary.
    for (field, original) in &snapshot.footer_before {
        let current = at(settings, &["statusLine", field]);
        if current == at(&applied_settings, &["statusLine", field]) {
            continue;
        }
        let original: Option<Value> = original
            .as_deref()
            .map(serde_yaml::from_str)
            .transpose()
            .map_err(|_| anyhow::anyhow!("OMP 连接快照损坏"))?;
        if current != original.as_ref() {
            return Ok(false);
        }
    }
    Ok(["api", "baseUrl", "apiKey", "auth"].iter().all(|field| {
        at(models, &["providers", "codexpress", field])
            == at(&applied_models, &["providers", "codexpress", field])
    }))
}
fn verify(snapshot: &Snapshot, models: &Document, settings: &Document) -> Result<()> {
    if models.bytes.is_none()
        || settings.bytes.is_none()
        || !owned_matches(snapshot, &models.value, &settings.value)?
    {
        bail!("OMP 连接、压缩或状态栏设置已被其他程序修改；为保留你的更改，未覆盖或恢复");
    }
    Ok(())
}
fn plugin_file(home: &Path) -> Result<PathBuf> {
    let directory = normalized_dir(&home.join("extensions"))?;
    Ok(directory.join("codexpress-status.ts"))
}

fn verify_plugin(snapshot: &Snapshot, current: Option<&[u8]>) -> Result<()> {
    if current.is_some() && snapshot.status_plugin.as_deref() != current {
        bail!("OMP 状态栏插件已存在或被其他程序修改，未覆盖或恢复");
    }
    Ok(())
}

fn restore_footer(snapshot: &Snapshot, settings: &mut Value) -> Result<()> {
    if snapshot.footer_before.is_empty() {
        return Ok(());
    }
    for (field, original) in &snapshot.footer_before {
        let original = original
            .as_ref()
            .map(|value| {
                serde_yaml::from_str(value).map_err(|_| anyhow::anyhow!("OMP 连接快照损坏"))
            })
            .transpose()?;
        put(settings, &["statusLine", field], original)?;
    }
    let original = parse(snapshot.settings.baseline.as_bytes())?;
    prune(settings, &original, &[&["statusLine"]])
}
fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 256
        && !model.chars().any(char::is_whitespace)
        && !model.chars().any(char::is_control)
}
fn upstream_model(selection: &str) -> &str {
    match selection.rsplit_once(':') {
        Some((model, "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "auto"))
            if !model.is_empty() =>
        {
            model
        }
        _ => selection,
    }
}
fn verify_model_access(settings: &Value, selected: &str) -> Result<()> {
    let strings = |field| -> Result<Vec<&str>> {
        match at(settings, &[field]) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Sequence(values)) => values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .context("OMP 模型允许列表和禁用列表必须为文本数组")
                })
                .collect(),
            _ => bail!("OMP 模型允许列表和禁用列表必须为文本数组"),
        }
    };
    if strings("disabledProviders")?
        .iter()
        .any(|provider| provider.eq_ignore_ascii_case("codexpress"))
    {
        bail!("请先从 disabledProviders 解除 codexpress 禁用，再重新应用 OMP 连接");
    }
    let enabled = strings("enabledModels")?;
    let selected = selected.to_lowercase();
    let qualified = format!("codexpress/{selected}");
    // OMP resolves bare names across providers. Only a qualified selector or
    // these unambiguous Bun.Glob forms prove this custom provider is allowed.
    let allowed = enabled.is_empty()
        || enabled.iter().any(|pattern| {
            let pattern = pattern.to_lowercase();
            pattern == qualified
                || matches!(pattern.as_str(), "**" | "codexpress/**")
                || (!selected.contains('/') && matches!(pattern.as_str(), "*" | "codexpress/*"))
        });
    if !allowed {
        bail!("请先将 codexpress/* 加入 enabledModels 允许列表，再重新应用；带命名空间的模型请使用 codexpress/**，也可填写完整 codexpress/模型ID");
    }
    Ok(())
}
fn model_metadata(ids: &BTreeSet<String>) -> Result<Value> {
    let catalog = crate::compaction::bundled_catalog()?;
    let models = catalog["models"].as_array().context("内置模型目录无效")?;
    let mut output = Vec::new();
    for id in ids {
        let mut model = serde_yaml::to_value(json!({"id":id}))?;
        let metadata = models
            .iter()
            .find(|model| model["slug"].as_str() == Some(id));
        if let Some(metadata) = metadata {
            if let Some(window) = metadata["context_window"]
                .as_i64()
                .filter(|value| *value > 0)
            {
                put(
                    &mut model,
                    &["contextWindow"],
                    Some(Value::Number(window.into())),
                )?;
            }
            if let Some(levels) = metadata["supported_reasoning_levels"].as_array() {
                put(
                    &mut model,
                    &["reasoning"],
                    Some(Value::Bool(!levels.is_empty())),
                )?;
            }
            if let Some(input) = metadata["input_modalities"].as_array() {
                let supported: Vec<_> = input
                    .iter()
                    .filter_map(Json::as_str)
                    .filter(|kind| ["text", "image"].contains(kind))
                    .collect();
                if !supported.is_empty() {
                    put(
                        &mut model,
                        &["input"],
                        Some(serde_yaml::to_value(supported)?),
                    )?;
                }
            }
        }
        output.push(model);
    }
    Ok(Value::Sequence(output))
}
fn attached_file(document: &Document, applied: Vec<u8>) -> Result<AttachedFile> {
    Ok(AttachedFile {
        name: document
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .context("OMP 文件名无效")?
            .into(),
        original: document.bytes.clone(),
        baseline: String::from_utf8(encode(&document.value)?).context("OMP 配置编码无效")?,
        applied,
        preserve_raw: false,
    })
}

// Compare immediately before each replace. On failure restore all already-written
// files, but never overwrite a concurrent writer's bytes during rollback.
type FileChange<'a> = (&'a Path, Option<&'a [u8]>, Option<&'a [u8]>);
fn write_files(changes: &[FileChange<'_>]) -> Result<()> {
    let mut written = Vec::new();
    let result = (|| -> Result<()> {
        for (path, before, after) in changes {
            if read(path, FILE_LIMIT)?.as_deref() != *before {
                bail!("OMP 配置在写入前发生变化，未覆盖");
            }
            if before == after {
                continue;
            }
            match after {
                Some(bytes) => {
                    secure_write(path, bytes).map_err(|_| anyhow::anyhow!("无法写入 OMP 配置"))?
                }
                None => {
                    if before.is_some() {
                        fs::remove_file(path).context("无法恢复 OMP 配置文件")?;
                    }
                }
            }
            written.push((*path, *before, *after));
            #[cfg(test)]
            if written.len() == 1 {
                let failure = FAIL_AFTER_FIRST_WRITE.with(|flag| flag.get());
                if failure == 2 {
                    fs::write(path, b"concurrent writer\n")?;
                }
                if failure != 0 {
                    bail!("模拟写入失败");
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        for (path, before, after) in written.into_iter().rev() {
            if !read(path, FILE_LIMIT).is_ok_and(|bytes| bytes.as_deref() == after) {
                bail!("OMP 写入失败且文件已被其他程序修改，请保留连接快照后检查文件");
            }
            let rollback = match before {
                Some(bytes) => secure_write(path, bytes),
                None => fs::remove_file(path).map_err(Into::into),
            };
            if rollback.is_err() {
                bail!("OMP 写入失败，自动恢复未完成；连接快照已保留");
            }
        }
        return Err(error);
    }
    Ok(())
}
#[cfg(test)]
thread_local! { static FAIL_AFTER_FIRST_WRITE: std::cell::Cell<u8> = const { std::cell::Cell::new(0) }; }

fn has_raw_edits(owned: &AttachedFile, current: &Document, mutable_default: bool) -> Result<bool> {
    if owned.applied.is_empty() || current.bytes.as_deref() == Some(owned.applied.as_slice()) {
        return Ok(false);
    }
    if mutable_default {
        let applied = parse(&owned.applied)?;
        let mut expected = applied.clone();
        put(&mut expected, DEFAULT_MODEL, at(&current.value, DEFAULT_MODEL).cloned())?;
        let bytes = document::patch(Some(&owned.applied), &applied, &expected)?;
        return Ok(current.bytes.as_deref() != Some(bytes.as_slice()));
    }
    Ok(true)
}

fn preserve_raw_edits(snapshot: &mut Snapshot, models: &Document, settings: &Document) -> Result<()> {
    snapshot.models.preserve_raw |= has_raw_edits(&snapshot.models, models, false)?;
    snapshot.settings.preserve_raw |= has_raw_edits(&snapshot.settings, settings, true)?;
    Ok(())
}

fn restored_bytes(owned: &AttachedFile, current: &Document, original: &Value, restored: &Value, mutable_default: bool) -> Result<Option<Vec<u8>>> {
    if restored == original && !owned.preserve_raw && !has_raw_edits(owned, current, mutable_default)? {
        return Ok(owned.original.clone());
    }
    let bytes = document::patch(current.bytes.as_deref(), &current.value, restored)?;
    // Re-serialization by OMP can reorder the managed fields. Once those fields
    // are removed, an originally absent file needs no empty mapping; comments
    // or any other surviving user content still keep the file.
    if owned.original.is_none() && bytes.trim_ascii() == b"{}" {
        return Ok(None);
    }
    Ok(Some(bytes))
}

type RestoredDocuments = (Option<Vec<u8>>, Option<Vec<u8>>);

fn restore_documents(
    snapshot: &Snapshot,
    models: &Document,
    settings: &Document,
) -> Result<RestoredDocuments> {
    let original_models = parse(snapshot.models.baseline.as_bytes())?;
    let original_settings = parse(snapshot.settings.baseline.as_bytes())?;
    let mut restored_models = models.value.clone();
    let mut restored_settings = settings.value.clone();
    configuration::restore(snapshot, &mut restored_models, &mut restored_settings)?;
    let selected_codexpress = at(&settings.value, DEFAULT_MODEL)
        .and_then(Value::as_str)
        .and_then(|selection| selection.split_once('/'))
        .is_some_and(|(provider, _)| provider.eq_ignore_ascii_case("codexpress"));
    if selected_codexpress {
        put(
            &mut restored_settings,
            DEFAULT_MODEL,
            at(&original_settings, DEFAULT_MODEL).cloned(),
        )?;
    }
    restore_footer(snapshot, &mut restored_settings)?;
    configuration::restore_provider(snapshot, &mut restored_models, &original_models)?;
    prune(&mut restored_models, &original_models, &[&["providers"]])?;
    prune(
        &mut restored_settings,
        &original_settings,
        &[
            THRESHOLDS,
            &["compaction"],
            &["modelRoles"],
            &["statusLine"],
        ],
    )?;
    let models_after = restored_bytes(&snapshot.models, models, &original_models, &restored_models, false)?;
    let settings_after = restored_bytes(&snapshot.settings, settings, &original_settings, &restored_settings, true)?;
    Ok((models_after, settings_after))
}

async fn source_models(state: &Arc<AppState>, selected_model: Option<&str>) -> Result<BTreeSet<String>> {
    let mut ids: BTreeSet<String> = state
        .sources
        .read()
        .await
        .iter()
        .filter(|source| source.source.enabled)
        .flat_map(|source| {
            source
                .source
                .models
                .iter()
                .filter(|model| valid_model(model))
                .cloned()
        })
        .collect();
    if let Some(model) = selected_model { ids.insert(model.into()); }
    if ids.len() > 10_000 {
        bail!("OMP 模型列表超过 10000 项");
    }
    Ok(ids)
}

fn prepare_attachment(
    home: &Path,
    models: &Document,
    settings: &Document,
    previous: &Option<Snapshot>,
    selected: &str,
    ids: &BTreeSet<String>,
    connection: (&str, &str),
) -> Result<(Snapshot, Value, Value)> {
    let (base_url, local_key) = connection;
    if previous.is_none() && at(&models.value, PROVIDER).is_some() {
        bail!("OMP 已有名为 codexpress 的供应商，未覆盖");
    }
    let mut models_after = models.value.clone();
    let mut settings_after = settings.value.clone();
    let mut snapshot = match previous.as_ref() {
        Some(snapshot) => snapshot.clone(),
        None => Snapshot {
            version: 2,
            agent_dir: home.to_path_buf(),
            models: attached_file(models, Vec::new())?,
            settings: attached_file(settings, Vec::new())?,
            threshold_before: BTreeMap::new(),
            native_before: Vec::new(),
            added_models: Vec::new(),
            status_plugin: None,
            footer_before: BTreeMap::new(),
        },
    };
    preserve_raw_edits(&mut snapshot, models, settings)?;
    for (field, value) in [
        ("api", "openai-responses"),
        ("baseUrl", base_url),
        ("apiKey", local_key),
        ("auth", "apiKey"),
    ] {
        put(
            &mut models_after,
            &["providers", "codexpress", field],
            Some(key(value)),
        )?;
    }
    configuration::merge_models(
        &mut models_after,
        model_metadata(ids)?,
        Some(&mut snapshot.added_models),
    )?;
    put(
        &mut settings_after,
        DEFAULT_MODEL,
        Some(key(&format!("codexpress/{selected}"))),
    )?;
    restore_footer(&snapshot, &mut settings_after)?;
    snapshot.footer_before.clear();
    snapshot.status_plugin = None;
    Ok((snapshot, models_after, settings_after))
}

fn status(
    snapshot: Option<&Snapshot>,
    home: &Path,
    models: &Document,
    settings: &Document,
    base_url: &str,
    local_key: &str,
) -> Result<Json> {
    let provider = at(&models.value, PROVIDER);
    let default = at(&settings.value, DEFAULT_MODEL)
        .and_then(Value::as_str)
        .and_then(|model| model.strip_prefix("codexpress/"));
    let needs_update = if let Some(snapshot) = snapshot {
        !owned_matches(snapshot, &models.value, &settings.value)?
            || at(&models.value, &["providers", "codexpress", "baseUrl"]).and_then(Value::as_str)
                != Some(base_url)
            || at(&models.value, &["providers", "codexpress", "apiKey"]).and_then(Value::as_str)
                != Some(local_key)
            || snapshot.status_plugin.is_some()
            || !snapshot.footer_before.is_empty()
    } else {
        false
    };
    Ok(
        json!({"attached":snapshot.is_some(),"agent_dir":home,"models_file":models.path,
        "settings_file":settings.path,"model_id":default,
        "model_count":provider.and_then(|provider| at(provider, &["models"]))
            .and_then(Value::as_sequence).map_or(0,Vec::len),
        "base_url":base_url,"needs_update":needs_update,
        "configuration_revision":configuration::revision(models, settings),
        "configuration":preferences::configuration(&settings.value, &models.value)?}),
    )
}

pub async fn handle(state: &Arc<AppState>, op: &str, payload: Json) -> Result<Json> {
    if matches!(op, "omp.remote.read" | "omp.remote.preview" | "omp.remote.restore") {
        return remote::handle(state, op, payload).await;
    }
    if !["omp.read", "omp.status", "omp.attach", "omp.restore", "omp.configure"].contains(&op) {
        bail!("未知 OMP 管理操作");
    }
    let _guard = state.config_lock.lock().await;
    let home = payload["agent_dir"].as_str().context("缺少 OMP 配置目录")?;
    let home = normalized_dir(Path::new(home))?;
    let mut models = choose(&home, "models", "models.json")?;
    let mut settings = choose(&home, "config", "settings.json")?;
    if op == "omp.read" {
        validate_migration(&home, &settings)?;
        let model = at(&settings.value, DEFAULT_MODEL)
            .and_then(Value::as_str)
            .and_then(|model| model.strip_prefix("codexpress/"))
            .filter(|model| valid_model(model));
        return Ok(json!({
            "agent_dir": home,
            "model_id": model,
            "configuration_revision": configuration::revision(&models, &settings),
            "configuration": preferences::configuration(&settings.value, &models.value)?,
        }));
    }
    let previous = load_snapshot(&state.state_dir)?;
    if let Some(snapshot) = &previous {
        same_home(snapshot, &home, &models, &settings)?;
    }
    let plugin_path = previous
        .as_ref()
        .filter(|snapshot| snapshot.status_plugin.is_some())
        .map(|_| plugin_file(&home))
        .transpose()?;
    let plugin_before = plugin_path
        .as_ref()
        .map(|path| read(path, FILE_LIMIT))
        .transpose()?
        .flatten();
    let access = state.chatbot.status().await?;
    let base_url = access["base_url"].as_str().context("聊天客户端地址无效")?;
    let local_key = state.chatbot.key()?["key"]
        .as_str()
        .context("聊天客户端密钥不可用")?
        .to_owned();
    if op == "omp.status" {
        return status(
            previous.as_ref(),
            &home,
            &models,
            &settings,
            base_url,
            &local_key,
        );
    }
    if op == "omp.configure" {
        let snapshot = previous.as_ref().context("请先接入 OMP，再修改压缩配置")?;
        let updated =
            configuration::configure(&state.state_dir, snapshot, &models, &settings, &payload)?;
        models = choose(&home, "models", "models.json")?;
        settings = choose(&home, "config", "settings.json")?;
        return status(
            Some(&updated),
            &home,
            &models,
            &settings,
            base_url,
            &local_key,
        );
    }
    if let Some(snapshot) = &previous {
        verify(snapshot, &models, &settings)?;
        verify_plugin(snapshot, plugin_before.as_deref())?;
    }
    if op == "omp.restore" {
        let Some(snapshot) = previous else {
            let mut result = status(None, &home, &models, &settings, base_url, &local_key)?;
            result["message"] = json!("没有需要恢复的 OMP 连接");
            return Ok(result);
        };
        let (models_after, settings_after) = restore_documents(&snapshot, &models, &settings)?;
        let mut changes: Vec<FileChange<'_>> = vec![
            (
                &models.path,
                models.bytes.as_deref(),
                models_after.as_deref(),
            ),
            (
                &settings.path,
                settings.bytes.as_deref(),
                settings_after.as_deref(),
            ),
        ];
        if let Some(path) = &plugin_path {
            changes.push((path, plugin_before.as_deref(), None));
        }
        write_files(&changes)?;
        fs::remove_file(snapshot_path(&state.state_dir))
            .context("OMP 已恢复，但连接快照未能移除")?;
        models = choose(&home, "models", "models.json")?;
        settings = choose(&home, "config", "settings.json")?;
        let mut result = status(None, &home, &models, &settings, base_url, &local_key)?;
        result["message"] = json!("已恢复原 OMP 连接，保留其他配置更改；请重新启动 OMP");
        return Ok(result);
    }
    if access["enabled"] != true || access["running"] != true {
        bail!("请先启用并启动聊天客户端接入");
    }
    validate_migration(&home, &settings)?;
    let selected = payload["model_id"]
        .as_str()
        .filter(|model| valid_model(model))
        .context("请提供有效的 OMP 默认模型 ID")?;
    let selected_model = upstream_model(selected);
    verify_model_access(&settings.value, selected_model)?;
    let ids = source_models(state, Some(selected_model)).await?;
    let (mut snapshot, models_after, settings_after) = prepare_attachment(
        &home, &models, &settings, &previous, selected, &ids, (base_url, &local_key),
    )?;
    snapshot.models.applied =
        document::patch(models.bytes.as_deref(), &models.value, &models_after)?;
    snapshot.settings.applied =
        document::patch(settings.bytes.as_deref(), &settings.value, &settings_after)?;
    // Fully prepare both documents and verify both originals before the first mutation.
    if read(&models.path, FILE_LIMIT)? != models.bytes
        || read(&settings.path, FILE_LIMIT)? != settings.bytes
    {
        bail!("OMP 配置在应用前发生变化，未写入");
    }
    let existed = home.exists();
    fs::create_dir_all(&home).context("无法创建 OMP 配置目录")?;
    if !existed {
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700))?;
    }
    save_snapshot(&state.state_dir, &snapshot)?;
    let mut changes: Vec<FileChange<'_>> = vec![
        (
            &models.path,
            models.bytes.as_deref(),
            Some(&snapshot.models.applied),
        ),
        (
            &settings.path,
            settings.bytes.as_deref(),
            Some(&snapshot.settings.applied),
        ),
    ];
    if let Some(path) = &plugin_path {
        changes.push((path, plugin_before.as_deref(), None));
    }
    if let Err(error) = write_files(&changes) {
        if !read(&models.path, FILE_LIMIT).is_ok_and(|bytes| bytes == models.bytes)
            || !read(&settings.path, FILE_LIMIT).is_ok_and(|bytes| bytes == settings.bytes)
            || plugin_path.as_ref().is_some_and(|path| {
                !read(path, FILE_LIMIT).is_ok_and(|bytes| bytes == plugin_before)
            })
        {
            bail!("OMP 应用失败，文件未能完整恢复；连接快照已保留，请检查配置文件");
        }
        if let Some(previous) = &previous {
            save_snapshot(&state.state_dir, previous)?;
        } else {
            fs::remove_file(snapshot_path(&state.state_dir))
                .context("无法清理未应用的 OMP 连接快照")?;
        }
        return Err(error);
    }
    models.bytes = Some(snapshot.models.applied.clone());
    models.value = models_after;
    settings.bytes = Some(snapshot.settings.applied.clone());
    settings.value = settings_after;
    let mut result = status(
        Some(&snapshot),
        &home,
        &models,
        &settings,
        base_url,
        &local_key,
    )?;
    result["message"] = json!("已连接 Codexpress 并应用模型压缩设置；请重新启动 OMP");
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn second_file_failure_rolls_back_first_file() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("models.yml");
        let second = directory.path().join("config.yml");
        fs::write(&first, b"original models").unwrap();
        fs::write(&second, b"original settings").unwrap();
        FAIL_AFTER_FIRST_WRITE.with(|flag| flag.set(1));
        let result = write_files(&[
            (&first, Some(b"original models"), Some(b"changed models")),
            (
                &second,
                Some(b"original settings"),
                Some(b"changed settings"),
            ),
        ]);
        FAIL_AFTER_FIRST_WRITE.with(|flag| flag.set(0));
        assert!(result.is_err());
        assert_eq!(fs::read(first).unwrap(), b"original models");
        assert_eq!(fs::read(second).unwrap(), b"original settings");
    }

    #[tokio::test]
    async fn apply_failure_clears_intent_only_after_complete_rollback() {
        let directory = tempfile::tempdir().unwrap();
        let state =
            AppState::new(directory.path().into(), 17863, "PRIVATE-NATIVE-KEY".into()).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        crate::chatbot::configure(&state, json!({"port":port}))
            .await
            .unwrap();
        let home = directory.path().join("agent");
        fs::create_dir(&home).unwrap();
        fs::write(
            home.join("models.yml"),
            b"# original models\nproviders: {}\n",
        )
        .unwrap();
        fs::write(
            home.join("config.yml"),
            b"# original settings\ncustom: true\n",
        )
        .unwrap();
        let original_models = fs::read(home.join("models.yml")).unwrap();
        let original_settings = fs::read(home.join("config.yml")).unwrap();
        let payload = json!({"agent_dir":home,"model_id":"explicit-model"});
        FAIL_AFTER_FIRST_WRITE.with(|flag| flag.set(1));
        let result = handle(&state, "omp.attach", payload.clone()).await;
        FAIL_AFTER_FIRST_WRITE.with(|flag| flag.set(0));
        assert!(result.is_err());
        assert_eq!(fs::read(home.join("models.yml")).unwrap(), original_models);
        assert_eq!(
            fs::read(home.join("config.yml")).unwrap(),
            original_settings
        );
        assert!(!snapshot_path(&state.state_dir).exists());

        FAIL_AFTER_FIRST_WRITE.with(|flag| flag.set(2));
        let result = handle(&state, "omp.attach", payload).await;
        FAIL_AFTER_FIRST_WRITE.with(|flag| flag.set(0));
        assert!(result.unwrap_err().to_string().contains("快照已保留"));
        assert_eq!(
            fs::read(home.join("models.yml")).unwrap(),
            b"concurrent writer\n"
        );
        assert_eq!(
            fs::read(home.join("config.yml")).unwrap(),
            original_settings
        );
        let snapshot = load_snapshot(&state.state_dir).unwrap().unwrap();
        assert_eq!(snapshot.models.original, Some(original_models));
        assert_eq!(snapshot.settings.original, Some(original_settings));
    }
}
