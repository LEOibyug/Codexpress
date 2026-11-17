//! Durable reset-credit operations and conservative handling of uncertain outcomes.
use super::{
    account, field, json_response, lock, refresh_inner, same_binding,
    auto_reset::Authorization,
    snapshots::{credits_snapshot, get, number, refresh_snapshots},
    source, token, Endpoints, REQUEST_TIMEOUT,
};
use crate::{
    model::{now, AppState},
    transport,
};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[derive(Serialize, Deserialize)]
struct Operation {
    source_id: String,
    account_id: Option<String>,
    credit_id: String,
    operation_id: String,
    created_at: u64,
    result: Value,
    weekly_generation: Option<u64>,
}
fn operation_path(state: &Arc<AppState>, id: &str) -> PathBuf {
    state
        .state_dir
        .join("official-operations")
        .join(format!("{:x}.json", Sha256::digest(id.as_bytes())))
}
fn write_operation(state: &Arc<AppState>, operation: &Operation, create: bool) -> Result<()> {
    write_record(&operation_path(state, &operation.operation_id), operation, create)
}
pub(super) fn write_record(path: &std::path::Path, value: &impl Serialize, create: bool) -> Result<()> {
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent).map_err(|_| anyhow!("无法保存重置操作记录"))?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .map_err(|_| anyhow!("无法保护重置操作目录"))?;
    let target = if create {
        path.to_path_buf()
    } else {
        path.with_extension("tmp")
    };
    let mut file = OpenOptions::new()
        .write(true)
        .mode(0o600)
        .create_new(true)
        .open(&target)
        .map_err(|_| anyhow!("无法保存重置操作记录，请先核实已有操作"))?;
    let bytes = serde_json::to_vec(value)?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| anyhow!("无法落盘重置操作记录"))?;
    if !create {
        fs::rename(&target, path).map_err(|_| anyhow!("无法更新重置操作记录"))?;
    }
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| anyhow!("无法落盘重置操作目录"))?;
    Ok(())
}
fn reset_result(code: &str, windows: Value) -> Value {
    json!({"code":code,"windows_reset":windows,"quota":null,"credits":null})
}
pub(super) async fn consume(
    state: &Arc<AppState>,
    payload: &Value,
    endpoints: Endpoints<'_>,
) -> Result<Value> {
    if payload.get("confirmed").and_then(Value::as_bool) != Some(true) {
        bail!("使用重置卡需要明确确认");
    }
    let id = field(payload, "source_id")?;
    let expected_account = field(payload, "expected_account_id")?;
    let initial = source(state, id).await?;
    if account(&initial) != Some(expected_account) {
        bail!("确认的账号已变化，请重新确认重置卡操作");
    }
    let mutex = lock(
        state,
        format!("consume:{}", account(&initial).unwrap_or(id)),
    )
    .await;
    let _guard = mutex.lock().await;
    consume_locked(state, payload, endpoints, &initial, None).await
}

// Automatic authorization is internal only; IPC always enters the confirmed wrapper above.
pub(super) async fn consume_locked(
    state: &Arc<AppState>,
    payload: &Value,
    endpoints: Endpoints<'_>,
    initial: &crate::model::Source,
    automatic: Option<&Authorization>,
) -> Result<Value> {
    let id = field(payload, "source_id")?;
    let credit = if automatic.is_some() { "" } else { field(payload, "credit_id")? };
    let operation_id = field(payload, "operation_id")?;
    let expected_account = field(payload, "expected_account_id")?;
    if account(&source(state, id).await?) != Some(expected_account) {
        bail!("确认的账号已变化，请重新确认重置卡操作");
    }
    let path = operation_path(state, operation_id);
    if path.exists() {
        let bytes =
            std::fs::read(&path).map_err(|_| anyhow!("无法读取重置操作记录，禁止重复提交"))?;
        let operation: Operation = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("重置操作记录损坏，禁止重复提交"))?;
        if (automatic.is_none() && (operation.source_id != id || operation.credit_id != credit))
            || operation.account_id.as_deref() != account(initial)
        {
            bail!("操作标识已经绑定其他来源、账号或重置卡");
        }
        let mut result = operation.result;
        if automatic.is_none() && ["pending_verification", "unknown"].contains(&result["code"].as_str().unwrap_or("")) {
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
    if account(initial) != account(&src) || initial.proxy != src.proxy {
        bail!("来源账号或代理已变化，请重新确认");
    }
    // A previous uncertain operation for this card blocks new IDs, including after restart.
    let directory = state.state_dir.join("official-operations");
    let mut excluded_cards = Vec::new();
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
            let same_account = operation.account_id.as_deref().map_or(operation.source_id == id, |account_id| {
                Some(account_id) == account(&src)
            });
            if !same_account || (automatic.is_none() && operation.credit_id != credit) { continue; }
            match operation.result["code"].as_str() {
                Some("pending_verification" | "unknown") => {
                    if automatic.is_none()
                        || !super::auto_reset::allows_uncertain(state, &src, operation.weekly_generation).await?
                    {
                        bail!("此重置卡有结果待核实的操作，请刷新核实，不能再次提交");
                    }
                    // A later confirmed recovery/new cycle permits another event,
                    // never a replay of the same uncertain card.
                    excluded_cards.push(operation.credit_id);
                }
                Some("reset" | "already_redeemed") if automatic.is_some() => excluded_cards.push(operation.credit_id),
                _ => {}
            }
        }
    }
    // A snapshot started before redemption must finish publishing before the
    // POST, so it cannot later restore an already-used card to the account.
    let credits_mutex = lock(state, format!("credits:{}", src.identity())).await;
    let credits_guard = credits_mutex.lock().await;
    let cards = credits_snapshot(&get(&src, endpoints.credits).await?)?;
    let credit = if automatic.is_some() {
        let Some(credit) = super::auto_reset::select_card(&cards, &excluded_cards) else {
            return Ok(reset_result("no_available_credit", Value::Null));
        };
        credit
    } else {
        credit
    };
    let card = cards["credits"]
        .as_array()
        .and_then(|cards| cards.iter().find(|c| c["id"] == credit))
        .ok_or_else(|| anyhow!("重置卡不属于此账号或已移除"))?;
    if card["status"] != "available" || card["expires_at"].as_u64().is_some_and(|exp| exp <= now())
    {
        bail!("重置卡不可用、已过期或状态未知，请刷新后重新确认");
    }
    // Compare the visible card to the fresh inventory; any meaningful change needs a new confirmation.
    if automatic.is_none() {
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
    let _routing = if automatic.is_some() {
        Some(state.routing_sync.lock().await)
    } else {
        None
    };
    let event_mutex = super::auto_reset::event_lock(state, &src).await;
    let event_guard = event_mutex.lock().await;
    let settings = state.settings.read().await;
    let mut sources = state.sources.write().await;
    let latest = sources.iter().find(|r| r.source.id == id)
        .ok_or_else(|| anyhow!("来源已删除"))?;
    if account(&latest.source) != Some(expected_account) || !same_binding(&latest.source, &src) {
        bail!("来源在确认期间发生变化，请重新确认");
    }
    if let Some(proof) = automatic {
        if !settings.auto_reset_weekly(&src) || !same_binding(initial, &src) || !proof.approved(state, latest)? {
            bail!("自动重置条件已变化");
        }
    }
    let weekly_generation = super::auto_reset::mark_submitted(
        state, &src, operation_id, &latest.quota, settings.auto_reset_weekly(&src), automatic,
    )?;
    let mut operation = Operation {
        source_id: id.into(),
        account_id: account(&src).map(str::to_owned),
        credit_id: credit.into(),
        operation_id: operation_id.into(),
        created_at: now(),
        result: reset_result("pending_verification", Value::Null),
        weekly_generation,
    };
    write_operation(state, &operation, true)?;
    if automatic.is_some() {
        // A usage query started before this durable dispatch cannot confirm it.
        let identity = src.identity();
        for runtime in sources.iter_mut().filter(|r| r.source.identity() == identity) {
            runtime.health_revision = runtime.health_revision.wrapping_add(1);
        }
    }
    drop(sources);
    drop(settings);
    drop(event_guard);
    drop(_routing);
    // All errors after durable intent are uncertain. Never retry this POST, including on 401.
    let mut request = transport::client(&src)?
        .post(endpoints.consume)
        .timeout(if automatic.is_some() { REQUEST_TIMEOUT.min(Duration::from_secs(4)) } else { REQUEST_TIMEOUT })
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
    drop(credits_guard);
    if automatic.is_none() {
        if let Ok(refreshed) = refresh_snapshots(state, &src, endpoints).await {
            operation.result["quota"] = refreshed["quota"].clone();
            operation.result["credits"] = refreshed["credits"].clone();
        }
    }
    // If a final write fails the original durable pending marker remains, preventing another POST.
    if write_operation(state, &operation, false).is_err() {
        operation.result["code"] = json!("pending_verification");
    }
    Ok(operation.result)
}
