use super::analytics::{rate_durations, CACHE_SAMPLE};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

pub(super) fn build(connection: &Connection) -> Result<Value> {
    let (_, duration) = rate_durations();
    let mut query = connection.prepare_cached(&format!(
        "SELECT model,TOTAL(input_tokens) AS input_total,TOTAL(output_tokens) AS output_total,COUNT(*),
             TOTAL(output_tokens) FILTER (WHERE ({duration}) IS NOT NULL) * 1000
                 / NULLIF(TOTAL({duration}),0),
             MIN(100, TOTAL(cached_input_tokens) FILTER (WHERE {CACHE_SAMPLE}) * 100
                 / NULLIF(TOTAL(input_tokens) FILTER (WHERE {CACHE_SAMPLE}),0))
         FROM (SELECT model,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms,metadata
               FROM usage ORDER BY started_at DESC,attempt DESC LIMIT 1000)
         GROUP BY model ORDER BY input_total+output_total DESC,model"
    ))?;
    let models = query
        .query_map([], |row| {
            let input = row.get::<_, f64>(1)?;
            let output = row.get::<_, f64>(2)?;
            Ok(json!({"model":row.get::<_, String>(0)?,"total_tokens":input+output,
                "input_tokens":input,"output_tokens":output,"attempts":row.get::<_, u64>(3)?,
                "tps":row.get::<_, Option<f64>>(4)?,"cache_rate":row.get::<_, Option<f64>>(5)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"models":models}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageStore;

    #[test]
    fn model_usage_groups_one_shared_window_by_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        let store = UsageStore::open(&path).unwrap();
        let fixture = Connection::open(&path).unwrap();
        fixture.execute_batch(r#"
            INSERT INTO usage VALUES (1,'older-model','a','A','old-only',9,'success',1000000,2000,800,1,20,'{}');
            INSERT INTO usage VALUES (2,'earlier-sequence','a','A','z-heavy',10,'success',1000,2000,800,1,20,'{}');
            WITH RECURSIVE attempts(n) AS (VALUES (3) UNION ALL SELECT n+1 FROM attempts WHERE n<997)
            INSERT INTO usage SELECT n,'failure','a','A','z-heavy',10,'error',100,500,0,0,100,'{}' FROM attempts;
            INSERT INTO usage VALUES
                (998,'legacy','a','A','z-heavy',10,'success',100,60,40,1,1000,'{}'),
                (999,'timed','b','B','z-heavy',10,'success',300,60,0,0,3000,'{"generation_timing_version":1,"first_token_latency_ms":2500,"generation_ms":500,"ingress":"omp","token_details":{"output_available":true,"output_estimated":false,"cached_input_reported":true}}'),
                (1000,'estimated','b','B','z-heavy',10,'error',100,300,100,0,200,'{"token_details":{"cached_input_reported":true,"input_estimated":true}}'),
                (1001,'reported-zero','a','A','a-zero',10,'success',100,0,0,1,1000,'{"token_details":{"cached_input_reported":true}}'),
                (1002,'untimed','b','B','b-unknown',10,'success',100,0,0,1,1000,'{"generation_timing_version":1,"token_details":{"output_available":true,"output_estimated":false}}'),
                (1003,'late-sequence','a','A','z-heavy',8,'success',1000,2000,800,1,20,'{}');
        "#).unwrap();
        let result = store.models().unwrap();
        assert_eq!(result["models"], json!([
            {"model":"z-heavy", "total_tokens":597920.0, "input_tokens":100000.0, "output_tokens":497920.0, "attempts":998, "tps":80.0, "cache_rate":10.0},
            {"model":"a-zero", "total_tokens":100.0, "input_tokens":100.0, "output_tokens":0.0, "attempts":1, "tps":0.0, "cache_rate":0.0},
            {"model":"b-unknown", "total_tokens":100.0, "input_tokens":100.0, "output_tokens":0.0, "attempts":1, "tps":null, "cache_rate":null}
        ]));
    }
}
