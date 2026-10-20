//! Official account operations. Secrets stay in memory; only reset operation metadata reaches disk.
use crate::{
    model::{now, AppState, Source},
    transport,
};
use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, OnceLock, Weak},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};

const PERSISTENCE_FAILURE: &str = "新凭据保存失败，账号已暂停；请检查钥匙串后重新登录";
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
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("官方响应读取失败"))?
    {
        if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
            bail!("官方响应过大");
        }
        bytes.extend_from_slice(&chunk);
    }
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
        .send().await.map_err(|_| anyhow!("官方令牌刷新连接失败，请检查此来源代理"))?;
    let status = response.status().as_u16();
    if matches!(status, 400 | 401) {
        // A malformed request/changed endpoint is not proof that credentials expired.
        let invalid = if status == 401 {
            true
        } else {
            let mut response = response;
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| anyhow!("官方令牌响应读取失败"))?
            {
                if bytes.len() + chunk.len() > 65536 {
                    bail!("官方令牌接口返回 HTTP 400");
                }
                bytes.extend_from_slice(&chunk);
            }
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
            .map_err(|_| anyhow!("登录交换连接失败，请检查此来源代理并重新登录"))?;
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

fn number(value: &Value) -> Value {
    value
        .as_f64()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map_or(Value::Null, |n| json!(n))
}
fn timestamp(value: &Value) -> Value {
    if let Some(n) = value.as_u64() {
        return json!(n);
    }
    let Some(s) = value.as_str() else {
        return Value::Null;
    };
    parse_iso(s).map_or(Value::Null, |n| json!(n))
}
// RFC3339 timestamps from reset-credit inventory (including fractional seconds and offsets).
fn parse_iso(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let n = |a, z| s.get(a..z)?.parse::<i64>().ok();
    let (year, month, day, hour, minute, second) = (
        n(0, 4)?,
        n(5, 7)?,
        n(8, 10)?,
        n(11, 13)?,
        n(14, 16)?,
        n(17, 19)?,
    );
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&month)
        || !(1..=days[(month - 1) as usize]).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let mut tail = &s[19..];
    if tail.starts_with('.') {
        let end = tail[1..].find(|c: char| !c.is_ascii_digit())? + 1;
        if end == 1 {
            return None;
        }
        tail = &tail[end..];
    }
    let offset = if tail == "Z" || tail == "z" {
        0
    } else {
        let t = tail.as_bytes();
        if t.len() != 6 || !matches!(t[0], b'+' | b'-') || t[3] != b':' {
            return None;
        }
        let h: i64 = tail.get(1..3)?.parse().ok()?;
        let m: i64 = tail.get(4..6)?.parse().ok()?;
        if h > 23 || m > 59 {
            return None;
        }
        (h * 3600 + m * 60) * if t[0] == b'+' { 1 } else { -1 }
    };
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let civil_days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    u64::try_from(civil_days * 86400 + hour * 3600 + minute * 60 + second - offset).ok()
}
fn quota_snapshot(raw: &Value) -> Result<Value> {
    if !raw.is_object() {
        bail!("官方额度响应格式变化");
    }
    let mut windows = Vec::new();
    let mut append = |prefix: &str, limits: &Value| {
        for (name, key) in [
            ("primary", "primary_window"),
            ("secondary", "secondary_window"),
        ] {
            if let Some(window) = limits.get(key).filter(|window| !window.is_null()) {
                let used = number(&window["used_percent"]);
                windows.push(json!({"name":format!("{prefix}{name}"),"used_percent":used,"reset_at":timestamp(&window["reset_at"]),"window_seconds":number(&window["limit_window_seconds"])}));
            }
        }
    };
    append("", &raw["rate_limit"]);
    let mut complete = raw.get("rate_limit").is_some_and(Value::is_object)
        && raw
            .get("additional_rate_limits")
            .is_none_or(|v| v.is_null() || v.is_array());
    if let Some(additional) = raw["additional_rate_limits"].as_array() {
        for (index, item) in additional.iter().enumerate() {
            complete &= item.get("rate_limit").is_some_and(|v| {
                v.is_object()
                    && ["primary_window", "secondary_window"]
                        .iter()
                        .any(|key| v.get(*key).is_some_and(|w| !w.is_null()))
            });
            append(
                &format!(
                    "{}:",
                    item["limit_name"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("additional_{index}"))
                ),
                &item["rate_limit"],
            );
        }
    }
    Ok(
        json!({"plan_type":raw.get("plan_type").and_then(Value::as_str),"windows":windows,"complete":complete,"fetched_at":now(),"stale":false,"error":null}),
    )
}
fn credits_snapshot(raw: &Value) -> Result<Value> {
    let cards = raw
        .get("credits")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("官方重置卡响应格式变化"))?;
    let credits:Vec<_> = cards.iter().map(|card| {
        json!({"id":card.get("id").and_then(Value::as_str),"title":card.get("title").and_then(Value::as_str),"status":card.get("status").and_then(Value::as_str).unwrap_or("unknown"),"expires_at":timestamp(&card["expires_at"]),"expiry_known":card.get("expires_at").is_some_and(|v|v.is_null() || !timestamp(v).is_null()),"reset_type":card.get("reset_type").and_then(Value::as_str)})
    }).collect();
    Ok(
        json!({"available_count":raw.get("available_count").and_then(Value::as_u64),"credits":credits,"fetched_at":now(),"stale":false,"error":null}),
    )
}
async fn get(source: &Source, url: &str) -> Result<Value> {
    let mut request = transport::client(source)?
        .get(url)
        .timeout(REQUEST_TIMEOUT)
        .bearer_auth(token(&source.secret, "access_token"))
        .header("Accept", "application/json")
        .header("OpenAI-Beta", "codex-1")
        .header("originator", "Codex Desktop");
    if let Some(id) = account(source) {
        request = request.header("ChatGPT-Account-ID", id);
    }
    json_response(
        request
            .send()
            .await
            .map_err(|_| anyhow!("官方查询连接失败，请检查此来源代理"))?,
    )
    .await
}
fn stale(mut previous: Value, kind: &str, error: &str) -> Value {
    if !previous.is_object() {
        previous = if kind == "quota" {
            json!({"plan_type":null,"windows":[],"fetched_at":null})
        } else {
            json!({"available_count":null,"credits":[],"fetched_at":null})
        };
    }
    previous["stale"] = json!(true);
    previous["error"] = json!(error);
    previous
}
fn recover_exhaustion(runtime: &mut crate::model::SourceRuntime) {
    if runtime.health != "exhausted"
        || runtime.quota["stale"] != false
        || runtime.quota["complete"] != true
    {
        return;
    }
    let Some(windows) = runtime.quota["windows"]
        .as_array()
        .filter(|v| !v.is_empty())
    else {
        return;
    };
    if windows
        .iter()
        .any(|window| window["used_percent"].as_f64().is_none())
    {
        return;
    }
    let exhausted: Vec<_> = windows
        .iter()
        .filter(|window| {
            window["used_percent"]
                .as_f64()
                .is_some_and(|used| used >= 100.0)
        })
        .collect();
    if exhausted.is_empty() {
        runtime.health = "ready".into();
        runtime.cooldown_until = None;
        runtime.last_error = None;
    } else {
        let resets: Option<Vec<_>> = exhausted
            .iter()
            .map(|window| window["reset_at"].as_u64().filter(|reset| *reset > now()))
            .collect();
        if let Some(resets) = resets {
            runtime.cooldown_until = resets.into_iter().max();
        }
    }
}
async fn refresh_snapshots(
    state: &Arc<AppState>,
    src: &Source,
    endpoints: Endpoints<'_>,
) -> Result<Value> {
    let quota = get(src, endpoints.usage)
        .await
        .and_then(|v| quota_snapshot(&v));
    let credits = get(src, endpoints.credits)
        .await
        .and_then(|v| credits_snapshot(&v));
    let mut sources = state.sources.write().await;
    let runtime = sources
        .iter_mut()
        .find(|r| same_binding(&r.source, src))
        .ok_or_else(|| anyhow!("来源在查询期间发生变化，请刷新"))?;
    runtime.quota = match quota {
        Ok(v) => v,
        Err(e) => stale(runtime.quota.clone(), "quota", &e.to_string()),
    };
    recover_exhaustion(runtime);
    runtime.credits = match credits {
        Ok(v) => v,
        Err(e) => stale(runtime.credits.clone(), "credits", &e.to_string()),
    };
    Ok(json!({"quota":runtime.quota,"credits":runtime.credits}))
}

#[derive(Serialize, Deserialize)]
struct Operation {
    source_id: String,
    account_id: Option<String>,
    credit_id: String,
    operation_id: String,
    created_at: u64,
    result: Value,
}
fn operation_path(state: &Arc<AppState>, id: &str) -> PathBuf {
    state
        .state_dir
        .join("official-operations")
        .join(format!("{:x}.json", Sha256::digest(id.as_bytes())))
}
fn write_operation(state: &Arc<AppState>, operation: &Operation, create: bool) -> Result<()> {
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };
    let path = operation_path(state, &operation.operation_id);
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent).map_err(|_| anyhow!("无法保存重置操作记录"))?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .map_err(|_| anyhow!("无法保护重置操作目录"))?;
    let target = if create {
        path.clone()
    } else {
        path.with_extension("tmp")
    };
    let mut file = OpenOptions::new()
        .write(true)
        .mode(0o600)
        .create_new(true)
        .open(&target)
        .map_err(|_| anyhow!("无法保存重置操作记录，请先核实已有操作"))?;
    let bytes = serde_json::to_vec(operation)?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| anyhow!("无法落盘重置操作记录"))?;
    if !create {
        fs::rename(&target, &path).map_err(|_| anyhow!("无法更新重置操作记录"))?;
    }
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| anyhow!("无法落盘重置操作目录"))?;
    Ok(())
}
fn reset_result(code: &str, windows: Value) -> Value {
    json!({"code":code,"windows_reset":windows,"quota":null,"credits":null})
}
async fn consume(
    state: &Arc<AppState>,
    payload: &Value,
    endpoints: Endpoints<'_>,
) -> Result<Value> {
    if payload.get("confirmed").and_then(Value::as_bool) != Some(true) {
        bail!("使用重置卡需要明确确认");
    }
    let id = field(payload, "source_id")?;
    let credit = field(payload, "credit_id")?;
    let operation_id = field(payload, "operation_id")?;
    let expected_account = field(payload, "expected_account_id")?;
    let initial = source(state, id).await?;
    let mutex = lock(
        state,
        format!("consume:{}", account(&initial).unwrap_or(id)),
    )
    .await;
    let _guard = mutex.lock().await;
    if account(&source(state, id).await?) != Some(expected_account) {
        bail!("确认的账号已变化，请重新确认重置卡操作");
    }
    let path = operation_path(state, operation_id);
    if path.exists() {
        let bytes =
            std::fs::read(&path).map_err(|_| anyhow!("无法读取重置操作记录，禁止重复提交"))?;
        let operation: Operation = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("重置操作记录损坏，禁止重复提交"))?;
        if operation.source_id != id
            || operation.credit_id != credit
            || operation.account_id.as_deref() != account(&initial)
        {
            bail!("操作标识已经绑定其他来源、账号或重置卡");
        }
        let mut result = operation.result;
        if ["pending_verification", "unknown"].contains(&result["code"].as_str().unwrap_or("")) {
            if let Ok(src) = refresh_inner(state, id, false, endpoints).await {
                if let Ok(refreshed) = refresh_snapshots(state, &src, endpoints).await {
                    result["quota"] = refreshed["quota"].clone();
                    result["credits"] = refreshed["credits"].clone();
                }
            }
        }
        return Ok(result);
    }
    let src = refresh_inner(state, id, false, endpoints).await?;
    if account(&initial) != account(&src) || initial.proxy != src.proxy {
        bail!("来源账号或代理已变化，请重新确认");
    }
    // A previous uncertain operation for this card blocks new IDs, including after restart.
    let directory = state.state_dir.join("official-operations");
    if directory.exists() {
        for entry in std::fs::read_dir(&directory).map_err(|_| anyhow!("无法核实待确认操作"))?
        {
            let entry = entry.map_err(|_| anyhow!("无法核实待确认操作"))?;
            if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let operation: Operation = serde_json::from_slice(
                &std::fs::read(entry.path()).map_err(|_| anyhow!("无法读取待确认操作"))?,
            )
            .map_err(|_| anyhow!("重置操作记录损坏，禁止提交"))?;
            if ((operation.account_id.is_some()
                && operation.account_id.as_deref() == account(&src))
                || operation.source_id == id)
                && operation.credit_id == credit
                && ["pending_verification", "unknown"]
                    .contains(&operation.result["code"].as_str().unwrap_or(""))
            {
                bail!("此重置卡有结果待核实的操作，请刷新核实，不能再次提交");
            }
        }
    }
    let cards = credits_snapshot(&get(&src, endpoints.credits).await?)?;
    let card = cards["credits"]
        .as_array()
        .and_then(|cards| cards.iter().find(|c| c["id"] == credit))
        .ok_or_else(|| anyhow!("重置卡不属于此账号或已移除"))?;
    if card["status"] != "available" || card["expires_at"].as_u64().is_some_and(|exp| exp <= now())
    {
        bail!("重置卡不可用、已过期或状态未知，请刷新后重新确认");
    }
    // Compare the visible card to the fresh inventory; any meaningful change needs a new confirmation.
    {
        let sources = state.sources.read().await;
        if let Some(previous) = sources
            .iter()
            .find(|r| r.source.id == id)
            .and_then(|r| r.credits["credits"].as_array())
            .and_then(|cards| cards.iter().find(|c| c["id"] == credit))
        {
            for key in ["status", "expires_at", "reset_type"] {
                if previous[key] != card[key] {
                    bail!("重置卡信息已变化，请刷新并重新确认");
                }
            }
        }
    }
    let latest = source(state, id).await?;
    if account(&latest) != Some(expected_account) || !same_binding(&latest, &src) {
        bail!("来源在确认期间发生变化，请重新确认");
    }
    let mut operation = Operation {
        source_id: id.into(),
        account_id: account(&src).map(str::to_owned),
        credit_id: credit.into(),
        operation_id: operation_id.into(),
        created_at: now(),
        result: reset_result("pending_verification", Value::Null),
    };
    write_operation(state, &operation, true)?;
    // All errors after durable intent are uncertain. Never retry this POST, including on 401.
    let mut request = transport::client(&src)?
        .post(endpoints.consume)
        .timeout(REQUEST_TIMEOUT)
        .bearer_auth(token(&src.secret, "access_token"))
        .header("Accept", "application/json")
        .header("OpenAI-Beta", "codex-1")
        .header("originator", "Codex Desktop")
        .json(&json!({"credit_id":credit,"redeem_request_id":operation_id}));
    if let Some(id) = account(&src) {
        request = request.header("ChatGPT-Account-ID", id);
    }
    if let Ok(response) = request.send().await {
        if let Ok(value) = json_response(response).await {
            let code = value
                .get("code")
                .or_else(|| value.get("result"))
                .or_else(|| value.get("outcome"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            operation.result = reset_result(
                if ["reset", "nothing_to_reset", "no_credit", "already_redeemed"].contains(&code) {
                    code
                } else {
                    "unknown"
                },
                number(&value["windows_reset"]),
            );
        }
    }
    if let Ok(refreshed) = refresh_snapshots(state, &src, endpoints).await {
        operation.result["quota"] = refreshed["quota"].clone();
        operation.result["credits"] = refreshed["credits"].clone();
    }
    // If a final write fails the original durable pending marker remains, preventing another POST.
    if write_operation(state, &operation, false).is_err() {
        operation.result["code"] = json!("pending_verification");
    }
    Ok(operation.result)
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
            let src = ensure_access(state, field(&payload, "source_id")?).await?;
            refresh_snapshots(state, &src, ENDPOINTS).await
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
        let (token, usage, credits, consume) = endpoints(base);
        refresh_snapshots(
            state,
            &source(state, id).await?,
            Endpoints {
                token: &token,
                usage: &usage,
                credits: &credits,
                consume: &consume,
            },
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
