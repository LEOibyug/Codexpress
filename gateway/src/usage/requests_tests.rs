use super::*;
use crate::usage::{TokenDetails, UsageRecord};
use rusqlite::{params, Connection};
use std::{collections::HashMap, time::Instant};

pub(in crate::usage) fn fixture() -> (tempfile::TempDir, StoreData) {
    let dir = tempfile::tempdir().unwrap();
    let connection = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    connection.execute_batch("CREATE TABLE usage (
        attempt INTEGER PRIMARY KEY,request_id TEXT NOT NULL,source_id TEXT NOT NULL,source_name TEXT NOT NULL,model TEXT NOT NULL,
        started_at INTEGER NOT NULL,status TEXT NOT NULL,input_tokens INTEGER NOT NULL,output_tokens INTEGER NOT NULL,
        cached_input_tokens INTEGER NOT NULL,complete INTEGER NOT NULL,latency_ms INTEGER NOT NULL,metadata TEXT NOT NULL DEFAULT '{}');
        CREATE INDEX usage_time ON usage(started_at);
        CREATE TABLE diagnostics(attempt_id TEXT PRIMARY KEY,created_at INTEGER NOT NULL,metadata TEXT NOT NULL,body BLOB);")
        .unwrap();
    (
        dir,
        StoreData {
            connection,
            active: HashMap::new(),
            analytics_cache: Default::default(),
        },
    )
}

pub(in crate::usage) fn record(id: &str, at: u64) -> UsageRecord {
    UsageRecord {
        attempt_id: id.into(),
        request_id: format!("request-{id}"),
        source_id: "a".into(),
        source_name: "来源 A".into(),
        model: "gpt-6.1-sol".into(),
        started_at: at,
        status: "success".into(),
        input_tokens: 100,
        output_tokens: 10,
        cached_input_tokens: 20,
        token_details: TokenDetails {
            output_available: true,
            cached_input_reported: true,
            ..Default::default()
        },
        complete: true,
        latency_ms: 100,
        first_token_latency_ms: Some(10),
        generation_ms: Some(50),
    }
}

pub(in crate::usage) fn persist(
    data: &StoreData,
    record: &UsageRecord,
    ingress: Option<&str>,
) -> i64 {
    let metadata = json!({"attempt_id":record.attempt_id,"token_details":record.token_details,
        "first_token_latency_ms":record.first_token_latency_ms,"generation_ms":record.generation_ms,
        "cost":crate::pricing::estimate(&serde_json::to_value(record).unwrap(),false)});
    data.connection.execute("INSERT INTO usage(request_id,source_id,source_name,model,started_at,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms,metadata) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![record.request_id,record.source_id,record.source_name,record.model,record.started_at,record.status,
            record.input_tokens,record.output_tokens,record.cached_input_tokens,record.complete,record.latency_ms,metadata.to_string()]).unwrap();
    let rowid = data.connection.last_insert_rowid();
    if let Some(ingress) = ingress {
        data.connection.execute("INSERT INTO diagnostics(attempt_id,created_at,metadata,body) VALUES(?1,?2,?3,?4)",
            params![record.attempt_id,record.started_at,json!({"ingress":ingress,"http_status":if record.status == "success" {200}else{429},"error_code":"rate_limited","error_message":"稍后重试"}).to_string(),b"body-only-needle".as_slice()]).unwrap();
    }
    rowid
}

#[test]
fn history_is_bounded_stable_and_searches_metadata_without_bodies() {
    let (_dir, mut data) = fixture();
    let first = record("first", 101);
    let mut second = record("second", 100); // rowid, rather than wall time, defines the cursor.
    second.status = "failed".into();
    let mut third = record("third", 103);
    third.source_id = "b".into();
    third.source_name = "来源 B".into();
    persist(&data, &first, None);
    persist(&data, &second, Some("ssh"));
    persist(&data, &third, Some("native"));
    let mut live = record("live", 105);
    live.status = "streaming".into();
    data.active
        .insert(live.attempt_id.clone(), (live, Instant::now()));
    // A stale in-memory entry is ignored even when the first page is small.
    data.active
        .insert(third.attempt_id.clone(), (third, Instant::now()));
    let page = build(&data, &json!({"from":100,"to":200,"limit":2})).unwrap();
    assert_eq!(page["total"], 4);
    assert_eq!(page["active_count"], 1);
    assert_eq!(page["records"].as_array().unwrap().len(), 2);
    assert_eq!(page["records"][0]["attempt_id"], "live");
    assert_eq!(page["records"][0]["status"], "streaming");
    assert_eq!(page["records"][0]["active"], true);
    assert!(page["records"][0]["sequence"].is_null());
    assert_eq!(page["records"][1]["attempt_id"], "third");
    assert_eq!(page["next_cursor"], 3);
    persist(&data, &record("arrived-later", 106), Some("image"));
    let next = build(&data, &json!({"from":100,"to":200,"before":3,"limit":2})).unwrap();
    assert_eq!(next["records"][0]["attempt_id"], "second");
    assert_eq!(next["records"][1]["attempt_id"], "first");
    assert!(next["next_cursor"].is_null());
    assert_eq!(next["records"][1]["ingress"], "unknown");
    assert_eq!(next["records"][1]["diagnostic_available"], false);
    let filtered = build(&data, &json!({"from":100,"to":200,"source_id":"a","model":"gpt-6.1-sol","ingress":"ssh","status":"failed","search":"稍后重试"})).unwrap();
    assert_eq!(filtered["total"], 1);
    assert_eq!(filtered["records"][0]["http_status"], 429);
    assert_eq!(filtered["records"][0]["error_code"], "rate_limited");
    assert_eq!(filtered["records"][0]["diagnostic_available"], true);
    let body = build(
        &data,
        &json!({"from":100,"to":200,"search":"body-only-needle"}),
    )
    .unwrap();
    assert_eq!(body["total"], 0);
    let injection = build(
        &data,
        &json!({"from":100,"to":200,"source_id":"' OR 1=1 --"}),
    )
    .unwrap();
    assert_eq!(injection["total"], 0);
    assert!(!page.to_string().contains("body-only-needle"));
}

#[test]
fn first_page_active_limit_legacy_fallback_and_validation_preserve_state() {
    let (_dir, mut data) = fixture();
    persist(&data, &record("old", 100), None);
    data.connection
        .execute("UPDATE usage SET metadata='{}'", [])
        .unwrap();
    let live = record("live", 101);
    data.active
        .insert(live.attempt_id.clone(), (live, Instant::now()));
    let page = build(&data, &json!({"from":100,"to":200,"limit":1})).unwrap();
    assert_eq!(page["records"][0]["attempt_id"], "live");
    assert_eq!(page["next_cursor"], 2);
    let next = build(&data, &json!({"from":100,"to":200,"limit":1,"before":2})).unwrap();
    assert_eq!(next["records"][0]["attempt_id"], "legacy:1");
    assert!(next["next_cursor"].is_null());
    let active = build(&data, &json!({"from":100,"to":200,"status":"active"})).unwrap();
    assert_eq!(active["total"], 1);
    assert_eq!(active["records"][0]["attempt_id"], "live");
    let snapshot = data.connection.total_changes();
    for invalid in [
        json!({"from":200,"to":100}),
        json!({"from":100,"to":200,"limit":101}),
        json!({"from":100,"to":200,"before":0}),
        json!({"from":100,"to":200,"search":"x".repeat(257)}),
        json!({"from":100,"to":200,"status":"arbitrary"}),
        json!({"from":100,"to":200,"ingress":true}),
        json!({"from":100,"to":200,"model":12}),
        json!({"from":100,"to":200,"unexpected":true}),
    ] {
        assert!(build(&data, &invalid).is_err(), "accepted {invalid}");
    }
    assert_eq!(snapshot, data.connection.total_changes());
    assert_eq!(data.active.len(), 1);
}

#[test]
fn upstream_and_unsuccessful_filter_include_failures_and_interruptions_only() {
    let (_dir, mut data) = fixture();
    for (id, source, outcome) in [
        ("ok", "a", "success"),
        ("bad", "a", "failed"),
        ("cut", "a", "interrupted"),
        ("other", "b", "failed"),
    ] {
        let mut item = record(id, 105);
        item.source_id = source.into();
        item.status = outcome.into();
        persist(&data, &item, Some("native"));
    }
    let mut active = record("live", 105);
    active.source_id = "a".into();
    active.status = "streaming".into();
    data.active
        .insert(active.attempt_id.clone(), (active, Instant::now()));
    let query = json!({"from":100,"to":200,"source_id":"a","status":"unsuccessful","limit":1});
    let page = build(&data, &query).unwrap();
    assert_eq!(page["total"], 2);
    assert_eq!(page["active_count"], 0);
    assert_eq!(page["records"][0]["attempt_id"], "cut");
    let mut next = query;
    next["before"] = page["next_cursor"].clone();
    let page = build(&data, &next).unwrap();
    assert_eq!(page["records"][0]["attempt_id"], "bad");
    assert!(page["next_cursor"].is_null());
    let success = build(
        &data,
        &json!({"from":100,"to":200,"source_id":"a","status":"success"}),
    )
    .unwrap();
    assert_eq!(success["total"], 1);
    assert_eq!(success["records"][0]["attempt_id"], "ok");
}
