#[path = "common/server.rs"]
mod server;
#[path = "common/source.rs"]
mod source_fixture;

use axum::{
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use codexpress_gateway::{
    management,
    model::{AppState, SourceRuntime},
    usage::{TokenDetails, UsageRecord},
};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

fn record(id: &str, input: u64, output: u64) -> UsageRecord {
    UsageRecord {
        attempt_id: id.into(),
        request_id: id.into(),
        source_id: "cch".into(),
        source_name: "CCH".into(),
        model: "custom-model".into(),
        started_at: 1_790_000_000,
        status: "success".into(),
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: 40,
        token_details: TokenDetails {
            source_kind: Some("api".into()),
            cached_input_reported: true,
            output_available: true,
            ..Default::default()
        },
        complete: true,
        latency_ms: 1000,
        first_token_latency_ms: None,
        generation_ms: None,
    }
}
fn row(id: u64, input: u64, output: u64, cost: f64) -> Value {
    json!({"id":id,"createdAt":"2026-09-21T14:13:49.000Z","model":"custom-model","billingModel":"custom-model",
        "inputTokens":input-40,"outputTokens":output,"cacheReadInputTokens":40,"cacheCreationInputTokens":0,
        "cost":cost,"statusCode":200,"duration":1000,"endpoint":"/v1/responses"})
}

#[tokio::test]
async fn paginated_ledger_backfills_only_mutually_unique_requests_and_does_not_poll_when_idle() {
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let fail_second = Arc::new(AtomicBool::new(true));
    let failed_page = fail_second.clone();
    let (address,task)=server::serve(Router::new()
        .route("/api/v1/me/quota",get(||async{Json(json!({"userLimitTotalUsd":100,"userCurrentTotalUsd":1}))}))
        .route("/api/auth/login",post(|Json(v):Json<Value>|async move{
            assert_eq!(v["key"],"PRIVATE-KEY");
            Response::builder().header("content-type","application/json").header("set-cookie","auth-token=PRIVATE-SESSION; Max-Age=3600")
                .body(axum::body::Body::from("{\"ok\":true}")).unwrap()
        }))
        .route("/api/v1/me/usage-logs",get(move|r:Request|{let hits=copy.clone();let failed_page=failed_page.clone();async move{
            if r.headers().get("cookie").and_then(|v|v.to_str().ok())!=Some("auth-token=PRIVATE-SESSION") {
                return (StatusCode::UNAUTHORIZED,Json(json!({"type":"urn:claude-code-hub:problem:auth.invalid"}))).into_response();
            }
            hits.fetch_add(1,Ordering::SeqCst);
            let next=r.uri().query().unwrap_or("").contains("cursorId=20");
            if next && failed_page.load(Ordering::SeqCst) {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            let body=if !next {
                json!({"items":[row(30,100,9,0.15652124),row(29,200,10,0.0),row(28,300,11,0.4),row(27,400,12,0.5)],
                    "pageInfo":{"hasMore":true,"nextCursor":{"createdAt":"2026-09-22T14:13:49Z","id":20}}})
            } else {
                // A second ledger entry matching the same local request must invalidate
                // the first page's tentative match, rather than double-charge it.
                json!({"items":[row(19,400,12,0.6),row(18,500,13,-1.0)],"pageInfo":{"hasMore":false,"nextCursor":null}})
            };
            Json(body).into_response()
        }}))).await;
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().into(), 0, "local".into()).unwrap();
    let source = source_fixture::api_source("cch", "CCH", &format!("{address}/v1"), "PRIVATE-KEY");
    state.sources.write().await.push(SourceRuntime::new(source));
    for r in [
        record("matched", 100, 9),
        record("free", 200, 10),
        record("duplicate-a", 300, 11),
        record("duplicate-b", 300, 11),
        record("remote-ambiguous", 400, 12),
        record("negative", 500, 13),
    ] {
        state.usage.record(r).unwrap();
    }
    management::dispatch(&state, "usage.reconcile", json!({"source_id":"cch"}))
        .await
        .unwrap();
    let failed = finish_sync(&state).await;
    assert_eq!(failed["errors"], 1);
    assert_eq!(failed["updated"], 0);
    assert!(
        state.usage.summary(1_000).unwrap()["recent"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["cost"]["billing_basis"] != "upstream_charge"),
        "后续分页失败不得应用前页部分匹配"
    );
    fail_second.store(false, Ordering::SeqCst);
    management::dispatch(&state, "usage.reconcile", json!({"source_id":"cch"}))
        .await
        .unwrap();
    let mut finished = false;
    for _ in 0..500 {
        let s = management::dispatch(&state, "usage.reconcile.status", json!({}))
            .await
            .unwrap();
        if s["running"] == false {
            assert_eq!(s["pages"], 2);
            assert_eq!(s["updated"], 2);
            assert_eq!(s["errors"], 0);
            finished = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(finished);
    let summary = state.usage.summary(1_000).unwrap();
    let recent = summary["recent"].as_array().unwrap();
    let matched = recent
        .iter()
        .find(|r| r["attempt_id"] == "matched")
        .unwrap();
    assert_eq!(matched["cost"]["total_usd"], 0.15652124);
    assert_eq!(matched["cost"]["billing_basis"], "upstream_charge");
    assert_eq!(
        recent.iter().find(|r| r["attempt_id"] == "free").unwrap()["cost"]["total_usd"],
        0.0
    );
    assert_eq!(
        recent
            .iter()
            .filter(|r| r["cost"]["status"] == "unknown_model")
            .count(),
        4
    );
    assert_eq!(summary["rows"][0]["total_usd"], 0.15652124);
    assert!(!summary.to_string().contains("PRIVATE"));
    let before = hits.load(Ordering::SeqCst);
    for _ in 0..3 {
        management::dispatch(&state, "usage", json!({"days":1}))
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(hits.load(Ordering::SeqCst), before);
    // Price/source settings do not overwrite an actual charge after a restart.
    let reopened =
        codexpress_gateway::usage::UsageStore::open(&dir.path().join("usage.sqlite")).unwrap();
    reopened
        .reprice_historical(&[("cch".into(), "api".into(), true)])
        .unwrap();
    assert_eq!(
        reopened.summary(1_000).unwrap()["rows"][0]["total_usd"],
        0.15652124
    );
    task.abort();
}

async fn spin_until(mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if condition() {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert!(condition(), "后台核对未达到预期状态");
}
async fn finish_sync(state: &Arc<AppState>) -> Value {
    // Paused-time advancement wakes the daemon before its worker runs.
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let value = management::dispatch(state, "usage.reconcile.status", json!({}))
            .await
            .unwrap();
        if value["running"] == false {
            return value;
        }
        tokio::task::yield_now().await;
    }
    panic!("后台核对未结束");
}
fn settle(state: &Arc<AppState>, source: &codexpress_gateway::model::Source, id: &str) {
    let mut value = record(id, 100, 9);
    value.token_details.source_identity = Some(source.identity());
    state.usage.start_for_source(&value, source);
    state.usage.record(value).unwrap();
}

#[tokio::test(start_paused = true)]
async fn automatic_reconciliation_counts_50_and_preserves_settlements_during_a_scan() {
    let hits = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (calls, releases) = (hits.clone(), gate.clone());
    let (address, task) = server::serve(
        Router::new()
            .route(
                "/api/v1/me/quota",
                get(|| async { Json(json!({"userLimitTotalUsd":100,"userCurrentTotalUsd":0})) }),
            )
            .route(
                "/api/v1/me/usage-logs",
                get(move || {
                    let (calls, releases) = (calls.clone(), releases.clone());
                    async move {
                        let index = calls.fetch_add(1, Ordering::SeqCst);
                        if index == 0 {
                            releases.acquire().await.unwrap().forget();
                        }
                        Json(json!({"items":[],"pageInfo":{"hasMore":false}}))
                    }
                }),
            ),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().into(), 0, "local".into()).unwrap();
    let source = source_fixture::api_source("cch", "CCH", &format!("{address}/v1"), "PRIVATE-KEY");
    state
        .sources
        .write()
        .await
        .push(SourceRuntime::new(source.clone()));
    codexpress_gateway::balance::billing::start(&state);
    for i in 0..49 {
        settle(&state, &source, &format!("first-{i}"));
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    finish_sync(&state).await;
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    // Fifty settled attempts trigger even while a different request is active.
    let active = record("still-active", 101, 10);
    state.usage.start_for_source(&active, &source);
    settle(&state, &source, "fiftieth");
    tokio::time::advance(Duration::from_secs(1)).await;
    spin_until(|| hits.load(Ordering::SeqCst) == 1).await;
    for i in 0..50 {
        settle(&state, &source, &format!("during-{i}"));
    }
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1, "同一来源必须single-flight");
    gate.add_permits(1);
    finish_sync(&state).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    spin_until(|| hits.load(Ordering::SeqCst) == 2).await;
    finish_sync(&state).await;
    // No new completions means no repeated idle scan, despite ongoing activity.
    tokio::time::advance(Duration::from_secs(600)).await;
    finish_sync(&state).await;
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    state.usage.record(active).unwrap();
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn automatic_idle_waits_300_seconds_after_last_end_and_failure_needs_new_work() {
    let hits = Arc::new(AtomicUsize::new(0));
    let calls = hits.clone();
    let (address, task) = server::serve(
        Router::new()
            .route(
                "/api/v1/me/quota",
                get(|| async { Json(json!({"userLimitTotalUsd":100,"userCurrentTotalUsd":0})) }),
            )
            .route(
                "/api/v1/me/usage-logs",
                get(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::SERVICE_UNAVAILABLE }
                }),
            ),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().into(), 0, "local".into()).unwrap();
    let mut source =
        source_fixture::api_source("cch", "CCH", &format!("{address}/v1"), "PRIVATE-KEY");
    state
        .sources
        .write()
        .await
        .push(SourceRuntime::new(source.clone()));
    codexpress_gateway::balance::billing::start(&state);
    settle(&state, &source, "early");
    let active = record("active", 200, 10);
    state.usage.start_for_source(&active, &source);
    tokio::time::advance(Duration::from_secs(600)).await;
    finish_sync(&state).await;
    assert_eq!(hits.load(Ordering::SeqCst), 0, "活跃来源不能按idle核对");
    state.usage.record(active).unwrap();
    tokio::time::advance(Duration::from_secs(299)).await;
    finish_sync(&state).await;
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_secs(1)).await;
    spin_until(|| hits.load(Ordering::SeqCst) == 1).await;
    let failed = finish_sync(&state).await;
    assert_eq!(failed["errors"], 1);
    tokio::time::advance(Duration::from_secs(600)).await;
    finish_sync(&state).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1, "失败不能在idle热重试");
    // Editing credentials abandons the previous identity's pending count.
    settle(&state, &source, "old-identity");
    source.secret = json!({"api_key":"PRIVATE-REPLACEMENT"});
    state.sources.write().await[0].source = source.clone();
    tokio::time::advance(Duration::from_secs(300)).await;
    finish_sync(&state).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    settle(&state, &source, "new-identity");
    tokio::time::advance(Duration::from_secs(300)).await;
    spin_until(|| hits.load(Ordering::SeqCst) == 2).await;
    finish_sync(&state).await;
    // Manual full reconciliation remains available without fresh traffic.
    management::dispatch(&state, "usage.reconcile", json!({"source_id":"cch"}))
        .await
        .unwrap();
    spin_until(|| hits.load(Ordering::SeqCst) == 3).await;
    finish_sync(&state).await;
    task.abort();
}
