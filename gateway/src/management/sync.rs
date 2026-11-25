use crate::model::{AppState, RoutingSettings, Source, SourceRuntime};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::{atomic::Ordering, Arc},
};

struct SyncRequest {
    sources: Vec<Source>,
    ids: HashSet<String>,
    settings: RoutingSettings,
    model_compaction: Vec<crate::compaction::ModelCompactionRule>,
    model_policies: Vec<crate::model_policy::ModelPolicy>,
}

impl SyncRequest {
    fn parse(payload: &Value) -> Result<Self> {
        let sources: Vec<Source> =
            serde_json::from_value(payload.get("sources").cloned().context("缺少来源列表")?)
                .context("来源配置格式错误")?;
        let mode = payload
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("auto");
        let fixed = payload
            .get("fixed_source_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let threshold = match payload.get("official_low_quota_percent") {
            None => 10,
            Some(value) => value
                .as_u64()
                .filter(|v| (1..=99).contains(v))
                .ok_or_else(|| anyhow::anyhow!("官方余量阈值须为1–99的整数"))?
                as u8,
        };
        let degraded_context_fallback = payload
            .get("degraded_context_fallback")
            .map(|v| v.as_bool().context("上下文兜底须为布尔值"))
            .transpose()?
            .unwrap_or(true);
        let load_balance = payload
            .get("load_balance")
            .map(|v| v.as_bool().context("负载均衡须为布尔值"))
            .transpose()?
            .unwrap_or(false);
        let mut auto_reset_weekly_accounts: Vec<String> = payload
            .get("auto_reset_weekly_accounts")
            .map(|v| serde_json::from_value(v.clone()).context("自动使用重置卡须为官方账号列表"))
            .transpose()?
            .unwrap_or_default();
        if auto_reset_weekly_accounts.iter().any(|id| id.is_empty() || id.len() > 256 || id.trim() != id) {
            bail!("自动使用重置卡的官方账号标识无效");
        }
        auto_reset_weekly_accounts.sort_unstable();
        auto_reset_weekly_accounts.dedup();
        let idle_return_minutes = match payload.get("idle_return_minutes") {
            None => 5,
            Some(value) => value
                .as_u64()
                .filter(|v| *v <= 10080)
                .context("闲置回切时间须为 0–10080 分钟的整数")? as u32,
        };
        let upstream_attempts = match payload.get("upstream_attempts") {
            None => 3,
            Some(value) => value
                .as_u64()
                .filter(|v| (1..=10).contains(v))
                .context("单个来源尝试次数须为 1–10 的整数")? as u8,
        };
        let upstream_retry_delay_seconds = match payload.get("upstream_retry_delay_seconds") {
            None => 3,
            Some(value) => value
                .as_u64()
                .filter(|v| *v <= 300)
                .context("重试等待时间须为 0–300 秒的整数")?,
        };
        let model_compaction = serde_json::from_value::<Vec<crate::compaction::ModelCompactionRule>>(
            payload.get("model_compaction").cloned().unwrap_or_else(|| serde_json::json!([])),
        ).context("模型压缩规则格式错误")?;
        crate::compaction::validate(&model_compaction)?;
        let model_policies = serde_json::from_value::<Vec<crate::model_policy::ModelPolicy>>(
            payload.get("model_policies").cloned().unwrap_or_else(|| serde_json::json!([])),
        ).context("模型策略格式错误")?;
        crate::model_policy::validate(&model_policies)?;
        if !["auto", "fixed"].contains(&mode) {
            bail!("未知路由模式");
        }
        if sources.len() > 200 {
            bail!("最多支持 200 个来源");
        }
        let mut ids = HashSet::new();
        for source in &sources {
            // A local source can outlive its private credential. Only the exact
            // empty object denotes this state; malformed credentials still fail.
            if source.secret.as_object().is_some_and(|secret| secret.is_empty()) {
                source.validate_configuration()?;
            } else {
                source.validate()?;
            }
            if !ids.insert(source.id.clone()) {
                bail!("来源标识不能重复");
            }
        }
        if mode == "fixed"
            && !sources
                .iter()
                .any(|s| Some(&s.id) == fixed.as_ref() && s.enabled)
        {
            bail!("请选择已启用的固定来源");
        }
        Ok(Self {
            sources,
            ids,
            model_compaction,
            model_policies,
            settings: RoutingSettings {
                strict_priority: true,
                mode: mode.into(),
                fixed_source_id: fixed,
                load_balance,
                official_low_quota_percent: threshold,
                auto_reset_weekly_accounts,
                degraded_context_fallback,
                idle_return_minutes,
                upstream_attempts,
                upstream_retry_delay_seconds,
            },
        })
    }
}

fn prepare_sources(
    sources: Vec<Source>,
    current: &[SourceRuntime],
) -> (Vec<SourceRuntime>, HashSet<String>) {
    let mut next = Vec::new();
    let mut invalidated = HashSet::new();
    for mut source in sources {
        if let Some(previous) = current.iter().find(|r| r.source.id == source.id) {
            if source.kind == "official" && previous.source.kind == "official"
                && source.base_url == previous.source.base_url && source.proxy == previous.source.proxy
            {
                // Refresh generations belong to this credential destination, not just the source ID.
                if previous
                    .source
                    .secret
                    .get("access_token")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
                {
                    source.secret = previous.source.secret.clone();
                    source.account_id = previous.source.account_id.clone();
                }
            }
            if source.secret == previous.source.secret
                && source.proxy == previous.source.proxy
                && source.base_url == previous.source.base_url
                && source.kind == previous.source.kind
            {
                let mut kept = previous.clone();
                kept.source = source;
                next.push(kept);
                continue;
            }
            if source.base_url != previous.source.base_url
                || source.kind != previous.source.kind
                || source.secret != previous.source.secret
                || source.account_id != previous.source.account_id
            {
                invalidated.insert(source.id.clone());
            }
        }
        let mut runtime = SourceRuntime::new(source);
        if runtime.source.kind == "official" {
            let identity = runtime.source.identity();
            if let Some(previous) = current
                .iter()
                .filter(|r| r.source.identity() == identity)
                .max_by_key(|r| r.quota["fetched_at"].as_u64().unwrap_or(0))
            {
                runtime.quota = crate::routing::account_quota_snapshot(previous, current);
                runtime.credits = previous.credits.clone();
                runtime.quota_exhausted = crate::routing::official_exhausted(previous, current);
                if runtime.quota_exhausted {
                    runtime.health = "exhausted".into();
                    runtime.last_error = Some("官方额度已耗尽，等待额度恢复".into());
                }
            }
        }
        next.push(runtime);
    }
    (next, invalidated)
}

fn validate_official_accounts(sources: &[SourceRuntime]) -> Result<()> {
    let mut accounts = HashSet::new();
    for source in sources
        .iter()
        .map(|runtime| &runtime.source)
        .filter(|source| source.kind == "official")
    {
        source.validate_official_account()?;
        if let Some(account) = source.official_account_id() {
            if !accounts.insert(account) {
                bail!("此官方账号已存在，请使用已有来源");
            }
        } else if ["access_token", "refresh_token"].iter().any(|key| {
            source.secret[*key]
                .as_str()
                .is_some_and(|token| !token.is_empty())
        }) {
            bail!("官方凭据缺少账号身份，请重新登录");
        }
    }
    Ok(())
}

pub(super) async fn handle(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    let _configuration = state.config_lock.lock().await;
    let _routing_sync = state.routing_sync.lock().await;
    let SyncRequest {
        sources,
        ids,
        settings,
        model_compaction,
        model_policies,
    } = SyncRequest::parse(&payload)?;
    let mut current = state.sources.write().await;
    let (next, invalidated) = prepare_sources(sources, &current);
    for runtime in &next {
        if runtime.source.secret["api_key"]
            .as_str()
            .is_some_and(|key| state.control.uses_key(key).unwrap_or(true))
        {
            bail!("来源不能复用管理 API 密钥");
        }
    }
    // Refresh credentials are helper-owned; validate the effective accounts
    // before affinity, billing or routing state can change.
    validate_official_accounts(&next)?;
    state.affinity.retain_identities(
        &next
            .iter()
            .map(|runtime| runtime.source.clone())
            .collect::<Vec<_>>(),
    )?;
    let mut pricing_sources = next
        .iter()
        .map(|runtime| {
            (
                runtime.source.id.clone(),
                runtime.source.kind.clone(),
                runtime.source.is_subscription,
            )
        })
        .collect::<Vec<_>>();
    pricing_sources.sort();
    let should_reprice =
        *state.last_pricing_sources.lock().await != Some(pricing_sources.clone());
    if should_reprice {
        let usage = state.usage.clone();
        let pricing = pricing_sources.clone();
        tokio::task::spawn_blocking(move || usage.reprice_historical(&pricing))
            .await
            .context("历史费用更新失败")??;
        *state.last_pricing_sources.lock().await = Some(pricing_sources);
    }
    let model_compaction_revision = crate::model_catalog::configuration_revision(
        &model_compaction, &next, &model_policies, &settings.mode, settings.fixed_source_id.as_deref(),
    );
    // Odd revisions describe publication in progress. Readers must not
    // submit using a mixture of old routing settings and new sources.
    state.routing_revision.fetch_add(1, Ordering::AcqRel);
    *current = next;
    state.manual_routing.retain_sources(&current);
    // Keep the source write lock through binding invalidation. Relay
    // readers and binding writers use sources -> bindings consistently.
    state
        .bindings
        .lock()
        .await
        .retain(|_, source| ids.contains(source) && !invalidated.contains(source));
    drop(current);
    *state.settings.write().await = settings;
    *state.model_compaction.write().await = model_compaction.into();
    *state.model_compaction_revision.write().await = model_compaction_revision;
    *state.model_policies.write().await = model_policies.into();
    state.routing_revision.fetch_add(1, Ordering::Release);
    Ok(super::credentials(state).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn weekly_reset_sync_never_enrolls_accounts_from_the_legacy_switch() {
        let legacy = SyncRequest::parse(&json!({"sources":[],"auto_reset_weekly_quota":true})).unwrap();
        assert!(legacy.settings.auto_reset_weekly_accounts.is_empty());
        let enrolled = SyncRequest::parse(&json!({"sources":[],"auto_reset_weekly_accounts":["account-a","account-a"]})).unwrap();
        assert_eq!(enrolled.settings.auto_reset_weekly_accounts, vec!["account-a"]);
        assert!(SyncRequest::parse(&json!({"sources":[],"auto_reset_weekly_accounts":true})).is_err());
    }

    #[tokio::test]
    async fn subscription_change_reprices_historical_usage() {
        let dir = tempfile::tempdir().unwrap();
        {
            drop(crate::usage::UsageStore::open(&dir.path().join("usage.sqlite")).unwrap());
            let db = rusqlite::Connection::open(dir.path().join("usage.sqlite")).unwrap();
            db.execute(
                "INSERT INTO usage (request_id,source_id,source_name,model,started_at,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms,metadata) VALUES ('legacy-sync','api-1','API 1','gpt-6.1-sol',?1,'success',512000,100,0,1,20,'{}')",
                [crate::model::now()],
            )
            .unwrap();
        }
        let state = AppState::new(dir.path().to_path_buf(), 0, "gateway-key".into()).unwrap();
        let payload = |is_subscription: bool| {
            json!({
                "sources": [{
                    "id": "api-1",
                    "name": "API 1",
                    "kind": "api",
                    "is_subscription": is_subscription,
                    "base_url": "https://example.com/v1",
                    "enabled": true,
                    "priority": 0,
                    "proxy": { "mode": "direct" },
                    "secret": { "api_key": "fixture-secret" }
                }]
            })
        };

        handle(&state, payload(false)).await.unwrap();
        let initial = state.usage.summary_filtered(1, Some("api-1"), None).unwrap();
        assert_eq!(initial["recent"][0]["cost"]["rates"]["input"], 4.0);

        handle(&state, payload(true)).await.unwrap();
        let subscribed = state.usage.summary_filtered(1, Some("api-1"), None).unwrap();
        assert_eq!(subscribed["recent"][0]["cost"]["long_context_waived"], true);
        assert_eq!(subscribed["recent"][0]["cost"]["rates"]["input"], 2.0);
    }
}
