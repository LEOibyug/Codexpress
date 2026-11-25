mod analytics;
pub(crate) mod billing;
pub mod diagnostics;
mod grouping;
mod models;
mod requests;
mod settlement;
mod summary;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
pub(crate) use settlement::identity as billing_identity;
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_30m_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_12h_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub reported_total_tokens: Option<u64>,
    pub stream_complete: bool,
    pub billing_model: Option<String>,
    pub service_tier: Option<String>,
    pub cache_write_retention: Option<String>,
    pub source_kind: Option<String>,
    pub is_subscription: Option<bool>,
    pub source_identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_peer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_cost: Option<ReportedCost>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReportedCost {
    pub usd: f64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ingress {
    Native,
    Ssh,
    Omp,
    Chatbot,
    Image,
    #[default]
    Unknown,
}

impl Ingress {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Ssh => "ssh",
            Self::Omp => "omp",
            Self::Chatbot => "chatbot",
            Self::Image => "image",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "native" => Some(Self::Native),
            "ssh" => Some(Self::Ssh),
            "omp" => Some(Self::Omp),
            "chatbot" => Some(Self::Chatbot),
            "image" => Some(Self::Image),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    pub(crate) fn from_extensions(extensions: &http::Extensions) -> Self {
        if extensions.get::<crate::omp_ingress::OmpIngress>().is_some() {
            Self::Omp
        } else if extensions.get::<crate::chatbot::ChatbotIngress>().is_some() {
            Self::Chatbot
        } else if extensions.get::<crate::remote::RemoteIngress>().is_some() {
            Self::Ssh
        } else {
            Self::Native
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageRecord {
    pub attempt_id: String,
    pub request_id: String,
    pub ingress: Ingress,
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
    analytics_cache: std::collections::VecDeque<analytics::Cached>,
    settlements: settlement::Settlements,
    last_attempt: i64,
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
            CREATE INDEX IF NOT EXISTS usage_source ON usage(source_id);
            CREATE INDEX IF NOT EXISTS usage_source_time ON usage(source_id,started_at);
            CREATE INDEX IF NOT EXISTS usage_model_recent ON usage(model,started_at DESC,attempt DESC,input_tokens,output_tokens);")?;
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
        // Google omitted cachedContentTokenCount on cache misses in earlier builds.
        // Mark complete uncached Google samples as reported before repricing so cache rate includes misses.
        loop {
            let pending = {
                let mut query = connection.prepare(
                    "SELECT attempt,metadata FROM usage WHERE json_extract(metadata,'$.token_details.source_kind')='google' AND cached_input_tokens=0 AND COALESCE(json_extract(metadata,'$.token_details.input_estimated'),0)=0 AND COALESCE(json_extract(metadata,'$.token_details.cached_input_reported'),0)=0 LIMIT 1000",
                )?;
                let values = query
                    .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                values
            };
            if pending.is_empty() {
                break;
            }
            let transaction = connection.transaction()?;
            for (id, metadata) in pending {
                let mut metadata: Value =
                    serde_json::from_str(&metadata).unwrap_or_else(|_| json!({}));
                if let Some(details) = metadata
                    .get_mut("token_details")
                    .and_then(Value::as_object_mut)
                {
                    details.insert("cached_input_reported".into(), json!(true));
                }
                transaction.execute(
                    "UPDATE usage SET metadata=?1 WHERE attempt=?2",
                    params![metadata.to_string(), id],
                )?;
            }
            transaction.commit()?;
        }
        // Fill legacy records and revisit previously unavailable prices once per
        // catalog revision. Existing priced snapshots are never rewritten.
        let revision = crate::pricing::catalog()["revision"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        loop {
            let pending = {
                let mut query = connection.prepare("SELECT attempt,request_id,source_id,source_name,model,started_at,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms,metadata FROM usage WHERE json_type(metadata,'$.cost') IS NULL OR (json_extract(metadata,'$.cost.status') IN ('unknown_model','unknown_tier') AND json_extract(metadata,'$.cost.revision') IS NOT ?1) LIMIT 1000")?;
                let values = query
                    .query_map([&revision], |row| {
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
                let historical = metadata["cost"]["historical"].as_bool().unwrap_or(true);
                metadata["cost"] = crate::pricing::estimate(&record, historical);
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
        let last_attempt =
            connection.query_row("SELECT COALESCE(MAX(attempt),0) FROM usage", [], |row| {
                row.get(0)
            })?;
        Ok(Self {
            data: Mutex::new(StoreData {
                connection,
                active: HashMap::new(),
                analytics_cache: Default::default(),
                settlements: Default::default(),
                last_attempt,
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
        // Only bounded attempt metadata enters SQLite; no bodies or headers.
        let cached = if record.token_details.input_estimated {
            record.cached_input_tokens
        } else {
            record.cached_input_tokens.min(record.input_tokens)
        };
        let mut value = serde_json::to_value(&record).expect("usage metadata");
        value["cached_input_tokens"] = json!(cached);
        let cost = crate::pricing::estimate(&value, false);
        let billing_identity = data.settlements.finish(&record);
        let metadata = json!({"attempt_id":record.attempt_id,"ingress":record.ingress,"token_details":record.token_details,
            "first_token_latency_ms":record.first_token_latency_ms,"generation_ms":record.generation_ms,
            "generation_timing_version":1,
            "billing_identity":billing_identity,"cost":cost}).to_string();
        let changes = data.connection.total_changes();
        let result = data.connection.execute("INSERT INTO usage (request_id,source_id,source_name,model,started_at,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms,metadata) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)", params![record.request_id,record.source_id,record.source_name,record.model,number(record.started_at),record.status,number(record.input_tokens),number(record.output_tokens),number(cached),record.complete,number(record.latency_ms),metadata]);
        data.active.remove(&record.attempt_id);
        result?;
        analytics::preserve_after_write(&mut data, changes, None);
        let attempt = data.connection.last_insert_rowid();
        data.last_attempt = data.last_attempt.max(attempt);
        if let Some(identity) = billing_identity {
            data.settlements.charge(
                &record.source_id,
                &identity,
                attempt,
                settlement::Cost::read(&cost),
            );
        }
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
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        analytics::build(&mut data, payload)
    }
    pub fn models(&self) -> Result<Value> {
        let data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        models::build(&data.connection)
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
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        let changes = data.connection.total_changes();
        let affected = if data.analytics_cache.is_empty() {
            None
        } else {
            let legacy = capture.attempt_id.strip_prefix("legacy:").and_then(|id| id.parse::<i64>().ok());
            data.connection.query_row(
                "SELECT MIN(attempt) FROM (
                    SELECT attempt FROM usage WHERE json_extract(metadata,'$.attempt_id')=?1
                    UNION ALL SELECT attempt FROM usage WHERE attempt=?2 AND NULLIF(json_extract(metadata,'$.attempt_id'),'') IS NULL
                )",
                params![capture.attempt_id, legacy], |row| row.get(0),
            )?
        };
        let result = diagnostics::save(&data.connection, capture, include_body);
        analytics::preserve_after_write(&mut data, changes, affected);
        result
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
    use super::*;
    #[test]
    fn ingress_survives_active_updates_completion_and_reopen_without_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        let now = crate::model::now();
        let query = json!({"from":now-1,"to":now+86400,"ingress":"omp"});
        {
            let store = UsageStore::open(&path).unwrap();
            for status in ["success", "failed", "interrupted"] {
                let mut record = requests::tests::record(status, now);
                record.ingress = Ingress::Omp;
                record.status = "connecting".into();
                store.start(&record);
                assert_eq!(store.requests(&query).unwrap()["records"][0]["ingress"], "omp");
                record.status = "streaming".into();
                store.update(&record);
                let detail = store.detail(&json!({"attempt_id":status,"record_only":true})).unwrap();
                assert_eq!(detail["record"]["ingress"], "omp");
                assert_eq!(detail["record"]["status"], "streaming");
                record.status = status.into();
                record.complete = status == "success";
                store.record(record).unwrap();
            }
        }
        let store = UsageStore::open(&path).unwrap();
        let records = store.requests(&query).unwrap();
        assert_eq!(records["total"], 3);
        assert!(store.summary(1).unwrap()["recent"].as_array().unwrap().iter().all(|r| r["ingress"] == "omp"));
        for status in ["success", "failed", "interrupted"] {
            for record_only in [false, true] {
                let detail = store.detail(&json!({"attempt_id":status,"record_only":record_only})).unwrap();
                assert_eq!(detail["record"]["ingress"], "omp");
                assert_eq!(detail["record"]["status"], status);
                assert!(detail["diagnostic"].is_null());
            }
        }
    }

    #[test]
    fn named_supplier_statistics_merge_without_merging_attempts_or_settlement() {
        let dir = tempfile::tempdir().unwrap();
        let store = UsageStore::open(&dir.path().join("usage.sqlite")).unwrap();
        let now = crate::model::now();
        let source = |id: &str| crate::model::Source {
            id: id.into(),
            name: "Shared supplier".into(),
            kind: "api".into(),
            show_in_cmux: true,
            is_subscription: false,
            base_url: format!("https://{id}.example.invalid"),
            enabled: true,
            priority: 0,
            max_active_conversations: None,
            active_conversation_window_seconds: 15,
            proxy: crate::model::ProxyPolicy { mode: "direct".into(), url: None },
            models: Vec::new(),
            account_id: None,
            secret: Value::Null,
        };
        let a_source = source("a");
        let mut b_source = source("b");
        b_source.name.push(' ');
        let mut c_source = source("c");
        c_source.name = format!("\u{a0}{}\t", c_source.name);
        let mut a = requests::tests::record("actual-a", now);
        a.source_name = a_source.name.clone();
        a.token_details.source_kind = Some("api".into());
        a.token_details.reasoning_output_tokens = Some(2);
        a.token_details.upstream_cost = Some(ReportedCost {
            usd: 1.25,
            method: "fixture-ledger".into(),
            record_id: Some("actual-a".into()),
        });
        store.start_for_source(&a, &a_source);
        store.record(a.clone()).unwrap();
        let mut b = UsageRecord {
            attempt_id: "estimated-b".into(),
            source_id: "b".into(),
            source_name: b_source.name.clone(),
            input_tokens: 200,
            output_tokens: 30,
            cached_input_tokens: 50,
            latency_ms: 400,
            first_token_latency_ms: Some(30),
            generation_ms: Some(200),
            ..a.clone()
        };
        b.token_details.upstream_cost = None;
        b.token_details.input_estimated = true;
        b.token_details.reasoning_output_tokens = Some(3);
        store.start_for_source(&b, &b_source);
        store.record(b.clone()).unwrap();
        let mut unknown = UsageRecord {
            attempt_id: "unknown-b".into(),
            model: "unknown-supplier-model".into(),
            input_tokens: 40,
            output_tokens: 4,
            cached_input_tokens: 0,
            latency_ms: 700,
            first_token_latency_ms: None,
            generation_ms: None,
            ..b.clone()
        };
        unknown.token_details.input_estimated = false;
        unknown.token_details.cached_input_reported = false;
        unknown.token_details.reasoning_output_tokens = None;
        store.start_for_source(&unknown, &b_source);
        store.record(unknown.clone()).unwrap();
        let mut live = UsageRecord {
            attempt_id: "active-c".into(),
            source_id: "c".into(),
            source_name: c_source.name.clone(),
            input_tokens: 7,
            output_tokens: 3,
            status: "streaming".into(),
            complete: false,
            ..unknown
        };
        live.token_details.input_estimated = true;
        store.start_for_source(&live, &c_source);

        let summary = store.summary(1).unwrap();
        assert_eq!(summary["rows"].as_array().unwrap().len(), 1);
        let supplier = &summary["rows"][0];
        assert_eq!(supplier["source_name"], "Shared supplier");
        assert!(supplier["source_id"].is_null());
        assert_eq!(supplier["source_ids"], json!(["a", "b", "c"]));
        assert_eq!(supplier["requests"], 4);
        assert_eq!(supplier["active_requests"], 1);
        assert_eq!(supplier["timed_output_tokens"], 40);
        assert_eq!(supplier["total_generation_ms"], 250);
        assert_eq!(supplier["throughput_output_tokens"], 40);
        assert_eq!(supplier["throughput_latency_ms"], 250);
        assert_eq!(supplier["total_first_token_ms"], 40);
        assert_eq!(supplier["first_token_samples"], 2);
        assert_eq!(supplier["cached_reported_input_tokens"], 100);
        assert_eq!(supplier["cached_reported_tokens"], 20);
        assert_eq!(supplier["estimated_requests"], 2);
        assert_eq!(supplier["reasoning_output_tokens"], 5);
        assert_eq!(summary["groups"].as_array().unwrap().len(), 2);
        let payload = json!({"from":now-1,"to":now+86_399,"day_starts":[now-1,now+86_399]});
        let analytics = store.analytics(&payload).unwrap();
        assert_eq!(analytics["groups"].as_array().unwrap().len(), 2);
        for output in [&summary, &analytics] {
            let groups = output["groups"].as_array().unwrap();
            let known = groups.iter().find(|row| row["model"] == a.model).unwrap();
            assert_eq!(known["source_ids"], json!(["a", "b"]));
            assert!(known["source_id"].is_null());
            assert_eq!(known["requests"], 2);
            assert_eq!(known["priced_requests"], 2);
            assert_eq!(known["upstream_priced_requests"], 1);
            assert_eq!(known["estimated_cost_requests"], 1);
            assert_eq!(known["timed_output_tokens"], 40);
            assert_eq!(known["total_generation_ms"], 250);
            assert_eq!(known["throughput_output_tokens"], 40);
            assert_eq!(known["throughput_latency_ms"], 250);
            let unknown = groups.iter().find(|row| row["model"] == live.model).unwrap();
            assert_eq!(unknown["source_ids"], json!(["b", "c"]));
            assert_eq!(unknown["requests"], 2);
            assert_eq!(unknown["throughput_output_tokens"], 0);
            assert_eq!(unknown["throughput_latency_ms"], 0);
            assert_eq!(unknown["unpriced_requests"], 2);
            assert_eq!(unknown["priced_requests"], 0);
            assert!(unknown["total_usd"].is_null());
            assert!(unknown["reasoning_output_tokens"].is_null());
        }
        assert_eq!(analytics["options"]["sources"].as_array().unwrap().len(), 3);
        let mut filtered_payload = payload.clone();
        filtered_payload["source_id"] = json!("a");
        let filtered = store.analytics(&filtered_payload).unwrap();
        assert_eq!(filtered["groups"].as_array().unwrap().len(), 1);
        assert_eq!(filtered["groups"][0]["source_id"], "a");
        assert_eq!(filtered["groups"][0]["source_ids"], json!(["a"]));
        assert_eq!(filtered["groups"][0]["requests"], 1);
        assert_eq!(filtered["groups"][0]["total_usd"], 1.25);
        assert_eq!(filtered["options"], analytics["options"]);
        let selected = store.summary_for(1, Some("a")).unwrap();
        assert_eq!(summary["recent"].as_array().unwrap().len(), 4);
        assert_eq!(selected["rows"], summary["rows"]);
        assert_eq!(selected["recent"].as_array().unwrap().len(), 1);
        let attempt = &selected["recent"][0];
        assert_eq!(attempt["attempt_id"], "actual-a");
        assert_eq!(attempt["source_id"], "a");
        assert!(attempt.get("source_ids").is_none());
        assert_eq!(attempt["cost"]["total_usd"], 1.25);
        assert_eq!(attempt["cost"]["upstream_record_id"], "actual-a");
        let requests = store.requests(&json!({"from":now-1,"to":now+1,"source_id":"a"})).unwrap();
        assert_eq!(requests["total"], 1);
        assert_eq!(requests["records"][0]["source_id"], "a");
        assert_eq!(requests["records"][0]["cost"], attempt["cost"]);
        let all_requests = store.requests(&json!({"from":now-1,"to":now+1})).unwrap();
        assert_eq!(all_requests["total"], 4);
        assert_eq!(all_requests["active_count"], 1);
        for record in all_requests["records"].as_array().unwrap() {
            assert!(matches!(record["source_id"].as_str(), Some("a" | "b" | "c")));
            assert!(record.get("source_ids").is_none());
        }
        assert_eq!(store.active_by_source().get("c"), Some(&1));
        for index in 0..49 {
            let next = UsageRecord { attempt_id: format!("settlement-a-{index}"), ..a.clone() };
            store.start_for_source(&next, &a_source);
            store.record(next).unwrap();
        }
        assert!(store.billing_claim(&a_source, false));
        assert!(!store.billing_claim(&b_source, false));
        assert!(!store.billing_claim(&c_source, false));
        for (id, name) in [
            ("unnamed-a", ""),
            ("unnamed-b", "\u{a0} \t"),
            ("case", "shared supplier"),
        ] {
            store.record(UsageRecord {
                attempt_id: id.into(),
                source_id: id.into(),
                source_name: name.into(),
                ..a.clone()
            }).unwrap();
        }
        let distinct = store.summary(1).unwrap();
        assert_eq!(distinct["rows"].as_array().unwrap().len(), 4);
        for id in ["unnamed-a", "unnamed-b", "case"] {
            let row = distinct["rows"].as_array().unwrap().iter().find(|row| row["source_id"] == id).unwrap();
            assert_eq!(row["source_ids"], json!([id]));
            assert_eq!(row["requests"], 1);
        }
    }
    #[test]
    fn costs_group_all_requests_by_source_and_model_and_snapshot_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        let store = UsageStore::open(&path).unwrap();
        let base = UsageRecord {
            attempt_id: "priced".into(),
            request_id: "r".into(),
            ingress: Ingress::Native,
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
    fn new_catalog_fills_unpriced_requests_without_changing_existing_costs_or_source_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        drop(UsageStore::open(&path).unwrap());
        let db = Connection::open(&path).unwrap();
        let metadata = json!({"token_details":{"source_kind":"api","is_subscription":true,"output_available":true,"cached_input_reported":true},
            "cost":{"status":"unknown_model","revision":"before-terra","historical":false}});
        let priced = json!({"status":"reported","total_usd":9.0,"revision":"older-price","historical":false});
        for (id, model, cost) in [
            ("terra", "gpt-6.1-terra", metadata["cost"].clone()),
            ("unknown", "unknown-model", metadata["cost"].clone()),
            ("priced", "gpt-6.1-terra", priced.clone()),
        ] {
            let mut meta = metadata.clone();
            meta["cost"] = cost;
            db.execute("INSERT INTO usage (request_id,source_id,source_name,model,started_at,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms,metadata) VALUES (?1,?1,?1,?2,?3,'success',512000,1000,400000,1,20,?4)", params![id, model, crate::model::now(), meta.to_string()]).unwrap();
        }
        drop(db);
        let store = UsageStore::open(&path).unwrap();
        // A changed source setting must not replace the captured subscription flag.
        store
            .reprice_historical(&[("terra".into(), "api".into(), false)])
            .unwrap();
        let terra = store.summary_for(1, Some("terra")).unwrap()["recent"][0]["cost"].clone();
        assert_eq!(terra["historical"], false);
        assert_eq!(terra["long_context_waived"], true);
        assert!((terra["total_usd"].as_f64().unwrap() - 0.137).abs() < 1e-9);
        assert_eq!(
            store.summary_for(1, Some("priced")).unwrap()["recent"][0]["cost"],
            priced
        );
        let unknown = store.summary_for(1, Some("unknown")).unwrap()["recent"][0]["cost"].clone();
        assert_eq!(unknown["status"], "unknown_model");
        assert_eq!(unknown["revision"], crate::pricing::catalog()["revision"]);
        drop(store);
        let reopened = UsageStore::open(&path).unwrap();
        assert_eq!(
            reopened.summary_for(1, Some("terra")).unwrap()["recent"][0]["cost"],
            terra
        );
        assert_eq!(
            reopened.summary_for(1, Some("unknown")).unwrap()["recent"][0]["cost"],
            unknown
        );
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
                ingress: Ingress::Native,
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
            ingress: Ingress::Native,
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
                latency_ms: 12_000,
                first_token_latency_ms: Some(7000),
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
        assert_eq!(selected["recent"][0]["output_tps"], 20.0);
        let legacy = store.summary_for(1, Some("a")).unwrap();
        assert_eq!(legacy["recent"][0]["attempt_id"], "legacy:1");
        assert_eq!(legacy["recent"][0]["output_tps"], 40.0);
        assert_eq!(legacy["rows"][0]["throughput_output_tokens"], 20);
        assert_eq!(legacy["rows"][0]["throughput_latency_ms"], 500);
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

    #[test]
    fn reopening_store_backfills_google_cache_miss_reporting_and_prices_gemini4() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        drop(UsageStore::open(&path).unwrap());
        let db = Connection::open(&path).unwrap();
        let metadata = json!({
            "token_details": {
                "source_kind": "google",
                "is_subscription": false,
                "input_estimated": false,
                "output_estimated": false,
                "output_available": true,
                "cached_input_reported": false
            },
            "cost": {
                "status": "unknown_model",
                "revision": "openai-2026-11-18-v1",
                "historical": false
            }
        });
        db.execute(
            "INSERT INTO usage (request_id,source_id,source_name,model,started_at,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms,metadata) VALUES ('g-miss','g','Google','gemini-4-hydrogen',?1,'success',1000,200,0,1,1000,?2)",
            params![crate::model::now(), metadata.to_string()],
        )
        .unwrap();
        drop(db);

        let reopened = UsageStore::open(&path).unwrap();
        let summary = reopened.summary_for(1, Some("g")).unwrap();
        let item = &summary["recent"][0];
        assert_eq!(item["token_details"]["cached_input_reported"], true);
        assert_eq!(item["cost"]["status"], "reported");
        // 1000 * 1.5/1M + 200 * 7.5/1M = 0.0015 + 0.0015 = 0.003
        assert!((item["cost"]["total_usd"].as_f64().unwrap() - 0.003).abs() < 1e-9);
        assert_eq!(summary["rows"][0]["cached_reported_input_tokens"], 1000);
        assert_eq!(summary["rows"][0]["cached_reported_tokens"], 0);
    }
}
