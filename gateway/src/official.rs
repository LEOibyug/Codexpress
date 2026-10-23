//! Official account operations. Secrets stay in memory; only reset operation metadata reaches disk.
use crate::{
    model::{now, AppState, Source},
    transport,
};
use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{rngs::OsRng, RngCore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, Weak},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};

#[path = "official/resets.rs"]
mod resets;
#[path = "official/snapshots.rs"]
mod snapshots;

use resets::consume;
use snapshots::refresh_selected_snapshots;
#[cfg(test)]
use snapshots::{credits_snapshot, quota_snapshot};

const PERSISTENCE_FAILURE: &str = "新凭据保存失败，账号已暂停；请检查凭据存储后重新登录";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REDIRECT: &str = "http://localhost:1455/auth/callback";
const TOKEN: &str = "https://auth.openai.com/oauth/token";
const USAGE: &str = "https://chatgpt.com/backend-api/wham/usage";
const CREDITS: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const CONSUME: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume";
#[cfg(not(test))]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);
#[cfg(test)]
const REQUEST_TIMEOUT: Duration = Duration::from_millis(200);

// Endpoints are private, never configurable through IPC/environment/source metadata.
#[derive(Clone, Copy)]
struct Endpoints<'a> {
    token: &'a str,
    usage: &'a str,
    credits: &'a str,
    consume: &'a str,
}
const ENDPOINTS: Endpoints<'static> = Endpoints {
    token: TOKEN,
    usage: USAGE,
    credits: CREDITS,
    consume: CONSUME,
};

struct Flow {
    owner: Weak<AppState>,
    source: Source,
    state: String,
    verifier: String,
    expires: u64,
    result: Option<Value>,
    error: Option<String>,
    exchanging: bool,
}
static FLOWS: OnceLock<Mutex<HashMap<String, Flow>>> = OnceLock::new();
fn flows() -> &'static Mutex<HashMap<String, Flow>> {
    FLOWS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or_else(|| anyhow!("缺少有效的 {key}"))
}
fn token<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
fn account(source: &Source) -> Option<&str> {
    source
        .account_id
        .as_deref()
        .or_else(|| source.secret.get("account_id").and_then(Value::as_str))
}
async fn source(state: &Arc<AppState>, id: &str) -> Result<Source> {
    let source = state
        .sources
        .read()
        .await
        .iter()
        .find(|r| r.source.id == id)
        .map(|r| r.source.clone())
        .ok_or_else(|| anyhow!("官方来源不存在"))?;
    if source.kind != "official" || source.proxy.mode != "explicit" {
        bail!("请选择指定代理的官方来源");
    }
    source.validate()?;
    Ok(source)
}
async fn lock(state: &Arc<AppState>, key: String) -> Arc<Mutex<()>> {
    state
        .official_locks
        .lock()
        .await
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}
fn same_binding(a: &Source, b: &Source) -> bool {
    a.id == b.id
        && a.kind == b.kind
        && a.base_url == b.base_url
        && a.proxy == b.proxy
        && account(a) == account(b)
        && a.secret == b.secret
}
async fn install(state: &Arc<AppState>, before: &Source, secret: Value) -> Result<Source> {
    let mut sources = state.sources.write().await;
    let current = sources
        .iter()
        .find(|r| r.source.id == before.id)
        .ok_or_else(|| anyhow!("登录来源已删除"))?;
    if !same_binding(&current.source, before) {
        bail!("来源或凭据已经变化，请重新操作");
    }
    // Persist the whole refresh-token family before any source can use the rotation.
    let refresh = token(&before.secret, "refresh_token");
    let targets: Vec<usize> = sources
        .iter()
        .enumerate()
        .filter(|(_, runtime)| {
            runtime.source.id == before.id
                || (!refresh.is_empty()
                    && account(before).is_some()
                    && account(before) == secret.get("account_id").and_then(Value::as_str)
                    && runtime.source.kind == "official"
                    && account(&runtime.source) == account(before)
                    && token(&runtime.source.secret, "refresh_token") == refresh)
        })
        .map(|(index, _)| index)
        .collect();
    for &index in &targets {
        if state
            .persist_credential(&sources[index].source.id, &secret)
            .is_err()
        {
            // The upstream may already have invalidated the previous refresh token.
            // Preserve old memory but block further use or retry of those credentials.
            for &index in &targets {
                sources[index].health = "auth_required".into();
                sources[index].last_error = Some(PERSISTENCE_FAILURE.into());
            }
            bail!("{PERSISTENCE_FAILURE}");
        }
    }
    let changed_accounts: Vec<_> = targets
        .iter()
        .filter(|index| {
            account(&sources[**index].source) != secret.get("account_id").and_then(Value::as_str)
        })
        .map(|index| sources[*index].source.id.clone())
        .collect();
    // Lock affinity state before publication so no old-account continuation survives login.
    if !changed_accounts.is_empty() {
        state
            .bindings
            .lock()
            .await
            .retain(|_, id| !changed_accounts.contains(id));
    }
    for &index in &targets {
        let runtime = &mut sources[index];
        if changed_accounts.contains(&runtime.source.id) {
            runtime.quota_exhausted = false;
            runtime.quota = Value::Null;
            runtime.credits = Value::Null;
            if runtime.health == "exhausted" {
                runtime.health = "ready".into();
                runtime.cooldown_until = None;
                runtime.last_error = None;
            }
        }
        runtime.source.account_id = secret
            .get("account_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        runtime.source.secret = secret.clone();
        if runtime.health == "auth_required" {
            runtime.health = "ready".into();
            runtime.last_error = None;
        }
    }
    Ok(sources
        .iter()
        .find(|r| r.source.id == before.id)
        .unwrap()
        .source
        .clone())
}
fn random() -> String {
    let mut bytes = [0; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
fn claims(jwt: &str) -> Value {
    jwt.split('.')
        .nth(1)
        .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}
fn merge_tokens(old: &Value, response: Value) -> Result<Value> {
    let access = token(&response, "access_token");
    if access.is_empty() {
        bail!("官方令牌响应缺少访问令牌");
    }
    let mut secret = old.as_object().cloned().unwrap_or_default();
    for key in ["access_token", "refresh_token", "id_token"] {
        if let Some(value) = response
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            secret.insert(key.into(), json!(value));
        }
    }
    let access_claims = claims(access);
    let id_claims = claims(secret.get("id_token").and_then(Value::as_str).unwrap_or(""));
    let expires = response
        .get("expires_in")
        .and_then(Value::as_u64)
        .map(|s| now().saturating_add(s))
        .or_else(|| access_claims.get("exp").and_then(Value::as_u64))
        .ok_or_else(|| anyhow!("官方令牌响应缺少有效期"))?;
    secret.insert("expires_at".into(), json!(expires));
    let identity = [&id_claims, &access_claims].into_iter().find_map(|v| {
        v.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
            .and_then(Value::as_str)
    });
    if let Some(id) = identity {
        secret.insert("account_id".into(), json!(id));
    }
    secret.insert(
        "generation".into(),
        json!(old
            .get("generation")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1)),
    );
    secret.insert("updated_at".into(), json!(now()));
    Ok(Value::Object(secret))
}
async fn json_response(response: reqwest::Response) -> Result<Value> {
    if !response.status().is_success() {
        bail!("官方接口返回 HTTP {}", response.status().as_u16());
    }
    if response
        .content_length()
        .is_some_and(|size| size > 2 * 1024 * 1024)
    {
        bail!("官方响应过大");
    }
    let mut response = response;
    let bytes = transport::read_limited(&mut response, 2 * 1024 * 1024)
        .await
        .map_err(|error| match error {
            transport::BodyReadError::TooLarge => anyhow!("官方响应过大"),
            transport::BodyReadError::ReadFailed => anyhow!("官方响应读取失败"),
        })?;
    serde_json::from_slice(&bytes).map_err(|_| anyhow!("官方接口响应格式变化"))
}
async fn refresh_inner(
    state: &Arc<AppState>,
    id: &str,
    force: bool,
    endpoints: Endpoints<'_>,
) -> Result<Source> {
    let initial = source(state, id).await?;
    let key = format!("token:{}", account(&initial).unwrap_or(id));
    let mutex = lock(state, key).await;
    let _guard = mutex.lock().await;
    let current = source(state, id).await?;
    if state
        .sources
        .read()
        .await
        .iter()
        .any(|r| r.source.id == id && r.last_error.as_deref() == Some(PERSISTENCE_FAILURE))
    {
        bail!("{PERSISTENCE_FAILURE}");
    }
    // A concurrent refresh already rotated this token while this request waited.
    let rotated = current.secret != initial.secret;
    let fresh = current
        .secret
        .get("expires_at")
        .and_then(Value::as_u64)
        .is_some_and(|exp| exp > now() + 60);
    if !token(&current.secret, "access_token").is_empty() && ((fresh && !force) || rotated) {
        return Ok(current);
    }
    let refresh = token(&current.secret, "refresh_token");
    if refresh.is_empty() {
        bail!("官方账号需要重新登录");
    }
    let response = transport::client(&current)?.post(endpoints.token).timeout(REQUEST_TIMEOUT)
        .json(&json!({"client_id":CLIENT_ID,"grant_type":"refresh_token","refresh_token":refresh,"scope":"openid profile email"}))
        .send().await.map_err(|e| anyhow!("官方令牌刷新{}",transport::failure_reason(&e)))?;
    let status = response.status().as_u16();
    if matches!(status, 400 | 401) {
        // A malformed request/changed endpoint is not proof that credentials expired.
        let invalid = if status == 401 {
            true
        } else {
            let mut response = response;
            let bytes = transport::read_limited(&mut response, 65536)
                .await
                .map_err(|error| match error {
                    transport::BodyReadError::TooLarge => anyhow!("官方令牌接口返回 HTTP 400"),
                    transport::BodyReadError::ReadFailed => anyhow!("官方令牌响应读取失败"),
                })?;
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let code = body
                .pointer("/error/code")
                .or_else(|| body.get("error"))
                .or_else(|| body.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("");
            [
                "invalid_grant",
                "refresh_token_expired",
                "refresh_token_revoked",
                "refresh_token_reused",
            ]
            .contains(&code)
        };
        if invalid {
            let mut sources = state.sources.write().await;
            if let Some(r) = sources
                .iter_mut()
                .find(|r| same_binding(&r.source, &current))
            {
                r.health = "auth_required".into();
                r.last_error = Some("官方登录已失效，请重新登录".into());
            }
            bail!("官方登录已失效，请重新登录");
        }
        bail!("官方令牌接口返回 HTTP 400，请稍后重试");
    }
    let secret = merge_tokens(&current.secret, json_response(response).await?)?;
    if let Some(identity) = account(&current) {
        if secret.get("account_id").and_then(Value::as_str) != Some(identity) {
            bail!("刷新返回了不同账号，请重新登录确认");
        }
    }
    install(state, &current, secret).await
}
pub async fn ensure_access(state: &Arc<AppState>, source_id: &str) -> Result<Source> {
    refresh_inner(state, source_id, false, ENDPOINTS).await
}
pub async fn refresh_access(state: &Arc<AppState>, source_id: &str) -> Result<Source> {
    refresh_inner(state, source_id, true, ENDPOINTS).await
}

fn callback_values(callback: &str, expected_state: &str) -> Result<String> {
    let url = url::Url::parse(callback).map_err(|_| anyhow!("回调地址无效"))?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
        || url.port() != Some(1455)
        || url.path() != "/auth/callback"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        bail!("回调地址必须是本次本地登录回调");
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    let states: Vec<_> = pairs.iter().filter(|(k, _)| k == "state").collect();
    if states.len() != 1 || states[0].1 != expected_state {
        bail!("登录状态不匹配，请重新登录");
    }
    if pairs.iter().any(|(k, _)| k == "error") {
        bail!("官方授权未完成，请重新登录");
    }
    let codes: Vec<_> = pairs.iter().filter(|(k, _)| k == "code").collect();
    if codes.len() != 1 || codes[0].1.is_empty() {
        bail!("回调缺少唯一授权码");
    }
    Ok(codes[0].1.to_string())
}
async fn complete(
    state: &Arc<AppState>,
    id: &str,
    flow_id: &str,
    callback: &str,
    endpoints: Endpoints<'_>,
) -> Result<Value> {
    let (before, verifier, code) = {
        let mut flows = flows().lock().await;
        let flow = flows
            .get_mut(flow_id)
            .ok_or_else(|| anyhow!("登录流程不存在或已过期"))?;
        if flow.source.id != id || !flow.owner.ptr_eq(&Arc::downgrade(state)) {
            bail!("登录流程不属于此来源");
        }
        if flow.expires <= now() {
            bail!("登录流程已过期");
        }
        // Validate state even when a flow is already complete.
        let code = callback_values(callback, &flow.state)?;
        if let Some(result) = &flow.result {
            return Ok(result.clone());
        }
        if flow.exchanging || flow.error.is_some() {
            bail!("登录交换已提交，请查询登录状态或重新登录");
        }
        flow.exchanging = true;
        (flow.source.clone(), flow.verifier.clone(), code)
    };
    let result = async {
        let current = source(state, id).await?;
        if !same_binding(&before, &current) {
            bail!("登录来源已变化，请重新登录");
        }
        let response = transport::client(&before)?
            .post(endpoints.token)
            .timeout(REQUEST_TIMEOUT)
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("code_verifier", &verifier),
            ])
            .send()
            .await
            .map_err(|e| anyhow!("登录交换{}，请重新发起登录", transport::failure_reason(&e)))?;
        let secret = merge_tokens(&json!({}), json_response(response).await?)?;
        if token(&secret, "refresh_token").is_empty() {
            bail!("官方未返回刷新令牌，请重新登录");
        }
        if token(&secret, "account_id").is_empty() {
            bail!("官方未返回账号身份，请重新登录");
        }
        let installed = install(state, &before, secret.clone()).await?;
        Ok(json!({"connected":true,"secret":secret,"account_id":installed.account_id}))
    }
    .await;
    let mut flows = flows().lock().await;
    if let Some(flow) = flows.get_mut(flow_id) {
        flow.verifier.clear();
        match &result {
            Ok(value) => flow.result = Some(value.clone()),
            Err(error) => flow.error = Some(error.to_string()),
        }
    }
    result
}
async fn start(state: &Arc<AppState>, id: &str) -> Result<Value> {
    let source = source(state, id).await?;
    let verifier = random();
    let nonce = random();
    let flow_id = uuid::Uuid::new_v4().to_string();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = url::Url::parse("https://auth.openai.com/oauth/authorize")?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &nonce),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "codex_cli_rs"),
    ]);
    let mut pending = flows().lock().await;
    pending.retain(|_, flow| flow.expires > now());
    // One active flow per source; replacing it invalidates old browser callbacks.
    pending.retain(|_, flow| !(flow.source.id == id && flow.owner.ptr_eq(&Arc::downgrade(state))));
    pending.insert(
        flow_id.clone(),
        Flow {
            owner: Arc::downgrade(state),
            source,
            state: nonce,
            verifier,
            expires: now() + 600,
            result: None,
            error: None,
            exchanging: false,
        },
    );
    drop(pending);
    let listening = if let Ok(listener) = TcpListener::bind("127.0.0.1:1455").await {
        tokio::spawn(async move {
            callback_server(listener).await;
        });
        true
    } else {
        false
    };
    Ok(
        json!({"flow_id":flow_id,"auth_url":url.as_str(),"callback_port":1455,"manual_callback_required":!listening}),
    )
}
async fn callback_server(listener: TcpListener) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    while let Ok(Ok((mut socket, _))) = tokio::time::timeout_at(deadline, listener.accept()).await {
        tokio::spawn(async move {
            let mut buffer = [0u8; 16384];
            let Ok(Ok(count)) =
                tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buffer)).await
            else {
                return;
            };
            let request = String::from_utf8_lossy(&buffer[..count]);
            let target = request
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("GET "))
                .and_then(|s| s.split_whitespace().next());
            let callback = target
                .filter(|s| s.starts_with("/auth/callback?"))
                .map(|s| format!("http://localhost:1455{s}"));
            let mut success = false;
            if let Some(callback) = callback {
                let nonce = url::Url::parse(&callback).ok().and_then(|url| {
                    url.query_pairs()
                        .find(|(k, _)| k == "state")
                        .map(|(_, v)| v.into_owned())
                });
                let selected = {
                    let flows = flows().lock().await;
                    flows
                        .iter()
                        .find(|(_, f)| Some(&f.state) == nonce.as_ref())
                        .and_then(|(id, f)| {
                            f.owner
                                .upgrade()
                                .map(|owner| (id.clone(), f.source.id.clone(), owner))
                        })
                };
                if let Some((flow_id, id, owner)) = selected {
                    success = complete(&owner, &id, &flow_id, &callback, ENDPOINTS)
                        .await
                        .is_ok();
                }
            }
            let body = if success {
                "登录完成，请返回 Codexpress。"
            } else {
                "登录未完成，请返回 Codexpress 查看状态或粘贴完整回调地址。"
            };
            let response = format!("HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", if success { "200 OK" } else { "400 Bad Request" }, body.len());
            let _ = socket.write_all(response.as_bytes()).await;
        });
    }
}

pub async fn handle(state: &Arc<AppState>, op: &str, payload: Value) -> Result<Value> {
    match op {
        "official.login.start" => start(state, field(&payload, "source_id")?).await,
        "official.login.complete" => {
            complete(
                state,
                field(&payload, "source_id")?,
                field(&payload, "flow_id")?,
                payload
                    .get("callback_url")
                    .and_then(Value::as_str)
                    .filter(|s| s.len() <= 16384)
                    .ok_or_else(|| anyhow!("缺少有效回调地址"))?,
                ENDPOINTS,
            )
            .await
        }
        "official.login.status" => {
            let id = field(&payload, "source_id")?;
            let flow_id = field(&payload, "flow_id")?;
            let current = source(state, id).await?;
            let flows = flows().lock().await;
            let flow = flows
                .get(flow_id)
                .ok_or_else(|| anyhow!("登录流程不存在"))?;
            if flow.source.id != id || !flow.owner.ptr_eq(&Arc::downgrade(state)) {
                bail!("登录流程不属于此来源");
            }
            if current.proxy != flow.source.proxy
                || current.base_url != flow.source.base_url
                || (flow.result.is_none() && !same_binding(&current, &flow.source))
                || flow
                    .result
                    .as_ref()
                    .is_some_and(|result| result["account_id"].as_str() != account(&current))
            {
                bail!("登录来源已经变化，请重新登录");
            }
            let expired = flow.expires <= now();
            Ok(
                json!({"state":if flow.result.is_some(){"complete"}else if flow.error.is_some()||expired{"failed"}else{"pending"},"secret":flow.result.as_ref().map(|_|&current.secret),"error":if expired{Some("登录流程已过期")}else{flow.error.as_deref()}}),
            )
        }
        "official.refresh" => {
            let include_credits = match payload.get("include_credits") {
                None => true,
                Some(Value::Bool(include)) => *include,
                _ => bail!("include_credits 需要布尔值"),
            };
            let src = ensure_access(state, field(&payload, "source_id")?).await?;
            refresh_selected_snapshots(state, &src, ENDPOINTS, include_credits).await
        }
        "official.credits.consume" => consume(state, &payload, ENDPOINTS).await,
        _ => bail!("未知官方账号操作"),
    }
}

// Test-only endpoint injection never exists in the production library or management IPC.
#[cfg(test)]
#[allow(dead_code)] // Private seam is exercised by the separate #[path] integration test crate.
pub(crate) mod fixture {
    use super::*;
    pub(crate) fn parse_quota(value: &Value) -> Result<Value> {
        quota_snapshot(value)
    }
    pub(crate) fn parse_credits(value: &Value) -> Result<Value> {
        credits_snapshot(value)
    }
    fn endpoints(base: &str) -> (String, String, String, String) {
        (
            format!("{base}/token"),
            format!("{base}/usage"),
            format!("{base}/credits"),
            format!("{base}/consume"),
        )
    }
    pub(crate) async fn consume_at(
        state: &Arc<AppState>,
        payload: &Value,
        base: &str,
    ) -> Result<Value> {
        let (token, usage, credits, consume_url) = endpoints(base);
        consume(
            state,
            payload,
            Endpoints {
                token: &token,
                usage: &usage,
                credits: &credits,
                consume: &consume_url,
            },
        )
        .await
    }
    pub(crate) async fn complete_at(
        state: &Arc<AppState>,
        id: &str,
        flow: &str,
        callback: &str,
        base: &str,
    ) -> Result<Value> {
        let (token, usage, credits, consume) = endpoints(base);
        complete(
            state,
            id,
            flow,
            callback,
            Endpoints {
                token: &token,
                usage: &usage,
                credits: &credits,
                consume: &consume,
            },
        )
        .await
    }
    pub(crate) async fn install_secret(
        state: &Arc<AppState>,
        id: &str,
        secret: Value,
    ) -> Result<Source> {
        install(state, &source(state, id).await?, secret).await
    }
    pub(crate) async fn snapshots_at(state: &Arc<AppState>, id: &str, base: &str) -> Result<Value> {
        selected_snapshots_at(state, id, base, true).await
    }
    pub(crate) async fn selected_snapshots_at(
        state: &Arc<AppState>,
        id: &str,
        base: &str,
        include_credits: bool,
    ) -> Result<Value> {
        let (token, usage, credits, consume) = endpoints(base);
        refresh_selected_snapshots(
            state,
            &source(state, id).await?,
            Endpoints {
                token: &token,
                usage: &usage,
                credits: &credits,
                consume: &consume,
            },
            include_credits,
        )
        .await
    }
    pub(crate) async fn refresh_at(state: &Arc<AppState>, id: &str, base: &str) -> Result<Source> {
        let (token, usage, credits, consume) = endpoints(base);
        refresh_inner(
            state,
            id,
            false,
            Endpoints {
                token: &token,
                usage: &usage,
                credits: &credits,
                consume: &consume,
            },
        )
        .await
    }
}
