use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, sync::Mutex, time::Instant};

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
        let mut grouped = data.connection.prepare(r#"SELECT source_id,
            (SELECT latest.source_name FROM usage AS latest WHERE latest.source_id=usage.source_id ORDER BY latest.attempt DESC LIMIT 1),
            COUNT(*), SUM(input_tokens), SUM(output_tokens), SUM(cached_input_tokens), SUM(complete=0), SUM(status!='success'),
            SUM(COALESCE(json_extract(metadata,'$.token_details.input_estimated'),0) OR COALESCE(json_extract(metadata,'$.token_details.output_estimated'),0)),
            SUM(json_extract(metadata,'$.token_details.reasoning_output_tokens')), SUM(status='success'), SUM(latency_ms),
            SUM(json_extract(metadata,'$.first_token_latency_ms')), COUNT(json_extract(metadata,'$.first_token_latency_ms')),
            SUM(CASE WHEN status='success' AND json_extract(metadata,'$.generation_ms')>0 THEN output_tokens ELSE 0 END),
            SUM(CASE WHEN status='success' AND json_extract(metadata,'$.generation_ms')>0 THEN json_extract(metadata,'$.generation_ms') ELSE 0 END),
            SUM(CASE WHEN (json_extract(metadata,'$.token_details.cached_input_reported')=1 AND json_extract(metadata,'$.token_details.input_estimated')=0) OR (json_extract(metadata,'$.token_details') IS NULL AND cached_input_tokens>0) THEN input_tokens ELSE 0 END),
            SUM(CASE WHEN (json_extract(metadata,'$.token_details.cached_input_reported')=1 AND json_extract(metadata,'$.token_details.input_estimated')=0) OR (json_extract(metadata,'$.token_details') IS NULL AND cached_input_tokens>0) THEN cached_input_tokens ELSE 0 END), model,
            SUM(CASE WHEN status='success' AND latency_ms>0 AND (json_extract(metadata,'$.token_details.output_available')=1 OR complete=1) THEN output_tokens ELSE 0 END),
            SUM(CASE WHEN status='success' AND latency_ms>0 AND (json_extract(metadata,'$.token_details.output_available')=1 OR complete=1) THEN latency_ms ELSE 0 END)
            FROM usage WHERE started_at>=?1 GROUP BY source_id,model ORDER BY source_id,model"#)?;
        let mut rows = grouped.query_map([since], |r| Ok(json!({"source_id":r.get::<_, String>(0)?,"source_name":r.get::<_, String>(1)?,"requests":r.get::<_,u64>(2)?,"input_tokens":r.get::<_,u64>(3)?,"output_tokens":r.get::<_,u64>(4)?,"cached_input_tokens":r.get::<_,u64>(5)?,"incomplete_requests":r.get::<_,u64>(6)?,"failures":r.get::<_,u64>(7)?,"estimated_requests":r.get::<_,u64>(8)?,"reasoning_output_tokens":r.get::<_,Option<u64>>(9)?,"successes":r.get::<_,u64>(10)?,"total_latency_ms":r.get::<_,u64>(11)?,"total_first_token_ms":r.get::<_,Option<u64>>(12)?,"first_token_samples":r.get::<_,u64>(13)?,"timed_output_tokens":r.get::<_,u64>(14)?,"total_generation_ms":r.get::<_,u64>(15)?,"cached_reported_input_tokens":r.get::<_,u64>(16)?,"cached_reported_tokens":r.get::<_,u64>(17)?,"active_requests":0,"model":r.get::<_,String>(18)?,"throughput_output_tokens":r.get::<_,u64>(19)?,"throughput_latency_ms":r.get::<_,u64>(20)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let cost_fields = [
            "total_usd",
            "input_usd",
            "cached_input_usd",
            "cache_write_usd",
            "output_usd",
            "cache_savings_usd",
        ];
        let sums = cost_fields
            .iter()
            .map(|name| format!("SUM(json_extract(metadata,'$.cost.{name}'))"))
            .collect::<Vec<_>>()
            .join(",");
        let mut cost_query = data.connection.prepare(&format!("SELECT source_id,model,{sums},
            SUM(json_type(metadata,'$.cost.total_usd') IS NOT NULL),
            SUM(json_extract(metadata,'$.cost.status') IN ('unknown_model','unknown_tier')),
            SUM(json_extract(metadata,'$.cost.status')='unobserved'),
            SUM(json_extract(metadata,'$.cost.status')='partial'),
            SUM(json_extract(metadata,'$.cost.status')='estimated' OR json_extract(metadata,'$.cost.historical')=1)
            FROM usage WHERE started_at>=?1 GROUP BY source_id,model"))?;
        for row in cost_query.query_map([since], |r| {
            let mut value =
                json!({"source_id":r.get::<_,String>(0)?,"model":r.get::<_,String>(1)?});
            for (i, name) in cost_fields.iter().enumerate() {
                value[*name] = json!(r.get::<_, Option<f64>>(i + 2)?);
            }
            for (i, name) in COST_COUNTS.iter().enumerate() {
                value[*name] = json!(r.get::<_, u64>(i + 8)?);
            }
            Ok(value)
        })? {
            let value = row?;
            if let Some(row) = rows.iter_mut().find(|row| {
                row["source_id"] == value["source_id"] && row["model"] == value["model"]
            }) {
                for name in cost_fields.iter().chain(COST_COUNTS.iter()) {
                    row[*name] = value[*name].clone();
                }
            }
        }
        let mut latest = data.connection.prepare("SELECT attempt,request_id,source_id,source_name,model,started_at,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms,metadata FROM usage WHERE started_at>=?1 AND (?2 IS NULL OR source_id=?2) AND (?3 IS NULL OR model=?3) ORDER BY attempt DESC LIMIT 200")?;
        let stored = latest
            .query_map(params![since, source_id, model], stored_record)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut recent: Vec<_> = data
            .active
            .values()
            .filter(|(record, _)| record.started_at >= since)
            .map(|(record, started)| {
                let mut value = serde_json::to_value(record).expect("serializable usage metadata");
                value["active"] = json!(true);
                value["latency_ms"] =
                    json!(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
                if let Some(first) = record
                    .first_token_latency_ms
                    .filter(|_| !record.token_details.stream_complete)
                {
                    value["generation_ms"] = json!(
                        started.elapsed().as_millis().min(u64::MAX as u128) as u64
                            - first.min(started.elapsed().as_millis() as u64)
                    );
                }
                add_rate(&mut value);
                value["cost"] = crate::pricing::estimate(&value, false);
                value
            })
            .collect();
        recent.sort_by_key(|r| std::cmp::Reverse(r["started_at"].as_u64().unwrap_or(0)));
        for active in &recent {
            let index = rows.iter().position(|row| row["source_id"] == active["source_id"] && row["model"] == active["model"]).unwrap_or_else(|| {
                rows.push(json!({"source_id":active["source_id"],"source_name":active["source_name"],"model":active["model"],"requests":0,"input_tokens":0,"output_tokens":0,"cached_input_tokens":0,"incomplete_requests":0,"failures":0,"estimated_requests":0,"active_requests":0,"reasoning_output_tokens":null,"successes":0,"total_latency_ms":0,"first_token_samples":0,"total_first_token_ms":0,"timed_output_tokens":0,"total_generation_ms":0,"cached_reported_input_tokens":0,"cached_reported_tokens":0}));
                rows.len()-1
            });
            let row = &mut rows[index];
            for (field, amount) in [
                ("requests", 1),
                ("active_requests", 1),
                ("input_tokens", active["input_tokens"].as_u64().unwrap_or(0)),
                (
                    "output_tokens",
                    active["output_tokens"].as_u64().unwrap_or(0),
                ),
                (
                    "cached_input_tokens",
                    active["cached_input_tokens"].as_u64().unwrap_or(0),
                ),
            ] {
                row[field] = json!(row[field].as_u64().unwrap_or(0).saturating_add(amount));
            }
            row["source_name"] = active["source_name"].clone();
            let cost = &active["cost"];
            for name in cost_fields {
                if let Some(amount) = cost[name].as_f64() {
                    row[name] = json!(row[name].as_f64().unwrap_or(0.0) + amount);
                }
            }
            for (name, applies) in [
                ("priced_requests", cost["total_usd"].is_number()),
                (
                    "unpriced_requests",
                    matches!(
                        cost["status"].as_str(),
                        Some("unknown_model" | "unknown_tier")
                    ),
                ),
                ("unobserved_requests", cost["status"] == "unobserved"),
                ("partial_cost_requests", cost["status"] == "partial"),
                ("estimated_cost_requests", cost["status"] == "estimated"),
            ] {
                if applies {
                    row[name] = json!(row[name].as_u64().unwrap_or(0) + 1);
                }
            }

            if active["token_details"]["input_estimated"] == true
                || active["token_details"]["output_estimated"] == true
            {
                row["estimated_requests"] =
                    json!(row["estimated_requests"].as_u64().unwrap_or(0) + 1);
            }
            if let Some(reasoning) = active["token_details"]["reasoning_output_tokens"].as_u64() {
                row["reasoning_output_tokens"] = json!(row["reasoning_output_tokens"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_add(reasoning));
            }
            if active["token_details"]["cached_input_reported"] == true
                && active["token_details"]["input_estimated"] == false
            {
                row["cached_reported_tokens"] = json!(row["cached_reported_tokens"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_add(active["cached_input_tokens"].as_u64().unwrap_or(0)));
                row["cached_reported_input_tokens"] = json!(row["cached_reported_input_tokens"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_add(active["input_tokens"].as_u64().unwrap_or(0)));
            }
        }
        recent.retain(|record| {
            source_id.is_none_or(|id| record["source_id"] == id)
                && model.is_none_or(|model| record["model"] == model)
        });
        let inflight = recent.len();
        recent.extend(stored);
        recent.truncate(200);
        let source_rows = aggregate_sources(&rows);
        Ok(
            json!({"rows":source_rows,"groups":rows,"recent":recent,"inflight":inflight,"pricing":crate::pricing::catalog(),"fetched_at":crate::model::now()}),
        )
    }
}

const COST_COUNTS: [&str; 5] = [
    "priced_requests",
    "unpriced_requests",
    "unobserved_requests",
    "partial_cost_requests",
    "estimated_cost_requests",
];
fn aggregate_sources(groups: &[Value]) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    for group in groups {
        let index = rows
            .iter()
            .position(|row| row["source_id"] == group["source_id"])
            .unwrap_or_else(|| {
                rows.push(
                    json!({"source_id":group["source_id"],"source_name":group["source_name"]}),
                );
                rows.len() - 1
            });
        for (name, value) in group.as_object().expect("aggregate").iter() {
            if let Some(number) = value.as_u64() {
                rows[index][name] = json!(rows[index][name]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_add(number));
            } else if let Some(number) = value.as_f64() {
                rows[index][name] = json!(rows[index][name].as_f64().unwrap_or(0.0) + number);
            }
        }
    }
    rows
}
fn stored_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let metadata: Value =
        serde_json::from_str(&r.get::<_, String>(12)?).unwrap_or_else(|_| json!({}));
    let complete = r.get::<_, bool>(10)?;
    let details=metadata.get("token_details").cloned().unwrap_or_else(||json!({"output_available":complete || r.get::<_,u64>(8).unwrap_or(0)>0,"input_available":complete || r.get::<_,u64>(7).unwrap_or(0)>0,"cached_input_reported":r.get::<_,u64>(9).unwrap_or(0)>0,"legacy":true}));
    let mut record = json!({"attempt_id":metadata.get("attempt_id").cloned().unwrap_or_else(||json!(format!("legacy:{}",r.get::<_,i64>(0).unwrap_or(0)))),"request_id":r.get::<_,String>(1)?,"source_id":r.get::<_,String>(2)?,"source_name":r.get::<_,String>(3)?,"model":r.get::<_,String>(4)?,"started_at":r.get::<_,u64>(5)?,"status":r.get::<_,String>(6)?,"input_tokens":r.get::<_,u64>(7)?,"output_tokens":r.get::<_,u64>(8)?,"cached_input_tokens":r.get::<_,u64>(9)?,"complete":complete,"latency_ms":r.get::<_,u64>(11)?,"token_details":details,"active":false,"first_token_latency_ms":metadata["first_token_latency_ms"],"generation_ms":metadata["generation_ms"],"cost":metadata["cost"]});
    add_rate(&mut record);
    Ok(record)
}
fn add_rate(record: &mut Value) {
    let duration = record["generation_ms"].as_u64().unwrap_or(0);
    record["generation_tps"] =
        if duration > 0 && record["token_details"]["output_available"] == true {
            json!(record["output_tokens"].as_f64().unwrap_or(0.0) * 1000.0 / duration as f64)
        } else {
            Value::Null
        };
    let elapsed = record["latency_ms"].as_u64().unwrap_or(0);
    record["output_tps"] = if elapsed > 0 && record["token_details"]["output_available"] == true {
        json!(record["output_tokens"].as_f64().unwrap_or(0.0) * 1000.0 / elapsed as f64)
    } else {
        Value::Null
    };
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
        let active = store
            .summary_filtered(1, Some("b"), Some("gpt-6-astra"))
            .unwrap();
        assert_eq!(active["recent"].as_array().unwrap().len(), 1);
        assert_eq!(active["groups"].as_array().unwrap().len(), 2);
        assert_eq!(active["rows"][0]["requests"], 205);
        assert_eq!(active["rows"][1]["active_requests"], 1);
        store.record(b).unwrap();
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
