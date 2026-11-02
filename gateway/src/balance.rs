//! Read-only balance discovery. All credentials and login sessions stay in
//! memory; only the normalized remaining amount crosses the management socket.
pub(crate) mod billing;
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
    billing: billing::Sync,
}

impl BalanceStore {
    /// Read a normalized snapshot without querying, waiting for discovery, or
    /// exposing credentials/session material. Busy or invalid caches are hidden.
    pub(crate) fn cached_amount(&self, source: &Source) -> Option<(f64, String)> {
        let entry = self.entries.lock().ok()?.get(&source.id)?.clone();
        let entry = entry.try_lock().ok()?;
        if entry.identity != source.identity() || entry.proxy.as_ref() != Some(&source.proxy) {
            return None;
        }
        let snapshot = entry.snapshot.as_ref()?;
        if snapshot.status != "available" || snapshot.stale {
            return None;
        }
        let amount = snapshot.amount.filter(|amount| amount.is_finite())?;
        let currency = snapshot
            .currency
            .as_ref()
            .filter(|currency| !currency.trim().is_empty())?;
        Some((amount, currency.clone()))
    }
    async fn query(&self, source: &Source, force: bool) -> Result<(Snapshot, bool)> {
        let started = Instant::now();
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
        if entry.attempted.is_some_and(|at| {
            // A forced request bypasses completed-cache TTL, but callers that
            // overlap discovery share its result rather than querying again.
            at >= started || (!force && at.elapsed() < Duration::from_secs(ttl))
        }) {
            if let Some(snapshot) = &entry.snapshot {
                return Ok((snapshot.clone(), false));
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
        Ok((snapshot, true))
    }
}

pub async fn query(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    query_inner(state, payload, false).await
}

/// Refresh only the read-only balance snapshot, even within the normal cache TTL.
pub(crate) async fn refresh(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    query_inner(state, payload, true).await
}

async fn query_inner(state: &Arc<AppState>, payload: Value, force: bool) -> Result<Value> {
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
    let (snapshot, published) = state.balances.query(&source, force).await?;
    // Never publish another key's result after an edit/deletion while awaiting HTTP.
    let sources = state.sources.read().await;
    if !sources.iter().any(|r| {
        r.source.id == source.id
            && r.source.identity() == source.identity()
            && r.source.proxy == source.proxy
    }) {
        bail!("来源已变化，请重新查询余额");
    }
    drop(sources);
    if published {
        state.chatbot.account_updated();
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
        self.request_with_timeout(path, auth, body, 6).await
    }
    async fn request_with_timeout(
        &self,
        path: &str,
        auth: Auth<'_>,
        body: Option<Value>,
        seconds: u64,
    ) -> Option<Reply> {
        if self.rate_limited.load(Ordering::Relaxed) {
            return None;
        }
        let mut url = self.base.clone();
        let (path, query) = path
            .split_once('?')
            .map_or((path, None), |(p, q)| (p, Some(q)));
        url.set_path(&format!("{}{path}", self.base.path().trim_end_matches('/')));
        url.set_query(query);
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
            .timeout(Duration::from_secs(seconds))
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
        let bytes = transport::read_limited(&mut response, BODY_LIMIT)
            .await
            .ok()?;
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

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[tokio::test]
    async fn cached_amount_never_discovers_and_rejects_busy_stale_or_changed_identity() {
        let source: Source = serde_json::from_value(json!({"id":"api","name":"API","kind":"api","base_url":"http://127.0.0.1:1","enabled":true,"priority":1,"proxy":{"mode":"direct"},"secret":{"api_key":"fixture-private-key"}})).unwrap();
        let store = BalanceStore::default();
        assert!(store.cached_amount(&source).is_none());
        assert!(store.entries.lock().unwrap().is_empty());
        let entry = Arc::new(AsyncMutex::new(Entry {
            identity: source.identity(),
            proxy: Some(source.proxy.clone()),
            snapshot: Some(Snapshot {
                status: "available",
                amount: Some(0.0),
                currency: Some("USD".into()),
                scope: Some("account"),
                stale: false,
                checked_at: now(),
                updated_at: Some(now()),
            }),
            ..Default::default()
        }));
        store
            .entries
            .lock()
            .unwrap()
            .insert(source.id.clone(), entry.clone());
        assert_eq!(store.cached_amount(&source), Some((0.0, "USD".into())));
        let mut changed = source.clone();
        changed.secret["api_key"] = json!("another-private-key");
        assert!(store.cached_amount(&changed).is_none());
        changed = source.clone();
        changed.proxy = ProxyPolicy {
            mode: "explicit".into(),
            url: Some("http://127.0.0.1:9".into()),
        };
        assert!(store.cached_amount(&changed).is_none());
        {
            let mut locked = entry.lock().await;
            assert!(store.cached_amount(&source).is_none());
            locked.snapshot.as_mut().unwrap().stale = true;
        }
        assert!(store.cached_amount(&source).is_none());
        assert!(entry.lock().await.attempted.is_none());
    }

    #[tokio::test]
    async fn turn_refresh_bypasses_success_and_failure_ttls_and_joins_normal_queries() {
        use axum::{http::StatusCode, routing::get, Json, Router};
        use std::sync::atomic::AtomicUsize;

        let hits = Arc::new(AtomicUsize::new(0));
        let used = Arc::new(AtomicUsize::new(10));
        let router = Router::new().route(
            "/api/v1/me/quota",
            get({
                let hits = hits.clone();
                let used = used.clone();
                move || {
                    let hits = hits.clone();
                    let used = used.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        tokio::task::yield_now().await;
                        let used = used.load(Ordering::SeqCst);
                        if used == usize::MAX {
                            return (
                                StatusCode::TOO_MANY_REQUESTS,
                                Json(json!({"error":"PRIVATE"})),
                            );
                        }
                        (
                            StatusCode::OK,
                            Json(json!({"userLimitTotalUsd":100,"userCurrentTotalUsd":used})),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(dir.path().into(), 0, "native-fixture".into()).unwrap();
        let source: Source = serde_json::from_value(json!({"id":"api","name":"API","kind":"api","base_url":format!("http://{address}/v1"),"enabled":true,"priority":1,"proxy":{"mode":"direct"},"secret":{"api_key":"PRIVATE-KEY"}})).unwrap();
        state
            .sources
            .write()
            .await
            .push(crate::model::SourceRuntime::new(source));
        let payload = || json!({"source_id":"api"});

        let (normal, first, second) = tokio::join!(
            query(&state, payload()),
            refresh(&state, payload()),
            refresh(&state, payload()),
        );
        let initial = normal.unwrap();
        assert_eq!(initial["amount"], 90.0);
        assert_eq!(first.unwrap(), initial);
        assert_eq!(second.unwrap(), initial);
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        used.store(20, Ordering::SeqCst);
        assert_eq!(query(&state, payload()).await.unwrap(), initial);
        assert_eq!(refresh(&state, payload()).await.unwrap()["amount"], 80.0);
        used.store(30, Ordering::SeqCst);
        assert_eq!(refresh(&state, payload()).await.unwrap()["amount"], 70.0);
        assert_eq!(query(&state, payload()).await.unwrap()["amount"], 70.0);
        assert_eq!(hits.load(Ordering::SeqCst), 3);

        used.store(40, Ordering::SeqCst);
        let (first, normal, second) = tokio::join!(
            refresh(&state, payload()),
            query(&state, payload()),
            refresh(&state, payload()),
        );
        let refreshed = first.unwrap();
        assert_eq!(refreshed["amount"], 60.0);
        assert_eq!(normal.unwrap(), refreshed);
        assert_eq!(second.unwrap(), refreshed);
        assert_eq!(hits.load(Ordering::SeqCst), 4);

        used.store(usize::MAX, Ordering::SeqCst);
        let stale = refresh(&state, payload()).await.unwrap();
        assert_eq!(stale["stale"], true);
        assert_eq!(stale["amount"], 60.0);
        assert!(!stale.to_string().contains("PRIVATE"));
        assert_eq!(query(&state, payload()).await.unwrap(), stale);
        assert_eq!(hits.load(Ordering::SeqCst), 5);
        assert_eq!(refresh(&state, payload()).await.unwrap()["stale"], true);
        assert_eq!(hits.load(Ordering::SeqCst), 6);
        assert!(state.usage.summary(1).unwrap()["recent"]
            .as_array()
            .unwrap()
            .is_empty());
        server.abort();
    }
}
