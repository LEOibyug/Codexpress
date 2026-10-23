use super::*;
use crate::usage::requests::tests::{fixture, persist, record};
use std::time::Instant;

#[test]
fn whole_window_filters_daily_zeroes_options_and_timings_are_consistent() {
    let (_dir, mut data) = fixture();
    let from = 100_000;
    let days = [from, from + 23 * 3600, from + 48 * 3600, from + 72 * 3600];
    for index in 0..225 {
        let mut record = record(&format!("stored-{index}"), from + 1);
        record.latency_ms = 100 + index;
        record.first_token_latency_ms = Some(10 + index);
        persist(&data, &record, Some("native"));
    }
    let mut unknown = record("unknown", from + 24 * 3600);
    unknown.source_id = "b".into();
    unknown.source_name = "来源 B".into();
    unknown.model = "not-in-price-list".into();
    unknown.status = "failed".into();
    unknown.complete = false;
    persist(&data, &unknown, Some("ssh"));
    let mut live = record("live", from + 2);
    live.input_tokens = 5;
    live.output_tokens = 2;
    data.active
        .insert(live.attempt_id.clone(), (live, Instant::now()));
    let stale = record("stored-0", from + 1);
    data.active
        .insert(stale.attempt_id.clone(), (stale, Instant::now()));
    // Outside the exclusive upper bound, this row cannot influence options or totals.
    let mut outside = record("outside", days[3]);
    outside.model = "outside-window".into();
    persist(&data, &outside, Some("image"));
    let full = build(&data, &json!({"from":from,"to":days[3],"day_starts":days})).unwrap();
    let a = &full["groups"][0];
    assert_eq!(a["requests"], 226);
    assert_eq!(a["active_requests"], 1);
    assert_eq!(a["successes"], 225);
    assert_eq!(a["failures"], 0);
    assert_eq!(a["input_tokens"], 22_505);
    assert_eq!(a["output_tokens"], 2_252);
    assert_eq!(a["throughput_output_tokens"], 2_250);
    assert_eq!(a["throughput_latency_ms"], (100..325).sum::<u64>());
    assert_eq!(a["total_latency_ms"], (100..325).sum::<u64>());
    assert_eq!(a["first_token_samples"], 225);
    assert_eq!(full["daily"][0]["requests"], 226);
    assert_eq!(full["daily"][1]["requests"], 1);
    assert_eq!(full["daily"][2]["requests"], 0);
    assert_eq!(full["daily"][2]["input_tokens"], 0);
    assert_eq!(full["daily"][2]["total_usd"], 0);
    assert!(full["daily"][1]["total_usd"].is_null());
    assert!(full["groups"][1]["total_usd"].is_null());
    assert_eq!(full["groups"][1]["priced_requests"], 0);
    assert_eq!(full["groups"][1]["unpriced_requests"], 1);
    assert_eq!(full["options"]["models"].as_array().unwrap().len(), 2);
    assert!(!full.to_string().contains("outside-window"));
    let filtered = build(&data, &json!({"from":from,"to":days[3],"day_starts":days,"source_id":"a","model":"gpt-6.1-sol","ingress":"native"})).unwrap();
    assert_eq!(filtered["groups"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["groups"][0]["requests"], 225); // unknown ingress active is filtered out.
    assert_eq!(filtered["daily"][0]["requests"], 225);
    assert_eq!(filtered["daily"][1]["requests"], 0);
    assert_eq!(filtered["performance"]["p50_latency_ms"], 212.0);
    assert!((filtered["performance"]["p95_latency_ms"].as_f64().unwrap() - 312.8).abs() < 0.00001);
    assert_eq!(filtered["performance"]["p50_first_token_ms"], 122.0);
    assert_eq!(filtered["options"], full["options"]);
    let missing = build(
        &data,
        &json!({"from":from,"to":days[3],"day_starts":days,"model":"absent"}),
    )
    .unwrap();
    assert!(missing["groups"].as_array().unwrap().is_empty());
    assert!(missing["performance"]["p50_latency_ms"].is_null());
    assert!(missing["daily"]
        .as_array()
        .unwrap()
        .iter()
        .all(|day| day["requests"] == 0));
}

#[test]
fn invalid_days_are_read_only_and_active_timings_never_become_completed_samples() {
    let (_dir, mut data) = fixture();
    let from = 100_000;
    let mut live = record("live", from);
    live.model = "unknown-active-model".into();
    data.active
        .insert(live.attempt_id.clone(), (live, Instant::now()));
    let payload = json!({"from":from,"to":from+86_400,"day_starts":[from,from+86_400]});
    let result = build(&data, &payload).unwrap();
    assert_eq!(result["groups"][0]["requests"], 1);
    assert_eq!(result["groups"][0]["successes"], 0);
    assert_eq!(result["groups"][0]["failures"], 0);
    assert_eq!(result["groups"][0]["total_latency_ms"], 0);
    assert_eq!(result["groups"][0]["first_token_samples"], 0);
    assert!(result["groups"][0]["total_usd"].is_null());
    assert!(result["daily"][0]["total_usd"].is_null());
    assert!(result["performance"]["p50_latency_ms"].is_null());
    assert!(result["performance"]["p50_first_token_ms"].is_null());
    let snapshot = data.connection.total_changes();
    for invalid in [
        json!({"from":from,"to":from+86_400}),
        json!({"from":from,"to":from+86_400,"day_starts":[from+1,from+86_400]}),
        json!({"from":from,"to":from+86_400,"day_starts":[from,from+10,from+86_400]}),
        json!({"from":from,"to":from+86_400,"day_starts":[from,from,from+86_400]}),
        json!({"from":from,"to":from+86_400,"day_starts":[from,from+86_400],"status":"success"}),
    ] {
        assert!(build(&data, &invalid).is_err(), "accepted {invalid}");
    }
    assert_eq!(snapshot, data.connection.total_changes());
    assert_eq!(data.active.len(), 1);
}
