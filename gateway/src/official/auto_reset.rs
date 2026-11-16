//! Account-wide weekly exhaustion epochs. No timer or status read performs upstream work.
use super::{account, lock, same_binding, source, Endpoints};
use crate::model::{now, AppState, Source};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
pub(super) struct Event {
    generation: u64,
    exhausted: bool,
    reset_at: u64,
    operation_id: Option<String>,
    cleared_generation: Option<u64>,
}

// Missing, retained, expired or incomplete observations cannot establish either edge.
fn weekly(quota: &Value) -> Option<(bool, u64)> {
    if quota["stale"] != false || quota["complete"] != true || !quota["error"].is_null()
        || !quota["fetched_at"].as_u64().is_some_and(|at| at <= now() && now() - at <= 120)
    {
        return None;
    }
    let windows = quota["windows"].as_array()?;
    let mut weekly_count = 0;
    let mut exhausted = false;
    let mut reset_at = 0_u64;
    for window in windows.iter().filter(|w| matches!(w["name"].as_str(), Some("primary" | "secondary"))) {
        let used = window["used_percent"].as_f64().filter(|used| used.is_finite() && *used >= 0.0)?;
        if window["window_seconds"].as_f64() != Some(604800.0) { continue; }
        let reset = window["reset_at"].as_u64().filter(|at| *at > now())?;
        weekly_count += 1;
        exhausted |= used >= 100.0;
        reset_at = reset_at.max(reset);
    }
    (weekly_count > 0).then_some((exhausted, reset_at))
}

pub(super) fn select_card<'a>(cards: &'a Value, excluded: &[String]) -> Option<&'a str> {
    cards["credits"].as_array()?.iter().filter(|card| {
        card["status"] == "available" && card["expiry_known"] == true
            && card["id"].as_str().is_some_and(|id| !id.is_empty())
            && !excluded.iter().any(|id| card["id"].as_str() == Some(id.as_str()))
            && (card["expires_at"].is_null()
                || card["expires_at"].as_u64().is_some_and(|at| at > now()))
    }).min_by(|a, b| {
        a["expires_at"].as_u64().unwrap_or(u64::MAX)
            .cmp(&b["expires_at"].as_u64().unwrap_or(u64::MAX))
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    })?["id"].as_str()
}

pub(super) async fn event_lock(state: &Arc<AppState>, src: &Source) -> Arc<tokio::sync::Mutex<()>> {
    lock(state, format!("weekly-event:{}", src.identity())).await
}

fn event_path(state: &Arc<AppState>, src: &Source) -> Option<std::path::PathBuf> {
    Some(state.state_dir.join("official-operations").join("weekly-events")
        .join(format!("{:x}.json", Sha256::digest(account(src)?.as_bytes()))))
}

// Caller holds weekly-event:<account>. Snapshot publication and commit use the
// same short, network-free critical section, preserving recovery edges even
// while redemption holds consume:<account> for its network requests.
pub(super) fn observe_snapshot(state: &Arc<AppState>, src: &Source, quota: &Value, enabled: bool) -> Result<Option<Event>> {
    let Some((exhausted, reset_at)) = weekly(quota) else { return Ok(None) };
    let Some(path) = event_path(state, src) else { return Ok(None) };
    let exists = path.exists();
    if !exists && !enabled { return Ok(None); }
    let mut event: Event = if exists {
        serde_json::from_slice(&std::fs::read(&path)?)
            .map_err(|_| anyhow!("自动重置事件记录损坏，禁止提交"))?
    } else {
        Event { generation: 0, exhausted: false, reset_at, operation_id: None, cleared_generation: None }
    };
    let new_cycle = event.reset_at <= now() && reset_at > event.reset_at;
    if !exhausted || new_cycle {
        event.cleared_generation = Some(event.generation);
    }
    if exhausted && (!event.exhausted || new_cycle) {
        event.generation = event.generation.checked_add(1)
            .ok_or_else(|| anyhow!("自动重置事件序号已耗尽"))?;
        event.operation_id = None;
    }
    let changed = !exists || event.exhausted != exhausted || event.reset_at != reset_at || new_cycle;
    event.exhausted = exhausted;
    event.reset_at = reset_at;
    if changed {
        super::resets::write_record(&path, &event, !exists)?;
    }
    Ok(Some(event))
}

pub(super) async fn observe_locked(state: &Arc<AppState>, src: &Source) -> Result<Option<Event>> {
    let mutex = event_lock(state, src).await;
    let _guard = mutex.lock().await;
    let enabled = state.settings.read().await.auto_reset_weekly_quota;
    let sources = state.sources.read().await;
    let Some(runtime) = sources.iter().find(|r| same_binding(&r.source, src)) else { return Ok(None) };
    observe_snapshot(state, src, &runtime.quota, enabled)
}

pub(super) fn mark_submitted(state: &Arc<AppState>, src: &Source, operation_id: &str, quota: &Value, enabled: bool) -> Result<Option<u64>> {
    let Some(mut event) = observe_snapshot(state, src, quota, enabled)? else { return Ok(None) };
    if event.exhausted {
        event.operation_id = Some(operation_id.to_owned());
        super::resets::write_record(&event_path(state, src).unwrap(), &event, false)?;
    }
    Ok(Some(event.generation))
}

pub(super) fn current_generation(state: &Arc<AppState>, src: &Source) -> Result<Option<u64>> {
    let Some(path) = event_path(state, src).filter(|path| path.exists()) else { return Ok(None) };
    let event: Event = serde_json::from_slice(&std::fs::read(path)?)?;
    Ok(Some(event.generation))
}

pub(super) async fn allows_uncertain(state: &Arc<AppState>, src: &Source, generation: Option<u64>) -> Result<bool> {
    Ok(observe_locked(state, src).await?.and_then(|event| event.cleared_generation)
        .is_some_and(|cleared| generation.is_none_or(|generation| generation <= cleared)))
}

pub(super) fn authorized_snapshot(sources: &[crate::model::SourceRuntime], expected: &Source, reset_at: u64) -> bool {
    sources.iter().any(|r| r.source.enabled && same_binding(&r.source, expected)
        && weekly(&r.quota) == Some((true, reset_at)))
}

async fn authorized(state: &Arc<AppState>, expected: &Source, reset_at: u64) -> bool {
    state.settings.read().await.auto_reset_weekly_quota
        && authorized_snapshot(&state.sources.read().await, expected, reset_at)
}

pub(super) async fn consider(state: &Arc<AppState>, expected: &Source, endpoints: Endpoints<'_>) -> Result<()> {
    let Some(account_id) = account(expected) else { return Ok(()) };
    let mutex = lock(state, format!("consume:{account_id}")).await;
    let _guard = mutex.lock().await;
    let current = source(state, &expected.id).await?;
    if !same_binding(&current, expected) { return Ok(()); }
    let Some(event) = observe_locked(state, &current).await? else { return Ok(()) };
    if !event.exhausted || !authorized(state, &current, event.reset_at).await { return Ok(()); }
    let schedule_key = format!("auto-reset:{account_id}:{}", event.generation);
    if state.official_queries.lock().await.get(&schedule_key).is_some_and(|s| !s.is_due()) {
        return Ok(());
    }
    let operation_id = event.operation_id.unwrap_or_else(||
        format!("weekly-{:x}-{}", Sha256::digest(account_id.as_bytes()), event.generation));
    let outcome = super::resets::consume_locked(state, &json!({
        "source_id":current.id,"expected_account_id":account_id,"operation_id":operation_id
    }), endpoints, &current, Some((&current, event.reset_at, event.generation))).await;
    // No card, failed preflight and unknown outcomes back off on future explicit
    // refreshes; this schedule never starts its own polling task.
    let failure = match &outcome {
        Err(error) => Some(error.to_string()),
        Ok(result) if matches!(result["code"].as_str(), Some("unknown" | "pending_verification")) =>
            Some("重置结果待核实".into()),
        _ => None,
    };
    state.official_queries.lock().await.entry(schedule_key).or_default().finish(failure);
    outcome.map(|_| ())
}
