use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, AtomicUsize},
        Arc,
    },
};
use tokio::sync::{Mutex, RwLock};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxyPolicy {
    pub mode: String,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Source {
    pub id: String,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub is_subscription: bool,
    pub base_url: String,
    pub enabled: bool,
    pub priority: i32,
    pub proxy: ProxyPolicy,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing)]
    pub secret: Value,
}

impl Source {
    /// Stable across token refresh for an official workspace, but different for
    /// another workspace, API credential or upstream. Used only for routing
    /// consistency; the digest itself is never sent to upstreams or logged.
    pub fn identity(&self) -> String {
        use sha2::{Digest, Sha256};
        let authority = if self.kind == "official" {
            self.account_id
                .as_deref()
                .or_else(|| self.secret.get("account_id").and_then(Value::as_str))
                .unwrap_or("")
        } else {
            self.secret
                .get("api_key")
                .and_then(Value::as_str)
                .unwrap_or("")
        };
        let encoded =
            serde_json::to_vec(&(&self.kind, &self.base_url, authority)).unwrap_or_default();
        format!("{:x}", Sha256::digest(encoded))
    }

    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || self.id.len() > 128
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || self.name.trim().is_empty()
        {
            bail!("来源的标识和名称不能为空");
        }
        if !["api", "official"].contains(&self.kind.as_str()) {
            bail!("未知来源类型");
        }
        let base =
            url::Url::parse(&self.base_url).map_err(|_| anyhow::anyhow!("来源地址格式错误"))?;
        if !["https", "http"].contains(&base.scheme())
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            bail!("来源地址必须是没有用户名、查询参数或片段的 HTTP(S) 基址");
        }
        if self.kind == "official"
            && self.base_url.trim_end_matches('/') != "https://chatgpt.com/backend-api/codex"
        {
            bail!("官方来源必须使用官方 Codex 地址");
        }
        match self.proxy.mode.as_str() {
            "direct" if self.kind == "official" => bail!("官方来源必须指定代理"),
            "direct" => {}
            "explicit" => {
                let proxy = self.proxy.url.as_deref().unwrap_or("");
                let parsed =
                    url::Url::parse(proxy).map_err(|_| anyhow::anyhow!("请填写有效的代理地址"))?;
                if !["http", "https", "socks5", "socks5h"].contains(&parsed.scheme())
                    || parsed.host_str().is_none()
                    || parsed.query().is_some()
                    || parsed.fragment().is_some()
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                {
                    bail!("代理地址只支持 HTTP(S) 和 SOCKS5");
                }
            }
            _ => bail!("未知代理策略"),
        }
        if self.kind == "api"
            && self
                .secret
                .get("api_key")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
        {
            bail!("API 来源需要密钥");
        }
        Ok(())
    }
}

#[derive(Clone, Serialize)]
pub struct SourceRuntime {
    #[serde(flatten)]
    pub source: Source,
    pub health: String,
    pub cooldown_until: Option<u64>,
    pub last_error: Option<String>,
    pub quota: Value,
    pub credits: Value,
    #[serde(skip)]
    pub health_revision: u64,
}

impl SourceRuntime {
    pub fn new(source: Source) -> Self {
        let ready = source.kind == "api"
            || source
                .secret
                .get("access_token")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
        Self {
            source,
            health: if ready { "ready" } else { "auth_required" }.into(),
            cooldown_until: None,
            last_error: None,
            quota: Value::Null,
            credits: Value::Null,
            health_revision: 0,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RoutingSettings {
    pub mode: String,
    pub fixed_source_id: Option<String>,
    #[serde(default = "default_low_quota")]
    pub official_low_quota_percent: u8,
    #[serde(default = "default_fallback")]
    pub degraded_context_fallback: bool,
    #[serde(default = "default_idle_return")]
    pub idle_return_minutes: u32,
}
fn default_idle_return() -> u32 {
    30
}
fn default_fallback() -> bool {
    true
}
fn default_low_quota() -> u8 {
    10
}

pub struct AppState {
    pub routing_revision: AtomicU64,
    pub routing_sync: Mutex<()>,
    pub sources: RwLock<Vec<SourceRuntime>>,
    pub settings: RwLock<RoutingSettings>,
    pub bindings: Mutex<HashMap<String, String>>,
    pub usage: Arc<crate::usage::UsageStore>,
    pub affinity: Arc<crate::affinity::AffinityStore>,
    pub history: Arc<crate::history::HistoryStore>,
    pub remote: crate::remote::RemoteAccess,
    pub notifications: crate::notifications::Notifications,
    pub balances: crate::balance::BalanceStore,
    pub gateway_key: String,
    pub port: u16,
    pub state_dir: PathBuf,
    pub inflight: AtomicUsize,
    pub official_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    pub config_lock: Mutex<()>,
    credential_store: Option<Arc<dyn crate::credentials::CredentialStore>>,
}

impl AppState {
    pub fn new(state_dir: PathBuf, port: u16, gateway_key: String) -> Result<Arc<Self>> {
        Self::new_with_credential_store(state_dir, port, gateway_key, None)
    }

    pub fn new_with_credential_store(
        state_dir: PathBuf,
        port: u16,
        gateway_key: String,
        credential_store: Option<Arc<dyn crate::credentials::CredentialStore>>,
    ) -> Result<Arc<Self>> {
        let usage = crate::usage::UsageStore::open(&state_dir.join("usage.sqlite"))?;
        Ok(Arc::new(Self {
            routing_revision: AtomicU64::new(0),
            routing_sync: Mutex::new(()),
            sources: RwLock::new(Vec::new()),
            settings: RwLock::new(RoutingSettings {
                mode: "auto".into(),
                fixed_source_id: None,
                official_low_quota_percent: 10,
                degraded_context_fallback: true,
                idle_return_minutes: default_idle_return(),
            }),
            bindings: Mutex::new(HashMap::new()),
            usage: Arc::new(usage),
            affinity: crate::affinity::AffinityStore::open(&state_dir.join("affinity.sqlite"))?,
            history: Arc::new(crate::history::HistoryStore::open(
                &state_dir.join("history.sqlite"),
            )?),
            remote: crate::remote::RemoteAccess::open(&state_dir)?,
            notifications: Default::default(),
            balances: Default::default(),
            gateway_key,
            port,
            state_dir,
            inflight: AtomicUsize::new(0),
            official_locks: Mutex::new(HashMap::new()),
            config_lock: Mutex::new(()),
            credential_store,
        }))
    }

    pub fn persist_credential(&self, source_id: &str, secret: &Value) -> Result<()> {
        if let Some(store) = &self.credential_store {
            store.save(source_id, secret)?;
        }
        Ok(())
    }
    pub fn credential_store(&self) -> Result<Arc<dyn crate::credentials::CredentialStore>> {
        self.credential_store
            .clone()
            .ok_or_else(|| anyhow::anyhow!("凭据存储未启用"))
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
