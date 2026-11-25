use super::{analytics::{rate_sums, COST_COUNTS, COST_FIELDS}, grouping, requests::RECORD_COLUMNS, Ingress, StoreData};
use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

pub(super) const INGRESS_COLUMN: &str = "COALESCE(NULLIF(json_extract(u.metadata,'$.ingress'),''),(SELECT NULLIF(json_extract(d.metadata,'$.ingress'),'') FROM diagnostics d WHERE d.attempt_id=COALESCE(NULLIF(json_extract(u.metadata,'$.attempt_id'),''),'legacy:'||u.attempt)),'unknown')";

pub(super) fn build(
    data: &StoreData,
    since: u64,
    source_id: Option<&str>,
    model: Option<&str>,
) -> Result<Value> {
    let mut rows = load_groups(&data.connection, since)?;
    let stored = load_recent(&data.connection, since, source_id, model)?;
    let mut recent = active_records(data, since);
    merge_active_groups(&mut rows, &recent);
    // Filters narrow recent attempts; overview groups still include all sources/models.
    recent.retain(|record| {
        source_id.is_none_or(|id| record["source_id"] == id)
            && model.is_none_or(|model| record["model"] == model)
    });
    let inflight = recent.len();
    recent.extend(stored);
    recent.truncate(200);
    let source_rows = grouping::merge(&rows, false);
    let rows = grouping::merge(&rows, true);
    Ok(
        json!({"rows":source_rows,"groups":rows,"recent":recent,"inflight":inflight,"pricing":crate::pricing::catalog(),"fetched_at":crate::model::now()}),
    )
}

fn load_groups(connection: &Connection, since: u64) -> Result<Vec<Value>> {
    let [timed_tokens, generation_ms, throughput_tokens, throughput_ms] = rate_sums();
    let sums = COST_FIELDS
        .iter()
        .map(|name| format!("SUM(json_extract(metadata,'$.cost.{name}'))"))
        .collect::<Vec<_>>()
        .join(",");
    let mut grouped = connection.prepare(&format!(r#"SELECT source_id,
        (SELECT latest.source_name FROM usage AS latest WHERE latest.source_id=usage.source_id ORDER BY latest.attempt DESC LIMIT 1),
        COUNT(*), SUM(input_tokens), SUM(output_tokens), SUM(cached_input_tokens), SUM(complete=0), SUM(status!='success'),
        SUM(COALESCE(json_extract(metadata,'$.token_details.input_estimated'),0) OR COALESCE(json_extract(metadata,'$.token_details.output_estimated'),0)),
        SUM(json_extract(metadata,'$.token_details.reasoning_output_tokens')), SUM(status='success'), SUM(latency_ms),
        SUM(json_extract(metadata,'$.first_token_latency_ms')), COUNT(json_extract(metadata,'$.first_token_latency_ms')),
        {timed_tokens}, {generation_ms},
        SUM(CASE WHEN (json_extract(metadata,'$.token_details.cached_input_reported')=1 AND json_extract(metadata,'$.token_details.input_estimated')=0) OR (json_extract(metadata,'$.token_details') IS NULL AND cached_input_tokens>0) THEN input_tokens ELSE 0 END),
        SUM(CASE WHEN (json_extract(metadata,'$.token_details.cached_input_reported')=1 AND json_extract(metadata,'$.token_details.input_estimated')=0) OR (json_extract(metadata,'$.token_details') IS NULL AND cached_input_tokens>0) THEN cached_input_tokens ELSE 0 END), model,
        {throughput_tokens}, {throughput_ms}, {sums},
        SUM(json_type(metadata,'$.cost.total_usd') IS NOT NULL),
        SUM(json_extract(metadata,'$.cost.status') IN ('unknown_model','unknown_tier')),
        SUM(json_extract(metadata,'$.cost.status')='unobserved'),
        SUM(json_extract(metadata,'$.cost.status')='partial'),
        SUM(json_extract(metadata,'$.cost.status')='estimated' OR json_extract(metadata,'$.cost.historical')=1),
        SUM(COALESCE(json_extract(metadata,'$.cost.billing_basis')='upstream_charge',0))
        FROM usage WHERE started_at>=?1 GROUP BY source_id,model ORDER BY source_id,model"#))?;
    let groups = grouped.query_map([since], |r| {
        let mut value = json!({
            "source_id":r.get::<_, String>(0)?, "source_name":r.get::<_, String>(1)?,
            "requests":r.get::<_,u64>(2)?, "input_tokens":r.get::<_,u64>(3)?,
            "output_tokens":r.get::<_,u64>(4)?, "cached_input_tokens":r.get::<_,u64>(5)?,
            "incomplete_requests":r.get::<_,u64>(6)?, "failures":r.get::<_,u64>(7)?,
            "estimated_requests":r.get::<_,u64>(8)?, "reasoning_output_tokens":r.get::<_,Option<u64>>(9)?,
            "successes":r.get::<_,u64>(10)?, "total_latency_ms":r.get::<_,u64>(11)?,
            "total_first_token_ms":r.get::<_,Option<u64>>(12)?, "first_token_samples":r.get::<_,u64>(13)?,
            "timed_output_tokens":r.get::<_,u64>(14)?, "total_generation_ms":r.get::<_,u64>(15)?,
            "cached_reported_input_tokens":r.get::<_,u64>(16)?, "cached_reported_tokens":r.get::<_,u64>(17)?,
            "active_requests":0, "model":r.get::<_,String>(18)?,
            "throughput_output_tokens":r.get::<_,u64>(19)?, "throughput_latency_ms":r.get::<_,u64>(20)?
        });
        for (i, name) in COST_FIELDS.iter().enumerate() {
            value[*name] = json!(r.get::<_, Option<f64>>(21 + i)?);
        }
        for (i, name) in COST_COUNTS.iter().enumerate() {
            value[*name] = json!(r.get::<_, u64>(21 + COST_FIELDS.len() + i)?);
        }
        Ok(value)
    })?;
    Ok(groups.collect::<rusqlite::Result<_>>()?)
}

fn load_recent(
    connection: &Connection,
    since: u64,
    source_id: Option<&str>,
    model: Option<&str>,
) -> Result<Vec<Value>> {
    let mut latest = connection.prepare(&format!("SELECT {RECORD_COLUMNS},{INGRESS_COLUMN} FROM usage u WHERE started_at>=?1 AND (?2 IS NULL OR source_id=?2) AND (?3 IS NULL OR model=?3) ORDER BY attempt DESC LIMIT 200"))?;
    let stored = latest
        .query_map(params![since, source_id, model], stored_record_with_ingress)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(stored)
}

fn active_records(data: &StoreData, since: u64) -> Vec<Value> {
    let mut recent: Vec<_> = data
        .active
        .values()
        .filter(|(record, _)| record.started_at >= since)
        .map(|(record, started)| {
            let mut value = serde_json::to_value(record).expect("serializable usage metadata");
            value["active"] = json!(true);
            value["latency_ms"] = json!(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
            add_rate(&mut value, false);
            value["cost"] = crate::pricing::estimate(&value, false);
            value
        })
        .collect();
    recent.sort_by_key(|r| std::cmp::Reverse(r["started_at"].as_u64().unwrap_or(0)));
    recent
}

fn merge_active_groups(rows: &mut Vec<Value>, active_records: &[Value]) {
    for active in active_records {
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
        for name in COST_FIELDS {
            if let Some(amount) = cost[name].as_f64() {
                row[name] = json!(row[name].as_f64().unwrap_or(0.0) + amount);
            }
        }
        for (name, applies) in COST_COUNTS.into_iter().zip([
            cost["total_usd"].is_number(),
            matches!(
                cost["status"].as_str(),
                Some("unknown_model" | "unknown_tier")
            ),
            cost["status"] == "unobserved",
            cost["status"] == "partial",
            cost["status"] == "estimated",
            cost["billing_basis"] == "upstream_charge",
        ]) {
            if applies {
                row[name] = json!(row[name].as_u64().unwrap_or(0) + 1);
            }
        }

        if active["token_details"]["input_estimated"] == true
            || active["token_details"]["output_estimated"] == true
        {
            row["estimated_requests"] = json!(row["estimated_requests"].as_u64().unwrap_or(0) + 1);
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
}

pub(super) fn stored_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let metadata: Value =
        serde_json::from_str(&r.get::<_, String>(12)?).unwrap_or_else(|_| json!({}));
    let complete = r.get::<_, bool>(10)?;
    let details=metadata.get("token_details").cloned().unwrap_or_else(||json!({"output_available":complete || r.get::<_,u64>(8).unwrap_or(0)>0,"input_available":complete || r.get::<_,u64>(7).unwrap_or(0)>0,"cached_input_reported":r.get::<_,u64>(9).unwrap_or(0)>0,"legacy":true}));
    let mut record = json!({"attempt_id":metadata.get("attempt_id").cloned().unwrap_or_else(||json!(format!("legacy:{}",r.get::<_,i64>(0).unwrap_or(0)))),"request_id":r.get::<_,String>(1)?,"source_id":r.get::<_,String>(2)?,"source_name":r.get::<_,String>(3)?,"model":r.get::<_,String>(4)?,"started_at":r.get::<_,u64>(5)?,"status":r.get::<_,String>(6)?,"input_tokens":r.get::<_,u64>(7)?,"output_tokens":r.get::<_,u64>(8)?,"cached_input_tokens":r.get::<_,u64>(9)?,"complete":complete,"latency_ms":r.get::<_,u64>(11)?,"token_details":details,"active":false,"first_token_latency_ms":metadata["first_token_latency_ms"],"generation_ms":metadata["generation_ms"],"cost":metadata["cost"]});
    record["ingress"] = json!(metadata["ingress"].as_str().and_then(Ingress::parse).unwrap_or_default());
    let legacy_timing = metadata["generation_timing_version"].as_u64().unwrap_or(0) < 1;
    add_rate(&mut record, legacy_timing);
    Ok(record)
}

pub(super) fn stored_record_with_ingress(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let mut record = stored_record(r)?;
    record["ingress"] = json!(r.get::<_, String>(13)?);
    Ok(record)
}

pub(super) fn add_rate(record: &mut Value, legacy_timing: bool) {
    let trusted_timing = !legacy_timing || record["token_details"]["stream_complete"] == true;
    if !trusted_timing {
        record["generation_ms"] = Value::Null;
    }
    let duration = record["generation_ms"].as_u64().filter(|&ms| ms > 0);
    let output_available = record["token_details"]["output_available"]
        .as_bool()
        .unwrap_or(record["complete"] == true);
    let output = record["output_tokens"].as_f64().filter(|_| {
        output_available && (legacy_timing || record["token_details"]["output_estimated"] == false)
    });
    let rate = |duration: Option<u64>| {
        json!(output.zip(duration).map(|(tokens, ms)| tokens * 1000.0 / ms as f64))
    };
    record["generation_tps"] = rate(duration);
    record["output_tps"] = rate(duration.or_else(|| {
        record["latency_ms"].as_u64().filter(|&ms| legacy_timing && ms > 0)
    }));
}
