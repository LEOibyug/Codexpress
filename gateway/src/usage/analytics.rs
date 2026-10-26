//! Window-wide aggregates; neither this module nor request lists fetch error bodies.
use super::{
    requests::{self, Query, ATTEMPTS, FILTERS},
    StoreData,
};
use anyhow::{ensure, Result};
use rusqlite::{types::Value as SqlValue, Connection};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const MAX_OPTIONS: usize = 10_000;
const COST_FIELDS: [&str; 6] = [
    "total_usd",
    "input_usd",
    "cached_input_usd",
    "cache_write_usd",
    "output_usd",
    "cache_savings_usd",
];
const COST_COUNTS: [&str; 5] = [
    "priced_requests",
    "unpriced_requests",
    "unobserved_requests",
    "partial_cost_requests",
    "estimated_cost_requests",
];

/// These are fixed SQL expressions; every caller-controlled value is bound.
fn aggregate_json() -> String {
    let mut fields = vec![
        ("requests", "COUNT(*)".to_owned()),
        ("active_requests", "SUM(status='active')".into()),
        ("input_tokens", "SUM(input_tokens)".into()),
        ("output_tokens", "SUM(output_tokens)".into()),
        ("cached_input_tokens", "SUM(cached_input_tokens)".into()),
        ("total_tokens", "SUM(input_tokens+output_tokens)".into()),
        ("incomplete_requests", "SUM(complete=0 AND status!='active')".into()),
        ("successes", "SUM(status='success')".into()),
        ("failures", "SUM(status NOT IN ('success','active'))".into()),
        ("estimated_requests", "SUM(COALESCE(json_extract(metadata,'$.token_details.input_estimated'),0) OR COALESCE(json_extract(metadata,'$.token_details.output_estimated'),0))".into()),
        ("reasoning_output_tokens", "SUM(json_extract(metadata,'$.token_details.reasoning_output_tokens'))".into()),
        ("total_latency_ms", "SUM(CASE WHEN status!='active' THEN latency_ms ELSE 0 END)".into()),
        ("total_first_token_ms", "SUM(CASE WHEN status!='active' THEN json_extract(metadata,'$.first_token_latency_ms') END)".into()),
        ("first_token_samples", "COUNT(CASE WHEN status!='active' THEN json_extract(metadata,'$.first_token_latency_ms') END)".into()),
        ("timed_output_tokens", "SUM(CASE WHEN status='success' AND json_extract(metadata,'$.generation_ms')>0 THEN output_tokens ELSE 0 END)".into()),
        ("total_generation_ms", "SUM(CASE WHEN status='success' AND json_extract(metadata,'$.generation_ms')>0 THEN json_extract(metadata,'$.generation_ms') ELSE 0 END)".into()),
        ("throughput_output_tokens", "SUM(CASE WHEN status='success' AND latency_ms>0 AND (json_extract(metadata,'$.token_details.output_available')=1 OR complete=1) THEN output_tokens ELSE 0 END)".into()),
        ("throughput_latency_ms", "SUM(CASE WHEN status='success' AND latency_ms>0 AND (json_extract(metadata,'$.token_details.output_available')=1 OR complete=1) THEN latency_ms ELSE 0 END)".into()),
        ("cached_reported_input_tokens", "SUM(CASE WHEN (json_extract(metadata,'$.token_details.cached_input_reported')=1 AND COALESCE(json_extract(metadata,'$.token_details.input_estimated'),0)=0) OR (json_extract(metadata,'$.token_details') IS NULL AND cached_input_tokens>0) THEN input_tokens ELSE 0 END)".into()),
        ("cached_reported_tokens", "SUM(CASE WHEN (json_extract(metadata,'$.token_details.cached_input_reported')=1 AND COALESCE(json_extract(metadata,'$.token_details.input_estimated'),0)=0) OR (json_extract(metadata,'$.token_details') IS NULL AND cached_input_tokens>0) THEN cached_input_tokens ELSE 0 END)".into()),
    ];
    for field in COST_FIELDS {
        fields.push((
            field,
            format!("SUM(json_extract(metadata,'$.cost.{field}'))"),
        ));
    }
    for (field, expression) in COST_COUNTS.into_iter().zip([
        "SUM(COALESCE(json_type(metadata,'$.cost.total_usd') IN ('integer','real'),0))",
        "SUM(COALESCE(json_extract(metadata,'$.cost.status') IN ('unknown_model','unknown_tier'),0))",
        "SUM(COALESCE(json_extract(metadata,'$.cost.status')='unobserved',0))",
        "SUM(COALESCE(json_extract(metadata,'$.cost.status')='partial',0))",
        "SUM(COALESCE(json_extract(metadata,'$.cost.status')='estimated',0) OR COALESCE(json_extract(metadata,'$.cost.historical'),0)=1)",
    ]) { fields.push((field, expression.into())); }
    format!(
        "json_object({})",
        fields
            .into_iter()
            .map(|(name, sql)| format!("'{name}',{sql}"))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn empty_totals() -> Value {
    let mut value = json!({"requests":0,"active_requests":0,"input_tokens":0,"output_tokens":0,"cached_input_tokens":0,"total_tokens":0,
        "incomplete_requests":0,"successes":0,"failures":0,"estimated_requests":0,"reasoning_output_tokens":null,
        "total_latency_ms":0,"total_first_token_ms":null,"first_token_samples":0,"timed_output_tokens":0,"total_generation_ms":0,
        "throughput_output_tokens":0,"throughput_latency_ms":0,"cached_reported_input_tokens":0,"cached_reported_tokens":0});
    for field in COST_FIELDS {
        value[field] = Value::Null;
    }
    for field in COST_COUNTS {
        value[field] = json!(0);
    }
    value
}

fn add_number(target: &mut Value, field: &str, amount: u64) {
    target[field] = json!(target[field].as_u64().unwrap_or(0).saturating_add(amount));
}

fn merge_active(target: &mut Value, active: &Value) {
    if target["requests"] == 0 {
        for field in COST_FIELDS {
            target[field] = Value::Null;
        }
    }
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
        (
            "total_tokens",
            active["input_tokens"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(active["output_tokens"].as_u64().unwrap_or(0)),
        ),
    ] {
        add_number(target, field, amount);
    }
    let details = &active["token_details"];
    if details["input_estimated"] == true || details["output_estimated"] == true {
        add_number(target, "estimated_requests", 1);
    }
    if let Some(reasoning) = details["reasoning_output_tokens"].as_u64() {
        add_number(target, "reasoning_output_tokens", reasoning);
    }
    if details["cached_input_reported"] == true && details["input_estimated"] == false {
        add_number(
            target,
            "cached_reported_tokens",
            active["cached_input_tokens"].as_u64().unwrap_or(0),
        );
        add_number(
            target,
            "cached_reported_input_tokens",
            active["input_tokens"].as_u64().unwrap_or(0),
        );
    }
    let cost = &active["cost"];
    for field in COST_FIELDS {
        if let Some(amount) = cost[field].as_f64() {
            target[field] = json!(target[field].as_f64().unwrap_or(0.0) + amount);
        }
    }
    for (field, applies) in COST_COUNTS.into_iter().zip([
        cost["total_usd"].is_number(),
        matches!(
            cost["status"].as_str(),
            Some("unknown_model" | "unknown_tier")
        ),
        cost["status"] == "unobserved",
        cost["status"] == "partial",
        cost["status"] == "estimated" || cost["historical"] == true,
    ]) {
        if applies {
            add_number(target, field, 1);
        }
    }
}

fn performance(connection: &Connection, query: &Query) -> Result<Value> {
    // Only scalar timings enter the window sort; no raw records or diagnostic payloads.
    let sql = format!(
        r#"{ATTEMPTS}, samples AS (
        SELECT 'latency' kind,latency_ms value FROM attempts WHERE {FILTERS} AND status!='active' AND latency_ms>0
        UNION ALL SELECT 'first_token',json_extract(metadata,'$.first_token_latency_ms') FROM attempts
            WHERE {FILTERS} AND status!='active' AND json_type(metadata,'$.first_token_latency_ms') IN ('integer','real') AND json_extract(metadata,'$.first_token_latency_ms')>=0
    ), ranked AS (SELECT kind,value,ROW_NUMBER() OVER(PARTITION BY kind ORDER BY value) AS rank,COUNT(*) OVER(PARTITION BY kind) AS count FROM samples)
    SELECT kind,MAX(count),
        MAX(CASE WHEN rank=1+(count-1)*50/100 THEN value END),MAX(CASE WHEN rank=1+((count-1)*50+99)/100 THEN value END),
        MAX(CASE WHEN rank=1+(count-1)*95/100 THEN value END),MAX(CASE WHEN rank=1+((count-1)*95+99)/100 THEN value END)
        FROM ranked GROUP BY kind"#
    );
    let mut result = json!({"p50_latency_ms":null,"p95_latency_ms":null,"p50_first_token_ms":null,"p95_first_token_ms":null});
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(query.bindings()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u64>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, f64>(3)?,
            row.get::<_, f64>(4)?,
            row.get::<_, f64>(5)?,
        ))
    })?;
    for row in rows {
        let (kind, count, p50_low, p50_high, p95_low, p95_high) = row?;
        for (percentile, low, high) in [(50, p50_low, p50_high), (95, p95_low, p95_high)] {
            let fraction = ((count - 1) * percentile % 100) as f64 / 100.0;
            result[format!("p{percentile}_{kind}_ms")] = json!(low + (high - low) * fraction);
        }
    }
    Ok(result)
}

fn options(data: &StoreData, query: &Query) -> Result<Options> {
    let mut sources = BTreeMap::new();
    let mut models = BTreeSet::new();
    let mut ingresses = BTreeSet::new();
    let mut statement = data.connection.prepare(&format!("{ATTEMPTS} SELECT source_id,source_name,MAX(attempt) FROM attempts GROUP BY source_id ORDER BY source_id LIMIT {}", MAX_OPTIONS + 1))?;
    for row in statement.query_map([query.from, query.to], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, name) = row?;
        sources.insert(id, name);
    }
    for (column, target) in [("model", &mut models), ("ingress", &mut ingresses)] {
        let mut statement = data.connection.prepare(&format!(
            "{ATTEMPTS} SELECT DISTINCT {column} FROM attempts ORDER BY {column} LIMIT {}",
            MAX_OPTIONS + 1
        ))?;
        for value in statement.query_map([query.from, query.to], |row| row.get::<_, String>(0))? {
            target.insert(value?);
        }
    }
    Ok(Options {
        sources,
        models,
        ingresses,
    })
}

#[derive(Clone)]
struct Options {
    sources: BTreeMap<String, String>,
    models: BTreeSet<String>,
    ingresses: BTreeSet<String>,
}
impl Options {
    fn with_active(mut self, active: &[Value]) -> Result<Value> {
        for record in active {
            self.sources.insert(
                record["source_id"].as_str().unwrap_or("").into(),
                record["source_name"].as_str().unwrap_or("").into(),
            );
            self.models
                .insert(record["model"].as_str().unwrap_or("").into());
            self.ingresses
                .insert(record["ingress"].as_str().unwrap_or("unknown").into());
        }
        ensure!(
            self.sources.len() <= MAX_OPTIONS
                && self.models.len() <= MAX_OPTIONS
                && self.ingresses.len() <= MAX_OPTIONS,
            "筛选选项过多，请缩小查询范围"
        );
        Ok(
            json!({"sources":self.sources.into_iter().map(|(id,name)|json!({"id":id,"name":name})).collect::<Vec<_>>(),"models":self.models,"ingresses":self.ingresses}),
        )
    }
}

#[derive(Clone)]
struct Snapshot {
    groups: BTreeMap<(String, String), Value>,
    daily: Vec<Value>,
    options: Options,
    performance: Value,
}

pub(super) struct Cached {
    payload: Value,
    changes: u64,
    data_version: i64,
    snapshot: Snapshot,
}

fn load_snapshot(data: &StoreData, query: &Query) -> Result<Snapshot> {
    let aggregate = aggregate_json();
    let mut statement = data.connection.prepare(&format!("{ATTEMPTS} SELECT source_id,source_name,model,MAX(attempt),{aggregate} FROM attempts WHERE {FILTERS} GROUP BY source_id,model ORDER BY source_id,model LIMIT {}", MAX_OPTIONS + 1))?;
    let mut groups: BTreeMap<(String, String), Value> = BTreeMap::new();
    for row in statement.query_map(rusqlite::params_from_iter(query.bindings()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(4)?,
        ))
    })? {
        let (source, name, model, totals) = row?;
        let mut totals: Value = serde_json::from_str(&totals)?;
        totals["source_id"] = json!(source);
        totals["source_name"] = json!(name);
        totals["model"] = json!(model);
        groups.insert((source, model), totals);
    }
    let mut zero_day = empty_totals();
    zero_day["total_usd"] = json!(0);
    let mut daily = vec![zero_day; query.days.len() - 1];
    let mut bindings = query.bindings();
    bindings.push(SqlValue::Text(serde_json::to_string(&query.days)?));
    let sql = format!("{ATTEMPTS} SELECT CAST(days.key AS INTEGER),{aggregate} FROM json_each(?6) days JOIN attempts ON started_at>=days.value AND started_at<json_extract(?6,'$['||(CAST(days.key AS INTEGER)+1)||']') WHERE {FILTERS} GROUP BY days.key ORDER BY CAST(days.key AS INTEGER)");
    let mut statement = data.connection.prepare(&sql)?;
    for row in statement.query_map(rusqlite::params_from_iter(bindings), |row| {
        Ok((row.get::<_, usize>(0)?, row.get::<_, String>(1)?))
    })? {
        let (index, totals) = row?;
        if let Some(day) = daily.get_mut(index) {
            *day = serde_json::from_str(&totals)?;
        }
    }
    Ok(Snapshot {
        groups,
        daily,
        options: options(data, query)?,
        performance: performance(&data.connection, query)?,
    })
}

pub(super) fn build(data: &mut StoreData, payload: &Value) -> Result<Value> {
    let query = Query::parse(payload, true)?;
    let changes = data.connection.total_changes();
    let data_version = data
        .connection
        .pragma_query_value(None, "data_version", |row| row.get(0))?;
    // Completed history is stable while tokens stream. Cache its aggregates;
    // active counters and elapsed timings are overlaid fresh on every call.
    data.analytics_cache
        .retain(|cached| cached.changes == changes && cached.data_version == data_version);
    let stored = if let Some(index) = data
        .analytics_cache
        .iter()
        .position(|cached| cached.payload == *payload)
    {
        let cached = data.analytics_cache.remove(index).expect("cached query");
        let snapshot = cached.snapshot.clone();
        data.analytics_cache.push_back(cached);
        snapshot
    } else {
        let snapshot = load_snapshot(data, &query)?;
        if data.analytics_cache.len() == 8 {
            data.analytics_cache.pop_front();
        }
        data.analytics_cache.push_back(Cached {
            payload: payload.clone(),
            changes,
            data_version,
            snapshot: snapshot.clone(),
        });
        snapshot
    };
    let Snapshot {
        mut groups,
        mut daily,
        options,
        performance,
    } = stored;
    let active = requests::active_records(data, &query)?;
    let options = options.with_active(&active)?;
    for record in active.iter().filter(|record| query.matches(record)) {
        let key = (
            record["source_id"].as_str().unwrap_or("").to_owned(),
            record["model"].as_str().unwrap_or("").to_owned(),
        );
        let group = groups.entry(key).or_insert_with(|| {
            let mut value = empty_totals();
            for field in ["source_id", "source_name", "model"] {
                value[field] = record[field].clone();
            }
            value
        });
        group["source_name"] = record["source_name"].clone();
        merge_active(group, record);
        let index = query
            .days
            .partition_point(|start| *start <= record["started_at"].as_u64().unwrap_or(0))
            .saturating_sub(1);
        if let Some(day) = daily.get_mut(index) {
            merge_active(day, record);
        }
    }
    ensure!(groups.len() <= MAX_OPTIONS, "统计分组过多，请缩小查询范围");
    let daily: Vec<_> = daily
        .into_iter()
        .zip(&query.days)
        .map(|(day, start)| {
            let mut value = json!({"start_at":start});
            for field in [
                "input_tokens",
                "output_tokens",
                "cached_input_tokens",
                "total_tokens",
                "total_usd",
                "requests",
                "successes",
                "failures",
                "estimated_requests",
                "priced_requests",
            ] {
                value[field] = day[field].clone();
            }
            value
        })
        .collect();
    Ok(
        json!({"groups":groups.into_values().collect::<Vec<_>>(),"daily":daily,"performance":performance,"options":options,"from":query.from,"to":query.to,"fetched_at":crate::model::now()}),
    )
}

#[cfg(test)]
#[path = "analytics_tests.rs"]
mod tests;
