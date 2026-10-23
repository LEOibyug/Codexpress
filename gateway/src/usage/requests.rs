//! Bounded request history queries. Error bodies are deliberately absent here.
use super::{summary, StoreData};
use anyhow::{bail, ensure, Result};
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Value};

pub(super) const ATTEMPTS: &str = r#"WITH attempts AS (
    SELECT u.*, COALESCE(NULLIF(json_extract(u.metadata,'$.attempt_id'),''),'legacy:'||u.attempt) AS attempt_id_key,
        COALESCE(NULLIF(json_extract(d.metadata,'$.ingress'),''),'unknown') AS ingress,
        json_extract(d.metadata,'$.http_status') AS http_status,
        json_extract(d.metadata,'$.error_code') AS error_code,
        json_extract(d.metadata,'$.error_kind') AS error_kind,
        json_extract(d.metadata,'$.error_message') AS error_message,
        json_extract(d.metadata,'$.error_param') AS error_param,
        d.attempt_id IS NOT NULL AS diagnostic_available
    FROM usage u LEFT JOIN diagnostics d ON d.attempt_id=
        COALESCE(NULLIF(json_extract(u.metadata,'$.attempt_id'),''),'legacy:'||u.attempt)
    WHERE u.started_at>=?1 AND u.started_at<?2
)"#;

pub(super) const FILTERS: &str =
    "(?3 IS NULL OR source_id=?3) AND (?4 IS NULL OR model=?4) AND (?5 IS NULL OR ingress=?5)";
const LOG_FILTERS: &str = r#"AND (?6 IS NULL OR status=?6)
    AND (?7 IS NULL OR instr(lower(request_id||char(10)||attempt_id_key||char(10)||source_id||char(10)||source_name||char(10)||model||char(10)||status||char(10)||ingress||char(10)||COALESCE(http_status,'')||char(10)||COALESCE(error_code,'')||char(10)||COALESCE(error_kind,'')||char(10)||COALESCE(error_message,'')||char(10)||COALESCE(error_param,'')),lower(?7))>0)"#;
const RECORD_COLUMNS: &str = "attempt,request_id,source_id,source_name,model,started_at,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms,metadata";
const MAX_INTERVAL: u64 = 366 * 26 * 3600;
const MAX_ACTIVE: usize = 10_000;

#[derive(Debug)]
pub(super) struct Query {
    pub from: u64,
    pub to: u64,
    pub days: Vec<u64>,
    pub source: Option<String>,
    pub model: Option<String>,
    pub ingress: Option<String>,
    pub status: Option<String>,
    pub search: Option<String>,
    pub before: Option<i64>,
    pub limit: usize,
}

impl Query {
    pub(super) fn parse(payload: &Value, analytics: bool) -> Result<Self> {
        let object = payload
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("查询参数必须为对象"))?;
        for key in object.keys() {
            let valid = matches!(
                key.as_str(),
                "from" | "to" | "day_starts" | "source_id" | "model" | "ingress"
            ) || !analytics
                && matches!(key.as_str(), "status" | "search" | "before" | "limit");
            ensure!(valid, "未知查询参数：{key}");
        }
        let integer = |key: &str| -> Result<u64> {
            payload[key]
                .as_u64()
                .filter(|value| *value <= i64::MAX as u64)
                .ok_or_else(|| anyhow::anyhow!("{key} 必须为有效的非负整数"))
        };
        let from = integer("from")?;
        let to = integer("to")?;
        ensure!(
            from < to && to - from <= MAX_INTERVAL,
            "查询范围无效，最多支持 366 天"
        );
        let mut days = Vec::new();
        if analytics || object.contains_key("day_starts") {
            let values = payload["day_starts"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("day_starts 必须为日期边界数组"))?;
            ensure!(
                (2..=367).contains(&values.len()),
                "日期边界数量必须为 2 至 367 个"
            );
            for value in values {
                days.push(
                    value
                        .as_u64()
                        .filter(|value| *value <= i64::MAX as u64)
                        .ok_or_else(|| anyhow::anyhow!("日期边界必须为有效的非负整数"))?,
                );
            }
            ensure!(
                days.first() == Some(&from) && days.last() == Some(&to),
                "日期边界必须与查询范围一致"
            );
            ensure!(
                days.windows(2).all(|pair| pair[1]
                    .checked_sub(pair[0])
                    .is_some_and(|duration| (22 * 3600..=26 * 3600).contains(&duration))),
                "日期边界必须递增且间隔 22 至 26 小时"
            );
        }
        let source = optional_text(payload, "source_id", 256)?;
        let model = optional_text(payload, "model", 256)?;
        let ingress = optional_text(payload, "ingress", 32)?;
        ensure!(
            ingress.as_deref().is_none_or(|value| matches!(
                value,
                "native" | "ssh" | "chatbot" | "image" | "unknown"
            )),
            "入口筛选无效"
        );
        let status = optional_text(payload, "status", 32)?;
        ensure!(
            status.as_deref().is_none_or(|value| matches!(
                value,
                "success" | "failed" | "interrupted" | "active"
            )),
            "状态筛选无效"
        );
        let search = optional_text(payload, "search", 256)?;
        let before = if payload.get("before").is_none_or(Value::is_null) {
            None
        } else {
            let value = integer("before")?;
            ensure!(value > 0, "分页游标必须为正整数");
            Some(value as i64)
        };
        let limit = if payload.get("limit").is_none_or(Value::is_null) {
            50
        } else {
            integer("limit")? as usize
        };
        ensure!((1..=100).contains(&limit), "每页数量必须为 1 至 100");
        Ok(Self {
            from,
            to,
            days,
            source,
            model,
            ingress,
            status,
            search,
            before,
            limit,
        })
    }

    pub(super) fn bindings(&self) -> Vec<SqlValue> {
        vec![
            SqlValue::Integer(self.from as i64),
            SqlValue::Integer(self.to as i64),
            sql_text(&self.source),
            sql_text(&self.model),
            sql_text(&self.ingress),
        ]
    }

    pub(super) fn matches(&self, record: &Value) -> bool {
        record["started_at"]
            .as_u64()
            .is_some_and(|time| self.from <= time && time < self.to)
            && self
                .source
                .as_deref()
                .is_none_or(|source| record["source_id"] == source)
            && self
                .model
                .as_deref()
                .is_none_or(|model| record["model"] == model)
            && self
                .ingress
                .as_deref()
                .is_none_or(|ingress| record["ingress"] == ingress)
    }
}

fn optional_text(payload: &Value, key: &str, max: usize) -> Result<Option<String>> {
    let Some(value) = payload.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(text) = value.as_str() else {
        bail!("{key} 必须为字符串")
    };
    ensure!(
        !text.is_empty() && text.len() <= max && !text.chars().any(char::is_control),
        "{key} 长度或内容无效"
    );
    Ok(Some(text.to_owned()))
}

fn sql_text(text: &Option<String>) -> SqlValue {
    text.as_ref()
        .map_or(SqlValue::Null, |text| SqlValue::Text(text.clone()))
}

/// Active snapshots have no cursor. Ignore stale snapshots already committed to SQLite.
pub(super) fn active_records(data: &StoreData, query: &Query) -> Result<Vec<Value>> {
    ensure!(
        data.active.len() <= MAX_ACTIVE,
        "进行中的请求过多，请稍后重试"
    );
    let mut persisted = data.connection.prepare_cached("SELECT 1 FROM usage WHERE json_extract(metadata,'$.attempt_id')=?1 UNION ALL SELECT 1 FROM usage WHERE attempt=?2 AND COALESCE(NULLIF(json_extract(metadata,'$.attempt_id'),''),'legacy:'||attempt)=?1 LIMIT 1")?;
    let mut diagnostics = data
        .connection
        .prepare_cached("SELECT metadata FROM diagnostics WHERE attempt_id=?1")?;
    let mut records = Vec::new();
    for (record, started) in data.active.values() {
        if record.started_at < query.from || record.started_at >= query.to {
            continue;
        }
        let legacy_rowid = record
            .attempt_id
            .strip_prefix("legacy:")
            .and_then(|id| id.parse::<i64>().ok());
        if persisted.exists(rusqlite::params![&record.attempt_id, legacy_rowid])? {
            continue;
        }
        let mut value = serde_json::to_value(record)?;
        let metadata = diagnostics
            .query_map([&record.attempt_id], |row| row.get::<_, String>(0))?
            .next()
            .transpose()?;
        let available = metadata.is_some();
        let diagnostic: Value = metadata
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_else(|| json!({}));
        value["active"] = json!(true);
        value["sequence"] = Value::Null;
        value["ingress"] = json!(diagnostic["ingress"]
            .as_str()
            .filter(|value| !value.is_empty())
            .unwrap_or("unknown"));
        for field in [
            "http_status",
            "error_code",
            "error_kind",
            "error_message",
            "error_param",
        ] {
            value[field] = diagnostic[field].clone();
        }
        value["diagnostic_available"] = json!(available);
        let elapsed = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        value["latency_ms"] = json!(elapsed);
        if let Some(first) = record
            .first_token_latency_ms
            .filter(|_| !record.token_details.stream_complete)
        {
            value["generation_ms"] = json!(elapsed.saturating_sub(first));
        }
        summary::add_rate(&mut value);
        value["cost"] = crate::pricing::estimate(&value, false);
        records.push(value);
    }
    records.sort_by(|left, right| {
        right["started_at"]
            .as_u64()
            .cmp(&left["started_at"].as_u64())
            .then_with(|| {
                left["attempt_id"]
                    .as_str()
                    .cmp(&right["attempt_id"].as_str())
            })
    });
    Ok(records)
}

fn log_matches(query: &Query, record: &Value) -> bool {
    query.matches(record)
        && query
            .status
            .as_deref()
            .is_none_or(|status| status == "active" && record["active"] == true)
        && query.search.as_ref().is_none_or(|search| {
            [
                "request_id",
                "attempt_id",
                "source_id",
                "source_name",
                "model",
                "status",
                "ingress",
                "http_status",
                "error_code",
                "error_kind",
                "error_message",
                "error_param",
            ]
            .iter()
            .map(|field| {
                record[*field]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        if record[*field].is_null() {
                            String::new()
                        } else {
                            record[*field].to_string()
                        }
                    })
            })
            .collect::<Vec<_>>()
            .join("\n")
            .to_ascii_lowercase()
            .contains(&search.to_ascii_lowercase())
        })
}

pub(super) fn build(data: &StoreData, payload: &Value) -> Result<Value> {
    let query = Query::parse(payload, false)?;
    let mut bindings = query.bindings();
    bindings.extend([sql_text(&query.status), sql_text(&query.search)]);
    let count_sql =
        format!("{ATTEMPTS} SELECT COUNT(*) FROM attempts WHERE {FILTERS} {LOG_FILTERS}");
    let total: u64 = data.connection.query_row(
        &count_sql,
        rusqlite::params_from_iter(bindings.iter()),
        |row| row.get(0),
    )?;
    let mut active = active_records(data, &query)?;
    active.retain(|record| log_matches(&query, record));
    let active_count = active.len();
    for record in &mut active {
        if let Some(fields) = record.as_object_mut() {
            for field in ["error_kind", "error_message", "error_param"] {
                fields.remove(field);
            }
        }
    }
    let mut records: Vec<_> = if query.before.is_none() {
        active.into_iter().take(query.limit).collect()
    } else {
        Vec::new()
    };
    let stored_limit = query.limit - records.len();
    bindings.extend([
        query.before.map_or(SqlValue::Null, SqlValue::Integer),
        SqlValue::Integer((stored_limit + 1) as i64),
    ]);
    let list_sql = format!("{ATTEMPTS} SELECT {RECORD_COLUMNS},ingress,http_status,error_code,diagnostic_available FROM attempts WHERE {FILTERS} {LOG_FILTERS} AND (?8 IS NULL OR attempt<?8) ORDER BY attempt DESC LIMIT ?9");
    let mut statement = data.connection.prepare(&list_sql)?;
    let mut stored = statement
        .query_map(rusqlite::params_from_iter(bindings.iter()), |row| {
            let mut record = summary::stored_record(row)?;
            if record["attempt_id"].as_str().is_none_or(str::is_empty) {
                record["attempt_id"] = json!(format!("legacy:{}", row.get::<_, i64>(0)?));
            }
            record["sequence"] = json!(row.get::<_, i64>(0)?);
            record["ingress"] = json!(row.get::<_, String>(13)?);
            record["http_status"] = json!(row.get::<_, Option<i64>>(14)?);
            record["error_code"] = json!(row.get::<_, Option<String>>(15)?);
            record["diagnostic_available"] = json!(row.get::<_, bool>(16)?);
            Ok(record)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = stored.len() > stored_limit;
    let next_cursor = if stored_limit == 0 && more {
        let first = stored
            .first()
            .and_then(|record| record["sequence"].as_i64())
            .unwrap_or(0);
        Some(
            first
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("分页游标超出范围"))?,
        )
    } else if more {
        stored
            .get(stored_limit - 1)
            .and_then(|record| record["sequence"].as_i64())
    } else {
        None
    };
    stored.truncate(stored_limit);
    records.extend(stored);
    Ok(
        json!({"records":records,"next_cursor":next_cursor,"total":total.saturating_add(active_count as u64),"active_count":active_count,"fetched_at":crate::model::now()}),
    )
}

#[cfg(test)]
#[path = "requests_tests.rs"]
pub(super) mod tests;
