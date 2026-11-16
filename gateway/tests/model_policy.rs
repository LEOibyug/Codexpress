#[path = "common/server.rs"]
mod server;
#[path = "common/source.rs"]
mod source_fixture;

use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Router,
};
use codexpress_gateway::{
    management,
    model::{AppState, Source},
    relay, routing,
};
use serde_json::{json, Value};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex};
use tower::ServiceExt;

fn source(id: &str, priority: i32, base: &str) -> Source {
    Source {
        priority,
        models: vec!["left".into(), "right".into()],
        ..source_fixture::api_source(id, id, base, "upstream-fixture")
    }
}

async fn sync(state: &Arc<AppState>, sources: &[Source], mut options: Value) {
    options["sources"] = Value::Array(sources.iter().map(|source| {
        let mut value = serde_json::to_value(source).unwrap();
        value["secret"] = source.secret.clone();
        value
    }).collect());
    options["upstream_attempts"] = json!(1);
    options["upstream_retry_delay_seconds"] = json!(0);
    management::dispatch(state, "sync", options).await.unwrap();
}

async fn ids(state: &Arc<AppState>, model: &str, session: Option<&str>) -> Vec<String> {
    routing::candidates_with_session(state, model, None, session).await
        .into_iter().map(|source| source.id).collect()
}

async fn send(state: &Arc<AppState>, model: &str, thread: &str) -> StatusCode {
    let request = Request::builder().method("POST").uri("/v1/responses")
        .header("host", "127.0.0.1:0")
        .header("authorization", "Bearer fixture")
        .header("thread-id", thread)
        .body(Body::from(serde_json::to_vec(&json!({"model":model,"input":"hello"})).unwrap()))
        .unwrap();
    let response = relay::router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    to_bytes(response.into_body(), 10000).await.unwrap();
    status
}

#[tokio::test]
async fn model_order_overrides_global_and_load_order_without_losing_allowed_affinity() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().into(), 0, "fixture".into()).unwrap();
    let sources = [source("a", 0, "http://127.0.0.1:1"), source("b", 1, "http://127.0.0.1:1")];
    let policies = json!([
        {"model":"left","source_ids":["b","a"]},
        {"model":"right","source_ids":["a","b"]}
    ]);
    let mut occupied = state.affinity.reserve(Some("occupied"), &sources[1], false, false, None).unwrap();
    occupied.commit().unwrap();
    drop(occupied);
    for load_balance in [false, true] {
        sync(&state, &sources, json!({"load_balance":load_balance,"model_policies":policies})).await;
        assert_eq!(ids(&state, "left", None).await, ["b", "a"]);
        assert_eq!(ids(&state, "right", None).await, ["a", "b"]);
    }
    let mut sticky = routing::reserve_for_model(&state, &sources[0], Some("sticky"), false, "left").await.unwrap();
    sticky.commit().unwrap();
    drop(sticky);
    assert_eq!(ids(&state, "left", Some("sticky")).await, ["a", "b"]);
    sync(&state, &sources, json!({"model_policies":[{"model":"left","source_ids":["b"]}]})).await;
    assert_eq!(ids(&state, "left", Some("sticky")).await, ["b"]);
    assert!(routing::reserve_for_model(&state, &sources[0], Some("sticky"), true, "left").await.is_none());
    assert!(routing::reserve_context_handoff_for_model(&state, &sources[0], Some("new"), "left").await.is_none());
    sync(&state, &sources, json!({"mode":"fixed","fixed_source_id":"a","model_policies":[{"model":"left","source_ids":["b"]}]})).await;
    assert!(ids(&state, "left", None).await.is_empty());
    sync(&state, &sources, json!({"model_policies":[{"model":"left","source_ids":["removed"]}]})).await;
    assert!(ids(&state, "left", None).await.is_empty());
    let mut unsupported = sources.clone();
    unsupported[1].models = vec!["right".into()];
    sync(&state, &unsupported, json!({"model_policies":[{"model":"left","source_ids":["b"]}]})).await;
    assert!(ids(&state, "left", None).await.is_empty());
    sync(&state, &sources, json!({})).await;
    assert_eq!(ids(&state, "left", None).await, ["a", "b"]);
    assert_eq!(ids(&state, "left", Some("sticky")).await, ["a", "b"]);
}

#[tokio::test]
async fn idle_return_uses_model_order_instead_of_raw_source_priority() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().into(), 0, "fixture".into()).unwrap();
    let sources = [source("a", 0, "http://127.0.0.1:1"), source("b", 1, "http://127.0.0.1:1")];
    sync(&state, &sources, json!({})).await;
    let mut lease = routing::reserve_for_model(&state, &sources[0], Some("idle"), false, "left").await.unwrap();
    lease.commit().unwrap();
    drop(lease);
    drop(state);
    let database = rusqlite::Connection::open(dir.path().join("affinity.sqlite")).unwrap();
    database.execute("UPDATE affinity SET last_seen=?1", [codexpress_gateway::model::now() - 86400]).unwrap();
    drop(database);
    let restored = AppState::new(dir.path().into(), 0, "fixture".into()).unwrap();
    sync(&restored, &sources, json!({"load_balance":true,"idle_return_minutes":1,
        "model_policies":[{"model":"left","source_ids":["b","a"]}]})).await;
    let (target, lease) = routing::reserve_idle_return(&restored, "left", "idle").await.unwrap();
    assert_eq!(target.id, "b");
    drop(lease);
    assert_eq!(restored.affinity.preferred("idle", &sources).as_deref(), Some("a"));
    sync(&restored, &sources, json!({"model_policies":[{"model":"left","source_ids":["a"]}]})).await;
    assert!(routing::reserve_idle_return(&restored, "left", "idle").await.is_none());
}

#[tokio::test]
async fn strict_fallback_and_manual_takeover_cannot_leave_the_model_source_pool() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let reject_a = Arc::new(AtomicBool::new(false));
    let mut sources = Vec::new();
    let mut tasks = Vec::new();
    for (priority, id) in ["a", "b", "c"].into_iter().enumerate() {
        let calls = calls.clone();
        let reject_a = reject_a.clone();
        let (base, task) = server::serve(Router::new().route("/responses", post(move || {
            let calls = calls.clone();
            let reject_a = reject_a.clone();
            async move {
                calls.lock().unwrap().push(id);
                if id == "a" && reject_a.load(Ordering::SeqCst) {
                    StatusCode::TOO_MANY_REQUESTS.into_response()
                } else {
                    axum::Json(json!({"status":"completed","output":[]})).into_response()
                }
            }
        }))).await;
        sources.push(source(id, priority as i32, &base));
        tasks.push(task);
    }
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().into(), 0, "fixture".into()).unwrap();
    sync(&state, &sources, json!({"load_balance":true,"model_policies":[
        {"model":"left","source_ids":["b","a"]},
        {"model":"right","source_ids":["a"]}
    ]})).await;
    assert_eq!(send(&state, "left", "ordered").await, StatusCode::OK);
    assert_eq!(*calls.lock().unwrap(), ["b"]);
    calls.lock().unwrap().clear();
    let trial = management::dispatch(&state, "routing.try", json!({"source_id":"b"})).await.unwrap();
    assert_eq!(send(&state, "right", "not-a-takeover").await, StatusCode::OK);
    assert_eq!(state.manual_routing.status()["id"], trial["id"]);
    assert_eq!(*calls.lock().unwrap(), ["a"]);
    calls.lock().unwrap().clear();
    reject_a.store(true, Ordering::SeqCst);
    assert_ne!(send(&state, "right", "no-forbidden-fallback").await, StatusCode::OK);
    assert_eq!(*calls.lock().unwrap(), ["a"]);
    assert_eq!(state.manual_routing.status()["id"], trial["id"]);
    for task in tasks { task.abort(); }
}

#[tokio::test]
async fn invalid_policies_reject_the_entire_sync_and_missing_fields_restore_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().into(), 0, "fixture".into()).unwrap();
    let sources = [source("a", 0, "http://127.0.0.1:1"), source("b", 1, "http://127.0.0.1:1")];
    sync(&state, &sources, json!({"model_policies":[{"model":"left","source_ids":["b"],"clients":["omp"]}]})).await;
    let expected = management::status(&state).await.unwrap()["model_policies"].clone();
    let revision = state.routing_revision.load(Ordering::Acquire);
    for invalid in [
        json!({}), json!(null),
        json!([{"model":"left","clients":["unknown"]}]),
        json!([{"model":"left"},{"model":"left"}]),
        json!([{"model":"left","source_ids":["a","a"]}]),
        json!([{"model":"left","source_ids":["bad/id"]}]),
        json!([{"model":"left","clients":["omp","omp"]}]),
        json!([{"model":"with whitespace"}]),
    ] {
        assert!(management::dispatch(&state, "sync", json!({"sources":[],"load_balance":true,
            "model_compaction":[{"model":"left","auto_compact_token_limit":1000}],
            "model_policies":invalid})).await.is_err());
        let status = management::status(&state).await.unwrap();
        assert_eq!(status["sources"].as_array().unwrap().len(), 2);
        assert_eq!(status["load_balance"], false);
        assert_eq!(status["model_policies"], expected);
        assert_eq!(state.routing_revision.load(Ordering::Acquire), revision);
        assert_eq!(ids(&state, "left", None).await, ["b"]);
    }
    for source_ids in [Value::Null, json!(["a", "b"])] {
        sync(&state, &sources, json!({"model_policies":[{"model":"left","source_ids":source_ids}]})).await;
        assert_eq!(ids(&state, "left", None).await, ["a", "b"]);
        let status = management::status(&state).await.unwrap();
        assert_eq!(status["model_policies"][0]["clients"], json!(["codex","omp","chatbot","images"]));
    }
    sync(&state, &sources, json!({"model_policies":[{"model":"left","source_ids":[]}]})).await;
    assert!(ids(&state, "left", None).await.is_empty());
    sync(&state, &sources, json!({})).await;
    assert_eq!(ids(&state, "left", None).await, ["a", "b"]);
    assert_eq!(management::status(&state).await.unwrap()["model_policies"], json!([]));
}
