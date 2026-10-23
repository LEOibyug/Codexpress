mod analytics;
pub mod diagnostics;
mod requests;
mod summary;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, sync::Mutex, time::Instant};
use summary::stored_record;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenDetails {
    pub input_estimated: bool,
    pub output_estimated: bool,
    pub output_available: bool,
    pub cached_input_reported: bool,
    pub cache_write_input_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub reported_total_tokens: Option<u64>,
    pub stream_complete: bool,
    pub billing_model: Option<String>,
    pub service_tier: Option<String>,
    pub cache_write_retention: Option<String>,
    pub source_kind: Option<String>,
    pub is_subscription: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageRecord {
    pub attempt_id: String,
    pub request_id: String,
    pub source_id: String,
    pub source_name: String,
    pub model: String,
    pub started_at: u64,
    pub status: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// A subset of input_tokens, never added again to the total.
    pub cached_input_tokens: u64,
    pub token_details: TokenDetails,
    pub complete: bool,
    pub latency_ms: u64,
    pub first_token_latency_ms: Option<u64>,
    pub generation_ms: Option<u64>,
}

struct StoreData {
    connection: Connection,
    active: HashMap<String, (UsageRecord, Instant)>,
}
pub struct UsageStore {
    data: Mutex<StoreData>,
}
impl UsageStore {
    pub fn open(path: &Path) -> Result<Self> {
        let mut connection = Connection::open(path).context("无法打开用量数据库")?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
            CREATE TABLE IF NOT EXISTS usage (
                attempt INTEGER PRIMARY KEY, request_id TEXT NOT NULL, source_id TEXT NOT NULL,
                source_name TEXT NOT NULL, model TEXT NOT NULL, started_at INTEGER NOT NULL,
                status TEXT NOT NULL, input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
                cached_input_tokens INTEGER NOT NULL, complete INTEGER NOT NULL, latency_ms INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS usage_time ON usage(started_at);
            CREATE INDEX IF NOT EXISTS usage_source ON usage(source_id);")?;
        let has_metadata = connection
            .prepare("PRAGMA table_info(usage)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|name| name == "metadata");
        if !has_metadata {
            connection.execute(
                "ALTER TABLE usage ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}'",
                [],
            )?;
        }
        diagnostics::initialize(&connection)?;
        // Backfill once with today's reference list, retaining an explicit
        // historical marker. Subsequent price updates never rewrite snapshots.
        loop {
            let pending = {
                let mut query = connection.prepare("SELECT attempt,request_id,source_id,source_name,model,started_at,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms,metadata FROM usage WHERE json_type(metadata,'$.cost') IS NULL LIMIT 1000")?;
                let values = query
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(12)?,
                            stored_record(row)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                values
            };
            if pending.is_empty() {
                break;
            }
            let transaction = connection.transaction()?;
            for (id, metadata, record) in pending {
                let mut metadata: Value =
                    serde_json::from_str(&metadata).unwrap_or_else(|_| json!({}));
                metadata["cost"] = crate::pricing::estimate(&record, true);
                transaction.execute(
                    "UPDATE usage SET metadata=?1 WHERE attempt=?2",
                    params![metadata.to_string(), id],
                )?;
            }
            transaction.commit()?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self {
            data: Mutex::new(StoreData {
                connection,
                active: HashMap::new(),
            }),
        })
    }
    /// In-memory counters only: status polling does not query SQLite or expose records.
    pub fn active_by_source(&self) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        if let Ok(data) = self.data.lock() {
            for (record, _) in data.active.values() {
                *counts.entry(record.source_id.clone()).or_insert(0) += 1;
            }
        }
        counts
    }
    pub fn start(&self, record: &UsageRecord) {
        if let Ok(mut data) = self.data.lock() {
            data.active
                .insert(record.attempt_id.clone(), (record.clone(), Instant::now()));
        }
    }
    pub fn update(&self, record: &UsageRecord) {
        if let Ok(mut data) = self.data.lock() {
            if let Some((active, _)) = data.active.get_mut(&record.attempt_id) {
                *active = record.clone();
            }
        }
    }
    pub fn record(&self, record: UsageRecord) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        let number = |v: u64| v.min(i64::MAX as u64) as i64;
        // One lock prevents a poll from seeing both the active and persisted attempt.
        // Only fixed numeric metadata enters SQLite; no bodies or headers.
        let cached = if record.token_details.input_estimated {
            record.cached_input_tokens
        } else {
            record.cached_input_tokens.min(record.input_tokens)
        };
        let mut value = serde_json::to_value(&record).expect("usage metadata");
        value["cached_input_tokens"] = json!(cached);
        let metadata = json!({"attempt_id":record.attempt_id,"token_details":record.token_details,
            "first_token_latency_ms":record.first_token_latency_ms,"generation_ms":record.generation_ms,
            "cost":crate::pricing::estimate(&value,false)}).to_string();
        let result = data.connection.execute("INSERT INTO usage (request_id,source_id,source_name,model,started_at,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms,metadata) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)", params![record.request_id,record.source_id,record.source_name,record.model,number(record.started_at),record.status,number(record.input_tokens),number(record.output_tokens),number(cached),record.complete,number(record.latency_ms),metadata]);
        data.active.remove(&record.attempt_id);
        result?;
        Ok(())
    }
    pub fn summary(&self, days: u32) -> Result<Value> {
        self.summary_for(days, None)
    }
    /// Only pre-pricing records use current source settings. New attempts retain
    /// their original cost snapshots even when a source is edited or removed.
    pub fn reprice_historical(&self, sources: &[(String, String, bool)]) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        let revision = crate::pricing::catalog()["revision"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        for (source_id, kind, subscription) in sources {
            loop {
                let pending = {
                    let mut query = data.connection.prepare("SELECT attempt,request_id,source_id,source_name,model,started_at,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms,metadata FROM usage WHERE source_id=?1 AND json_extract(metadata,'$.cost.historical')=1 AND (json_extract(metadata,'$.token_details.source_kind') IS NOT ?2 OR json_extract(metadata,'$.token_details.is_subscription') IS NOT ?3 OR json_extract(metadata,'$.cost.revision') IS NOT ?4) LIMIT 1000")?;
                    let values = query
                        .query_map(params![source_id, kind, subscription, revision], |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(12)?,
                                stored_record(row)?,
                            ))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    values
                };
                if pending.is_empty() {
                    break;
                }
                let transaction = data.connection.transaction()?;
                for (id, metadata, mut record) in pending {
                    record["token_details"]["source_kind"] = json!(kind);
                    record["token_details"]["is_subscription"] = json!(subscription);
                    let mut metadata: Value = serde_json::from_str(&metadata)?;
                    metadata["token_details"] = record["token_details"].clone();
                    metadata["cost"] = crate::pricing::estimate(&record, true);
                    transaction.execute(
                        "UPDATE usage SET metadata=?1 WHERE attempt=?2",
                        params![metadata.to_string(), id],
                    )?;
                }
                transaction.commit()?;
            }
        }
        Ok(())
    }
    pub fn summary_for(&self, days: u32, source_id: Option<&str>) -> Result<Value> {
        self.summary_filtered(days, source_id, None)
    }
    pub fn analytics(&self, payload: &Value) -> Result<Value> {
        let data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        analytics::build(&data, payload)
    }
    pub fn requests(&self, payload: &Value) -> Result<Value> {
        let data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        requests::build(&data, payload)
    }
    pub fn detail(&self, payload: &Value) -> Result<Value> {
        let data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        diagnostics::detail(&data, payload)
    }
    pub fn diagnostic(&self, capture: &diagnostics::Capture, include_body: bool) -> Result<()> {
        let data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        diagnostics::save(&data.connection, capture, include_body)
    }
    pub fn summary_filtered(
        &self,
        days: u32,
        source_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<Value> {
        let since = crate::model::now().saturating_sub(u64::from(days.clamp(1, 3660)) * 86400);
        let data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        summary::build(&data, since, source_id, model)
    }
}

/// Quarter-token units: CJK scripts approximately one token per character,
/// other text approximately four characters per token. No tokenizer claim.
pub fn text_units(text: &str) -> u64 {
    text.chars().map(|c| if matches!(c as u32, 0x2e80..=0x9fff | 0xac00..=0xd7af | 0xf900..=0xfaff | 0x20000..=0x323af | 0xff00..=0xffef) { 4 } else { 1 }).sum()
}
pub fn content_units(value: &Value) -> u64 {
    match value {
        Value::String(text) if !text.starts_with("data:") => text_units(text),
        Value::Array(items) => items.iter().map(content_units).sum(),
        Value::Object(fields)
            if !matches!(
                fields.get("type").and_then(Value::as_str),
                Some("input_image" | "input_audio" | "input_file" | "output_audio")
            ) =>
        {
            fields
                .iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "encrypted_content"
                            | "image_url"
                            | "file_url"
                            | "file_id"
                            | "file_data"
                            | "audio"
                            | "data"
                            | "url"
                            | "id"
                            | "call_id"
                            | "type"
                            | "role"
                            | "status"
                            | "annotations"
                    )
                })
                .map(|(_, value)| content_units(value))
                .sum()
        }
        _ => 0,
    }
}
pub fn estimated_tokens(units: u64) -> u64 {
    units.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::{summary::add_rate, *};
    #[test]
    fn buffered_output_uses_total_duration_not_tiny_delivery_window_or_input_tokens() {
        let mut record = json!({"input_tokens":348804,"output_tokens":375,"latency_ms":18179,"generation_ms":342,"token_details":{"output_available":true}});
        add_rate(&mut record);
        assert!((record["output_tps"].as_f64().unwrap() - 20.628197).abs() < 0.00001);
        assert!(record["generation_tps"].as_f64().unwrap() > 1000.0);
    }
    #[test]
    fn costs_group_all_requests_by_source_and_model_and_snapshot_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        let store = UsageStore::open(&path).unwrap();
        let base = UsageRecord {
            attempt_id: "priced".into(),
            request_id: "r".into(),
            source_id: "a".into(),
            source_name: "A".into(),
            model: "gpt-6.1-sol".into(),
            started_at: crate::model::now(),
            status: "success".into(),
            input_tokens: 1000,
            output_tokens: 100,
            cached_input_tokens: 500,
            token_details: TokenDetails {
                output_available: true,
                cached_input_reported: true,
                ..Default::default()
            },
            complete: true,
            latency_ms: 100,
            first_token_latency_ms: Some(5),
            generation_ms: Some(50),
        };
        for n in 0..205 {
            store
                .record(UsageRecord {
                    attempt_id: format!("a-{n}"),
                    ..base.clone()
                })
                .unwrap();
        }
        let b = UsageRecord {
            attempt_id: "b".into(),
            source_id: "b".into(),
            source_name: "B".into(),
            model: "gpt-6-astra".into(),
            ..base.clone()
        };
        store.start(&b);
        assert_eq!(store.active_by_source().get("b"), Some(&1));
        assert!(!store.active_by_source().contains_key("a"));
        store.start(&b); // Updating the same attempt never double counts.
        assert_eq!(store.active_by_source().get("b"), Some(&1));
        let active = store
            .summary_filtered(1, Some("b"), Some("gpt-6-astra"))
            .unwrap();
        assert_eq!(active["recent"].as_array().unwrap().len(), 1);
        assert_eq!(active["groups"].as_array().unwrap().len(), 2);
        assert_eq!(active["rows"][0]["requests"], 205);
        assert_eq!(active["rows"][1]["active_requests"], 1);
        store.record(b).unwrap();
        assert!(store.active_by_source().is_empty());
        let all = store.summary(1).unwrap();
        assert_eq!(all["recent"].as_array().unwrap().len(), 200);
        assert_eq!(all["rows"][0]["priced_requests"], 205);
        assert!((all["rows"][0]["total_usd"].as_f64().unwrap() - 0.42025).abs() < 1e-9);
        assert_eq!(all["rows"][1]["requests"], 1);
        assert_eq!(all["inflight"], 0);
        let snapshot = all["recent"][0]["cost"].clone();
        drop(store);
        let db = Connection::open(&path).unwrap();
        db.execute("INSERT INTO usage (request_id,source_id,source_name,model,started_at,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms,metadata) VALUES ('legacy','c','C','gpt-6.1-sol',?1,'success',512000,100,0,1,20,'{}')",[crate::model::now()]).unwrap();
        drop(db);
        let reopened = UsageStore::open(&path).unwrap();
        let restored = reopened.summary_filtered(1, Some("b"), None).unwrap();
        assert_eq!(restored["recent"][0]["cost"], snapshot);
        let old = reopened.summary_filtered(1, Some("c"), None).unwrap();
        assert_eq!(old["recent"][0]["cost"]["historical"], true);
        assert_eq!(old["recent"][0]["cost"]["status"], "estimated");
        assert!(old["recent"][0]["cost"]["total_usd"].is_number());
        assert_eq!(old["recent"][0]["cost"]["rates"]["input"], 4.0);
        reopened
            .reprice_historical(&[
                ("c".into(), "api".into(), true),
                ("b".into(), "api".into(), true),
            ])
            .unwrap();
        let subscribed = reopened.summary_filtered(1, Some("c"), None).unwrap();
        assert_eq!(subscribed["recent"][0]["cost"]["long_context_waived"], true);
        assert_eq!(subscribed["recent"][0]["cost"]["rates"]["input"], 2.0);
        assert_eq!(
            reopened.summary_filtered(1, Some("b"), None).unwrap()["recent"][0]["cost"],
            snapshot
        );
        reopened
            .reprice_historical(&[("c".into(), "api".into(), false)])
            .unwrap();
        assert_eq!(
            reopened.summary_filtered(1, Some("c"), None).unwrap()["recent"][0]["cost"]["rates"]
                ["input"],
            4.0
        );
        reopened
            .reprice_historical(&[("c".into(), "official".into(), false)])
            .unwrap();
        assert_eq!(
            reopened.summary_filtered(1, Some("c"), None).unwrap()["recent"][0]["cost"]
                ["billing_basis"],
            "official_subscription_reference"
        );
        assert!(reopened
            .summary_filtered(1, Some("a"), Some("gpt-6-astra"))
            .unwrap()["recent"]
            .as_array()
            .unwrap()
            .is_empty());
    }
    #[test]
    fn persisted_attempts_keep_name_snapshot_and_cached_input_subset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        {
            let store = UsageStore::open(&path).unwrap();
            let first = UsageRecord {
                attempt_id: "first".into(),
                request_id: "same-request".into(),
                source_id: "removed-source".into(),
                source_name: "旧名称".into(),
                model: "m".into(),
                started_at: crate::model::now(),
                status: "failed".into(),
                input_tokens: 0,
                output_tokens: 0,
                cached_input_tokens: 0,
                token_details: TokenDetails::default(),
                complete: false,
                latency_ms: 1,
                first_token_latency_ms: None,
                generation_ms: None,
            };
            store.record(first.clone()).unwrap();
            store
                .record(UsageRecord {
                    attempt_id: "second".into(),
                    source_name: "新名称".into(),
                    status: "success".into(),
                    input_tokens: 10,
                    output_tokens: 3,
                    cached_input_tokens: 99,
                    complete: true,
                    ..first
                })
                .unwrap();
        }
        let summary = UsageStore::open(&path).unwrap().summary(30).unwrap();
        assert_eq!(summary["rows"][0]["source_name"], "新名称");
        assert_eq!(summary["rows"][0]["requests"], 2);
        assert_eq!(summary["rows"][0]["failures"], 1);
        assert_eq!(summary["rows"][0]["incomplete_requests"], 1);
        assert_eq!(summary["rows"][0]["input_tokens"], 10);
        assert_eq!(summary["rows"][0]["cached_input_tokens"], 10);
        assert_eq!(summary["recent"][1]["source_name"], "旧名称");
    }

    #[test]
    fn legacy_database_migrates_and_source_filter_keeps_all_source_totals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        let old = Connection::open(&path).unwrap();
        old.execute_batch("CREATE TABLE usage (attempt INTEGER PRIMARY KEY, request_id TEXT NOT NULL, source_id TEXT NOT NULL, source_name TEXT NOT NULL, model TEXT NOT NULL, started_at INTEGER NOT NULL, status TEXT NOT NULL, input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL, cached_input_tokens INTEGER NOT NULL, complete INTEGER NOT NULL, latency_ms INTEGER NOT NULL);").unwrap();
        old.execute(
            "INSERT INTO usage VALUES (1,'old','a','Old name','model',?1,'success',100,20,0,1,500)",
            [crate::model::now()],
        )
        .unwrap();
        drop(old);
        let store = UsageStore::open(&path).unwrap();
        let record = UsageRecord {
            attempt_id: "new-b".into(),
            request_id: "r-b".into(),
            source_id: "b".into(),
            source_name: "Provider B".into(),
            model: "fixture".into(),
            started_at: crate::model::now(),
            status: "success".into(),
            input_tokens: 40,
            output_tokens: 10,
            cached_input_tokens: 0,
            token_details: TokenDetails {
                output_available: true,
                ..Default::default()
            },
            complete: true,
            latency_ms: 1500,
            first_token_latency_ms: Some(500),
            generation_ms: Some(1000),
        };
        store.record(record.clone()).unwrap();
        store
            .record(UsageRecord {
                attempt_id: "new-b-2".into(),
                output_tokens: 100,
                generation_ms: Some(5000),
                ..record
            })
            .unwrap();
        let selected = store.summary_for(1, Some("b")).unwrap();
        assert_eq!(selected["rows"].as_array().unwrap().len(), 2);
        assert_eq!(selected["recent"].as_array().unwrap().len(), 2);
        assert!(selected["recent"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["source_id"] == "b"));
        let b = &selected["rows"][1];
        assert_eq!(b["timed_output_tokens"], 110);
        assert_eq!(b["total_generation_ms"], 6000);
        assert_eq!(b["first_token_samples"], 2);
        assert_eq!(selected["recent"][0]["generation_tps"], 20.0);
        assert!(
            (selected["recent"][0]["output_tps"].as_f64().unwrap() - 100000.0 / 1500.0).abs()
                < 1e-9
        );
        let legacy = store.summary_for(1, Some("a")).unwrap();
        assert_eq!(legacy["recent"][0]["attempt_id"], "legacy:1");
        assert_eq!(legacy["recent"][0]["output_tps"], 40.0);
        assert!(legacy["recent"][0]["generation_tps"].is_null());
        assert!(legacy["recent"][0]["token_details"]["legacy"]
            .as_bool()
            .unwrap());
        drop(store);
        let reopened = UsageStore::open(&path)
            .unwrap()
            .summary_for(1, Some("b"))
            .unwrap();
        assert_eq!(reopened["recent"][0]["attempt_id"], "new-b-2");
    }

    #[test]
    fn character_estimates_exclude_encrypted_and_multimodal_payloads() {
        let input = json!([{"type":"message","content":[{"type":"input_text","text":"你好abcd"},{"type":"input_image","image_url":"data:image/fixture","detail":"high"}]},{"type":"reasoning","encrypted_content":"opaque-blob"},{"type":"input_file","file_data":"a-lot-of-data","filename":"file.pdf"}]);
        assert_eq!(estimated_tokens(content_units(&input)), 3);
    }
}
