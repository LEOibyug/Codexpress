use crate::model::Source;
use anyhow::{bail, Result};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

#[derive(Clone, Hash, Eq, PartialEq)]
struct ClientKey {
    source_id: String,
    kind: String,
    base_url: String,
    mode: String,
    proxy_url: Option<String>,
}
struct CachedClient {
    client: reqwest::Client,
    used: Instant,
}

/// Every caller uses this policy, including OAuth and quota operations. In
/// particular, a failed explicit proxy never degrades to a direct connection.
pub fn client(source: &Source) -> Result<reqwest::Client> {
    static CLIENTS: OnceLock<Mutex<HashMap<ClientKey, CachedClient>>> = OnceLock::new();
    let key = ClientKey {
        source_id: source.id.clone(),
        kind: source.kind.clone(),
        base_url: source.base_url.clone(),
        mode: source.proxy.mode.clone(),
        proxy_url: source.proxy.url.clone(),
    };
    let mut clients = CLIENTS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("无法读取来源连接池"))?;
    let now = Instant::now();
    clients.retain(|_, cached| now.duration_since(cached.used) < Duration::from_secs(300));
    if let Some(cached) = clients.get_mut(&key) {
        cached.used = now;
        return Ok(cached.client.clone());
    }
    let client = build_client(source)?;
    // Credentials stay on individual requests. Source and policy boundaries
    // are separate pools; edits cannot reuse a connection through an old proxy.
    if clients.len() >= 256 {
        if let Some(oldest) = clients
            .iter()
            .min_by_key(|(_, cached)| cached.used)
            .map(|(key, _)| key.clone())
        {
            clients.remove(&oldest);
        }
    }
    clients.insert(
        key,
        CachedClient {
            client: client.clone(),
            used: now,
        },
    );
    Ok(client)
}

fn build_client(source: &Source) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(2)
        .connect_timeout(std::time::Duration::from_secs(15));
    match source.proxy.mode.as_str() {
        "direct" if source.kind != "official" => {}
        "explicit" => {
            let address = source
                .proxy
                .url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("来源未配置代理"))?;
            let parsed = url::Url::parse(address).map_err(|_| anyhow::anyhow!("代理地址无效"))?;
            if !["http", "https", "socks5", "socks5h"].contains(&parsed.scheme()) {
                bail!("代理协议不受支持");
            }
            builder = builder
                .proxy(reqwest::Proxy::all(address).map_err(|_| anyhow::anyhow!("代理地址无效"))?);
        }
        _ => bail!("来源的网络策略无效"),
    }
    // Decompression features are intentionally disabled in Cargo.toml. Raw
    // Content-Encoding and the original response bytes remain paired.
    builder
        .build()
        .map_err(|_| anyhow::anyhow!("无法创建来源连接"))
}

/// Reviewed categories only: reqwest's full error chain can include request URLs.
pub fn failure_reason(error: &reqwest::Error) -> &'static str {
    match (error.is_connect(), error.is_timeout()) {
        (true, true) => "连接建立超时（DNS、代理隧道或 TLS 握手）",
        (true, false) => "连接建立失败（DNS、代理隧道或 TLS 握手）",
        (false, true) => "请求超时，提交结果可能不明确",
        _ => "数据传输中断，提交结果可能不明确",
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum BodyReadError {
    TooLarge,
    ReadFailed,
}

/// Read raw chunks only; status, length hints and timeouts remain caller policy.
pub async fn read_limited(
    response: &mut reqwest::Response,
    limit: usize,
) -> std::result::Result<Vec<u8>, BodyReadError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| BodyReadError::ReadFailed)?
    {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(BodyReadError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
