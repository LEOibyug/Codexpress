//! Cache-only, exact-thread status for authenticated chatbot clients.
use super::{error, ChatbotIngress};
use crate::model::{AppState, Source};
use axum::{
    extract::{Extension, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

const RETENTION: Duration = Duration::from_secs(30 * 60);
const CAPACITY: usize = 4096;

#[derive(Default)]
pub(super) struct Store {
    entries: Mutex<HashMap<String, Entry>>,
}
struct Entry {
    request: Uuid,
    updated: Instant,
    source: Option<ActualSource>,
}
#[derive(Clone)]
struct ActualSource {
    attempt: Uuid,
    active: bool,
    id: String,
    identity: String,
}

// Keys contain only digests, not client session IDs or credentials. A generation
// supersedes all older generations of the same thread/model, including overlap.
fn key(namespace: &str, thread: &str, model: &str) -> String {
    crate::continuity::digest(
        "chatbot-status",
        &json!([namespace, thread, model]).to_string(),
    )
}
pub(super) fn thread(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("thread-id")
        .or_else(|| headers.get("thread_id"))?
        .to_str()
        .ok()
        .filter(|s| valid(s, 512))
}
pub(super) fn valid(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}
impl Store {
    pub(super) fn begin(self: &Arc<Self>, namespace: &str, thread: &str, model: &str) -> Tracking {
        let key = key(namespace, thread, model);
        let request = Uuid::new_v4();
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|_, entry| {
                entry.source.as_ref().is_some_and(|s| s.active)
                    || entry.updated.elapsed() < RETENTION
            });
            if entries.len() >= CAPACITY && !entries.contains_key(&key) {
                if let Some(oldest) = entries
                    .iter()
                    .min_by_key(|(_, e)| e.updated)
                    .map(|(k, _)| k.clone())
                {
                    entries.remove(&oldest);
                }
            }
            entries.insert(
                key.clone(),
                Entry {
                    request,
                    updated: Instant::now(),
                    source: None,
                },
            );
        }
        Tracking {
            store: self.clone(),
            key,
            request,
        }
    }
    fn actual(&self, namespace: &str, thread: &str, model: &str) -> Option<ActualSource> {
        let entries = self.entries.lock().ok()?;
        let entry = entries.get(&key(namespace, thread, model))?;
        (entry.source.as_ref().is_some_and(|s| s.active) || entry.updated.elapsed() < RETENTION)
            .then(|| entry.source.clone())
            .flatten()
    }
}

#[derive(Clone)]
pub(crate) struct Tracking {
    store: Arc<Store>,
    key: String,
    request: Uuid,
}
impl Tracking {
    /// Called immediately before sending upstream, never during route planning.
    pub(crate) fn submit(&self, source: &Source) -> ActiveAttempt {
        let actual = ActualSource {
            attempt: Uuid::new_v4(),
            active: true,
            id: source.id.clone(),
            identity: source.identity(),
        };
        if let Ok(mut entries) = self.store.entries.lock() {
            if let Some(entry) = entries
                .get_mut(&self.key)
                .filter(|e| e.request == self.request)
            {
                entry.source = Some(actual.clone());
                entry.updated = Instant::now();
            }
        }
        ActiveAttempt {
            tracking: self.clone(),
            attempt: actual.attempt,
            successful: false,
        }
    }
}
pub(crate) struct ActiveAttempt {
    tracking: Tracking,
    attempt: Uuid,
    successful: bool,
}
impl ActiveAttempt {
    pub(crate) fn finish(&mut self, status: &str) {
        if status == "success" {
            self.successful = true;
            if let Ok(mut entries) = self.tracking.store.entries.lock() {
                if let Some(entry) = entries
                    .get_mut(&self.tracking.key)
                    .filter(|e| e.request == self.tracking.request)
                {
                    if let Some(source) =
                        entry.source.as_mut().filter(|s| s.attempt == self.attempt)
                    {
                        source.active = false;
                        entry.updated = Instant::now();
                    }
                }
            }
        } else if matches!(status, "failed" | "interrupted") {
            self.successful = false;
            self.clear();
        }
    }
    fn clear(&self) {
        if let Ok(mut entries) = self.tracking.store.entries.lock() {
            if let Some(entry) = entries.get_mut(&self.tracking.key).filter(|e| {
                e.request == self.tracking.request
                    && e.source.as_ref().is_some_and(|s| s.attempt == self.attempt)
            }) {
                entry.source = None;
                entry.updated = Instant::now();
            }
        }
    }
}
impl Drop for ActiveAttempt {
    fn drop(&mut self) {
        if !self.successful {
            self.clear();
        }
    }
}

#[derive(Deserialize)]
pub(super) struct StatusQuery {
    model: String,
    session_id: String,
}
pub(super) async fn get(
    State(state): State<Arc<AppState>>,
    Extension(ingress): Extension<ChatbotIngress>,
    Query(query): Query<StatusQuery>,
) -> Response {
    if !valid(&query.model, 512) || !valid(&query.session_id, 512) {
        return error(
            StatusCode::BAD_REQUEST,
            "model 和 session_id 须为有效非空标识",
            None,
        );
    }
    let mut response =
        Json(snapshot(&state, &ingress.namespace, &query.session_id, &query.model).await)
            .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
async fn snapshot(state: &AppState, namespace: &str, thread: &str, model: &str) -> Value {
    let Some(actual) = state
        .chatbot
        .conversation_status
        .actual(namespace, thread, model)
    else {
        return json!({"source":null});
    };
    // Retain the read lock while consulting balance cache so an edit cannot
    // publish a prior identity's data. No network work or async wait occurs.
    let sources = state.sources.read().await;
    let Some(runtime) = sources
        .iter()
        .find(|r| r.source.id == actual.id && r.source.identity() == actual.identity)
    else {
        return json!({"source":null});
    };
    let source = &runtime.source;
    let mut result = json!({"id":source.id,"name":source.name,"kind":source.kind});
    match source.kind.as_str() {
        "official" => {
            let Some(quota) = main_quota(&runtime.quota) else {
                return json!({"source":null});
            };
            result["quota"] = quota;
        }
        "api" => {
            let Some((amount, currency)) = state.balances.cached_amount(source) else {
                return json!({"source":null});
            };
            result["balance"] = json!({"amount":amount,"currency":currency});
        }
        _ => return json!({"source":null}),
    }
    json!({"source":result})
}
fn main_quota(quota: &Value) -> Option<Value> {
    if quota["stale"] == true || quota.get("error").is_some_and(|error| !error.is_null()) {
        return None;
    }
    let windows = quota["windows"].as_array()?;
    let remaining = |weekly: bool| {
        windows
            .iter()
            .find(|window| {
                matches!(window["name"].as_str(), Some("primary" | "secondary"))
                    && window["window_seconds"].as_f64().is_some_and(|seconds| {
                        if weekly {
                            seconds == 604800.0
                        } else {
                            seconds > 0.0 && seconds < 604800.0
                        }
                    })
            })
            .and_then(|window| window["used_percent"].as_f64())
            .filter(|n| n.is_finite() && (0.0..=100.0).contains(n))
            .map(|used| 100.0 - used)
    };
    let short = remaining(false);
    let weekly = remaining(true);
    (short.is_some() && weekly.is_some())
        .then(|| json!({"short_remaining_percent":short,"weekly_remaining_percent":weekly}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source(id: &str) -> Source {
        serde_json::from_value(json!({"id":id,"name":id,"kind":"api","base_url":"http://127.0.0.1:1","enabled":true,"priority":1,"proxy":{"mode":"direct"},"secret":{"api_key":"private-fixture"}})).unwrap()
    }
    #[test]
    fn attempts_are_exact_isolated_and_failed_fallbacks_clear() {
        let store = Arc::new(Store::default());
        let tracking = store.begin("chatbot-key-a", "thread-a", "model-a");
        assert!(store
            .actual("chatbot-key-a", "thread-a", "model-a")
            .is_none());
        let mut first = tracking.submit(&source("first"));
        assert_eq!(
            store
                .actual("chatbot-key-a", "thread-a", "model-a")
                .unwrap()
                .id,
            "first"
        );
        for (namespace, thread, model) in [
            ("native", "thread-a", "model-a"),
            ("chatbot-key-b", "thread-a", "model-a"),
            ("chatbot-key-a", "thread-b", "model-a"),
            ("chatbot-key-a", "thread-a", "model-b"),
        ] {
            assert!(store.actual(namespace, thread, model).is_none());
        }
        first.finish("failed");
        assert!(store
            .actual("chatbot-key-a", "thread-a", "model-a")
            .is_none());
        let mut fallback = tracking.submit(&source("fallback"));
        drop(first);
        assert_eq!(
            store
                .actual("chatbot-key-a", "thread-a", "model-a")
                .unwrap()
                .id,
            "fallback"
        );
        fallback.finish("success");
        drop(fallback);
        assert_eq!(
            store
                .actual("chatbot-key-a", "thread-a", "model-a")
                .unwrap()
                .id,
            "fallback"
        );
        let newer = store.begin("chatbot-key-a", "thread-a", "model-a");
        let old_attempt = tracking.submit(&source("obsolete"));
        let cancelled = newer.submit(&source("current"));
        drop(old_attempt);
        assert_eq!(
            store
                .actual("chatbot-key-a", "thread-a", "model-a")
                .unwrap()
                .id,
            "current"
        );
        drop(cancelled);
        assert!(store
            .actual("chatbot-key-a", "thread-a", "model-a")
            .is_none());
    }
    #[test]
    fn inflight_routes_outlive_idle_retention_but_completed_routes_expire() {
        let store = Arc::new(Store::default());
        let tracking = store.begin("namespace", "thread", "model");
        let mut attempt = tracking.submit(&source("fixture"));
        let age = || {
            store
                .entries
                .lock()
                .unwrap()
                .get_mut(&tracking.key)
                .unwrap()
                .updated = Instant::now() - RETENTION - Duration::from_secs(1);
        };
        age();
        assert!(store.actual("namespace", "thread", "model").is_some());
        let _other = store.begin("namespace", "other", "model");
        assert!(store.actual("namespace", "thread", "model").is_some());
        attempt.finish("success");
        age();
        assert!(store.actual("namespace", "thread", "model").is_none());
    }
    #[test]
    fn only_valid_main_quota_windows_are_visible() {
        let quota = main_quota(&json!({"windows":[{"name":"primary","window_seconds":18000,"used_percent":25},{"name":"secondary","window_seconds":604800,"used_percent":100},{"name":"extra:primary","window_seconds":18000,"used_percent":0}]})).unwrap();
        assert_eq!(
            quota,
            json!({"short_remaining_percent":75.0,"weekly_remaining_percent":0.0})
        );
        assert!(main_quota(
            &json!({"windows":[{"name":"primary","window_seconds":18000,"used_percent":25}]})
        )
        .is_none());
        assert!(main_quota(
            &json!({"windows":[{"name":"secondary","window_seconds":604800,"used_percent":25}]})
        )
        .is_none());
        assert!(main_quota(&json!({"windows":[{"name":"extra:primary","window_seconds":18000,"used_percent":1},{"name":"secondary","window_seconds":604800,"used_percent":1}]})).is_none());
        assert!(main_quota(&json!({"stale":true,"windows":[{"name":"primary","window_seconds":18000,"used_percent":1},{"name":"secondary","window_seconds":604800,"used_percent":1}]})).is_none());
        assert!(main_quota(&json!({"windows":[{"name":"primary","window_seconds":18000,"used_percent":101},{"name":"secondary","window_seconds":604800,"used_percent":1}]})).is_none());
        assert!(main_quota(&json!({"stale":false,"error":"fixture error","windows":[{"name":"primary","window_seconds":18000,"used_percent":1},{"name":"secondary","window_seconds":604800,"used_percent":1}]})).is_none());
        assert!(main_quota(&json!({"windows":[{"name":"primary","used_percent":25},{"name":"secondary","window_seconds":604800,"used_percent":1}]})).is_none());
        assert!(main_quota(&json!({"windows":[{"name":"primary","window_seconds":0,"used_percent":25},{"name":"secondary","window_seconds":604800,"used_percent":1}]})).is_none());
        let reversed = main_quota(&json!({"windows":[{"name":"primary","window_seconds":604800,"used_percent":60},{"name":"secondary","window_seconds":18000,"used_percent":20}]})).unwrap();
        assert_eq!(
            reversed,
            json!({"short_remaining_percent":80.0,"weekly_remaining_percent":40.0})
        );
    }
    #[tokio::test]
    async fn snapshot_requires_current_identity_and_usable_corresponding_data() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(directory.path().into(), 17863, "fixture-native".into()).unwrap();
        let mut source = source("official");
        source.kind = "official".into();
        source.account_id = Some("account-fixture".into());
        source.secret = json!({"access_token":"private-access-fixture"});
        let mut runtime = crate::model::SourceRuntime::new(source.clone());
        runtime.quota = json!({"stale":false,"windows":[{"name":"primary","window_seconds":18000,"used_percent":20},{"name":"secondary","window_seconds":604800,"used_percent":60},{"name":"extra:primary","window_seconds":18000,"used_percent":99}]});
        state.sources.write().await.push(runtime);
        let tracking = state
            .chatbot
            .conversation_status
            .begin("namespace", "thread", "model");
        let mut attempt = tracking.submit(&source);
        let expected = json!({"source":{"id":"official","name":"official","kind":"official","quota":{"short_remaining_percent":80.0,"weekly_remaining_percent":40.0}}});
        assert_eq!(
            snapshot(&state, "namespace", "thread", "model").await,
            expected
        );
        state.sources.write().await[0].source.secret["access_token"] =
            json!("refreshed-access-fixture");
        assert_eq!(
            snapshot(&state, "namespace", "thread", "model").await,
            expected
        );
        state.sources.write().await[0].quota["stale"] = json!(true);
        assert_eq!(
            snapshot(&state, "namespace", "thread", "model").await,
            json!({"source":null})
        );
        state.sources.write().await[0].quota["stale"] = json!(false);
        state.sources.write().await[0].source.account_id = Some("changed-account".into());
        assert_eq!(
            snapshot(&state, "namespace", "thread", "model").await,
            json!({"source":null})
        );
        state.sources.write().await[0].source = source;
        attempt.finish("success");
        drop(attempt);
        assert_eq!(
            snapshot(&state, "namespace", "thread", "model").await,
            expected
        );
        state.sources.write().await.clear();
        assert_eq!(
            snapshot(&state, "namespace", "thread", "model").await,
            json!({"source":null})
        );
        let api = self::source("api-with-no-balance");
        state
            .sources
            .write()
            .await
            .push(crate::model::SourceRuntime::new(api.clone()));
        let _attempt = tracking.submit(&api);
        assert_eq!(
            snapshot(&state, "namespace", "thread", "model").await,
            json!({"source":null})
        );
    }
}
