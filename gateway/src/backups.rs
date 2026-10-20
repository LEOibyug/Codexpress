//! Private, file-based recovery points. Credentials never cross IPC or appear
//! in metadata. Only explicit configuration mutations call `create`.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const LIMIT: u64 = 16 * 1024 * 1024;
const TOTAL_LIMIT: usize = 32 * 1024 * 1024;
const MAX_FILES: usize = 200;
const FIXED: [&str; 3] = ["config.toml", "auth.json", "auth.json.bak"];
type Files = BTreeMap<String, Option<Vec<u8>>>;
type Captured = (Files, Option<Vec<u8>>);

#[derive(Serialize, Deserialize)]
struct Entry {
    present: bool,
    digest: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    id: String,
    created_at: u64,
    reason: String,
    codex_home: PathBuf,
    files: BTreeMap<String, Entry>,
    attachment: Entry,
}

fn private_dir(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("备份目录不能是符号链接");
    }
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn root(state: &Path) -> PathBuf {
    state.join("configuration-backups")
}
fn safe_name(name: &str) -> bool {
    FIXED.contains(&name)
        || (name.ends_with(".config.toml") && !name.contains(['/', '\\']) && !name.starts_with('.'))
}
fn read_file(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("无法读取配置或认证文件；请检查权限及符号链接"),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > LIMIT {
        bail!("配置或认证文件类型无效，或超过 16 MiB 限制");
    }
    let mut data = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut data)?;
    if data.len() as u64 > LIMIT {
        bail!("配置或认证文件超过 16 MiB 限制");
    }
    Ok(Some(data))
}
fn names(home: &Path) -> Result<Vec<String>> {
    let mut result: Vec<String> = FIXED.iter().map(|n| (*n).into()).collect();
    match fs::read_dir(home) {
        Ok(entries) => {
            for entry in entries {
                let name = entry?
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("配置文件名称无效"))?;
                if name.ends_with(".config.toml") {
                    if !safe_name(&name) {
                        bail!("配置档案名称无效");
                    }
                    result.push(name);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => bail!("无法读取配置目录"),
    }
    if result.len() > MAX_FILES {
        bail!("配置档案数量超过备份限制");
    }
    result.sort();
    result.dedup();
    Ok(result)
}
fn entry(data: &Option<Vec<u8>>) -> Entry {
    Entry {
        present: data.is_some(),
        digest: data.as_ref().map(|d| format!("{:x}", Sha256::digest(d))),
    }
}
fn capture(home: &Path, state: &Path) -> Result<Captured> {
    let mut files = BTreeMap::new();
    let mut total = 0;
    for name in names(home)? {
        let data = read_file(&home.join(&name))?;
        total += data.as_ref().map_or(0, Vec::len);
        if total > TOTAL_LIMIT {
            bail!("配置与认证备份超过 32 MiB 限制");
        }
        files.insert(name, data);
    }
    Ok((files, read_file(&state.join("config-attachment.json"))?))
}
pub fn create(home: &Path, state: &Path, reason: &str) -> Result<String> {
    let (files, attachment) = capture(home, state)?;
    private_dir(&root(state))?;
    let id = format!("{}-{}", crate::model::now(), uuid::Uuid::new_v4());
    let pending = root(state).join(format!(".pending-{id}"));
    private_dir(&pending)?;
    let result = (|| {
        private_dir(&pending.join("files"))?;
        let manifest = Manifest {
            version: 1,
            id: id.clone(),
            created_at: crate::model::now(),
            reason: reason.into(),
            codex_home: fs::canonicalize(home).unwrap_or_else(|_| home.to_owned()),
            files: files.iter().map(|(n, d)| (n.clone(), entry(d))).collect(),
            attachment: entry(&attachment),
        };
        for (name, data) in &files {
            if let Some(data) = data {
                crate::config::secure_write(&pending.join("files").join(name), data)?;
            }
        }
        if let Some(data) = &attachment {
            crate::config::secure_write(&pending.join("attachment.json"), data)?;
        }
        crate::config::secure_write(
            &pending.join("manifest.json"),
            &serde_json::to_vec(&manifest)?,
        )?;
        // Detect a concurrent native login/config write before calling this a recovery point.
        let (after, after_attachment) = capture(home, state)?;
        if after != files || after_attachment != attachment {
            bail!("备份期间配置或认证发生变化，请先退出 Codex 后重试");
        }
        fs::rename(&pending, root(state).join(&id))?;
        Ok(id.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&pending);
    }
    result
}
fn load(state: &Path, id: &str) -> Result<(PathBuf, Manifest)> {
    if id.is_empty()
        || id.len() > 100
        || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        bail!("备份标识无效");
    }
    let dir = root(state).join(id);
    if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink())
        || fs::symlink_metadata(dir.join("files")).is_ok_and(|m| m.file_type().is_symlink())
    {
        bail!("备份目录无效");
    }
    let manifest: Manifest =
        serde_json::from_slice(&read_file(&dir.join("manifest.json"))?.context("备份不存在")?)
            .context("备份记录损坏")?;
    if manifest.version != 1
        || manifest.id != id
        || manifest.files.len() > MAX_FILES
        || FIXED.iter().any(|n| !manifest.files.contains_key(*n))
        || manifest.files.keys().any(|n| !safe_name(n))
    {
        bail!("备份记录无效");
    }
    Ok((dir, manifest))
}
pub fn list(home: &Path, state: &Path) -> Result<Value> {
    let mut rows = Vec::new();
    match fs::read_dir(root(state)) {
        Ok(entries) => {
            for entry in entries {
                let id = entry?.file_name().to_string_lossy().into_owned();
                if id.starts_with('.') {
                    continue;
                }
                let (_, m) = load(state, &id)?;
                if m.codex_home != fs::canonicalize(home).unwrap_or_else(|_| home.to_owned()) {
                    continue;
                }
                rows.push(json!({"id":m.id,"created_at":m.created_at,"reason":m.reason,"has_auth":m.files.get("auth.json").is_some_and(|e|e.present),"file_count":m.files.values().filter(|e|e.present).count()}));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => bail!("无法读取备份列表"),
    }
    rows.sort_by(|a, b| {
        b["created_at"]
            .as_u64()
            .cmp(&a["created_at"].as_u64())
            .then(b["id"].as_str().cmp(&a["id"].as_str()))
    });
    Ok(json!({"backups": rows}))
}
fn checked(path: &Path, expected: &Entry) -> Result<Option<Vec<u8>>> {
    let data = read_file(path)?;
    if expected.present != data.is_some() || expected.digest != entry(&data).digest {
        bail!("备份文件缺失或校验失败，未还原");
    }
    Ok(data)
}
fn contents(home: &Path, state: &Path, id: &str) -> Result<Captured> {
    let (dir, m) = load(state, id)?;
    if m.codex_home != fs::canonicalize(home).unwrap_or_else(|_| home.to_owned()) {
        bail!("备份与当前配置目录不一致");
    }
    let mut files = BTreeMap::new();
    for (name, e) in m.files {
        files.insert(name.clone(), checked(&dir.join("files").join(name), &e)?);
    }
    let attachment = checked(&dir.join("attachment.json"), &m.attachment)?;
    Ok((files, attachment))
}
pub fn preflight(home: &Path, state: &Path, id: &str) -> Result<Value> {
    let _ = contents(home, state, id)?;
    let _ = capture(home, state)?;
    Ok(json!({"restorable":true}))
}
pub fn restore_auth(home: &Path, state: &Path, id: &str) -> Result<()> {
    let (files, _) = contents(home, state, id)?;
    let mut current = BTreeMap::new();
    for name in ["auth.json", "auth.json.bak"] {
        current.insert(name, read_file(&home.join(name))?);
    }
    for name in ["auth.json", "auth.json.bak"] {
        if replace(&home.join(name), files.get(name).and_then(|d| d.as_deref())).is_err() {
            for (name, data) in &current {
                let _ = replace(&home.join(name), data.as_deref());
            }
            bail!("恢复此模式的认证失败，请从配置备份还原");
        }
    }
    Ok(())
}
fn replace(path: &Path, data: Option<&[u8]>) -> Result<()> {
    match data {
        Some(data) => crate::config::secure_write(path, data),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        },
    }
}
pub fn restore(home: &Path, state: &Path, id: &str) -> Result<Value> {
    let (mut target, attachment) = contents(home, state, id)?;
    let (current, current_attachment) = capture(home, state)?;
    // Remove only profile/config/auth files that were absent at the selected point.
    for name in current.keys() {
        target.entry(name.clone()).or_insert(None);
    }
    let recovery = create(home, state, "before_restore")?;
    let (unchanged, unchanged_attachment) = capture(home, state)?;
    if unchanged != current || unchanged_attachment != current_attachment {
        bail!("配置或认证在还原前发生变化，未覆盖");
    }
    let result = (|| {
        for (name, data) in &target {
            replace(&home.join(name), data.as_deref())?;
        }
        replace(&state.join("config-attachment.json"), attachment.as_deref())?;
        Ok::<_, anyhow::Error>(())
    })();
    if result.is_err() {
        let rollback = (|| {
            for (name, data) in &current {
                replace(&home.join(name), data.as_deref())?;
            }
            for name in target.keys().filter(|n| !current.contains_key(*n)) {
                replace(&home.join(name), None)?;
            }
            replace(
                &state.join("config-attachment.json"),
                current_attachment.as_deref(),
            )
        })();
        if rollback.is_err() {
            bail!("还原未完成，自动回退也失败；当前状态已备份，请从备份重试");
        }
        bail!("还原失败，已回退到还原前状态");
    }
    Ok(json!({"restored":true,"recovery_backup_id":recovery}))
}
