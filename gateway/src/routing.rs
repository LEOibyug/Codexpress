use crate::model::{now, AppState, Source};
use std::sync::Arc;

pub async fn candidates(state: &Arc<AppState>, model: &str, binding: Option<&str>) -> Vec<Source> {
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
    sources
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
        }
    }
}
