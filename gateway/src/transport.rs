use crate::model::Source;
use anyhow::{bail, Result};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const CLIENT_TTL: Duration = Duration::from_secs(300);
const SYSTEM_CLIENT_TTL: Duration = Duration::from_secs(5);

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
    created: Instant,
    used: Instant,
}

fn is_expired(key: &ClientKey, cached: &CachedClient, now: Instant) -> bool {
    if key.mode == "system" {
        now.duration_since(cached.created) >= SYSTEM_CLIENT_TTL
    } else {
        now.duration_since(cached.used) >= CLIENT_TTL
    }
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
        proxy_url: if source.proxy.mode == "explicit" {
            source.proxy.url.clone()
        } else {
            None
        },
    };
    let mut clients = CLIENTS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("无法读取来源连接池"))?;
    let now = Instant::now();
    clients.retain(|k, cached| !is_expired(k, cached, now));
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
            created: now,
            used: now,
        },
    );
    Ok(client)
}

fn build_client(source: &Source) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(2)
        .connect_timeout(std::time::Duration::from_secs(15));
    match source.proxy.mode.as_str() {
        "direct" if source.kind != "official" => {
            builder = builder.no_proxy();
        }
        "system" => {
            // Leave reqwest's default system/env proxy behavior active without overriding.
        }
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
                .no_proxy()
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                unsafe {
                    match value {
                        Some(v) => std::env::set_var(key, v),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    fn source(id: &str, kind: &str, base_url: &str, mode: &str) -> Source {
        serde_json::from_value(json!({
            "id": id,
            "name": id,
            "kind": kind,
            "base_url": base_url,
            "enabled": true,
            "priority": 0,
            "proxy": { "mode": mode },
            "account_id": if kind == "official" { json!("account-fixture") } else { json!(null) },
            "secret": if kind == "official" { json!({"access_token":"fixture"}) } else { json!({"api_key":"fixture"}) }
        }))
        .unwrap()
    }

    #[test]
    fn client_accepts_system_proxy_for_official_and_api_while_rejecting_official_direct() {
        let off_system = source(
            &format!("off-sys-{}", uuid::Uuid::new_v4()),
            "official",
            "https://chatgpt.com/backend-api/codex",
            "system",
        );
        assert!(client(&off_system).is_ok());

        let api_system = source(
            &format!("api-sys-{}", uuid::Uuid::new_v4()),
            "api",
            "https://example.com/v1",
            "system",
        );
        assert!(client(&api_system).is_ok());

        let off_direct = source(
            &format!("off-dir-{}", uuid::Uuid::new_v4()),
            "official",
            "https://chatgpt.com/backend-api/codex",
            "direct",
        );
        assert!(client(&off_direct).is_err());
    }

    #[test]
    fn system_clients_expire_by_creation_time_and_explicit_clients_by_idle_time() {
        let now = Instant::now();
        let sys_key = ClientKey {
            source_id: "s".into(),
            kind: "api".into(),
            base_url: "https://example.com/v1".into(),
            mode: "system".into(),
            proxy_url: None,
        };
        let fresh_sys = CachedClient {
            client: reqwest::Client::new(),
            created: now - Duration::from_secs(4),
            used: now,
        };
        assert!(!is_expired(&sys_key, &fresh_sys, now));
        let stale_sys = CachedClient {
            client: reqwest::Client::new(),
            created: now - Duration::from_secs(6),
            used: now,
        };
        assert!(is_expired(&sys_key, &stale_sys, now));

        let dir_key = ClientKey {
            mode: "direct".into(),
            ..sys_key
        };
        let active_dir = CachedClient {
            client: reqwest::Client::new(),
            created: now - Duration::from_secs(600),
            used: now - Duration::from_secs(10),
        };
        assert!(!is_expired(&dir_key, &active_dir, now));
    }

    #[tokio::test]
    async fn system_mode_honors_env_proxy_while_direct_mode_bypasses_it() {
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let origin_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_addr = origin_listener.local_addr().unwrap();

        let proxy_hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let origin_hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let proxy_hits_srv = proxy_hits.clone();
        let proxy_task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = proxy_listener.accept().await else {
                    break;
                };
                proxy_hits_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nproxy")
                    .await;
            }
        });

        let origin_hits_srv = origin_hits.clone();
        let origin_task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = origin_listener.accept().await else {
                    break;
                };
                origin_hits_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\norigin")
                    .await;
            }
        });

        let origin_url = format!("http://{origin_addr}");
        let proxy_url = format!("http://{proxy_addr}");

        let (sys_client, dir_client) = {
            let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let keys = [
                "HTTP_PROXY",
                "http_proxy",
                "HTTPS_PROXY",
                "https_proxy",
                "ALL_PROXY",
                "all_proxy",
                "NO_PROXY",
                "no_proxy",
            ];
            let _guard = EnvGuard(
                keys.iter()
                    .map(|k| (*k, std::env::var_os(k)))
                    .collect(),
            );
            unsafe {
                for k in keys {
                    std::env::remove_var(k);
                }
                std::env::set_var("HTTP_PROXY", &proxy_url);
            }
            let sys_src = source(
                &format!("sys-http-{}", uuid::Uuid::new_v4()),
                "api",
                &origin_url,
                "system",
            );
            let dir_src = source(
                &format!("dir-http-{}", uuid::Uuid::new_v4()),
                "api",
                &origin_url,
                "direct",
            );
            (client(&sys_src).unwrap(), client(&dir_src).unwrap())
        };

        let sys_body = sys_client.get(format!("{origin_url}/test")).send().await.unwrap().text().await.unwrap();
        assert_eq!(sys_body, "proxy");
        assert_eq!(proxy_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(origin_hits.load(std::sync::atomic::Ordering::SeqCst), 0);

        let dir_body = dir_client.get(format!("{origin_url}/test")).send().await.unwrap().text().await.unwrap();
        assert_eq!(dir_body, "origin");
        assert_eq!(proxy_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(origin_hits.load(std::sync::atomic::Ordering::SeqCst), 1);

        proxy_task.abort();
        origin_task.abort();
    }
}
