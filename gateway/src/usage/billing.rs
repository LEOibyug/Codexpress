//! Reconcile CCH's USD ledger using mutually unique numeric request signatures.
//! No prompts, response text, cookies or provider credentials enter this module.
use super::UsageStore;
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;

pub(crate) struct Plan {
    pub since: u64,
    pub max_attempt: i64,
}
pub(crate) struct ChargeMatch {
    pub attempt: i64,
    pub record_id: String,
    pub usd: f64,
}

impl UsageStore {
    pub(crate) fn billing_plan(
        &self,
        source: &str,
        identity: &str,
        full: bool,
    ) -> Result<Option<Plan>> {
        let data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        let (since, max_attempt): (Option<u64>, Option<i64>) = data.connection.query_row(
            "SELECT MIN(started_at), MAX(attempt) FROM usage WHERE source_id=?1
             AND (json_extract(metadata,'$.token_details.source_identity') IS NULL OR json_extract(metadata,'$.token_details.source_identity')=?2)
             AND COALESCE(json_extract(metadata,'$.cost.billing_basis'),'')!='upstream_charge'
             AND (?3 OR json_extract(metadata,'$.billing_checked_identity') IS NOT ?2 OR started_at+latency_ms/1000>=?4)",
            params![source, identity, full, crate::model::now().saturating_sub(600)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(since.zip(max_attempt).map(|(since, max_attempt)| Plan {
            since: since.saturating_sub(120),
            max_attempt,
        }))
    }

    pub(crate) fn billing_matches(
        &self,
        source: &str,
        identity: &str,
        rows: &[Value],
    ) -> Result<Vec<ChargeMatch>> {
        let data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        let mut query = data.connection.prepare_cached(
            "SELECT attempt FROM usage WHERE source_id=?1 AND status='success' AND complete=1
             AND started_at BETWEEN unixepoch(?2)-120 AND unixepoch(?2)+120
             AND input_tokens IN (?3,?4) AND output_tokens=?5 AND cached_input_tokens=?6
             AND ABS(latency_ms-?7)<=30000
             AND COALESCE(json_extract(metadata,'$.token_details.billing_model'),model) IN (?8,?9)
             AND (json_extract(metadata,'$.token_details.input_estimated')=0 OR json_type(metadata,'$.token_details') IS NULL)
             AND (json_extract(metadata,'$.token_details.output_estimated')=0 OR json_type(metadata,'$.token_details') IS NULL)
             AND (json_extract(metadata,'$.token_details.source_identity') IS NULL OR json_extract(metadata,'$.token_details.source_identity')=?10)
             LIMIT 2")?;
        let mut matches = Vec::new();
        for row in rows {
            if row["statusCode"] != 200
                || row["endpoint"]
                    .as_str()
                    .is_none_or(|p| !matches!(p, "/v1/responses" | "/responses"))
            {
                continue;
            }
            let Some(usd) = row["cost"].as_f64().filter(|v| v.is_finite() && *v >= 0.0) else {
                continue;
            };
            let Some(id) = row["id"].as_u64().filter(|v| *v > 0) else {
                continue;
            };
            let Some(at) = row["createdAt"].as_str().filter(|s| s.len() <= 40) else {
                continue;
            };
            let Some(model) = row["model"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 256)
            else {
                continue;
            };
            let billing_model = row["billingModel"]
                .as_str()
                .filter(|s| s.len() <= 256)
                .unwrap_or(model);
            let (Some(input), Some(output), Some(duration)) = (
                row["inputTokens"].as_u64(),
                row["outputTokens"].as_u64(),
                row["duration"].as_u64(),
            ) else {
                continue;
            };
            let cached = row["cacheReadInputTokens"].as_u64().unwrap_or(0);
            let write = row["cacheCreationInputTokens"].as_u64().unwrap_or(0);
            let Some(total) = input.checked_add(cached).and_then(|v| v.checked_add(write)) else {
                continue;
            };
            if input.max(total).max(output).max(duration) > i64::MAX as u64
                || duration == 0
                || total.saturating_add(output) == 0
            {
                continue;
            }
            // CCH versions use either disjoint cache buckets or OpenAI's inclusive input.
            let candidates = query
                .query_map(
                    params![
                        source,
                        at,
                        input,
                        total,
                        output,
                        cached,
                        duration,
                        model,
                        billing_model,
                        identity
                    ],
                    |r| r.get::<_, i64>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if let [attempt] = candidates.as_slice() {
                matches.push(ChargeMatch {
                    attempt: *attempt,
                    record_id: id.to_string(),
                    usd,
                });
            }
        }
        Ok(matches)
    }

    pub(crate) fn apply_billing(
        &self,
        source: &str,
        identity: &str,
        plan: &Plan,
        matches: HashMap<i64, Option<ChargeMatch>>,
    ) -> Result<usize> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        let tx = data.connection.transaction()?;
        let mut updated = 0;
        for item in matches.into_values().flatten() {
            let raw: Option<String> = tx
                .query_row(
                    "SELECT metadata FROM usage WHERE attempt=?1 AND source_id=?2",
                    params![item.attempt, source],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(raw) = raw else {
                continue;
            };
            let mut metadata: Value = serde_json::from_str(&raw)?;
            if metadata["cost"]["billing_basis"] == "upstream_charge" {
                continue;
            }
            let recorded = metadata["token_details"]["source_identity"].as_str();
            if recorded.is_some_and(|v| v != identity) {
                continue;
            }
            metadata["token_details"]["source_kind"] = json!("api");
            metadata["token_details"]["upstream_cost"] =
                json!({"usd":item.usd,"method":"cch_usage","record_id":item.record_id});
            metadata["cost"] = crate::pricing::estimate(
                &json!({"token_details":metadata["token_details"]}),
                false,
            );
            updated += tx.execute(
                "UPDATE usage SET metadata=?1 WHERE attempt=?2",
                params![metadata.to_string(), item.attempt],
            )?;
        }
        // A completed scan marks only its initial snapshot. Long-running requests
        // persisted later remain pending regardless of when they originally began.
        tx.execute("UPDATE usage SET metadata=json_set(metadata,'$.billing_checked_identity',?1)
            WHERE source_id=?2 AND attempt<=?3 AND COALESCE(json_extract(metadata,'$.cost.billing_basis'),'')!='upstream_charge'
            AND (json_extract(metadata,'$.token_details.source_identity') IS NULL OR json_extract(metadata,'$.token_details.source_identity')=?1)
            AND json_extract(metadata,'$.billing_checked_identity') IS NOT ?1",params![identity,source,plan.max_attempt])?;
        tx.commit()?;
        Ok(updated)
    }
}
