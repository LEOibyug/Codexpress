//! Independent loopback management HTTP ingress. Configuration and reset approval
//! remain available only through the private Unix-socket management bridge.
mod dashboard;
mod http;
mod jobs;
mod resets;
mod safety;

use crate::{config::secure_write, model::AppState};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    enabled: bool,
    port: u16,
    key: String,
}
struct Listener {
    port: u16,
    task: tokio::task::JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
}
pub struct ControlAccess {
    settings: Mutex<Settings>,
    revision: AtomicU64,
    path: PathBuf,
    listener: AsyncMutex<Option<Listener>>,
    resets: Mutex<resets::Queue>,
    operations: Mutex<jobs::Operations>,
}
fn generate_key() -> String {
    format!(
        "cxm-{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
fn validate_key(key: &str) -> Result<()> {
    if !(16..=256).contains(&key.len()) || !key.bytes().all(|b| b.is_ascii_graphic()) {
        bail!("管理 API Key 须为 16–256 位不含空白的 ASCII 字符");
    }
    Ok(())
}
impl ControlAccess {
    pub fn open(
        directory: &Path,
        main_port: u16,
        gateway_key: &str,
        chatbot: &crate::chatbot::ChatbotAccess,
        images: &crate::images::ImageAccess,
    ) -> Result<Self> {
        let path = directory.join("control-access.json");
        let settings: Settings = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() && meta.len() <= 16 * 1024 => {
                serde_json::from_slice(&std::fs::read(&path)?).context("管理 API 设置损坏")?
            }
            Ok(_) => bail!("管理 API 设置文件无效"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let chatbot_port = chatbot.port()?;
                let images_port = images.port()?;
                let port = (17867..=17870)
                    .find(|port| ![main_port, chatbot_port, images_port].contains(port))
                    .context("没有可用的管理 API 默认端口")?;
                let settings = Settings {
                    enabled: false,
                    port,
                    key: generate_key(),
                };
                secure_write(&path, &serde_json::to_vec(&settings)?)?;
                settings
            }
            Err(_) => bail!("无法读取管理 API 设置"),
        };
        validate_key(&settings.key)?;
        if settings.port == 0
            || settings.port == main_port
            || settings.port == chatbot.port()?
            || settings.port == images.port()?
            || settings.key == gateway_key
            || chatbot.uses_key(&settings.key)?
            || images.uses_key(&settings.key)?
        {
            bail!("管理 API 设置与其他入口冲突");
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .context("无法保护管理 API 设置")?;
        Ok(Self {
            settings: Mutex::new(settings),
            revision: AtomicU64::new(0),
            resets: Mutex::new(resets::Queue::open(directory)?),
            operations: Mutex::new(jobs::Operations::default()),
            path,
            listener: AsyncMutex::new(None),
        })
    }
    fn settings(&self) -> Result<Settings> {
        self.settings
            .lock()
            .map(|s| s.clone())
            .map_err(|_| anyhow::anyhow!("管理 API 设置忙碌"))
    }
    pub fn port(&self) -> Result<u16> {
        Ok(self.settings()?.port)
    }
    pub fn uses_key(&self, key: &str) -> Result<bool> {
        Ok(self.settings()?.key == key)
    }
    pub fn key(&self) -> Result<Value> {
        Ok(json!({"key":self.settings()?.key}))
    }
    pub async fn status(&self) -> Result<Value> {
        let listener = self.listener.lock().await;
        let settings = self.settings()?;
        Ok(
            json!({"enabled":settings.enabled,"running":listener.as_ref().is_some_and(|l|!l.task.is_finished()),"port":settings.port,"base_url":format!("http://127.0.0.1:{}/v1",settings.port),"key_set":true}),
        )
    }
}

pub async fn configure(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    safety::fields(&payload, &["enabled", "port", "key", "regenerate_key"])?;
    let _configuration = state.config_lock.lock().await;
    let mut listener = state.control.listener.lock().await;
    let mut next = state.control.settings()?;
    if let Some(value) = payload.get("enabled") {
        next.enabled = value.as_bool().context("请指定是否启用管理 API")?;
    }
    if let Some(value) = payload.get("port") {
        next.port = value
            .as_u64()
            .filter(|p| (1..=65535).contains(p))
            .context("管理 API 端口须为 1–65535")? as u16;
    }
    if next.port == state.port
        || next.port == state.chatbot.port()?
        || next.port == state.images.port()?
    {
        bail!("管理 API 须使用与网关、聊天客户端、图片生成不同的端口");
    }
    if let Some(value) = payload.get("regenerate_key") {
        value.as_bool().context("重新生成 Key 参数无效")?;
    }
    if payload["regenerate_key"] == true {
        if payload.get("key").is_some() {
            bail!("不能同时指定和重新生成 Key");
        }
        next.key = generate_key();
    } else if let Some(value) = payload.get("key") {
        next.key = value.as_str().context("管理 API Key 格式无效")?.to_owned();
    }
    validate_key(&next.key)?;
    if next.key == state.gateway_key
        || state.chatbot.uses_key(&next.key)?
        || state.images.uses_key(&next.key)?
        || state.remote.uses_key(&next.key)?
        || state
            .sources
            .read()
            .await
            .iter()
            .any(|runtime| safety::contains_secret(&runtime.source.secret, &next.key))
    {
        bail!("管理 API 必须使用独立 Key，不能复用其他入口或上游凭据");
    }
    let current = listener
        .as_ref()
        .is_some_and(|l| l.port == next.port && !l.task.is_finished());
    let socket = if next.enabled && !current {
        Some(
            tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, next.port))
                .await
                .context("管理 API 端口被占用，请更换端口")?,
        )
    } else {
        None
    };
    secure_write(&state.control.path, &serde_json::to_vec(&next)?)
        .map_err(|_| anyhow::anyhow!("无法保存管理 API 设置"))?;
    *state
        .control
        .settings
        .lock()
        .map_err(|_| anyhow::anyhow!("管理 API 设置忙碌"))? = next.clone();
    state.control.revision.fetch_add(1, Ordering::Release);
    if let Some(socket) = socket {
        let shared = state.clone();
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = axum::serve(socket, router(shared))
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await;
        });
        if let Some(old) = listener.take() {
            let _ = old.shutdown.send(());
        }
        *listener = Some(Listener {
            port: next.port,
            task,
            shutdown,
        });
    } else if !next.enabled {
        if let Some(old) = listener.take() {
            let _ = old.shutdown.send(());
        }
    }
    drop(listener);
    state.control.status().await
}
pub async fn resume(state: &Arc<AppState>) -> Result<()> {
    configure(state, json!({})).await.map(|_| ())
}
/// This handler is private IPC only; the HTTP router never accepts an IPC op.
pub async fn handle(state: &Arc<AppState>, op: &str, payload: Value) -> Result<Value> {
    match op {
        "control.status" => state.control.status().await,
        "control.key" => state.control.key(),
        "control.configure" => configure(state, payload).await,
        "control.reset.pending" => resets::pending(state).await,
        "control.reset.present" => resets::present(state, payload).await,
        "control.reset.cancel" => resets::cancel(state, payload).await,
        "control.reset.confirm" => resets::confirm(state, payload).await,
        _ => bail!("未知管理 API 操作"),
    }
}
pub fn router(state: Arc<AppState>) -> axum::Router {
    http::router(state)
}
