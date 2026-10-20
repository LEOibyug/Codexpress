use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    io::Read,
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub trait CredentialStore: Send + Sync {
    fn save(&self, source_id: &str, secret: &Value) -> Result<()>;
    fn load(&self, _source_id: &str) -> Result<Option<Value>> {
        bail!("凭据读取未启用")
    }
    fn delete(&self, _source_id: &str) -> Result<()> {
        bail!("凭据删除未启用")
    }
    fn storage_status(&self) -> Result<Value> {
        bail!("存储设置未启用")
    }
    fn switch_storage(&self, _mode: &str, _file: Option<&Path>, _ids: &[String]) -> Result<Value> {
        bail!("存储切换未启用")
    }
}

pub struct KeychainStore {
    service: String,
}

impl KeychainStore {
    pub fn new(state_dir: &Path) -> Self {
        use base64::Engine;
        let service = format!(
            "com.codexpress.upstream.{}",
            base64::engine::general_purpose::STANDARD
                .encode(state_dir.to_string_lossy().as_bytes())
        );
        Self { service }
    }
}

impl CredentialStore for KeychainStore {
    fn load(&self, source_id: &str) -> Result<Option<Value>> {
        #[cfg(target_os = "macos")]
        {
            match security_framework::passwords::get_generic_password(&self.service, source_id) {
                Ok(bytes) => {
                    Ok(Some(serde_json::from_slice(&bytes).map_err(|_| {
                        anyhow::anyhow!("Codexpress 钥匙串凭据格式无效")
                    })?))
                }
                Err(error) if error.code() == -25300 => Ok(None),
                Err(_) => bail!("无法读取 Codexpress 钥匙串凭据，请在系统窗口允许访问或重试"),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = source_id;
            bail!("此版本仅支持 macOS 钥匙串")
        }
    }
    fn delete(&self, source_id: &str) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            match security_framework::passwords::delete_generic_password(&self.service, source_id) {
                Ok(()) => Ok(()),
                Err(error) if error.code() == -25300 => Ok(()),
                Err(_) => bail!("无法删除 Codexpress 钥匙串凭据"),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = source_id;
            bail!("此版本仅支持 macOS 钥匙串")
        }
    }
    fn save(&self, source_id: &str, secret: &Value) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let bytes = serde_json::to_vec(secret)?;
            security_framework::passwords::set_generic_password(&self.service, source_id, &bytes)
                .map_err(|_| {
                anyhow::anyhow!(
                    "无法保存 Codexpress 钥匙串凭据，请检查钥匙串访问权限"
                )
            })?;
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (&self.service, source_id, secret);
            anyhow::bail!("此版本仅支持 macOS 钥匙串");
        }
    }
}

const LIMIT: u64 = 8 * 1024 * 1024;

fn read_private(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("无法读取凭据文件；请检查文件权限，文件不能是符号链接"),
    };
    let meta = file.metadata().context("无法检查凭据文件")?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
        || meta.len() > LIMIT
    {
        bail!("凭据文件须为当前用户独占的普通文件，权限为 0600，大小不超过 8 MiB");
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .context("无法读取凭据文件")?;
    if bytes.len() as u64 > LIMIT {
        bail!("凭据文件过大");
    }
    Ok(Some(bytes))
}

#[derive(Serialize, Deserialize)]
struct FileContents {
    version: u32,
    sources: HashMap<String, Value>,
}
pub struct FileStore {
    path: PathBuf,
}
impl FileStore {
    pub fn new(path: PathBuf) -> Result<Self> {
        if !path.is_absolute()
            || path.file_name().and_then(|s| s.to_str()) != Some("credentials.json")
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            bail!("本地凭据文件须为绝对路径下的 credentials.json");
        }
        Ok(Self { path })
    }
    fn access<T>(
        &self,
        write: bool,
        action: impl FnOnce(&mut FileContents) -> Result<T>,
    ) -> Result<T> {
        let directory = self.path.parent().context("凭据目录无效")?;
        if !directory.exists() && write {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)
                .context("无法创建本地凭据目录")?;
        }
        let meta = match fs::symlink_metadata(directory) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !write => {
                return action(&mut FileContents {
                    version: 1,
                    sources: HashMap::new(),
                });
            }
            Err(_) => bail!("无法访问本地凭据目录"),
        };
        if !meta.is_dir()
            || meta.file_type().is_symlink()
            || meta.uid() != unsafe { libc::geteuid() }
        {
            bail!("本地凭据目录须属于当前用户且不能是符号链接");
        }
        if write {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                .context("无法保护本地凭据目录")?;
        } else if meta.mode() & 0o077 != 0 {
            bail!("本地凭据目录权限须为 0700");
        }
        let lock = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(directory.join(".credentials.lock"))
            .context("无法锁定本地凭据文件")?;
        let meta = lock.metadata().context("无法检查凭据锁")?;
        if !meta.is_file()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.nlink() != 1
            || meta.mode() & 0o077 != 0
        {
            bail!("本地凭据锁文件无效");
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("本地凭据文件正在被其他进程使用，请稍后重试");
        }
        let mut content = match read_private(&self.path)? {
            Some(bytes) => serde_json::from_slice::<FileContents>(&bytes)
                .map_err(|_| anyhow::anyhow!("本地凭据文件损坏；未覆盖，请从备份恢复"))?,
            None => FileContents {
                version: 1,
                sources: HashMap::new(),
            },
        };
        if content.version != 1 {
            bail!("本地凭据文件版本不支持");
        }
        let result = action(&mut content)?;
        if write {
            let bytes = serde_json::to_vec(&content)?;
            if bytes.len() as u64 > LIMIT {
                bail!("本地凭据文件过大，未保存");
            }
            crate::config::secure_write(&self.path, &bytes)
                .map_err(|_| anyhow::anyhow!("无法保存本地凭据，原文件未被替换"))?;
        }
        Ok(result)
    }
}
use std::os::unix::fs::DirBuilderExt;
impl CredentialStore for FileStore {
    fn load(&self, source_id: &str) -> Result<Option<Value>> {
        self.access(false, |content| Ok(content.sources.get(source_id).cloned()))
    }
    fn save(&self, source_id: &str, secret: &Value) -> Result<()> {
        self.access(true, |content| {
            content.sources.insert(source_id.into(), secret.clone());
            Ok(())
        })
    }
    fn delete(&self, source_id: &str) -> Result<()> {
        self.access(true, |content| {
            content.sources.remove(source_id);
            Ok(())
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Policy {
    mode: String,
    file_path: Option<PathBuf>,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            mode: "keychain".into(),
            file_path: None,
        }
    }
}
struct ManagedState {
    policy: Policy,
    cache: HashMap<String, Option<Value>>,
}
pub struct ManagedCredentialStore {
    keychain: Arc<dyn CredentialStore>,
    policy_path: PathBuf,
    state: Mutex<ManagedState>,
}
impl ManagedCredentialStore {
    pub fn open(state_dir: &Path, keychain: Arc<dyn CredentialStore>) -> Result<Self> {
        let policy_path = state_dir.join("credential-storage.json");
        let policy = match read_private(&policy_path)? {
            Some(bytes) => serde_json::from_slice::<Policy>(&bytes)
                .map_err(|_| anyhow::anyhow!("凭据存储设置损坏；未自动切换到其他存储"))?,
            None => Policy::default(),
        };
        if !["keychain", "file"].contains(&policy.mode.as_str())
            || policy.mode == "file" && policy.file_path.is_none()
        {
            bail!("凭据存储设置无效");
        }
        if let Some(file) = &policy.file_path {
            FileStore::new(file.clone())?;
        }
        Ok(Self {
            keychain,
            policy_path,
            state: Mutex::new(ManagedState {
                policy,
                cache: HashMap::new(),
            }),
        })
    }
    fn backend(&self, policy: &Policy) -> Result<Arc<dyn CredentialStore>> {
        if policy.mode == "keychain" {
            return Ok(self.keychain.clone());
        }
        Ok(Arc::new(FileStore::new(
            policy.file_path.clone().context("缺少本地凭据路径")?,
        )?))
    }
    fn status(policy: &Policy) -> Value {
        serde_json::json!({"mode":policy.mode,"file_path":policy.file_path})
    }
    fn loaded(&self, state: &mut ManagedState, id: &str) -> Result<Option<Value>> {
        if let Some(value) = state.cache.get(id) {
            return Ok(value.clone());
        }
        let value = self.backend(&state.policy)?.load(id)?;
        state.cache.insert(id.into(), value.clone());
        Ok(value)
    }
}
impl CredentialStore for ManagedCredentialStore {
    fn save(&self, id: &str, secret: &Value) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("凭据存储忙碌"))?;
        self.backend(&state.policy)?.save(id, secret)?;
        state.cache.insert(id.into(), Some(secret.clone()));
        Ok(())
    }
    fn load(&self, id: &str) -> Result<Option<Value>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("凭据存储忙碌"))?;
        self.loaded(&mut state, id)
    }
    fn delete(&self, id: &str) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("凭据存储忙碌"))?;
        self.backend(&state.policy)?.delete(id)?;
        state.cache.insert(id.into(), None);
        Ok(())
    }
    fn storage_status(&self) -> Result<Value> {
        Ok(Self::status(
            &self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("凭据存储忙碌"))?
                .policy,
        ))
    }
    fn switch_storage(&self, mode: &str, file: Option<&Path>, ids: &[String]) -> Result<Value> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("凭据存储忙碌"))?;
        if !["keychain", "file"].contains(&mode) {
            bail!("凭据存储方式无效");
        }
        if state.policy.mode == mode {
            return Ok(Self::status(&state.policy));
        }
        let next = Policy {
            mode: mode.into(),
            file_path: if mode == "file" {
                Some(file.context("缺少本地凭据路径")?.to_path_buf())
            } else {
                None
            },
        };
        let target = self.backend(&next)?;
        // Collect everything before writing the target; failed reads leave the active store untouched.
        let mut ids = ids.to_vec();
        ids.extend(
            state
                .cache
                .iter()
                .filter(|(_, value)| value.is_some())
                .map(|(id, _)| id.clone()),
        );
        ids.sort();
        ids.dedup();
        let mut values = Vec::new();
        for id in ids {
            let value = self.loaded(&mut state, &id)?;
            values.push((id, value));
        }
        if let Some(path) = &next.file_path {
            FileStore::new(path.clone())?.access(true, |_| Ok(()))?;
        }
        for (id, value) in &values {
            match value {
                Some(secret) => target.save(id, secret)?,
                None => target.delete(id)?,
            }
            if target.load(id)? != *value {
                bail!("凭据迁移校验失败，原存储方式保持不变");
            }
        }
        crate::config::secure_write(&self.policy_path, &serde_json::to_vec(&next)?)
            .map_err(|_| anyhow::anyhow!("无法保存存储选择，原存储方式保持不变"))?;
        state.policy = next;
        // Original copies remain available for recovery; future writes use only the selected store.
        Ok(Self::status(&state.policy))
    }
}

fn ids(payload: &Value) -> Result<Vec<String>> {
    let ids: Vec<String> =
        serde_json::from_value(payload["source_ids"].clone()).context("缺少来源列表")?;
    if ids.len() > 200
        || ids.iter().any(|id| {
            id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
    {
        bail!("来源列表无效");
    }
    Ok(ids)
}
pub async fn handle(
    state: &Arc<crate::model::AppState>,
    op: &str,
    payload: Value,
) -> Result<Value> {
    let store = state.credential_store()?;
    match op {
        "credentials.storage.status" => store.storage_status(),
        "credentials.read" => {
            let ids = ids(&payload)?;
            tokio::task::spawn_blocking(move || -> Result<Value> {
                let mut items = Vec::new();
                for id in ids {
                    items.push(serde_json::json!({"source_id":id,"secret":store.load(&id)?}));
                }
                Ok(serde_json::json!({"items":items}))
            })
            .await
            .context("凭据读取任务中断")?
        }
        "credentials.save_api" | "credentials.delete" => {
            let id = payload["source_id"]
                .as_str()
                .context("缺少来源标识")?
                .to_owned();
            ids(&serde_json::json!({"source_ids":[id]}))?;
            let sources = state.sources.write().await;
            if sources.iter().any(|r| {
                r.source.id == id && (op != "credentials.save_api" || r.source.kind == "official")
            }) {
                bail!("此来源仍在使用，或不允许写入 API 凭据");
            }
            let key = if op == "credentials.save_api" {
                Some(
                    payload["api_key"]
                        .as_str()
                        .filter(|key| !key.trim().is_empty() && key.len() <= 131072)
                        .context("API 密钥无效")?
                        .to_owned(),
                )
            } else {
                None
            };
            tokio::task::spawn_blocking(move || -> Result<Value> {
                if let Some(key) = key {
                    store.save(&id, &serde_json::json!({"api_key":key}))?;
                } else {
                    store.delete(&id)?;
                }
                Ok(serde_json::json!({"saved":true}))
            })
            .await
            .context("凭据保存任务中断")?
        }
        "credentials.storage.switch" => {
            let mut ids = ids(&payload)?;
            let mode = payload["mode"].as_str().context("缺少存储方式")?.to_owned();
            let file = payload["file_path"].as_str().map(PathBuf::from);
            // Same lock as official::install: refresh cannot race migration or publish to the old store.
            let sources = state.sources.write().await;
            ids.extend(sources.iter().map(|r| r.source.id.clone()));
            ids.sort();
            ids.dedup();
            tokio::task::spawn_blocking(move || store.switch_storage(&mode, file.as_deref(), &ids))
                .await
                .context("凭据迁移任务中断")?
        }
        _ => bail!("未知凭据操作"),
    }
}
