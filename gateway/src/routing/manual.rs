//! Explicit source takeover, bounded by capacity and remaining conversations.
use crate::{
    affinity::AffinityStore,
    model::{AppState, Source, SourceRuntime},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(crate) struct Trial {
    id: String,
    pub source_id: String,
    identity: String,
    started: bool,
}

#[derive(Default)]
pub struct ManualRouting {
    pending: Mutex<Option<Trial>>,
}

impl ManualRouting {
    pub(crate) fn retain_sources(&self, sources: &[crate::model::SourceRuntime]) {
        if let Ok(mut pending) = self.pending.lock() {
            if pending.as_ref().is_some_and(|trial| {
                !sources.iter().any(|runtime| {
                    runtime.source.id == trial.source_id
                        && runtime.source.identity() == trial.identity
                        && runtime.source.enabled
                })
            }) {
                *pending = None;
            }
        }
    }
    pub fn status(&self) -> Value {
        self.pending
            .lock()
            .ok()
            .and_then(|pending| {
                pending
                    .as_ref()
                    .map(|trial| json!({"id":trial.id,"source_id":trial.source_id}))
            })
            .unwrap_or(Value::Null)
    }

    pub(crate) fn stop(&self, trial: &Trial) -> bool {
        let Ok(mut pending) = self.pending.lock() else {
            return false;
        };
        if pending
            .as_ref()
            .is_some_and(|current| current.id == trial.id)
        {
            *pending = None;
            return true;
        }
        false
    }

    pub(crate) fn refresh(&self, sources: &[SourceRuntime], affinity: &AffinityStore) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        let Some(trial) = pending.as_ref() else {
            return;
        };
        let source = sources.iter().find(|runtime| {
            runtime.source.id == trial.source_id
                && runtime.source.identity() == trial.identity
                && runtime.source.enabled
                && has_credentials(&runtime.source)
                && !super::official_exhausted(runtime, sources)
        });
        if source.is_none_or(|runtime| {
            trial.started
                && (!affinity.capacity_status(&runtime.source).1
                    || !affinity.has_other_conversations(&runtime.source.id, sources))
        }) {
            *pending = None;
        }
    }

    fn claim(&self, model: &str, sources: &[SourceRuntime]) -> Option<Trial> {
        let mut pending = self.pending.lock().ok()?;
        let trial = pending.as_mut()?;
        let source = &sources
            .iter()
            .find(|r| r.source.id == trial.source_id)?
            .source;
        if !source.models.is_empty() && !source.models.iter().any(|m| m == model) {
            return None;
        }
        trial.started = true;
        Some(trial.clone())
    }
}

pub(crate) async fn claim(state: &Arc<AppState>, model: &str) -> Option<Trial> {
    let sources = state.sources.read().await;
    state.manual_routing.refresh(&sources, &state.affinity);
    state.manual_routing.claim(model, &sources)
}

pub(super) fn has_credentials(source: &Source) -> bool {
    let fields: &[&str] = if source.kind == "official" {
        &["access_token", "refresh_token"]
    } else {
        &["api_key"]
    };
    fields.iter().any(|field| {
        source.secret[*field]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    })
}

pub(crate) async fn handle(state: &Arc<AppState>, op: &str, payload: Value) -> Result<Value> {
    if op == "routing.cancel" {
        let id = payload["id"].as_str().context("缺少抢占操作标识")?;
        let mut pending = state
            .manual_routing
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("无法停止来源抢占"))?;
        if pending.as_ref().is_some_and(|trial| trial.id == id) {
            *pending = None;
            return Ok(json!({"cancelled":true}));
        }
        return Ok(json!({"cancelled":false}));
    }
    let id = payload["source_id"].as_str().context("请选择来源")?;
    if op == "routing.recover" {
        let mut sources = state.sources.write().await;
        if sources
            .iter()
            .find(|r| r.source.id == id)
            .is_some_and(|runtime| super::official_exhausted(runtime, &sources))
        {
            bail!("官方额度已耗尽，请刷新额度并确认恢复后再使用");
        }
        let runtime = sources
            .iter_mut()
            .find(|r| r.source.id == id)
            .context("来源已被移除")?;
        if !runtime.source.enabled {
            bail!("请先启用此来源");
        }
        if !matches!(runtime.health.as_str(), "cooldown" | "proxy_error") {
            bail!("此来源不处于临时冷却状态");
        }
        runtime.health = "ready".into();
        runtime.cooldown_until = None;
        runtime.last_error = None;
        runtime.health_revision = runtime.health_revision.wrapping_add(1);
        return Ok(json!({"recovered":true}));
    }
    let sources = state.sources.read().await;
    let runtime = sources
        .iter()
        .find(|r| r.source.id == id)
        .context("来源已被移除")?;
    let source = &runtime.source;
    if !source.enabled {
        bail!("请先启用此来源");
    }
    if !has_credentials(source) {
        bail!("请先登录或填写此来源的密钥");
    }
    if super::official_exhausted(runtime, &sources) {
        bail!("官方额度已耗尽，请刷新额度并确认恢复后再使用");
    }
    let trial = Trial {
        id: uuid::Uuid::new_v4().to_string(),
        source_id: id.into(),
        identity: source.identity(),
        started: false,
    };
    let response = json!({"id":trial.id,"source_id":trial.source_id});
    *state
        .manual_routing
        .pending
        .lock()
        .map_err(|_| anyhow::anyhow!("无法开始来源抢占"))? = Some(trial);
    Ok(response)
}

pub(crate) async fn source_for_trial(
    state: &Arc<AppState>,
    trial: &Trial,
    model: &str,
) -> Option<Source> {
    let sources = state.sources.read().await;
    sources
        .iter()
        .find(|r| {
            r.source.id == trial.source_id
                && r.source.identity() == trial.identity
                && r.source.enabled
                && !super::official_exhausted(r, &sources)
                && has_credentials(&r.source)
                && (r.source.models.is_empty() || r.source.models.iter().any(|m| m == model))
        })
        .map(|r| r.source.clone())
}

pub(crate) async fn reserve(
    state: &Arc<AppState>,
    source: &Source,
    session: Option<&str>,
    model: &str,
) -> Option<crate::affinity::RouteLease> {
    let sources = state.sources.read().await;
    let runtime = sources.iter().find(|r| {
        r.source.id == source.id
            && r.source.identity() == source.identity()
            && r.source.enabled
            && !super::official_exhausted(r, &sources)
            && (r.source.models.is_empty() || r.source.models.iter().any(|m| m == model))
    })?;
    // Explicit probes bypass cooldown and soft affinity. Exhausted official
    // quota and concurrent conversation limits remain hard admission blocks.
    state
        .affinity
        .reserve(session, &runtime.source, false, false, None)
}
