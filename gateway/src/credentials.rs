use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    io::Read,
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
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
}

const LIMIT: u64 = 8 * 1024 * 1024;

fn private_directory_exists(path: &Path) -> Result<bool> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => bail!("无法访问本地凭据目录"),
    };
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
        bail!("本地凭据目录须属于当前用户且不能是符号链接");
    }
    if meta.mode() & 0o7777 != 0o700 {
        bail!("本地凭据目录权限须为 0700");
    }
    Ok(true)
}

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
        || meta.mode() & 0o7777 != 0o600
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
enum FileAccess<'a> {
    Read,
    Save(&'a str, &'a Value),
    Delete(&'a str),
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
    fn access(&self, operation: FileAccess<'_>) -> Result<HashMap<String, Value>> {
        let write = !matches!(operation, FileAccess::Read);
        let directory = self.path.parent().context("凭据目录无效")?;
        if !private_directory_exists(directory)? {
            if !write {
                return Ok(HashMap::new());
            }
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)
                .context("无法创建本地凭据目录")?;
            if !private_directory_exists(directory)? {
                bail!("本地凭据目录不存在");
            }
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
            || meta.mode() & 0o7777 != 0o600
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
        match operation {
            FileAccess::Read => return Ok(content.sources),
            FileAccess::Save(id, secret) => {
                content.sources.insert(id.to_owned(), secret.clone());
            }
            FileAccess::Delete(id) => {
                content.sources.remove(id);
            }
        }
        let bytes = serde_json::to_vec(&content)?;
        if bytes.len() as u64 > LIMIT {
            bail!("本地凭据文件过大，未保存");
        }
        crate::config::secure_write(&self.path, &bytes)
            .map_err(|_| anyhow::anyhow!("无法保存本地凭据，原文件未被替换"))?;
        Ok(content.sources)
    }
}
impl CredentialStore for FileStore {
    fn load(&self, source_id: &str) -> Result<Option<Value>> {
        Ok(self.access(FileAccess::Read)?.remove(source_id))
    }
    fn save(&self, source_id: &str, secret: &Value) -> Result<()> {
        self.access(FileAccess::Save(source_id, secret)).map(drop)
    }
    fn delete(&self, source_id: &str) -> Result<()> {
        self.access(FileAccess::Delete(source_id)).map(drop)
    }
    fn storage_status(&self) -> Result<Value> {
        Ok(serde_json::json!({"mode":"file","file_path":self.path}))
    }
}

#[derive(Deserialize)]
struct LegacyPolicy {
    mode: String,
    file_path: Option<PathBuf>,
}

// Only the local gateway owns its credential snapshot. Shared FileStore clients
// read under the file lock each time so they see changes from other processes.
pub struct ManagedCredentialStore {
    file: FileStore,
    cache: Mutex<Option<HashMap<String, Value>>>,
}
impl ManagedCredentialStore {
    pub fn open(state_dir: &Path) -> Result<Self> {
        let policy = if private_directory_exists(state_dir)? {
            read_private(&state_dir.join("credential-storage.json"))?
        } else {
            None
        };
        let legacy_file = match policy {
            None => None,
            Some(bytes) => {
                let policy: LegacyPolicy = serde_json::from_slice(&bytes)
                    .map_err(|_| anyhow::anyhow!("凭据存储设置损坏；未自动切换到其他存储"))?;
                let file = policy.file_path.map(FileStore::new).transpose()?;
                match policy.mode.as_str() {
                    "file" => Some(file.context("缺少本地凭据路径")?),
                    // A former file-mode copy is not authoritative in this mode.
                    "keychain" => None,
                    _ => bail!("凭据存储设置无效"),
                }
            }
        };
        let file = match legacy_file {
            Some(file) => file,
            None => FileStore::new(state_dir.join("credentials.json"))?,
        };
        Ok(Self {
            file,
            cache: Mutex::new(None),
        })
    }

    fn update(&self, operation: FileAccess<'_>) -> Result<()> {
        let mut cache = self.cache.lock().map_err(|_| anyhow::anyhow!("凭据存储忙碌"))?;
        *cache = Some(self.file.access(operation)?);
        Ok(())
    }
}
impl CredentialStore for ManagedCredentialStore {
    fn load(&self, id: &str) -> Result<Option<Value>> {
        let mut cache = self.cache.lock().map_err(|_| anyhow::anyhow!("凭据存储忙碌"))?;
        if cache.is_none() {
            *cache = Some(self.file.access(FileAccess::Read)?);
        }
        Ok(cache.as_ref().and_then(|sources| sources.get(id)).cloned())
    }
    fn save(&self, id: &str, secret: &Value) -> Result<()> {
        self.update(FileAccess::Save(id, secret))
    }
    fn delete(&self, id: &str) -> Result<()> {
        self.update(FileAccess::Delete(id))
    }
    fn storage_status(&self) -> Result<Value> {
        self.file.storage_status()
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
        _ => bail!("未知凭据操作"),
    }
}
