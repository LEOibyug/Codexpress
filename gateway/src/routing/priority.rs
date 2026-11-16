use crate::{model::{AppState, Source}, model_policy};
use std::sync::Arc;

pub(crate) async fn candidates(
    state: &Arc<AppState>,
    model: &str,
    session: Option<&str>,
) -> Vec<Source> {
    let settings = state.settings.read().await.clone();
    let policies = state.model_policies.read().await.clone();
    let runtimes = state.sources.read().await;
    let mut sources: Vec<_> = runtimes
        .iter()
        .filter(|runtime| {
            runtime.source.enabled
                && model_policy::source_allowed(&policies, model, &runtime.source.id)
                && (settings.mode != "fixed" || settings.fixed_source_id.as_ref() == Some(&runtime.source.id))
                && !super::official_exhausted(runtime, &runtimes)
                && super::manual::has_credentials(&runtime.source)
                && (runtime.source.models.is_empty()
                    || runtime.source.models.iter().any(|m| m == model))
        })
        .map(|runtime| runtime.source.clone())
        .collect();
    model_policy::sort_sources(&policies, model, &mut sources);
    if let Some(preferred) = session.and_then(|key| {
        state
            .affinity
            .preferred_within(key, &sources, u64::from(settings.idle_return_minutes) * 60)
    }) {
        sources.sort_by_key(|source| source.id != preferred);
    }
    sources
}

pub(crate) async fn eligible(state: &Arc<AppState>, source: &Source, model: &str) -> bool {
    let settings = state.settings.read().await.clone();
    let policies = state.model_policies.read().await.clone();
    let runtimes = state.sources.read().await;
    runtimes.iter().any(|runtime| {
        runtime.source.id == source.id
            && runtime.source.identity() == source.identity()
            && runtime.source.enabled
            && model_policy::source_allowed(&policies, model, &runtime.source.id)
            && (settings.mode != "fixed" || settings.fixed_source_id.as_ref() == Some(&runtime.source.id))
            && !super::official_exhausted(runtime, &runtimes)
            && super::manual::has_credentials(&runtime.source)
            && (runtime.source.models.is_empty()
                || runtime.source.models.iter().any(|m| m == model))
    })
}
