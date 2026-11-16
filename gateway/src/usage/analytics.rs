//! Window-wide aggregates; neither this module nor request lists fetch error bodies.
use super::{grouping, requests::{self, Query}, StoreData};
use anyhow::{ensure, Result};
use serde_json::{json, Value};

#[path = "analytics_cache.rs"]
mod cache;
pub(super) use cache::{preserve_after_write, Cached};

const MAX_OPTIONS: usize = 10_000;
const COST_FIELDS: [&str; 6] = [
    "total_usd",
    "input_usd",
    "cached_input_usd",
    "cache_write_usd",
    "output_usd",
    "cache_savings_usd",
];
const COST_COUNTS: [&str; 6] = [
    "priced_requests",
    "unpriced_requests",
    "unobserved_requests",
    "partial_cost_requests",
    "estimated_cost_requests",
    "upstream_priced_requests",
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
        "SUM(COALESCE(json_extract(metadata,'$.cost.billing_basis')='upstream_charge',0))",
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
        cost["billing_basis"] == "upstream_charge",
    ]) {
        if applies {
            add_number(target, field, 1);
        }
    }
}


pub(super) fn build(data: &mut StoreData, payload: &Value) -> Result<Value> {
    let query = Query::parse(payload, true)?;
    let cache::Snapshot {
        mut groups,
        mut daily,
        options,
        performance,
    } = cache::snapshot(data, &query)?;
    let active = requests::active_records(data, &query)?;
    let options = options.with_active(&active)?;
    for record in active.iter().filter(|record| requests::log_matches(&query, record)) {
        let key = (
            record["source_id"].as_str().unwrap_or("").to_owned(),
            record["model"].as_str().unwrap_or("").to_owned(),
        );
        let group = groups.entry(key).or_insert_with(|| {
            let mut totals = empty_totals();
            for field in ["source_id", "source_name", "model"] {
                totals[field] = record[field].clone();
            }
            cache::Group { attempt: 0, totals }
        });
        group.totals["source_name"] = record["source_name"].clone();
        merge_active(&mut group.totals, record);
        let index = query
            .days
            .partition_point(|start| *start <= record["started_at"].as_u64().unwrap_or(0))
            .saturating_sub(1);
        if let Some(day) = daily.get_mut(index) {
            merge_active(day, record);
        }
    }
    ensure!(groups.len() <= MAX_OPTIONS, "统计分组过多，请缩小查询范围");
    let groups = grouping::merge(groups.values().map(|group| &group.totals), true);
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
                "upstream_priced_requests",
            ] {
                value[field] = day[field].clone();
            }
            value
        })
        .collect();
    Ok(
        json!({"groups":groups,"daily":daily,"performance":performance,"options":options,"from":query.from,"to":query.to,"fetched_at":crate::model::now()}),
    )
}

#[cfg(test)]
#[path = "analytics_tests.rs"]
mod tests;
