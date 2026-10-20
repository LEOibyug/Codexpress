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
pub async fn reserve_for_model(
    state: &Arc<AppState>,
    source: &Source,
    session: Option<&str>,
    continuation: bool,
    model: &str,
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
        source,
        continuation,
        low,
        if !continuation && settings.mode == "auto" {
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
    } else if remaining.is_some_and(|value| value <= f64::from(threshold)) && protected_active > 0 {
        "仅续接已有对话"
    } else {
        "可接新对话"
    };
    json!({"active_conversations":active,"account_active_conversations":protected_active,"remaining_percent":remaining,"accepting_new_conversations":note=="可接新对话","routing_note":note})
}

pub async fn mark_failure(
    state: &Arc<AppState>,
    source_id: &str,
    status: u16,
    retry_after: Option<u64>,
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
        runtime.last_error = Some(message.into());
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
