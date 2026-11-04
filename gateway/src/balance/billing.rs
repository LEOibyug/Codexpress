//! Per-source settled-request and idle-triggered CCH ledger synchronization.
use super::{Auth, Entry, Probe, Protocol};
use crate::{model::AppState, usage::billing::ChargeMatch};
use base64::Engine;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[derive(Default)]
pub(super) struct Sync {
    running: AtomicBool,
    started: AtomicBool,
    progress: Mutex<Value>,
}
pub(crate) fn status(state: &AppState) -> Value {
    let sync = &state.balances.billing;
    let mut value = sync
        .progress
        .lock()
        .map(|v| v.clone())
        .unwrap_or_else(|_| json!({}));
    if !value.is_object() {
        value = json!({});
    }
    value["running"] = json!(sync.running.load(Ordering::Acquire));
    value
}
fn progress(state: &AppState, pages: usize, updated: usize, errors: usize) {
    if let Ok(mut p) = state.balances.billing.progress.lock() {
        *p = json!({"pages":pages,"updated":updated,"errors":errors});
    }
}

/// Poll idle settlement boundaries; provider access starts only when a source is due.
pub fn start(state: &Arc<AppState>) {
    if state.balances.billing.started.swap(true, Ordering::AcqRel) {
        return;
    }
    let weak = Arc::downgrade(state);
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(Duration::from_secs(1));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            let Some(state) = weak.upgrade() else {
                break;
            };
            schedule(&state, false, None);
        }
    });
}
pub(crate) fn schedule(state: &Arc<AppState>, full: bool, source_id: Option<&str>) -> Value {
    let sync = &state.balances.billing;
    if sync
        .running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return status(state);
    }
    let worker = state.clone();
    let requested = source_id.map(str::to_owned);
    tokio::spawn(async move {
        let state = worker;
        let sources: Vec<_> = state
            .sources
            .read()
            .await
            .iter()
            .filter(|s| {
                s.source.kind == "api"
                    && requested.as_ref().is_none_or(|id| id == &s.source.id)
                    && state.usage.billing_claim(&s.source, full)
            })
            .map(|s| s.source.clone())
            .collect();
        if sources.is_empty() {
            state
                .balances
                .billing
                .running
                .store(false, Ordering::Release);
            return;
        }
        progress(&state, 0, 0, 0);
        let (mut pages, mut updated, mut errors) = (0, 0, 0);
        for source in sources {
            let recognized = state
                .balances
                .entries
                .lock()
                .ok()
                .and_then(|e| e.get(&source.id).cloned());
            let recognized = if let Some(entry) = recognized {
                let saved = entry.lock().await;
                saved.preferred.is_some()
                    && saved.identity == source.identity()
                    && saved.proxy.as_ref() == Some(&source.proxy)
            } else {
                false
            };
            // First automatic reconciliation must not depend on a UI balance query.
            if !recognized || (full && requested.is_some()) {
                let _ = super::query(&state, json!({"source_id":source.id})).await;
            }
            let entry = state
                .balances
                .entries
                .lock()
                .ok()
                .and_then(|e| e.get(&source.id).cloned());
            let Some(entry) = entry else {
                continue;
            };
            {
                let saved = entry.lock().await;
                if saved.preferred != Some(Protocol::Cch)
                    || saved.identity != source.identity()
                    || saved.proxy.as_ref() != Some(&source.proxy)
                {
                    continue;
                }
            }
            let usage = state.usage.clone();
            let id = source.id.clone();
            let identity = source.identity();
            let billing_identity = crate::usage::billing_identity(&source);
            let plan = tokio::task::spawn_blocking(move || {
                usage.billing_plan(&id, &identity, &billing_identity, full)
            })
            .await;
            let Ok(Ok(Some(plan))) = plan else {
                continue;
            };
            let Ok(probe) = Probe::new(&source) else {
                continue;
            };
            let mut cursor = None;
            let mut cursors = HashSet::new();
            let mut matches: HashMap<i64, Option<ChargeMatch>> = HashMap::new();
            let mut invoices = HashMap::new();
            let mut complete = false;
            loop {
                let page = {
                    let mut saved = entry.lock().await;
                    if saved.identity != source.identity()
                        || saved.proxy.as_ref() != Some(&source.proxy)
                    {
                        None
                    } else {
                        ledger_page(&probe, &mut saved, plan.since, cursor.as_ref()).await
                    }
                };
                let Some(page) = page else {
                    errors += 1;
                    break;
                };
                pages += 1;
                let body = page.get("data").unwrap_or(&page);
                let Some(rows) = body
                    .get("items")
                    .or_else(|| body.get("logs"))
                    .and_then(Value::as_array)
                else {
                    errors += 1;
                    break;
                };
                let usage = state.usage.clone();
                let id = source.id.clone();
                let identity = source.identity();
                let billing_identity = crate::usage::billing_identity(&source);
                let rows = rows.clone();
                let candidates = tokio::task::spawn_blocking(move || {
                    usage.billing_matches(&id, &identity, &billing_identity, &rows)
                })
                .await;
                let Ok(Ok(candidates)) = candidates else {
                    errors += 1;
                    break;
                };
                for candidate in candidates {
                    if let Some(previous) =
                        invoices.insert(candidate.record_id.clone(), candidate.attempt)
                    {
                        if previous != candidate.attempt {
                            matches.insert(previous, None);
                            matches.insert(candidate.attempt, None);
                            continue;
                        }
                    }
                    match matches.entry(candidate.attempt) {
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(Some(candidate));
                        }
                        std::collections::hash_map::Entry::Occupied(mut e) => {
                            if e.get()
                                .as_ref()
                                .is_some_and(|old| old.record_id != candidate.record_id)
                            {
                                e.insert(None);
                            }
                        }
                    }
                }
                progress(&state, pages, updated, errors);
                let info = body.get("pageInfo").unwrap_or(body);
                match info["hasMore"].as_bool() {
                    Some(false) => {
                        complete = true;
                        break;
                    }
                    Some(true) => {}
                    None => {
                        errors += 1;
                        break;
                    }
                }
                let Some(next) = normalize_cursor(&info["nextCursor"]) else {
                    errors += 1;
                    break;
                };
                if !cursors.insert(next.to_string()) {
                    errors += 1;
                    break;
                }
                cursor = Some(next);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if complete {
                // Recheck after network waits; edited credentials cannot receive an old ledger.
                let current = state.sources.read().await;
                if current.iter().any(|s| {
                    s.source.id == source.id
                        && s.source.identity() == source.identity()
                        && s.source.proxy == source.proxy
                }) {
                    let usage = state.usage.clone();
                    let identity = source.identity();
                    let id = source.id.clone();
                    let billing_identity = crate::usage::billing_identity(&source);
                    match tokio::task::spawn_blocking(move || {
                        usage.apply_billing(&id, &identity, &billing_identity, &plan, matches)
                    })
                    .await
                    {
                        Ok(Ok(n)) => updated += n,
                        _ => errors += 1,
                    }
                }
            }
            progress(&state, pages, updated, errors);
        }
        progress(&state, pages, updated, errors);
        state
            .balances
            .billing
            .running
            .store(false, Ordering::Release);
    });
    status(state)
}

fn normalize_cursor(value: &Value) -> Option<Value> {
    let value = if let Some(raw) = value.as_str().filter(|s| s.len() <= 4096) {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(raw.trim_end_matches('='))
            .ok()?;
        serde_json::from_slice(&bytes).ok()?
    } else {
        value.clone()
    };
    let at = value["createdAt"].as_str().filter(|s| s.len() <= 40)?;
    let id = value["id"].as_u64().filter(|v| *v > 0)?;
    Some(json!({"createdAt":at,"id":id}))
}
async fn ledger_page(
    probe: &Probe<'_>,
    entry: &mut Entry,
    since: u64,
    cursor: Option<&Value>,
) -> Option<Value> {
    let path = {
        let mut args = url::form_urlencoded::Serializer::new(String::new());
        args.append_pair("limit", "100")
            .append_pair("startTime", &since.saturating_mul(1000).to_string());
        if let Some(cursor) = cursor {
            args.append_pair("cursorCreatedAt", cursor["createdAt"].as_str()?)
                .append_pair("cursorId", &cursor["id"].as_u64()?.to_string());
        }
        format!("/api/v1/me/usage-logs?{}", args.finish())
    };
    if entry
        .session
        .as_ref()
        .is_some_and(|s| s.expires <= std::time::Instant::now())
    {
        entry.session = None;
    }
    let auth = entry
        .session
        .as_ref()
        .map_or(Auth::ApiKey, |s| Auth::Session(&s.cookie));
    let mut reply = probe.request_with_timeout(&path, auth, None, 25).await?;
    if reply.status == 401
        && reply.json["type"]
            .as_str()
            .is_some_and(|s| s.starts_with("urn:claude-code-hub:problem:"))
    {
        entry.session = None;
        let mut login = probe
            .request(
                "/api/auth/login",
                Auth::Login,
                Some(json!({"key":probe.source.secret["api_key"]})),
            )
            .await?;
        if login.status != 200 || login.json["ok"] != true {
            return None;
        }
        entry.session = login.session.take();
        reply = probe
            .request_with_timeout(
                &path,
                Auth::Session(&entry.session.as_ref()?.cookie),
                None,
                25,
            )
            .await?;
    }
    (reply.status == 200).then_some(reply.json)
}
