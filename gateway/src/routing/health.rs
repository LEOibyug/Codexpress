//! Health updates belong to the exact upstream and network policy attempted.
use super::official_exhausted;
use crate::model::{now, AppState, Source};
use std::sync::Arc;

fn same_target(current: &Source, attempted: &Source) -> bool {
    current.id == attempted.id
        && current.proxy == attempted.proxy
        && current.identity() == attempted.identity()
}

pub async fn mark_failure(
    state: &Arc<AppState>,
    source_id: &str,
    status: u16,
    retry_after: Option<u64>,
) {
    mark_failure_reason(state, source_id, status, retry_after, None).await;
}

pub(crate) async fn mark_upstream_failure(
    state: &Arc<AppState>,
    source: &Source,
    status: u16,
    retry_after: Option<u64>,
    upstream_errors: u8,
) {
    if status == 401 || upstream_errors >= 3 {
        mark_failure_reason(state, &source.id, status, retry_after, Some(source)).await;
    }
}

async fn mark_failure_reason(
    state: &Arc<AppState>,
    source_id: &str,
    status: u16,
    retry_after: Option<u64>,
    attempted: Option<&Source>,
) {
    if let Some(runtime) = state.sources.write().await.iter_mut().find(|r| {
        r.source.id == source_id && attempted.is_none_or(|source| same_target(&r.source, source))
    }) {
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
        if runtime.source.kind == "official" && runtime.health == "exhausted" {
            runtime.quota_exhausted = true;
        }
        runtime.health = health.into();
        runtime.health_revision = runtime.health_revision.wrapping_add(1);
        runtime.last_error = Some(message.into());
        runtime.cooldown_until = delay.map(|seconds| now().saturating_add(seconds));
    }
}

pub async fn mark_auth_preflight_failure(state: &Arc<AppState>, source: &Source) {
    let mut sources = state.sources.write().await;
    if let Some(runtime) = sources.iter_mut().find(|runtime| {
        same_target(&runtime.source, source)
            && matches!(
                runtime.health.as_str(),
                "ready" | "cooldown" | "proxy_error" | "exhausted"
            )
    }) {
        if runtime.health == "exhausted" {
            runtime.quota_exhausted = true;
        }
        runtime.health_revision = runtime.health_revision.wrapping_add(1);
        runtime.last_error = Some("官方认证查询暂不可用".into());
    }
}

pub async fn mark_ready(state: &Arc<AppState>, source: &Source) {
    let mut sources = state.sources.write().await;
    let Some(index) = sources.iter().position(|r| same_target(&r.source, source)) else {
        return;
    };
    if official_exhausted(&sources[index], &sources) {
        return;
    }
    {
        let runtime = &mut sources[index];
        runtime.health = "ready".into();
        runtime.health_revision = runtime.health_revision.wrapping_add(1);
        runtime.cooldown_until = None;
        runtime.last_error = None;
    }
}

/// Only recognized upstream error codes can establish quota exhaustion.
pub async fn mark_rejection(
    state: &Arc<AppState>,
    source: &Source,
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
            .find(|r| same_target(&r.source, source))
        {
            runtime.health = if exhausted {
                "exhausted"
            } else {
                "auth_required"
            }
            .into();
            runtime.health_revision = runtime.health_revision.wrapping_add(1);
            if exhausted && runtime.source.kind == "official" {
                runtime.quota_exhausted = true;
            }
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
    use serde_json::json;

    #[tokio::test]
    async fn stale_upstream_results_cannot_change_a_replaced_source() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(directory.path().into(), 0, "fixture".into()).unwrap();
        let original: Source = serde_json::from_value(json!({
            "id":"api","name":"Fixture","kind":"api","enabled":true,"priority":0,
            "base_url":"http://127.0.0.1:1/v1","proxy":{"mode":"direct"},
            "secret":{"api_key":"fixture-old"}
        }))
        .unwrap();
        for change in ["key", "url", "proxy"] {
            let mut current = original.clone();
            match change {
                "key" => current.secret = json!({"api_key":"fixture-new"}),
                "url" => current.base_url = "http://127.0.0.1:2/v1".into(),
                _ => {
                    current.proxy.mode = "explicit".into();
                    current.proxy.url = Some("http://127.0.0.1:3".into());
                }
            }
            *state.sources.write().await = vec![SourceRuntime::new(current.clone())];
            mark_rejection(&state, &original, Some("insufficient_quota"), None).await;
            mark_upstream_failure(&state, &original, 0, None, 3).await;
            mark_auth_preflight_failure(&state, &original).await;
            assert_eq!(
                state.sources.read().await[0].health_revision,
                0,
                "stale {change} failure"
            );

            mark_rejection(&state, &current, Some("invalid_api_key"), None).await;
            mark_ready(&state, &original).await;
            assert_eq!(
                state.sources.read().await[0].health,
                "auth_required",
                "stale {change} success"
            );
            mark_ready(&state, &current).await;
            assert_eq!(state.sources.read().await[0].health, "ready");
        }
        let mut official = original;
        official.kind = "official".into();
        official.base_url = "https://chatgpt.com/backend-api/codex".into();
        official.proxy.mode = "explicit".into();
        official.proxy.url = Some("http://127.0.0.1:3".into());
        official.account_id = Some("fixture-account".into());
        official.secret = json!({"access_token":"old-token"});
        let mut refreshed = official.clone();
        refreshed.secret = json!({"access_token":"new-token"});
        *state.sources.write().await = vec![SourceRuntime::new(refreshed)];
        mark_rejection(&state, &official, Some("usage_limit_reached"), None).await;
        assert!(state.sources.read().await[0].quota_exhausted);
    }
}
