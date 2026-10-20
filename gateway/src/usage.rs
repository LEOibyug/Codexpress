use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{path::Path, sync::Mutex};

#[derive(Clone, Debug)]
pub struct UsageRecord {
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
    pub complete: bool,
    pub latency_ms: u64,
}

pub struct UsageStore {
    connection: Mutex<Connection>,
}
impl UsageStore {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path).context("无法打开用量数据库")?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
            CREATE TABLE IF NOT EXISTS usage (
                attempt INTEGER PRIMARY KEY, request_id TEXT NOT NULL, source_id TEXT NOT NULL,
                source_name TEXT NOT NULL, model TEXT NOT NULL, started_at INTEGER NOT NULL,
                status TEXT NOT NULL, input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
                cached_input_tokens INTEGER NOT NULL, complete INTEGER NOT NULL, latency_ms INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS usage_time ON usage(started_at);")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }
    pub fn record(&self, record: UsageRecord) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        // Only the schema's metadata enters SQLite. There is no body/header column.
        let number = |v: u64| v.min(i64::MAX as u64) as i64;
        connection.execute("INSERT INTO usage (request_id,source_id,source_name,model,started_at,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)", params![record.request_id,record.source_id,record.source_name,record.model,number(record.started_at),record.status,number(record.input_tokens),number(record.output_tokens),number(record.cached_input_tokens.min(record.input_tokens)),record.complete,number(record.latency_ms)])?;
        Ok(())
    }
    pub fn summary(&self, days: u32) -> Result<Value> {
        let since = crate::model::now().saturating_sub(u64::from(days.clamp(1, 3660)) * 86400);
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        let mut grouped = connection.prepare("SELECT source_id, (SELECT latest.source_name FROM usage AS latest WHERE latest.source_id=usage.source_id ORDER BY latest.attempt DESC LIMIT 1), COUNT(*), SUM(input_tokens), SUM(output_tokens), SUM(cached_input_tokens), SUM(complete=0), SUM(status!='success') FROM usage WHERE started_at>=?1 GROUP BY source_id ORDER BY source_id")?;
        let rows = grouped.query_map([since], |r| Ok(json!({"source_id":r.get::<_, String>(0)?,"source_name":r.get::<_, String>(1)?,"requests":r.get::<_,u64>(2)?,"input_tokens":r.get::<_,u64>(3)?,"output_tokens":r.get::<_,u64>(4)?,"cached_input_tokens":r.get::<_,u64>(5)?,"incomplete_requests":r.get::<_,u64>(6)?,"failures":r.get::<_,u64>(7)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut latest = connection.prepare("SELECT request_id,source_id,source_name,model,started_at,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms FROM usage WHERE started_at>=?1 ORDER BY attempt DESC LIMIT 200")?;
        let recent = latest.query_map([since], |r| Ok(json!({"request_id":r.get::<_,String>(0)?,"source_id":r.get::<_,String>(1)?,"source_name":r.get::<_,String>(2)?,"model":r.get::<_,String>(3)?,"started_at":r.get::<_,u64>(4)?,"status":r.get::<_,String>(5)?,"input_tokens":r.get::<_,u64>(6)?,"output_tokens":r.get::<_,u64>(7)?,"cached_input_tokens":r.get::<_,u64>(8)?,"complete":r.get::<_,bool>(9)?,"latency_ms":r.get::<_,u64>(10)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({"rows":rows,"recent":recent}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persisted_attempts_keep_name_snapshot_and_cached_input_subset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        {
            let store = UsageStore::open(&path).unwrap();
            let first = UsageRecord {
                request_id: "same-request".into(),
                source_id: "removed-source".into(),
                source_name: "旧名称".into(),
                model: "m".into(),
                started_at: crate::model::now(),
                status: "failed".into(),
                input_tokens: 0,
                output_tokens: 0,
                cached_input_tokens: 0,
                complete: false,
                latency_ms: 1,
            };
            store.record(first.clone()).unwrap();
            store
                .record(UsageRecord {
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
        let reopened = UsageStore::open(&path).unwrap();
        let summary = reopened.summary(30).unwrap();
        assert_eq!(summary["rows"][0]["source_name"], "新名称");
        assert_eq!(summary["rows"][0]["requests"], 2);
        assert_eq!(summary["rows"][0]["failures"], 1);
        assert_eq!(summary["rows"][0]["incomplete_requests"], 1);
        assert_eq!(summary["rows"][0]["input_tokens"], 10);
        assert_eq!(summary["rows"][0]["cached_input_tokens"], 10);
        assert_eq!(summary["recent"][1]["source_name"], "旧名称");
    }
}
