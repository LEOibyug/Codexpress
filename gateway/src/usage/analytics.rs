//! Window-wide aggregates; neither this module nor request lists fetch error bodies.
use super::{grouping, requests::{self, Query}, StoreData};
use anyhow::{ensure, Result};
use serde_json::{json, Value};

#[path = "analytics_cache.rs"]
mod cache;
pub(super) use cache::{preserve_after_write, Cached};

const MAX_OPTIONS: usize = 10_000;
pub(super) const COST_FIELDS: [&str; 6] = [
    "total_usd",
    "input_usd",
    "cached_input_usd",
    "cache_write_usd",
    "output_usd",
    "cache_savings_usd",
];
pub(super) const COST_COUNTS: [&str; 6] = [
    "priced_requests",
    "unpriced_requests",
    "unobserved_requests",
    "partial_cost_requests",
    "estimated_cost_requests",
    "upstream_priced_requests",
];

const CURRENT_TIMING: &str = "COALESCE(json_extract(metadata,'$.generation_timing_version'),0)>=1";
const OUTPUT_AVAILABLE: &str = "COALESCE(json_extract(metadata,'$.token_details.output_available'),complete=1 OR (json_extract(metadata,'$.token_details') IS NULL AND output_tokens>0))=1";
pub(super) const CACHE_SAMPLE: &str = "((json_extract(metadata,'$.token_details.cached_input_reported')=1 AND COALESCE(json_extract(metadata,'$.token_details.input_estimated'),0)=0) OR (json_extract(metadata,'$.token_details') IS NULL AND cached_input_tokens>0))";

// Both durations are NULL for ineligible samples. Old incomplete streams could
// include time spent waiting for EOF; only their terminal timing is usable.
pub(super) fn rate_durations() -> (String, String) {
    const DURATION: &str = "json_extract(metadata,'$.generation_ms')";
    let measured = format!("CASE WHEN status='success' AND {OUTPUT_AVAILABLE} AND {DURATION}>0 AND (CASE WHEN {CURRENT_TIMING} THEN json_extract(metadata,'$.token_details.output_estimated')=0 ELSE json_extract(metadata,'$.token_details.stream_complete')=1 END) THEN {DURATION} END");
    let throughput = format!("COALESCE({measured}, CASE WHEN status='success' AND NOT ({CURRENT_TIMING}) AND {OUTPUT_AVAILABLE} AND latency_ms>0 THEN latency_ms END)");
    (measured, throughput)
}

pub(super) fn rate_sums() -> [String; 4] {
    let (measured, throughput) = rate_durations();
    [
        format!("SUM(CASE WHEN ({measured}) IS NOT NULL THEN output_tokens ELSE 0 END)"),
        format!("SUM(COALESCE({measured},0))"),
        format!("SUM(CASE WHEN ({throughput}) IS NOT NULL THEN output_tokens ELSE 0 END)"),
        format!("SUM(COALESCE({throughput},0))"),
    ]
}

/// These are fixed SQL expressions; every caller-controlled value is bound.
fn aggregate_json() -> String {
    let [timed_tokens, generation_ms, throughput_tokens, throughput_ms] = rate_sums();
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
        ("timed_output_tokens", timed_tokens),
        ("total_generation_ms", generation_ms),
        ("throughput_output_tokens", throughput_tokens),
        ("throughput_latency_ms", throughput_ms),
        ("cached_reported_input_tokens", format!("SUM(CASE WHEN {CACHE_SAMPLE} THEN input_tokens ELSE 0 END)")),
        ("cached_reported_tokens", format!("SUM(CASE WHEN {CACHE_SAMPLE} THEN cached_input_tokens ELSE 0 END)")),
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
