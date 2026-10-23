use crate::model::{AppState, Source};
use std::sync::Arc;

pub(crate) async fn candidates(
    state: &Arc<AppState>,
    model: &str,
    session: Option<&str>,
) -> Vec<Source> {
    let minutes = state.settings.read().await.idle_return_minutes;
    let mut sources: Vec<_> = state
        .sources
        .read()
        .await
        .iter()
        .filter(|runtime| {
            runtime.source.enabled
                && super::manual::has_credentials(&runtime.source)
                && (runtime.source.models.is_empty()
                    || runtime.source.models.iter().any(|m| m == model))
        })
        .map(|runtime| runtime.source.clone())
        .collect();
    sources.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.id.cmp(&b.id)));
    if let Some(preferred) = session.and_then(|key| {
        state
            .affinity
            .preferred_within(key, &sources, u64::from(minutes) * 60)
    }) {
        sources.sort_by_key(|source| source.id != preferred);
    }
    sources
}

pub(crate) async fn eligible(state: &Arc<AppState>, source: &Source, model: &str) -> bool {
    state.sources.read().await.iter().any(|runtime| {
        runtime.source.id == source.id
            && runtime.source.identity() == source.identity()
            && runtime.source.enabled
            && super::manual::has_credentials(&runtime.source)
            && (runtime.source.models.is_empty()
                || runtime.source.models.iter().any(|m| m == model))
    })
}
