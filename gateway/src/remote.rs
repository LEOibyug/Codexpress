//! Dedicated remote inference ingress; management remains on the owner-only Unix socket.
use crate::{config::secure_write, model::AppState};
use anyhow::{bail, Context, Result};
use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::get,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::Mutex as AsyncMutex;
use toml_edit::{value, DocumentMut, Item, Table};

#[derive(Clone, Serialize, Deserialize)]
struct Peer {
    id: String,
    name: String,
    host: String,
    key: String,
    #[serde(default)]
    accepted_hosts: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Settings {
    enabled: bool,
    port: u16,
    peers: Vec<Peer>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 17864,
            peers: vec![],
        }
    }
}
#[derive(Clone)]
pub struct RemoteIngress {
    pub peer_id: String,
}
struct Listener {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}
pub struct RemoteAccess {
    settings: Mutex<Settings>,
    path: PathBuf,
    listener: AsyncMutex<Option<Listener>>,
}
impl RemoteAccess {
    pub fn open(directory: &Path) -> Result<Self> {
        let path = directory.join("remote-access.json");
        let settings = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() && meta.len() <= 1024 * 1024 => {
                serde_json::from_slice(&std::fs::read(&path)?).context("远端接入设置损坏")?
            }
            Ok(_) => bail!("远端接入设置文件无效"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(_) => bail!("无法读取远端接入设置"),
        };
        Ok(Self {
            settings: Mutex::new(settings),
            path,
            listener: AsyncMutex::new(None),
        })
    }
    fn set_listener(&self, enabled: bool, port: Option<u16>) -> Result<()> {
        let mut settings = self
            .settings
            .lock()
            .map_err(|_| anyhow::anyhow!("远端入口忙碌"))?;
        let mut next = settings.clone();
        next.enabled = enabled;
        if let Some(port) = port {
            next.port = port;
        }
        secure_write(&self.path, &serde_json::to_vec(&next)?)?;
        *settings = next;
        Ok(())
    }
    pub fn peer(&self, headers: &HeaderMap) -> Option<String> {
        let token = headers
            .get("authorization")?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")?;
        let mut hosts = headers.get_all("host").iter();
        let host = hosts.next()?.to_str().ok()?;
        if hosts.next().is_some() || headers.contains_key("origin") {
            return None;
        }
        let settings = self.settings.lock().ok()?;
        if !settings.enabled {
            return None;
        }
        settings
            .peers
            .iter()
            .find(|p| {
                p.key == token
                    && std::iter::once(&p.host)
                        .chain(&p.accepted_hosts)
                        .any(|name| authority(name, settings.port).eq_ignore_ascii_case(host))
            })
            .map(|p| p.id.clone())
    }
    pub async fn status(&self) -> Result<Value> {
        let listener = self.listener.lock().await;
        let settings = self
            .settings
            .lock()
            .map_err(|_| anyhow::anyhow!("远端入口忙碌"))?;
        Ok(
            json!({"enabled":settings.enabled,"running":listener.as_ref().is_some_and(|l|!l.task.is_finished()),"port":settings.port,"peers":settings.peers.iter().map(|p|json!({"id":p.id,"name":p.name,"host":p.host})).collect::<Vec<_>>()}),
        )
    }
    pub fn prepare(
        &self,
        id: &str,
        name: &str,
        host: &str,
        config: &str,
        alias: bool,
    ) -> Result<Value> {
        if id.is_empty()
            || id.len() > 128
            || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || name.trim().is_empty()
            || name.len() > 256
        {
            bail!("远端机器标识或名称无效");
        }
        validate_host(host)?;
        let mut current = self
            .settings
            .lock()
            .map_err(|_| anyhow::anyhow!("远端入口忙碌"))?;
        let mut next = current.clone();
        let key = next
            .peers
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.key.clone())
            .unwrap_or_else(|| {
                format!(
                    "cxr-{}{}",
                    uuid::Uuid::new_v4().simple(),
                    uuid::Uuid::new_v4().simple()
                )
            });
        let base_url = format!("http://{}/v1", authority(host, next.port));
        let (after, aliases) = render_config(config, &base_url, &key, alias)?;
        let mut accepted_hosts = next
            .peers
            .iter()
            .find(|p| p.id == id)
            .map(|p| {
                let mut names = p.accepted_hosts.clone();
                names.push(p.host.clone());
                names
            })
            .unwrap_or_default();
        accepted_hosts.retain(|name| name != host);
        accepted_hosts.sort();
        accepted_hosts.dedup();
        if accepted_hosts.len() > 16 {
            bail!("此机器累计地址过多，请撤销旧 Key 后重新配置");
        }
        next.peers.retain(|p| p.id != id);
        if next.peers.len() >= 100 {
            bail!("最多支持100台远端机器");
        }
        next.peers.push(Peer {
            id: id.into(),
            name: name.into(),
            host: host.into(),
            key: key.clone(),
            accepted_hosts,
        });
        secure_write(&self.path, &serde_json::to_vec(&next)?)?;
        *current = next;
        // Private IPC only. UI displays summary, never after/key/config.
        Ok(
            json!({"base_url":base_url,"key":key,"config":after,"aliases":aliases,"provider":"codexpress","summary":format!("model_provider = codexpress\nbase_url = {base_url}\nAPI Key = 已设置（独立远端凭据）\n认证方式 = 来源池")}),
        )
    }
    pub async fn revoke(&self, id: &str) -> Result<Value> {
        {
            let mut current = self
                .settings
                .lock()
                .map_err(|_| anyhow::anyhow!("远端入口忙碌"))?;
            let mut next = current.clone();
            next.peers.retain(|p| p.id != id);
            secure_write(&self.path, &serde_json::to_vec(&next)?)?;
            *current = next;
        }
        self.status().await
    }
}
pub fn validate_host(host: &str) -> Result<()> {
    if host.is_empty()
        || host.len() > 253
        || host
            .chars()
            .any(|c| !c.is_ascii_alphanumeric() && !matches!(c, '.' | '-' | ':'))
        || host.starts_with('-')
    {
        bail!("请填写远端可访问的本机 IP 或主机名，不含协议和端口");
    }
    let url =
        url::Url::parse(&format!("http://{}/", authority(host, 17864))).context("本机地址无效")?;
    if url.host_str().is_none() {
        bail!("本机地址无效");
    }
    Ok(())
}
fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

pub async fn start(state: Arc<AppState>, port: u16) -> Result<Value> {
    if port == 0 || port == state.port {
        bail!("远端入口端口须与本机入口不同，且为1–65535");
    }
    let mut listener = state.remote.listener.lock().await;
    if listener
        .as_ref()
        .is_some_and(|l| l.port == port && !l.task.is_finished())
    {
        drop(listener);
        return state.remote.status().await;
    }
    let socket = tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port))
        .await
        .context("远端入口端口不可用")?;
    state.remote.set_listener(true, Some(port))?;
    let server_state = state.clone();
    let task = tokio::spawn(async move {
        let _ = axum::serve(socket, router(server_state)).await;
    });
    if let Some(previous) = listener.take() {
        previous.task.abort();
    }
    *listener = Some(Listener { port, task });
    drop(listener);
    state.remote.status().await
}
pub async fn resume(state: Arc<AppState>) -> Result<()> {
    let settings = state
        .remote
        .settings
        .lock()
        .map_err(|_| anyhow::anyhow!("远端入口忙碌"))?
        .clone();
    if settings.enabled {
        start(state, settings.port).await?;
    }
    Ok(())
}
pub async fn stop(state: &Arc<AppState>) -> Result<Value> {
    let mut listener = state.remote.listener.lock().await;
    state.remote.set_listener(false, None)?;
    if let Some(previous) = listener.take() {
        previous.task.abort();
    }
    drop(listener);
    state.remote.status().await
}
async fn gate(State(state): State<Arc<AppState>>, mut request: Request, next: Next) -> Response {
    let Some(peer_id) = state.remote.peer(request.headers()) else {
        return (StatusCode::UNAUTHORIZED, "远端网关认证失败").into_response();
    };
    request.extensions_mut().insert(RemoteIngress { peer_id });
    next.run(request).await
}
use axum::response::IntoResponse;
pub fn router(state: Arc<AppState>) -> axum::Router {
    crate::relay::router(state.clone())
        .route(
            "/v1/remote-check",
            get(|| async { Json(json!({"service":"codexpress","mode":"pool","version":1})) }),
        )
        .layer(middleware::from_fn_with_state(state, gate))
}

pub fn render_config(
    before: &str,
    base: &str,
    key: &str,
    aliases: bool,
) -> Result<(String, Vec<String>)> {
    if before.len() > 8 * 1024 * 1024 {
        bail!("远端配置过大");
    }
    let mut doc = before
        .parse::<DocumentMut>()
        .map_err(|_| anyhow::anyhow!("远端配置不是有效TOML，未修改"))?;
    // External profiles cannot be fetched implicitly; require an explicit root.
    if doc.get("profile").is_some() {
        bail!("远端已选择 profile，请先取消该档案或直接编辑档案后重试");
    }
    let mut provider = Table::new();
    provider["name"] = value("Codexpress");
    provider["base_url"] = value(base);
    provider["wire_api"] = value("responses");
    provider["experimental_bearer_token"] = value(key);
    provider["requires_openai_auth"] = value(false);
    provider["supports_websockets"] = value(false);
    let mut changed = vec![];
    if let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
    {
        if aliases {
            for (id, item) in providers.iter_mut() {
                if !["openai", "ollama", "lmstudio", "codexpress"].contains(&id.get()) {
                    *item = Item::Table(provider.clone());
                    changed.push(id.to_owned());
                }
            }
        }
    }
    doc["model_provider"] = value("codexpress");
    for name in ["openai_base_url", "chatgpt_base_url", "forced_login_method"] {
        doc.remove(name);
    }
    if doc.get("model_providers").is_none() {
        doc["model_providers"] = Item::Table(Table::new());
    }
    let table = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
        .context("远端供应商配置格式无效")?;
    table.insert("codexpress", Item::Table(provider));
    Ok((doc.to_string(), changed))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_preserves_tools_limits_and_auth_is_explicit_without_touching_files() {
        let before="model='m'\nmodel_auto_compact_token_limit=180000\nforced_login_method='chatgpt'\n[mcp_servers.test]\ncommand='test'\n[model_providers.old]\nbase_url='https://old.example/v1'\n";
        let (rendered, aliases) = render_config(
            before,
            "http://192.168.1.4:17864/v1",
            "remote-fixture",
            true,
        )
        .unwrap();
        let doc = rendered.parse::<DocumentMut>().unwrap();
        assert_eq!(doc["model"].as_str(), Some("m"));
        assert_eq!(doc["mcp_servers"]["test"]["command"].as_str(), Some("test"));
        assert_eq!(
            doc["model_auto_compact_token_limit"].as_integer(),
            Some(180000)
        );
        assert!(doc.get("forced_login_method").is_none());
        assert_eq!(
            doc["model_providers"]["old"]["requires_openai_auth"].as_bool(),
            Some(false)
        );
        assert_eq!(aliases, vec!["old"]);
        assert!(render_config("profile='work'", "x", "x", false).is_err());
        assert!(validate_host("127.0.0.1; touch /tmp/x").is_err());
        assert!(validate_host("https://example.com").is_err());
        assert!(validate_host("192.168.1.4").is_ok());
    }
}
