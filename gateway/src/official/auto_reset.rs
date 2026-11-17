//! Account-wide weekly exhaustion epochs. No timer or status read performs upstream work.
use super::{account, lock, same_binding, source, Endpoints};
use crate::model::{now, AppState, Source, SourceRuntime};
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
    let at = now();
    let mut weekly_count = 0;
    let mut exhausted = false;
    let mut reset_at = 0_u64;
    for window in super::snapshots::confirmed_main_windows(quota)? {
        let used = window["used_percent"].as_f64()?;
        let period = window["window_seconds"].as_f64().filter(|period| *period > 0.0)?;
        if period != 604800.0 { continue; }
        let reset = window["reset_at"].as_u64().filter(|reset| *reset > at)?;
        weekly_count += 1;
        exhausted |= used >= 100.0;
        reset_at = reset_at.max(reset);
    }
    (weekly_count > 0).then_some((exhausted, reset_at))
}

pub(super) fn week_only(quota: &Value) -> Option<u64> {
    if quota["windows"].as_array()?.len() != 1 { return None; }
    weekly(quota).map(|(_, reset_at)| reset_at)
}

fn eligible_week(runtime: &SourceRuntime, inferred_revision: Option<u64>) -> Option<u64> {
    if !runtime.source.enabled || !runtime.quota_exhausted { return None; }
    match weekly(&runtime.quota) {
        Some((true, reset_at)) => Some(reset_at),
        _ if inferred_revision == Some(runtime.health_revision) => week_only(&runtime.quota),
        _ => None,
    }
}

pub(super) struct Authorization {
    reset_at: u64,
    generation: u64,
    inferred_revision: Option<u64>,
}

impl Authorization {
    pub(super) fn approved(&self, state: &Arc<AppState>, runtime: &SourceRuntime) -> Result<bool> {
        Ok(eligible_week(runtime, self.inferred_revision) == Some(self.reset_at)
            && current_generation(state, &runtime.source)? == Some(self.generation))
    }
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
    observe_week(state, src, exhausted, reset_at, enabled)
}

fn observe_week(state: &Arc<AppState>, src: &Source, exhausted: bool, reset_at: u64, enabled: bool) -> Result<Option<Event>> {
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

pub(super) fn mark_submitted(state: &Arc<AppState>, src: &Source, operation_id: &str, quota: &Value, enabled: bool, automatic: Option<&Authorization>) -> Result<Option<u64>> {
    let event = match automatic {
        Some(proof) => observe_week(state, src, true, proof.reset_at, true)?,
        None => observe_snapshot(state, src, quota, enabled)?,
    };
    let event = match event {
        Some(event) => Some(event),
        None => read_event(state, src)?,
    };
    let Some(mut event) = event else { return Ok(account(src).map(|_| 1)); };
    if !event.exhausted {
        // An uncertain manual submission without a current exhausted event
        // must not be cleared by quota observations from an older event.
        let next = event.generation.checked_add(1).ok_or_else(|| anyhow!("自动重置事件序号已耗尽"))?;
        return Ok(Some(next));
    }
    event.operation_id = Some(operation_id.to_owned());
    super::resets::write_record(&event_path(state, src).unwrap(), &event, false)?;
    Ok(Some(event.generation))
}

fn read_event(state: &Arc<AppState>, src: &Source) -> Result<Option<Event>> {
    let Some(path) = event_path(state, src).filter(|path| path.exists()) else { return Ok(None) };
    Ok(Some(serde_json::from_slice(&std::fs::read(path)?)?))
}

fn current_generation(state: &Arc<AppState>, src: &Source) -> Result<Option<u64>> {
    Ok(read_event(state, src)?.filter(|event| event.exhausted).map(|event| event.generation))
}

pub(super) async fn allows_uncertain(state: &Arc<AppState>, src: &Source, generation: Option<u64>) -> Result<bool> {
    let mutex = event_lock(state, src).await;
    let _guard = mutex.lock().await;
    Ok(read_event(state, src)?.and_then(|event| event.cleared_generation)
        .is_some_and(|cleared| generation.map_or(cleared > 0, |generation| generation <= cleared)))
}

pub(super) async fn consider(
    state: &Arc<AppState>, expected: &Source, endpoints: Endpoints<'_>, inferred_revision: Option<u64>,
) -> Result<Option<Value>> {
    let Some(account_id) = account(expected) else { return Ok(None) };
    let mutex = lock(state, format!("consume:{account_id}")).await;
    let _guard = mutex.lock().await;
    let current = source(state, &expected.id).await?;
    if !same_binding(&current, expected) { return Ok(None); }
    let event_mutex = event_lock(state, &current).await;
    let (event, reset_at) = {
        let _event_guard = event_mutex.lock().await;
        let settings = state.settings.read().await;
        if !settings.auto_reset_weekly(&current) { return Ok(None); }
        let sources = state.sources.read().await;
        let Some(runtime) = sources.iter().find(|r| same_binding(&r.source, &current)) else { return Ok(None) };
        let Some(reset_at) = eligible_week(runtime, inferred_revision) else { return Ok(None) };
        let Some(event) = observe_week(state, &current, true, reset_at, true)? else { return Ok(None) };
        (event, reset_at)
    };
    let operation_id = event.operation_id.unwrap_or_else(||
        format!("weekly-{:x}-{}", Sha256::digest(account_id.as_bytes()), event.generation));
    let proof = Authorization { reset_at, generation: event.generation, inferred_revision };
    super::resets::consume_locked(state, &json!({
        "source_id":current.id,"expected_account_id":account_id,"operation_id":operation_id
    }), endpoints, &current, Some(&proof)).await.map(Some)
}
