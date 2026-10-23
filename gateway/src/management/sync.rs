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
        if !["auto", "fixed"].contains(&mode) {
            bail!("未知路由模式");
        }
        if sources.len() > 200 {
            bail!("最多支持 200 个来源");
        }
        let mut ids = HashSet::new();
        for source in &sources {
            source.validate()?;
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
            settings: RoutingSettings {
                strict_priority: true,
                mode: mode.into(),
                fixed_source_id: fixed,
                load_balance,
                official_low_quota_percent: threshold,
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
            if source.kind == "official" && previous.source.kind == "official" {
                // The helper owns the refresh generation for this process. Never
                // overwrite a newer refresh token with the UI's stale snapshot.
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

pub(super) async fn handle(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    let _routing_sync = state.routing_sync.lock().await;
    let SyncRequest {
        sources,
        ids,
        settings,
    } = SyncRequest::parse(&payload)?;
    let mut current = state.sources.write().await;
    let (next, invalidated) = prepare_sources(sources, &current);
    state.affinity.retain_identities(
        &next
            .iter()
            .map(|runtime| runtime.source.clone())
            .collect::<Vec<_>>(),
    )?;
    let pricing_sources = next
        .iter()
        .map(|runtime| {
            (
                runtime.source.id.clone(),
                runtime.source.kind.clone(),
                runtime.source.is_subscription,
            )
        })
        .collect::<Vec<_>>();
    let usage = state.usage.clone();
    tokio::task::spawn_blocking(move || usage.reprice_historical(&pricing_sources))
        .await
        .context("历史费用更新失败")??;
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
    state.routing_revision.fetch_add(1, Ordering::Release);
    Ok(super::credentials(state).await)
}
