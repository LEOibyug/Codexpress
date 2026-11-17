use super::analytics::{rate_durations, CACHE_SAMPLE};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

pub(super) fn build(connection: &Connection) -> Result<Value> {
    // Only finished attempts are persisted. The model index covers the lifetime scan,
    // and provides a bounded, ordered lookup without parsing older metadata.
    let mut totals = connection.prepare_cached(
        "SELECT model,TOTAL(input_tokens+output_tokens) AS tokens FROM usage
         GROUP BY model ORDER BY tokens DESC,model",
    )?;
    let (_, duration) = rate_durations();
    let mut recent = connection.prepare_cached(&format!(
        "SELECT COUNT(*),
             TOTAL(output_tokens) FILTER (WHERE ({duration}) IS NOT NULL) * 1000
                 / NULLIF(TOTAL({duration}),0),
             MIN(100, TOTAL(cached_input_tokens) FILTER (WHERE {CACHE_SAMPLE}) * 100
                 / NULLIF(TOTAL(input_tokens) FILTER (WHERE {CACHE_SAMPLE}),0))
         FROM (SELECT status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms,metadata
               FROM usage WHERE model=?1 ORDER BY started_at DESC,attempt DESC LIMIT 500)"
    ))?;
    let models = totals
        .query_map([], |row| {
            let model: String = row.get(0)?;
            let total_tokens: f64 = row.get(1)?;
            recent.query_row([&model], |sample| {
                Ok(json!({"model":model,"total_tokens":total_tokens,"attempts":sample.get::<_, u64>(0)?,
                    "tps":sample.get::<_, Option<f64>>(1)?,"cache_rate":sample.get::<_, Option<f64>>(2)?}))
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"models":models}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageStore;

    #[test]
    fn model_usage_ranks_lifetime_tokens_and_samples_the_latest_500_finished_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        let store = UsageStore::open(&path).unwrap();
        let fixture = Connection::open(&path).unwrap();
        fixture.execute_batch(r#"
            INSERT INTO usage VALUES (1,'older-start','a','A','z-heavy',9,'success',1000,2000,800,1,20,'{}');
            INSERT INTO usage VALUES (2,'earlier-sequence','a','A','z-heavy',10,'success',1000,2000,800,1,20,'{}');
            WITH RECURSIVE attempts(n) AS (VALUES (3) UNION ALL SELECT n+1 FROM attempts WHERE n<499)
            INSERT INTO usage SELECT n,'failure','a','A','z-heavy',10,'error',100,500,0,0,100,'{}' FROM attempts;
            INSERT INTO usage VALUES
                (500,'legacy','a','A','z-heavy',10,'success',100,60,40,1,1000,'{}'),
                (501,'timed','b','B','z-heavy',10,'success',300,60,0,0,3000,'{"generation_timing_version":1,"first_token_latency_ms":2500,"generation_ms":500,"ingress":"omp","token_details":{"output_available":true,"output_estimated":false,"cached_input_reported":true}}'),
                (502,'estimated','b','B','z-heavy',10,'error',100,300,100,0,200,'{"token_details":{"cached_input_reported":true,"input_estimated":true}}'),
                (503,'late-sequence','a','A','z-heavy',8,'success',1000,2000,800,1,20,'{}'),
                (504,'reported-zero','a','A','a-zero',10,'success',100,0,0,1,1000,'{"token_details":{"cached_input_reported":true}}'),
                (505,'untimed','b','B','b-unknown',10,'success',100,0,0,1,1000,'{"generation_timing_version":1,"token_details":{"output_available":true,"output_estimated":false}}');
        "#).unwrap();
        let result = store.models().unwrap();
        assert_eq!(result["models"], json!([
            {"model":"z-heavy", "total_tokens": 308120.0, "attempts":500, "tps":80.0, "cache_rate":10.0},
            {"model":"a-zero", "total_tokens":100.0, "attempts":1, "tps":0.0, "cache_rate":0.0},
            {"model":"b-unknown", "total_tokens":100.0, "attempts":1, "tps":null, "cache_rate":null}
        ]));
    }
}
