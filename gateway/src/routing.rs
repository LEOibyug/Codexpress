use crate::model::{now, AppState, Source};
use serde_json::{json, Value};
use std::sync::Arc;

pub fn notify_quotas(state: &Arc<AppState>, sources: &[crate::model::SourceRuntime]) {
    let mut seen = std::collections::HashSet::new();
    for runtime in sources.iter().filter(|r| r.source.kind == "official") {
        let identity = runtime.source.identity();
        if !seen.insert(identity.clone()) {
            continue;
        }
        let remaining = account_remaining(runtime, sources);
        let exhausted = sources.iter().any(|r| {
            r.source.kind == "official"
                && r.source.identity() == identity
                && r.health == "exhausted"
        }) || remaining == Some(0.0);
        if exhausted || remaining.is_some_and(|v| v > 0.0) {
            state
                .notifications
                .quota(&identity, &runtime.source.name, exhausted);
        }
    }
}

pub async fn candidates(state: &Arc<AppState>, model: &str, binding: Option<&str>) -> Vec<Source> {
    candidates_with_session(state, model, binding, None).await
}
pub async fn candidates_with_session(
    state: &Arc<AppState>,
    model: &str,
    binding: Option<&str>,
    session: Option<&str>,
) -> Vec<Source> {
    // An empty model also serves discovery requests, which do not take a
    // conversation slot. Generation always rechecks capacity during reserve.
    candidate_sources(state, model, binding, session, !model.is_empty()).await
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RecoveryWait {
    Ready,
    TimedOut { retry_after: u64 },
    Unavailable,
}

enum RecoveryReadiness {
    Ready,
    Waiting(u64),
    Blocked,
}

#[derive(Clone, Copy)]
pub(crate) struct RecoveryOrigin<'a> {
    pub source_id: &'a str,
    pub identity: Option<&'a str>,
}

/// Wait only for transient admission failures. No inference or health mutation
/// happens here, and no state lock is retained across a timer wait.
pub(crate) async fn wait_for_recovery(
    state: &Arc<AppState>,
    model: &str,
    binding: Option<&str>,
    session: Option<&str>,
    origin: Option<RecoveryOrigin<'_>>,
    deadline: tokio::time::Instant,
) -> RecoveryWait {
    loop {
        let snapshot = tokio::time::timeout_at(
            deadline,
            recovery_readiness(state, model, binding, session, origin),
        )
        .await;
        let retry_after = match snapshot {
            Ok(RecoveryReadiness::Ready) => return RecoveryWait::Ready,
            Ok(RecoveryReadiness::Blocked) => return RecoveryWait::Unavailable,
            Ok(RecoveryReadiness::Waiting(seconds)) => seconds,
            Err(_) => return RecoveryWait::TimedOut { retry_after: 1 },
        };
        let timestamp = tokio::time::Instant::now();
        if timestamp >= deadline {
            return RecoveryWait::TimedOut { retry_after };
        }
        tokio::time::sleep_until((timestamp + std::time::Duration::from_millis(250)).min(deadline))
            .await;
    }
}

async fn recovery_readiness(
    state: &Arc<AppState>,
    model: &str,
    binding: Option<&str>,
    session: Option<&str>,
    origin: Option<RecoveryOrigin<'_>>,
) -> RecoveryReadiness {
    let settings = state.settings.read().await.clone();
    let runtimes = state.sources.read().await;
    let bound_id = if let Some(binding) = binding {
        match state.bindings.lock().await.get(binding).cloned() {
            Some(id) => Some(id),
            // A confirmed legacy credential can establish the origin of opaque
            // state that has never been observed by this gateway.
            None if origin.is_some() => None,
            None => return RecoveryReadiness::Blocked,
        }
    } else {
        None
    };
    let timestamp = now();
    let mut retry_after: Option<u64> = None;
    for runtime in runtimes.iter() {
        let source = &runtime.source;
        if !source.enabled
            || !(model.is_empty()
                || source.models.is_empty()
                || source.models.iter().any(|m| m == model))
            || bound_id.as_ref().is_some_and(|id| id != &source.id)
            || origin.is_some_and(|origin| {
                origin.source_id != source.id
                    || origin
                        .identity
                        .is_some_and(|identity| identity != source.identity())
            })
            || (settings.mode == "fixed" && settings.fixed_source_id.as_ref() != Some(&source.id))
            || account_remaining(runtime, &runtimes).is_some_and(|remaining| remaining <= 0.0)
        {
            continue;
        }
        let cooling = match runtime.health.as_str() {
            "ready" => 0,
            "cooldown" | "proxy_error" => match runtime.cooldown_until {
                Some(until) => until.saturating_sub(timestamp),
                None => continue,
            },
            _ => continue,
        };
        if cooling == 0 && state.affinity.capacity_available(session, source) {
            return RecoveryReadiness::Ready;
        }
        let seconds = cooling.max(1);
        retry_after = Some(retry_after.map_or(seconds, |previous| previous.min(seconds)));
    }
    retry_after.map_or(RecoveryReadiness::Blocked, RecoveryReadiness::Waiting)
}

/// Revalidate a source for a caller that already owns its capacity lease.
/// Anonymous leases have no reusable session key, so reserving the last slot
/// must not make this request reject its own source during preflight.
pub async fn source_is_eligible(state: &Arc<AppState>, source: &Source, model: &str) -> bool {
    candidate_sources(state, model, None, None, false)
        .await
        .iter()
        .any(|candidate| candidate.id == source.id && candidate.identity() == source.identity())
}

/// Safe metadata for a rejected route, indexed by the current source order.
/// Keep values on this explicit allowlist: source and request strings are private.
pub(crate) async fn rejection_diagnostics(
    state: &Arc<AppState>,
    model: &str,
    session: Option<&str>,
) -> Value {
    let settings = state.settings.read().await.clone();
    let runtimes = state.sources.read().await;
    let timestamp = now();
    Value::Array(
        runtimes
            .iter()
            .enumerate()
            .map(|(index, runtime)| {
                let source = &runtime.source;
                let health = match runtime.health.as_str() {
                    "ready" => "ready",
                    "cooldown" => "cooldown",
                    "proxy_error" => "proxy_error",
                    "exhausted" => "exhausted",
                    "auth_required" => "auth_required",
                    _ => "unknown",
                };
                json!({
                    "index":index,
                    "enabled":source.enabled,
                    "model_supported":model.is_empty() || source.models.is_empty()
                        || source.models.iter().any(|candidate| candidate == model),
                    "fixed_selected":settings.mode != "fixed"
                        || settings.fixed_source_id.as_ref() == Some(&source.id),
                    "quota_available":account_remaining(runtime, &runtimes)
                        .is_none_or(|remaining| remaining > 0.0),
                    "capacity_available":state.affinity.capacity_available(session, source),
                    "health":health,
                    "remaining_cooldown_seconds":runtime.cooldown_until
                        .map_or(0, |until| until.saturating_sub(timestamp))
                })
            })
            .collect(),
    )
}

async fn candidate_sources(
    state: &Arc<AppState>,
    model: &str,
    binding: Option<&str>,
    session: Option<&str>,
    check_capacity: bool,
) -> Vec<Source> {
    let settings = state.settings.read().await.clone();
    // Identity updates clear bindings while holding sources.write. Read both
    // under the same lock order so an old binding cannot select a new identity.
    let runtimes = state.sources.read().await;
    let bound_id = if let Some(binding) = binding {
        // Unknown server state must never be guessed onto an unrelated source.
        match state.bindings.lock().await.get(binding).cloned() {
            Some(id) => Some(id),
            None => return Vec::new(),
        }
    } else {
        None
    };
    let timestamp = now();
    let mut sources: Vec<_> = runtimes
        .iter()
        .filter(|runtime| {
            let source = &runtime.source;
            source.enabled
                && (model.is_empty()
                    || source.models.is_empty()
                    || source.models.iter().any(|m| m == model))
                && bound_id.as_ref().is_none_or(|id| id == &source.id)
                && (settings.mode != "fixed"
                    || settings.fixed_source_id.as_ref() == Some(&source.id))
                && account_remaining(runtime, &runtimes).is_none_or(|percent| percent > 0.0)
                && (!check_capacity || state.affinity.capacity_available(session, source))
                && match runtime.health.as_str() {
                    "ready" => true,
                    "cooldown" | "proxy_error" | "exhausted" => runtime
                        .cooldown_until
                        .is_some_and(|until| until <= timestamp),
                    _ => false,
                }
        })
        .map(|r| r.source.clone())
        .collect();
    sources.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.id.cmp(&b.id)));
    if settings.load_balance && settings.mode == "auto" && binding.is_none() {
        state.affinity.order_by_load(&mut sources);
    }
    if binding.is_none() {
        if let Some(preferred) = session.and_then(|key| state.affinity.preferred(key, &sources)) {
            sources.sort_by_key(|source| source.id != preferred);
        }
    }
    sources
}

/// Main account windows only. Additional model-specific pools must not disable
/// unrelated models. Unknown does not mean full or exhausted; known stale zero
/// remains blocked until an actual refresh confirms recovery.
pub fn remaining(runtime: &crate::model::SourceRuntime) -> Option<f64> {
    if runtime.source.kind != "official" {
        return None;
    }
    runtime.quota["windows"]
        .as_array()?
        .iter()
        .filter(|window| matches!(window["name"].as_str(), Some("primary" | "secondary")))
        .filter_map(|window| {
            window["used_percent"]
                .as_f64()
                .filter(|used| used.is_finite() && *used >= 0.0)
                .map(|used| (100.0 - used).clamp(0.0, 100.0))
        })
        .min_by(f64::total_cmp)
}
pub fn account_remaining(
    runtime: &crate::model::SourceRuntime,
    sources: &[crate::model::SourceRuntime],
) -> Option<f64> {
    if runtime.source.kind != "official" {
        return None;
    }
    let identity = runtime.source.identity();
    ["primary", "secondary"]
        .iter()
        .filter_map(|name| {
            let mut latest: Option<(u64, f64)> = None;
            for candidate in sources.iter().filter(|candidate| {
                candidate.source.kind == "official" && candidate.source.identity() == identity
            }) {
                let Some(windows) = candidate.quota["windows"].as_array() else {
                    continue;
                };
                for window in windows.iter().filter(|window| window["name"] == *name) {
                    let fetched = window["observed_at"]
                        .as_u64()
                        .or_else(|| candidate.quota["fetched_at"].as_u64())
                        .unwrap_or(0);
                    let Some(used) = window["used_percent"]
                        .as_f64()
                        .filter(|used| used.is_finite() && *used >= 0.0)
                    else {
                        continue;
                    };
                    let remaining = (100.0 - used).clamp(0.0, 100.0);
                    if latest.is_none_or(|(timestamp, value)| {
                        fetched > timestamp || fetched == timestamp && remaining < value
                    }) {
                        latest = Some((fetched, remaining));
                    }
                }
            }
            latest.map(|(_, remaining)| remaining)
        })
        .min_by(f64::total_cmp)
}

pub async fn reserve(
    state: &Arc<AppState>,
    source: &Source,
    session: Option<&str>,
    continuation: bool,
) -> Option<crate::affinity::RouteLease> {
    reserve_for_model(state, source, session, continuation, "").await
}

/// A planned return is separate from normal affinity: merely considering a
/// recovered source must not discard the last confirmed route or its context.
pub async fn reserve_idle_return(
    state: &Arc<AppState>,
    model: &str,
    session: &str,
) -> Option<(Source, crate::affinity::RouteLease)> {
    let settings = state.settings.read().await.clone();
    if settings.mode != "auto" || settings.load_balance || settings.idle_return_minutes == 0 {
        return None;
    }
    let sources = state.sources.read().await;
    let all: Vec<_> = sources.iter().map(|r| r.source.clone()).collect();
    let preferred = state.affinity.preferred(session, &all)?;
    let from = all.iter().find(|source| source.id == preferred)?;
    let mut targets: Vec<_> = sources.iter().filter(|runtime| {
        let source = &runtime.source;
        source.enabled && source.priority < from.priority
            && (model.is_empty() || source.models.is_empty() || source.models.iter().any(|m| m == model))
            && (runtime.health == "ready" || matches!(runtime.health.as_str(), "cooldown" | "proxy_error" | "exhausted")
                && runtime.cooldown_until.is_some_and(|until| until <= now()))
            // Unlike first-time routing, reclaiming a working route requires
            // positive evidence of account recovery, including after restart.
            && (source.kind != "official" || account_remaining(runtime, &sources).is_some_and(|v| v > 0.0))
    }).collect();
    targets.sort_by(|a, b| {
        a.source
            .priority
            .cmp(&b.source.priority)
            .then(a.source.id.cmp(&b.source.id))
    });
    for target in targets {
        let low = account_remaining(target, &sources)
            .is_some_and(|v| v <= f64::from(settings.official_low_quota_percent));
        if let Some(lease) = state.affinity.reserve_idle_return(
            session,
            from,
            &target.source,
            low,
            u64::from(settings.idle_return_minutes) * 60,
        ) {
            return Some((target.source.clone(), lease));
        }
    }
    None
}
pub async fn reserve_for_model(
    state: &Arc<AppState>,
    source: &Source,
    session: Option<&str>,
    continuation: bool,
    model: &str,
) -> Option<crate::affinity::RouteLease> {
    reserve_model_with_policy(state, source, session, continuation, model, false).await
}

/// Used for a reconstructed context or probing an unbound upstream ID after
/// explicit rejection. The target still admits a new conversation; this authorizes leaving
/// the prior soft affinity without granting low-quota continuation privileges.
pub async fn reserve_context_handoff_for_model(
    state: &Arc<AppState>,
    source: &Source,
    session: Option<&str>,
    model: &str,
) -> Option<crate::affinity::RouteLease> {
    reserve_model_with_policy(state, source, session, false, model, true).await
}

async fn reserve_model_with_policy(
    state: &Arc<AppState>,
    source: &Source,
    session: Option<&str>,
    continuation: bool,
    model: &str,
    allow_handoff: bool,
) -> Option<crate::affinity::RouteLease> {
    let settings = state.settings.read().await.clone();
    let sources = state.sources.read().await;
    let runtime = sources.iter().find(|runtime| {
        runtime.source.id == source.id
            && runtime.source.identity() == source.identity()
            && runtime.source.enabled
            && (model.is_empty()
                || runtime.source.models.is_empty()
                || runtime.source.models.iter().any(|m| m == model))
    })?;
    let eligible: Vec<_> = sources
        .iter()
        .filter(|runtime| {
            runtime.source.enabled
                && (model.is_empty()
                    || runtime.source.models.is_empty()
                    || runtime.source.models.iter().any(|m| m == model))
                && account_remaining(runtime, &sources).is_none_or(|v| v > 0.0)
                && (runtime.health == "ready"
                    || matches!(
                        runtime.health.as_str(),
                        "cooldown" | "proxy_error" | "exhausted"
                    ) && runtime.cooldown_until.is_some_and(|until| until <= now()))
        })
        .map(|runtime| runtime.source.clone())
        .collect();
    if settings.mode == "fixed" && settings.fixed_source_id.as_ref() != Some(&source.id) {
        return None;
    }
    if !matches!(runtime.health.as_str(), "ready")
        && !matches!(
            runtime.health.as_str(),
            "cooldown" | "proxy_error" | "exhausted"
        )
    {
        return None;
    }
    if runtime.health != "ready" && !runtime.cooldown_until.is_some_and(|until| until <= now()) {
        return None;
    }
    let remaining = account_remaining(runtime, &sources);
    if remaining.is_some_and(|value| value <= 0.0) {
        return None;
    }
    let low =
        remaining.is_some_and(|value| value <= f64::from(settings.official_low_quota_percent));
    state.affinity.reserve(
        session,
        &runtime.source,
        continuation,
        low,
        if !continuation && !allow_handoff && settings.mode == "auto" {
            Some(&eligible)
        } else {
            None
        },
    )
}

pub fn admission(
    state: &Arc<AppState>,
    runtime: &crate::model::SourceRuntime,
    sources: &[crate::model::SourceRuntime],
    threshold: u8,
) -> Value {
    let active = state.affinity.active_count(&runtime.source);
    let protected_active = state.affinity.protected_active_count(&runtime.source);
    let (capacity_active, capacity_available) = state.affinity.capacity_status(&runtime.source);
    let remaining = account_remaining(runtime, sources);
    let note = if !runtime.source.enabled {
        "已停用"
    } else if remaining.is_some_and(|value| value <= 0.0) {
        "额度耗尽"
    } else if runtime.health == "auth_required" {
        "需要登录"
    } else if runtime.health != "ready"
        && !runtime.cooldown_until.is_some_and(|until| until <= now())
    {
        "暂停分配"
    } else if !capacity_available {
        "活跃对话已满"
    } else if remaining.is_some_and(|value| value <= f64::from(threshold)) && protected_active > 0 {
        "仅续接已有对话"
    } else {
        "可接新对话"
    };
    json!({"active_conversations":active,"account_active_conversations":protected_active,"capacity_active_conversations":capacity_active,"max_active_conversations":runtime.source.max_active_conversations,"activity_window_seconds":runtime.source.active_conversation_window_seconds,"capacity_available":capacity_available,"remaining_percent":remaining,"accepting_new_conversations":note=="可接新对话","routing_note":note})
}

pub async fn mark_failure(
    state: &Arc<AppState>,
    source_id: &str,
    status: u16,
    retry_after: Option<u64>,
) {
    mark_failure_reason(state, source_id, status, retry_after, None).await;
}

pub async fn mark_transport_failure(
    state: &Arc<AppState>,
    source_id: &str,
    error: &reqwest::Error,
) {
    mark_failure_reason(
        state,
        source_id,
        0,
        None,
        Some(crate::transport::failure_reason(error)),
    )
    .await;
}

async fn mark_failure_reason(
    state: &Arc<AppState>,
    source_id: &str,
    status: u16,
    retry_after: Option<u64>,
    reason: Option<&str>,
) {
    if let Some(runtime) = state
        .sources
        .write()
        .await
        .iter_mut()
        .find(|r| r.source.id == source_id)
    {
        let (health, message, delay) = match status {
            401 => ("auth_required", "来源认证已失效", None),
            402 => ("cooldown", "来源拒绝计费请求，原因待核实", Some(30)),
            403 => ("cooldown", "来源拒绝访问，请检查权限", Some(30)),
            429 => (
                "cooldown",
                "来源限流",
                Some(retry_after.unwrap_or(30).clamp(1, 86400)),
            ),
            0 if runtime.source.proxy.mode == "explicit" => {
                ("proxy_error", "代理连接失败", Some(15))
            }
            _ => ("cooldown", "来源暂时不可用", Some(15)),
        };
        runtime.health = health.into();
        runtime.health_revision = runtime.health_revision.wrapping_add(1);
        runtime.last_error = Some(reason.unwrap_or(message).into());
        runtime.cooldown_until = delay.map(|seconds| now().saturating_add(seconds));
    }
}

pub async fn mark_auth_preflight_failure(state: &Arc<AppState>, source: &Source) {
    let mut sources = state.sources.write().await;
    if let Some(runtime) = sources.iter_mut().find(|runtime| {
        runtime.source.id == source.id
            && runtime.source.identity() == source.identity()
            && matches!(
                runtime.health.as_str(),
                "ready" | "cooldown" | "proxy_error" | "exhausted"
            )
    }) {
        runtime.health = "proxy_error".into();
        runtime.health_revision = runtime.health_revision.wrapping_add(1);
        runtime.last_error = Some("官方认证查询暂不可用".into());
        runtime.cooldown_until = Some(now() + 15);
    }
}

pub async fn mark_ready(state: &Arc<AppState>, source_id: &str) {
    if let Some(runtime) = state
        .sources
        .write()
        .await
        .iter_mut()
        .find(|r| r.source.id == source_id)
    {
        runtime.health = "ready".into();
        runtime.health_revision = runtime.health_revision.wrapping_add(1);
        runtime.cooldown_until = None;
        runtime.last_error = None;
    }
}

/// Only recognized upstream error codes can establish quota exhaustion.
pub async fn mark_rejection(
    state: &Arc<AppState>,
    source_id: &str,
    code: Option<&str>,
    reset_after: Option<u64>,
) {
    let exhausted = matches!(
        code,
        Some(
            "insufficient_quota"
                | "billing_hard_limit_reached"
                | "usage_limit_reached"
                | "quota_exceeded"
        )
    );
    let authentication = matches!(
        code,
        Some("invalid_api_key" | "token_expired" | "account_deactivated" | "authentication_error")
    );
    if exhausted || authentication {
        if let Some(runtime) = state
            .sources
            .write()
            .await
            .iter_mut()
            .find(|r| r.source.id == source_id)
        {
            runtime.health = if exhausted {
                "exhausted"
            } else {
                "auth_required"
            }
            .into();
            runtime.health_revision = runtime.health_revision.wrapping_add(1);
            runtime.last_error = Some(
                if exhausted {
                    "来源额度已耗尽"
                } else {
                    "来源认证已失效"
                }
                .into(),
            );
            runtime.cooldown_until = if exhausted {
                reset_after.map(|delay| now().saturating_add(delay))
            } else {
                None
            };
            if exhausted && runtime.source.kind == "official" {
                state
                    .notifications
                    .quota(&runtime.source.identity(), &runtime.source.name, true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceRuntime;

    fn recovery_source() -> Source {
        serde_json::from_value(json!({
            "id":"recovery","name":"Fixture","kind":"api",
            "base_url":"http://127.0.0.1:1/v1","enabled":true,"priority":0,
            "proxy":{"mode":"direct"},"secret":{"api_key":"fixture"}
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn recovery_wait_rechecks_capacity_without_holding_state_locks() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(directory.path().to_owned(), 0, "fixture".into()).unwrap();
        let mut source = recovery_source();
        source.max_active_conversations = Some(1);
        let lease = state
            .affinity
            .reserve(Some("occupied"), &source, false, false, None)
            .unwrap();
        *state.sources.write().await = vec![SourceRuntime::new(source)];
        let copy = state.clone();
        let update = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            let _sources = copy.sources.write().await;
            drop(lease);
        });
        assert_eq!(
            wait_for_recovery(
                &state,
                "m",
                None,
                Some("new"),
                None,
                tokio::time::Instant::now() + std::time::Duration::from_secs(1)
            )
            .await,
            RecoveryWait::Ready
        );
        update.await.unwrap();
    }

    #[tokio::test]
    async fn recovery_wait_respects_terminal_policy_and_bounds_cooldown_and_lock_waits() {
        for case in [
            "disabled",
            "model",
            "fixed",
            "auth",
            "quota",
            "exhausted",
            "binding",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let state = AppState::new(directory.path().to_owned(), 0, "fixture".into()).unwrap();
            let mut runtime = SourceRuntime::new(recovery_source());
            runtime.health = "cooldown".into();
            runtime.cooldown_until = Some(now() + 60);
            match case {
                "disabled" => runtime.source.enabled = false,
                "model" => runtime.source.models = vec!["different".into()],
                "fixed" => {
                    let mut settings = state.settings.write().await;
                    settings.mode = "fixed".into();
                    settings.fixed_source_id = Some("another".into());
                }
                "auth" => runtime.health = "auth_required".into(),
                "quota" => {
                    runtime.source.kind = "official".into();
                    runtime.quota = json!({"windows":[{"name":"primary","used_percent":100}]});
                }
                "exhausted" => runtime.health = "exhausted".into(),
                _ => {}
            }
            *state.sources.write().await = vec![runtime];
            assert_eq!(
                wait_for_recovery(
                    &state,
                    "m",
                    (case == "binding").then_some("unknown"),
                    None,
                    None,
                    tokio::time::Instant::now() + std::time::Duration::from_millis(60)
                )
                .await,
                RecoveryWait::Unavailable,
                "{case} must not be bypassed by recovery"
            );
        }
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(directory.path().to_owned(), 0, "fixture".into()).unwrap();
        let mut runtime = SourceRuntime::new(recovery_source());
        runtime.health = "proxy_error".into();
        runtime.cooldown_until = Some(now() + 60);
        *state.sources.write().await = vec![runtime];
        for duration in [0, 50] {
            assert!(matches!(
                wait_for_recovery(
                    &state,
                    "m",
                    None,
                    None,
                    None,
                    tokio::time::Instant::now() + std::time::Duration::from_millis(duration)
                )
                .await,
                RecoveryWait::TimedOut { retry_after: 1.. }
            ));
        }
        let _locked = state.sources.write().await;
        let started = tokio::time::Instant::now();
        assert_eq!(
            wait_for_recovery(
                &state,
                "m",
                None,
                None,
                None,
                started + std::time::Duration::from_millis(50)
            )
            .await,
            RecoveryWait::TimedOut { retry_after: 1 }
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[tokio::test]
    async fn rejection_diagnostics_exposes_reasons_without_private_source_data() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(
            directory.path().to_owned(),
            0,
            "private-gateway-key-marker".into(),
        )
        .unwrap();
        let source = |index: usize| -> Source {
            serde_json::from_value(json!({
                "id":format!("private-source-marker-{index}"),
                "name":format!("private-name-marker-{index}"),
                "kind":"api",
                "base_url":format!("https://private-url-marker-{index}.invalid/v1"),
                "enabled":true,
                "priority":index,
                "proxy":{"mode":"direct"},
                "secret":{"api_key":"private-api-key-marker"}
            }))
            .unwrap()
        };
        let mut blocked = SourceRuntime::new(source(0));
        blocked.source.enabled = false;
        blocked.source.models = vec!["private-model-marker".into()];
        blocked.source.max_active_conversations = Some(1);
        blocked.health = "cooldown".into();
        blocked.cooldown_until = Some(now() + 300);
        blocked.last_error = Some("private-error-marker".into());
        let _lease = state
            .affinity
            .reserve(
                Some("private-occupied-session-marker"),
                &blocked.source,
                false,
                false,
                None,
            )
            .unwrap();
        let mut exhausted = SourceRuntime::new(source(1));
        exhausted.source.kind = "official".into();
        exhausted.source.base_url = "https://chatgpt.com/backend-api/codex".into();
        exhausted.source.secret = json!({"access_token":"private-access-token-marker"});
        exhausted.health = "exhausted".into();
        exhausted.quota = json!({"windows":[{"name":"primary","used_percent":100}]});
        let mut unknown = SourceRuntime::new(source(2));
        unknown.health = "private-health-marker".into();
        unknown.cooldown_until = Some(now().saturating_sub(1));
        let identities = [
            blocked.source.identity(),
            exhausted.source.identity(),
            unknown.source.identity(),
        ];
        {
            let mut settings = state.settings.write().await;
            settings.mode = "fixed".into();
            settings.fixed_source_id = Some(exhausted.source.id.clone());
        }
        *state.sources.write().await = vec![blocked, exhausted, unknown];

        // Diagnosis must not depend on a binding lookup, even if one is busy.
        let _bindings = state.bindings.lock().await;
        let snapshot = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            rejection_diagnostics(
                &state,
                "private-request-model-marker",
                Some("private-request-session-marker"),
            ),
        )
        .await
        .unwrap();
        let rows = snapshot.as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["index"], 0);
        assert_eq!(rows[0]["enabled"], false);
        assert_eq!(rows[0]["model_supported"], false);
        assert_eq!(rows[0]["fixed_selected"], false);
        assert_eq!(rows[0]["quota_available"], true);
        assert_eq!(rows[0]["capacity_available"], false);
        assert_eq!(rows[0]["health"], "cooldown");
        assert!((1..=300).contains(&rows[0]["remaining_cooldown_seconds"].as_u64().unwrap()));
        assert_eq!(
            rows[1],
            json!({
                "index":1,"enabled":true,"model_supported":true,"fixed_selected":true,
                "quota_available":false,"capacity_available":true,"health":"exhausted",
                "remaining_cooldown_seconds":0
            })
        );
        assert_eq!(rows[2]["health"], "unknown");
        assert_eq!(rows[2]["remaining_cooldown_seconds"], 0);
        let serialized = snapshot.to_string();
        for marker in [
            "private-",
            "https://",
            "api_key",
            "access_token",
            "account_id",
        ] {
            assert!(!serialized.contains(marker), "diagnostic leaked {marker}");
        }
        for identity in identities {
            assert!(!serialized.contains(&identity));
        }
        for row in rows {
            assert_eq!(row.as_object().unwrap().len(), 8);
        }
    }
}
