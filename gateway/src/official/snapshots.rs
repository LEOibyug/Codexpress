//! Official quota and reset-credit snapshots, including recovery and low-quota retention.
use super::{account, json_response, same_binding, token, Endpoints, REQUEST_TIMEOUT};
use crate::{
    model::{now, AppState, Source},
    transport,
};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) fn number(value: &Value) -> Value {
    value
        .as_f64()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map_or(Value::Null, |n| json!(n))
}
fn timestamp(value: &Value) -> Value {
    if let Some(n) = value.as_u64() {
        return json!(n);
    }
    let Some(s) = value.as_str() else {
        return Value::Null;
    };
    parse_iso(s).map_or(Value::Null, |n| json!(n))
}
// RFC3339 timestamps from reset-credit inventory (including fractional seconds and offsets).
fn parse_iso(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let n = |a, z| s.get(a..z)?.parse::<i64>().ok();
    let (year, month, day, hour, minute, second) = (
        n(0, 4)?,
        n(5, 7)?,
        n(8, 10)?,
        n(11, 13)?,
        n(14, 16)?,
        n(17, 19)?,
    );
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&month)
        || !(1..=days[(month - 1) as usize]).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let mut tail = &s[19..];
    if tail.starts_with('.') {
        let end = tail[1..].find(|c: char| !c.is_ascii_digit())? + 1;
        if end == 1 {
            return None;
        }
        tail = &tail[end..];
    }
    let offset = if tail == "Z" || tail == "z" {
        0
    } else {
        let t = tail.as_bytes();
        if t.len() != 6 || !matches!(t[0], b'+' | b'-') || t[3] != b':' {
            return None;
        }
        let h: i64 = tail.get(1..3)?.parse().ok()?;
        let m: i64 = tail.get(4..6)?.parse().ok()?;
        if h > 23 || m > 59 {
            return None;
        }
        (h * 3600 + m * 60) * if t[0] == b'+' { 1 } else { -1 }
    };
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let civil_days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    u64::try_from(civil_days * 86400 + hour * 3600 + minute * 60 + second - offset).ok()
}
pub(super) fn quota_snapshot(raw: &Value) -> Result<Value> {
    if !raw.is_object() {
        bail!("官方额度响应格式变化");
    }
    let fetched_at = now();
    let mut windows = Vec::new();
    let mut append = |prefix: &str, limits: &Value| {
        for (name, key) in [
            ("primary", "primary_window"),
            ("secondary", "secondary_window"),
        ] {
            if let Some(window) = limits.get(key).filter(|window| !window.is_null()) {
                let used = number(&window["used_percent"]);
                windows.push(json!({"name":format!("{prefix}{name}"),"used_percent":used,"reset_at":timestamp(&window["reset_at"]),"window_seconds":number(&window["limit_window_seconds"]),"observed_at":fetched_at}));
            }
        }
    };
    append("", &raw["rate_limit"]);
    let mut complete = raw.get("rate_limit").is_some_and(Value::is_object)
        && raw
            .get("additional_rate_limits")
            .is_none_or(|v| v.is_null() || v.is_array());
    if let Some(additional) = raw["additional_rate_limits"].as_array() {
        for (index, item) in additional.iter().enumerate() {
            complete &= item.get("rate_limit").is_some_and(|v| {
                v.is_object()
                    && ["primary_window", "secondary_window"]
                        .iter()
                        .any(|key| v.get(*key).is_some_and(|w| !w.is_null()))
            });
            append(
                &format!(
                    "{}:",
                    item["limit_name"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("additional_{index}"))
                ),
                &item["rate_limit"],
            );
        }
    }
    Ok(
        json!({"plan_type":raw.get("plan_type").and_then(Value::as_str),"windows":windows,"complete":complete,"fetched_at":fetched_at,"stale":false,"error":null}),
    )
}
pub(super) fn credits_snapshot(raw: &Value) -> Result<Value> {
    let cards = raw
        .get("credits")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("官方重置卡响应格式变化"))?;
    let credits:Vec<_> = cards.iter().map(|card| {
        json!({"id":card.get("id").and_then(Value::as_str),"title":card.get("title").and_then(Value::as_str),"status":card.get("status").and_then(Value::as_str).unwrap_or("unknown"),"expires_at":timestamp(&card["expires_at"]),"expiry_known":card.get("expires_at").is_some_and(|v|v.is_null() || !timestamp(v).is_null()),"reset_type":card.get("reset_type").and_then(Value::as_str)})
    }).collect();
    Ok(
        json!({"available_count":raw.get("available_count").and_then(Value::as_u64),"credits":credits,"fetched_at":now(),"stale":false,"error":null}),
    )
}
pub(super) async fn get(source: &Source, url: &str) -> Result<Value> {
    let mut request = transport::client(source)?
        .get(url)
        .timeout(REQUEST_TIMEOUT)
        .bearer_auth(token(&source.secret, "access_token"))
        .header("Accept", "application/json")
        .header("OpenAI-Beta", "codex-1")
        .header("originator", "Codex Desktop");
    if let Some(id) = account(source) {
        request = request.header("ChatGPT-Account-ID", id);
    }
    json_response(
        request
            .send()
            .await
            .map_err(|e| anyhow!("官方查询{}", transport::failure_reason(&e)))?,
    )
    .await
}
fn stale(mut previous: Value, kind: &str, error: &str) -> Value {
    if !previous.is_object() {
        previous = if kind == "quota" {
            json!({"plan_type":null,"windows":[],"fetched_at":null})
        } else {
            json!({"available_count":null,"credits":[],"fetched_at":null})
        };
    }
    previous["stale"] = json!(true);
    previous["error"] = json!(error);
    previous
}
fn recover_exhaustion(runtime: &mut crate::model::SourceRuntime) {
    if runtime.health != "exhausted"
        || runtime.quota["stale"] != false
        || runtime.quota["complete"] != true
    {
        return;
    }
    let Some(windows) = runtime.quota["windows"]
        .as_array()
        .filter(|v| !v.is_empty())
    else {
        return;
    };
    if windows
        .iter()
        .any(|window| window["used_percent"].as_f64().is_none())
    {
        return;
    }
    let exhausted: Vec<_> = windows
        .iter()
        .filter(|window| {
            window["used_percent"]
                .as_f64()
                .is_some_and(|used| used >= 100.0)
        })
        .collect();
    if exhausted.is_empty() {
        runtime.health = "ready".into();
        runtime.cooldown_until = None;
        runtime.last_error = None;
    } else {
        let resets: Option<Vec<_>> = exhausted
            .iter()
            .map(|window| window["reset_at"].as_u64().filter(|reset| *reset > now()))
            .collect();
        if let Some(resets) = resets {
            runtime.cooldown_until = resets.into_iter().max();
        }
    }
}
pub(super) async fn refresh_snapshots(
    state: &Arc<AppState>,
    src: &Source,
    endpoints: Endpoints<'_>,
) -> Result<Value> {
    refresh_selected_snapshots(state, src, endpoints, true).await
}
pub(super) async fn refresh_selected_snapshots(
    state: &Arc<AppState>,
    src: &Source,
    endpoints: Endpoints<'_>,
    include_credits: bool,
) -> Result<Value> {
    let threshold = state.settings.read().await.official_low_quota_percent;
    let recovering_revision = state
        .sources
        .read()
        .await
        .iter()
        .find(|r| {
            same_binding(&r.source, src) && r.health == "proxy_error" && r.cooldown_until.is_some()
        })
        .map(|r| r.health_revision);
    let quota = get(src, endpoints.usage)
        .await
        .and_then(|v| quota_snapshot(&v));
    let credits = if include_credits {
        Some(
            get(src, endpoints.credits)
                .await
                .and_then(|v| credits_snapshot(&v)),
        )
    } else {
        None
    };
    let mut sources = state.sources.write().await;
    let runtime = sources
        .iter_mut()
        .find(|r| same_binding(&r.source, src))
        .ok_or_else(|| anyhow!("来源在查询期间发生变化，请刷新"))?;
    runtime.quota = match quota {
        Ok(mut v) => {
            preserve_low_windows(&mut v, &runtime.quota, threshold);
            // A fresh authenticated query proves this proxy path works. Never
            // clear a newer failure, HTTP throttling, or an auth/quota block.
            if runtime.health == "proxy_error"
                && recovering_revision == Some(runtime.health_revision)
            {
                runtime.health = "ready".into();
                runtime.last_error = None;
                runtime.cooldown_until = None;
                runtime.health_revision = runtime.health_revision.wrapping_add(1);
            }
            v
        }
        Err(e) => stale(runtime.quota.clone(), "quota", &e.to_string()),
    };
    recover_exhaustion(runtime);
    if let Some(credits) = credits {
        runtime.credits = match credits {
            Ok(v) => v,
            Err(e) => stale(runtime.credits.clone(), "credits", &e.to_string()),
        };
    }
    let result = json!({"quota":runtime.quota,"credits":runtime.credits});
    crate::routing::notify_quotas(state, &sources);
    Ok(result)
}

fn preserve_low_windows(fresh: &mut Value, previous: &Value, threshold: u8) {
    let Some(old) = previous["windows"].as_array() else {
        return;
    };
    let Some(windows) = fresh.get_mut("windows").and_then(Value::as_array_mut) else {
        return;
    };
    let mut retained = false;
    for old_window in old.iter().filter(|window| {
        matches!(window["name"].as_str(), Some("primary" | "secondary"))
            && window["used_percent"]
                .as_f64()
                .is_some_and(|used| used.is_finite() && used >= 100.0 - f64::from(threshold))
    }) {
        // Retained observations must not become newer than a confirmed recovery
        // seen through another source for the same account.
        let mut retained_window = old_window.clone();
        retained_window["observed_at"] = json!(old_window["observed_at"]
            .as_u64()
            .or_else(|| previous["fetched_at"].as_u64())
            .unwrap_or(0));
        if let Some(current) = windows
            .iter_mut()
            .find(|window| window["name"] == old_window["name"])
        {
            if current["used_percent"]
                .as_f64()
                .is_some_and(|used| used.is_finite() && used >= 0.0)
            {
                continue;
            }
            *current = retained_window;
        } else {
            windows.push(retained_window);
        }
        retained = true;
    }
    if retained {
        fresh["stale"] = json!(true);
        fresh["complete"] = json!(false);
        fresh["error"] = json!("官方未返回完整额度，保留上次已知值");
    }
}
