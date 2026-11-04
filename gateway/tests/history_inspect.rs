#[path = "common/server.rs"]
mod server;
#[path = "common/source.rs"]
mod source_fixture;

use axum::{body::{to_bytes, Body}, extract::Request, routing::post, Router};
use codexpress_gateway::{history::{CaptureMetadata, HistoryStore, MIN_LIMIT}, management, model::AppState, relay};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

fn directory() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap()
}
fn response(id: &str) -> Value {
    json!({"id":id,"status":"completed","output":[{"role":"assistant","content":"完成🌏"}]})
}
fn metadata(timestamp: u64, attempt: &str) -> CaptureMetadata {
    CaptureMetadata { captured_at: timestamp, source_id: "source-a".into(), source_name: "来源甲".into(), model: "model-a".into(), ingress: "native".into(), request_id: "request-a".into(), attempt_id: attempt.into() }
}
fn page_all(store: &HistoryStore, id: &Value, limit: u64) -> (String, u64) {
    let mut result = String::new();
    let mut offset = 0;
    loop {
        let page = store.inspect_detail(&json!({"id":id,"offset":offset,"limit":limit})).unwrap();
        assert_eq!(page["status"], "complete");
        assert!(page["reason"].is_null());
        let text = page["text"].as_str().unwrap();
        result.push_str(text);
        match page["next_offset"].as_u64() {
            Some(next) => { assert!(next > offset); assert_eq!(next - offset, text.len() as u64); offset = next; }
            None => return (result, page["total_bytes"].as_u64().unwrap()),
        }
    }
}
fn lru(db: &Connection) -> Vec<(String, String, u64)> {
    db.prepare("SELECT 'blocks',hash,last_used FROM blocks UNION ALL SELECT 'access',hash,last_used FROM block_access UNION ALL SELECT 'nodes',hash,last_used FROM nodes UNION ALL SELECT 'snapshots',key,last_used FROM snapshots ORDER BY 1,2").unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

#[test]
fn unicode_tools_and_disabled_reads_preserve_the_retained_cache() {
    let dir = directory();
    let path = dir.path().join("history.sqlite");
    let store = HistoryStore::open(&path).unwrap();
    store.configure(true, MIN_LIMIT).unwrap();
    let secret = "fixture-private-key-crosses-pages";
    let original = format!("{}:{}", "你好🌏".repeat(11000), secret);
    let request = json!({"instructions":format!("遵守约定 {secret}"),"tools":[{"type":"function","name":"查询","description":format!("工具 {secret}"),"parameters":{"type":"object"}}],"input":[{"role":"user","content":original}]});
    store.capture("scope-never-export", &request, &response("first")).unwrap();
    let next = json!({"previous_response_id":"first","instructions":request["instructions"],"tools":request["tools"],"input":[{"type":"function_call","call_id":"tool-1","name":"查询","arguments":"{}"},{"type":"function_call_output","call_id":"tool-1","output":"数据🧪"}]});
    store.capture_with_metadata_at("scope-never-export", &next, &response("second"), store.generation(), Some(&metadata(100, "attempt-two"))).unwrap();
    let listed = store.inspect_list(&json!({})).unwrap();
    assert_eq!(listed["total"], 2);
    assert_eq!(listed["legacy_total"], 1);
    assert_eq!(listed["records"][1]["legacy"], true);
    assert!(listed["records"][1]["captured_at"].is_null());
    let id = &listed["records"][0]["id"];
    store.configure(false, MIN_LIMIT).unwrap();
    let db = Connection::open(&path).unwrap();
    let before = lru(&db);
    let generation = store.generation();
    let (plain, total) = page_all(&store, id, 4093);
    assert_eq!(total, plain.len() as u64);
    let parsed: Value = serde_json::from_str(&plain).unwrap();
    assert_eq!(parsed, json!({"instructions":request["instructions"],"tools":request["tools"],"items":[request["input"][0],response("first")["output"][0],next["input"][0],next["input"][1],response("second")["output"][0]]}));
    assert!(plain.contains(secret));
    assert!(!plain.contains("scope-never-export"));
    let tiny = store.inspect_detail(&json!({"id":id,"offset":plain.find('你').unwrap(),"limit":1})).unwrap();
    assert_eq!(tiny["text"], "你");
    assert_eq!(lru(&db), before);
    assert_eq!(store.generation(), generation);
    assert!(!store.enabled());
    assert!(store.restore("scope-never-export", "second").is_err());
    drop(store);
    let reopened = HistoryStore::open(&path).unwrap();
    assert_eq!(reopened.inspect_list(&json!({"attempt_id":"attempt-two"})).unwrap()["records"][0]["id"], *id);
    reopened.clear().unwrap();
    assert_eq!(reopened.inspect_list(&json!({})).unwrap()["total"], 0);
}

#[test]
fn legacy_schema_migration_and_metadata_filters_use_capture_time_not_lru() {
    let dir = directory();
    let path = dir.path().join("history.sqlite");
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE snapshots(key TEXT PRIMARY KEY,tail TEXT NOT NULL,envelope TEXT NOT NULL,last_used INTEGER NOT NULL); CREATE TABLE blocks(hash TEXT PRIMARY KEY,payload BLOB NOT NULL,raw_bytes INTEGER NOT NULL,last_used INTEGER NOT NULL);").unwrap();
    let envelope = "{\"instructions\":\"旧缓存\"}".as_bytes();
    let envelope_hash = format!("{:x}", Sha256::digest(envelope));
    db.execute("INSERT INTO blocks VALUES(?1,?2,?3,99)", params![envelope_hash,zstd::stream::encode_all(envelope,3).unwrap(),envelope.len()]).unwrap();
    db.execute("INSERT INTO snapshots VALUES(?1,'',?2,99)", params!["a".repeat(64),envelope_hash]).unwrap();
    drop(db);
    let store = HistoryStore::open(&path).unwrap();
    let legacy = store.inspect_list(&json!({})).unwrap();
    assert_eq!(legacy["legacy_total"], 1);
    let (text, _) = page_all(&store, &legacy["records"][0]["id"], 2);
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), json!({"instructions":"旧缓存","items":[]}));
    store.configure(true, MIN_LIMIT).unwrap();
    for (id, time) in [("one", 10), ("two", 20), ("three", 30)] {
        store.capture_with_metadata_at("s", &json!({"input":"输入"}), &response(id), store.generation(), Some(&metadata(time, id))).unwrap();
    }
    let first = store.inspect_list(&json!({"limit":1})).unwrap();
    store.restore("s", "one").unwrap();
    let second = store.inspect_list(&json!({"limit":1,"before":first["next_cursor"]})).unwrap();
    assert_eq!(first["records"][0]["attempt_id"], "three");
    assert_eq!(second["records"][0]["attempt_id"], "two");
    assert_eq!(second["total"], 4);
    for query in [json!({"from":20,"to":30}), json!({"attempt_id":"two"}), json!({"from":20,"to":30,"source_id":"source-a","model":"model-a","ingress":"native"})] {
        let filtered = store.inspect_list(&query).unwrap();
        assert_eq!(filtered["total"], 1);
        assert_eq!(filtered["legacy_total"], 1);
        assert_eq!(filtered["records"][0]["attempt_id"], "two");
    }
    assert_eq!(store.inspect_list(&json!({"source_id":"unknown"})).unwrap()["total"], 0);
    for query in [json!({"scope":"s"}), json!({"limit":0}), json!({"limit":101}), json!({"from":30,"to":20}), json!({"before":-1}), json!({"model":false})] { assert!(store.inspect_list(&query).is_err()); }
    for query in [json!({"id":"not-a-hash"}), json!({"id":"a".repeat(64),"scope":"s"}), json!({"id":"a".repeat(64),"limit":0})] { assert!(store.inspect_detail(&query).is_err()); }
}

#[test]
fn missing_corrupt_and_cyclic_material_never_claims_a_complete_history() {
    for mutation in ["DELETE FROM blocks WHERE hash=(SELECT block FROM nodes LIMIT 1)", "UPDATE blocks SET payload=x'1234'", "DELETE FROM nodes WHERE hash=(SELECT tail FROM snapshots LIMIT 1)", "UPDATE nodes SET parent=hash WHERE hash=(SELECT tail FROM snapshots LIMIT 1)", "UPDATE blocks SET raw_bytes=raw_bytes+1"] {
        let dir = directory();
        let path = dir.path().join("history.sqlite");
        let store = HistoryStore::open(&path).unwrap();
        store.configure(true, MIN_LIMIT).unwrap();
        store.capture("s", &json!({"input":"不要返回部分历史"}), &response("r")).unwrap();
        let id = store.inspect_list(&json!({})).unwrap()["records"][0]["id"].clone();
        Connection::open(&path).unwrap().execute_batch(mutation).unwrap();
        assert_eq!(store.inspect_list(&json!({})).unwrap()["total"], 1);
        let page = store.inspect_detail(&json!({"id":id})).unwrap();
        assert_eq!(page["status"], "incomplete", "{mutation}");
        assert!(page["reason"].as_str().is_some_and(|reason| !reason.is_empty()));
        assert_eq!(page["text"], "");
        assert!(page["next_offset"].is_null());
        assert!(store.restore("s", "r").is_err());
    }
}

#[test]
fn readable_cache_has_no_32_mib_truncation() {
    let dir = directory();
    let store = HistoryStore::open(&dir.path().join("history.sqlite")).unwrap();
    store.configure(true, MIN_LIMIT).unwrap();
    let text = format!("{}终点🧪", "x".repeat(33 * 1024 * 1024));
    store.capture("s", &json!({"input":text}), &response("large")).unwrap();
    let id = store.inspect_list(&json!({})).unwrap()["records"][0]["id"].clone();
    let first = store.inspect_detail(&json!({"id":id,"limit":1})).unwrap();
    let total = first["total_bytes"].as_u64().unwrap();
    assert!(total > 33 * 1024 * 1024);
    let last = store.inspect_detail(&json!({"id":id,"offset":total-128,"limit":128})).unwrap();
    assert_eq!(last["status"], "complete");
    assert!(last["text"].as_str().unwrap().contains("终点🧪"));
    assert!(last["next_offset"].is_null());
}

#[tokio::test]
async fn relay_capture_provenance_links_to_the_real_completed_attempt() {
    let (base, task) = server::serve(Router::new().route("/responses", post(|| async { axum::Json(response("relay-response")) }))).await;
    let dir = directory();
    let state = AppState::new(dir.path().into(), 17863, "local-fixture".into()).unwrap();
    let source = source_fixture::api_source("api", "实际来源", &base, "fixture-upstream-key");
    let mut source_json = serde_json::to_value(&source).unwrap();
    source_json["secret"] = source.secret;
    management::dispatch(&state, "sync", json!({"sources":[source_json]})).await.unwrap();
    state.history.configure(true, MIN_LIMIT).unwrap();
    let request = Request::builder().method("POST").uri("/v1/responses").header("host","127.0.0.1:17863").header("authorization","Bearer local-fixture").header("content-type","application/json").body(Body::from(r#"{"model":"m","input":"实际正文"}"#)).unwrap();
    let output = relay::router(state.clone()).oneshot(request).await.unwrap();
    assert!(output.status().is_success());
    to_bytes(output.into_body(), usize::MAX).await.unwrap();
    let rows = state.history.inspect_list(&json!({"source_id":"api","model":"m","ingress":"native"})).unwrap();
    assert_eq!(rows["total"], 1);
    let row = &rows["records"][0];
    let usage = state.usage.summary(1).unwrap();
    let attempt = &usage["recent"][0];
    for name in ["source_id","source_name","model","request_id","attempt_id"] { assert_eq!(row[name], attempt[name], "{name}"); }
    assert_eq!(row["source_name"], "实际来源");
    assert_eq!(row["legacy"], false);
    assert!(row["captured_at"].as_u64().unwrap() >= attempt["started_at"].as_u64().unwrap());
    assert_eq!(state.history.inspect_list(&json!({"attempt_id":attempt["attempt_id"]})).unwrap()["records"][0]["id"], row["id"]);
    task.abort();
}
