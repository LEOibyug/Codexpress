//! Read-only balance discovery. All credentials and login sessions stay in
//! memory; only the normalized remaining amount crosses the management socket.
mod parsers;

use crate::{
    model::{now, AppState, ProxyPolicy, Source},
    transport,
};
use anyhow::{anyhow, bail, Context, Result};
use reqwest::{header, Client, Method};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex as AsyncMutex;

const BODY_LIMIT: usize = 256 * 1024;

#[derive(Clone, Serialize)]
pub struct Snapshot {
    status: &'static str,
    amount: Option<f64>,
    currency: Option<String>,
    scope: Option<&'static str>,
    stale: bool,
    checked_at: u64,
    updated_at: Option<u64>,
}

#[derive(Clone)]
struct Amount {
    value: f64,
    currency: String,
    scope: &'static str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Cch,
    NewApi,
    Sub2Api,
    Credits,
    DeepSeek,
    Moonshot,
    SiliconFlow,
    OpenRouter,
}

struct Session {
    cookie: String,
    expires: Instant,
}

#[derive(Default)]
struct Entry {
    identity: String,
    proxy: Option<ProxyPolicy>,
    snapshot: Option<Snapshot>,
    attempted: Option<Instant>,
    preferred: Option<Protocol>,
    session: Option<Session>,
}

#[derive(Default)]
pub struct BalanceStore {
    entries: Mutex<HashMap<String, Arc<AsyncMutex<Entry>>>>,
}

impl BalanceStore {
    async fn query(&self, source: &Source) -> Result<Snapshot> {
        let entry = {
            let mut entries = self
                .entries
                .lock()
                .map_err(|_| anyhow!("无法读取余额缓存"))?;
            if !entries.contains_key(&source.id) && entries.len() >= 256 {
                // Evict only idle entries: pending callers must share one lock/session.
                if let Some(id) = entries
                    .iter()
                    .find(|(_, v)| Arc::strong_count(v) == 1)
                    .map(|(k, _)| k.clone())
                {
                    entries.remove(&id);
                } else {
                    bail!("余额查询繁忙，请稍后重试");
                }
            }
            entries.entry(source.id.clone()).or_default().clone()
        };
        let mut entry = entry.lock().await;
        if entry.identity != source.identity() || entry.proxy.as_ref() != Some(&source.proxy) {
            *entry = Entry {
                identity: source.identity(),
                proxy: Some(source.proxy.clone()),
                ..Default::default()
            };
        }
        let ttl = if entry
            .snapshot
            .as_ref()
            .is_some_and(|s| s.status == "available" && !s.stale)
        {
            15
        } else {
            60
        };
        if entry
            .attempted
            .is_some_and(|at| at.elapsed() < Duration::from_secs(ttl))
        {
            if let Some(snapshot) = &entry.snapshot {
                return Ok(snapshot.clone());
            }
        }
        let found = tokio::time::timeout(Duration::from_secs(45), discover(source, &mut entry))
            .await
            .ok()
            .flatten();
        entry.attempted = Some(Instant::now());
        let snapshot = if let Some(amount) = found {
            Snapshot {
                status: "available",
                amount: Some(amount.value),
                currency: Some(amount.currency),
                scope: Some(amount.scope),
                stale: false,
                checked_at: now(),
                updated_at: Some(now()),
            }
        } else if let Some(previous) = &entry.snapshot {
            Snapshot {
                stale: previous.amount.is_some(),
                checked_at: now(),
                ..previous.clone()
            }
        } else {
            Snapshot {
                status: "unavailable",
                amount: None,
                currency: None,
                scope: None,
                stale: false,
                checked_at: now(),
                updated_at: None,
            }
        };
        entry.snapshot = Some(snapshot.clone());
        Ok(snapshot)
    }
}

pub async fn query(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    let id = payload["source_id"].as_str().context("缺少来源标识")?;
    let source = state
        .sources
        .read()
        .await
        .iter()
        .find(|r| r.source.id == id)
        .map(|r| r.source.clone())
        .context("来源不存在")?;
    if source.kind != "api" {
        bail!("此接口仅查询自定义来源余额");
    }
    source.validate()?;
    if source.secret["api_key"].as_str().is_none_or(str::is_empty) {
        bail!("来源未配置密钥");
    }
    let snapshot = state.balances.query(&source).await?;
    // Never publish another key's result after an edit/deletion while awaiting HTTP.
    let sources = state.sources.read().await;
    if !sources.iter().any(|r| {
        r.source.id == source.id
            && r.source.identity() == source.identity()
            && r.source.proxy == source.proxy
    }) {
        bail!("来源已变化，请重新查询余额");
    }
    Ok(serde_json::to_value(snapshot)?)
}

struct Reply {
    status: u16,
    json: Value,
    session: Option<Session>,
}
struct Probe<'a> {
    source: &'a Source,
    client: Client,
    base: url::Url,
    rate_limited: AtomicBool,
}

impl<'a> Probe<'a> {
    fn new(source: &'a Source) -> Result<Self> {
        let mut base = url::Url::parse(&source.base_url)?;
        let path = base.path().trim_end_matches('/');
        let path = path
            .strip_suffix("/responses")
            .unwrap_or(path)
            .trim_end_matches('/');
        let path = ["/v1", "/v1beta", "/v1alpha"]
            .iter()
            .find_map(|suffix| path.strip_suffix(suffix))
            .unwrap_or(path)
            .to_owned();
        base.set_path(&path);
        Ok(Self {
            source,
            client: transport::client(source)?,
            base,
            rate_limited: AtomicBool::new(false),
        })
    }
    async fn request(&self, path: &str, auth: Auth<'_>, body: Option<Value>) -> Option<Reply> {
        if self.rate_limited.load(Ordering::Relaxed) {
            return None;
        }
        let mut url = self.base.clone();
        url.set_path(&format!("{}{path}", self.base.path().trim_end_matches('/')));
        // Endpoints always remain on the exact configured origin and mount.
        // No redirects, automatic cookies, external login URLs or query-string keys.
        let mut request = self
            .client
            .request(
                if body.is_some() {
                    Method::POST
                } else {
                    Method::GET
                },
                url,
            )
            .timeout(Duration::from_secs(6))
            .header(header::ACCEPT, "application/json")
            .header(header::ACCEPT_ENCODING, "identity")
            .header(header::USER_AGENT, "Mozilla/5.0 Codexpress/0.1");
        let key = self.source.secret["api_key"].as_str()?;
        request = match auth {
            Auth::Bearer => request.bearer_auth(key),
            Auth::ApiKey => request.header("x-api-key", key),
            Auth::Session(cookie) => request.header(header::COOKIE, cookie),
            Auth::Login => request,
        };
        if let Some(body) = body {
            request = request.json(&body);
        }
        let mut response = request.send().await.ok()?;
        if response
            .content_length()
            .is_some_and(|n| n > BODY_LIMIT as u64)
        {
            return None;
        }
        let status = response.status().as_u16();
        if status == 429 {
            self.rate_limited.store(true, Ordering::Relaxed);
            return None;
        }
        // Only this application's temporary CCH cookie is used, exclusively on
        // the same origin's self-service routes. Its value is never serialized.
        let session = if status == 200 && matches!(auth, Auth::Login) {
            response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .find_map(|h| {
                    let raw = h.to_str().ok()?;
                    let mut parts = raw.split(';');
                    let first = parts.next()?.trim();
                    let token = first.strip_prefix("auth-token=")?;
                    if token.is_empty()
                        || token.len() > 8192
                        || !token
                            .bytes()
                            .all(|b| b.is_ascii_graphic() && b != b';' && b != b',')
                    {
                        return None;
                    }
                    let lifetime = parts
                        .find_map(|p| {
                            p.trim()
                                .to_ascii_lowercase()
                                .strip_prefix("max-age=")?
                                .parse::<u64>()
                                .ok()
                        })
                        .unwrap_or(3600)
                        .min(3600);
                    if lifetime == 0 {
                        return None;
                    }
                    Some(Session {
                        cookie: first.to_owned(),
                        expires: Instant::now() + Duration::from_secs(lifetime),
                    })
                })
        } else {
            None
        };
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if bytes.len() + chunk.len() > BODY_LIMIT {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        Some(Reply {
            status,
            json: serde_json::from_slice(&bytes).ok()?,
            session,
        })
    }

    async fn cch(&self, entry: &mut Entry) -> Option<Amount> {
        if entry
            .session
            .as_ref()
            .is_some_and(|s| s.expires <= Instant::now())
        {
            entry.session = None;
        }
        let auth = entry
            .session
            .as_ref()
            .map_or(Auth::ApiKey, |s| Auth::Session(&s.cookie));
        let reply = self.request("/api/v1/me/quota", auth, None).await?;
        if let Some(amount) = parsers::cch(&reply) {
            return Some(amount);
        }
        let legacy = matches!(reply.status, 404 | 405);
        let challenge = reply.status == 401
            && reply.json["type"]
                .as_str()
                .is_some_and(|s| s.starts_with("urn:claude-code-hub:problem:"));
        if !challenge && !legacy {
            return None;
        }
        // Old CCH exposes read-only functions as POST actions. Never call any
        // write action; only getMyQuota is part of this compatibility path.
        if legacy {
            let reply = self
                .request(
                    "/api/actions/my-usage/getMyQuota",
                    Auth::Bearer,
                    Some(json!({})),
                )
                .await?;
            if let Some(amount) = parsers::cch(&reply) {
                return Some(amount);
            }
            if reply.status != 401 || reply.json.get("ok") != Some(&Value::Bool(false)) {
                return None;
            }
        }
        entry.session = None;
        let mut login = self
            .request(
                "/api/auth/login",
                Auth::Login,
                Some(json!({"key":self.source.secret["api_key"]})),
            )
            .await?;
        if login.status != 200 || login.json["ok"] != true {
            return None;
        }
        entry.session = login.session.take();
        let session = entry.session.as_ref()?;
        let reply = self
            .request(
                if legacy {
                    "/api/actions/my-usage/getMyQuota"
                } else {
                    "/api/v1/me/quota"
                },
                Auth::Session(&session.cookie),
                if legacy { Some(json!({})) } else { None },
            )
            .await?;
        parsers::cch(&reply)
    }

    async fn run(&self, protocol: Protocol, entry: &mut Entry) -> Option<Amount> {
        match protocol {
            Protocol::Cch => self.cch(entry).await,
            Protocol::NewApi => {
                let reply = self
                    .request("/api/usage/token/", Auth::Bearer, None)
                    .await?;
                let token = parsers::new_api_token(&reply);
                if token.is_some()
                    || parsers::data(&reply).is_some_and(|d| d["unlimited_quota"] == true)
                {
                    if let Some(account) = self
                        .request("/api/user/self", Auth::Bearer, None)
                        .await
                        .and_then(|r| parsers::new_api_account(&r))
                    {
                        return Some(account);
                    }
                }
                token
            }
            Protocol::Sub2Api => {
                parsers::sub2api(&self.request("/v1/usage", Auth::Bearer, None).await?)
            }
            Protocol::Credits => {
                for path in [
                    "/v1/dashboard/billing/credit_grants",
                    "/dashboard/billing/credit_grants",
                    "/v1/dashboard/billing/subscription",
                ] {
                    let Some(reply) = self.request(path, Auth::Bearer, None).await else {
                        continue;
                    };
                    if let Some(amount) = parsers::credits(&reply, path.ends_with("credit_grants"))
                    {
                        return Some(amount);
                    }
                }
                None
            }
            Protocol::DeepSeek => {
                parsers::deepseek(&self.request("/user/balance", Auth::Bearer, None).await?)
            }
            Protocol::Moonshot => parsers::moonshot(
                &self
                    .request("/v1/users/me/balance", Auth::Bearer, None)
                    .await?,
                self.base.host_str() == Some("api.moonshot.ai"),
            ),
            Protocol::SiliconFlow => parsers::siliconflow(
                &self.request("/v1/user/info", Auth::Bearer, None).await?,
                self.base.host_str() == Some("api.siliconflow.com"),
            ),
            Protocol::OpenRouter => {
                parsers::openrouter(&self.request("/v1/credits", Auth::Bearer, None).await?)
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Auth<'a> {
    Bearer,
    ApiKey,
    Session(&'a str),
    Login,
}

async fn discover(source: &Source, entry: &mut Entry) -> Option<Amount> {
    let probe = Probe::new(source).ok()?;
    if matches!(
        probe.base.host_str(),
        Some("api.openai.com" | "api.anthropic.com" | "generativelanguage.googleapis.com")
    ) {
        return None;
    }
    let mut protocols = vec![
        Protocol::Cch,
        Protocol::NewApi,
        Protocol::Sub2Api,
        Protocol::DeepSeek,
        Protocol::Moonshot,
        Protocol::SiliconFlow,
        Protocol::OpenRouter,
        Protocol::Credits,
    ];
    let preferred = entry.preferred.or(match probe.base.host_str() {
        Some("api.deepseek.com") => Some(Protocol::DeepSeek),
        Some("api.moonshot.ai" | "api.moonshot.cn") => Some(Protocol::Moonshot),
        Some("api.siliconflow.cn" | "api.siliconflow.com") => Some(Protocol::SiliconFlow),
        Some("openrouter.ai") => Some(Protocol::OpenRouter),
        _ => None,
    });
    if let Some(preferred) = preferred {
        protocols.retain(|p| *p != preferred);
        protocols.insert(0, preferred);
    }
    for protocol in protocols {
        if let Some(amount) = probe.run(protocol, entry).await {
            entry.preferred = Some(protocol);
            return Some(amount);
        }
    }
    None
}
