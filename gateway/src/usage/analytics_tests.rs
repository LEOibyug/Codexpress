use super::*;
use crate::usage::requests::tests::{fixture, persist, record};
use std::time::Instant;

#[test]
fn cached_history_overlays_live_counters_and_invalidates_after_database_changes() {
    let (_dir, mut data) = fixture();
    let from = 100_000;
    let payload = json!({"from":from,"to":from+86400,"day_starts":[from,from+86400]});
    persist(&data, &record("stored", from), Some("native"));
    let initial = build(&mut data, &payload).unwrap();
    assert_eq!(initial["groups"][0]["requests"], 1);
    let mut active = record("live", from + 1);
    active.output_tokens = 123;
    data.active
        .insert(active.attempt_id.clone(), (active.clone(), Instant::now()));
    let live = build(&mut data, &payload).unwrap();
    assert_eq!(live["groups"][0]["output_tokens"], 133);
    active.output_tokens = 456;
    data.active
        .insert(active.attempt_id.clone(), (active.clone(), Instant::now()));
    let updated = build(&mut data, &payload).unwrap();
    assert_eq!(updated["groups"][0]["output_tokens"], 466);
    persist(&data, &active, Some("chatbot"));
    data.active.clear();
    let completed = build(&mut data, &payload).unwrap();
    assert_eq!(completed["groups"][0]["requests"], 2);
    assert_eq!(completed["groups"][0]["output_tokens"], 466);
    assert_eq!(completed["groups"][0]["active_requests"], 0);
    assert!(completed["options"]["ingresses"]
        .as_array()
        .unwrap()
        .contains(&json!("chatbot")));
    data.connection.execute("DELETE FROM usage", []).unwrap();
    let empty = build(&mut data, &payload).unwrap();
    assert!(empty["groups"].as_array().unwrap().is_empty());
}

#[test]
fn whole_window_filters_daily_zeroes_options_and_timings_are_consistent() {
    let (_dir, mut data) = fixture();
    let from = 100_000;
    let days = [from, from + 23 * 3600, from + 48 * 3600, from + 72 * 3600];
    for index in 0..225 {
        let mut record = record(&format!("stored-{index}"), from + 1);
        record.latency_ms = 100 + index;
        record.first_token_latency_ms = Some(10 + index);
        record.token_details.stream_complete = index == 0;
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
    let full = build(
        &mut data,
        &json!({"from":from,"to":days[3],"day_starts":days}),
    )
    .unwrap();
    let a = &full["groups"][0];
    assert_eq!(a["requests"], 226);
    assert_eq!(a["active_requests"], 1);
    assert_eq!(a["successes"], 225);
    assert_eq!(a["failures"], 0);
    assert_eq!(a["input_tokens"], 22_505);
    assert_eq!(a["output_tokens"], 2_252);
    assert_eq!(a["throughput_output_tokens"], 2_250);
    assert_eq!(a["throughput_latency_ms"], (100..325).sum::<u64>() - 100 + 50);
    assert_eq!(a["timed_output_tokens"], 10);
    assert_eq!(a["total_generation_ms"], 50);
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
    let filtered = build(&mut data, &json!({"from":from,"to":days[3],"day_starts":days,"source_id":"a","model":"gpt-6.1-sol","ingress":"native"})).unwrap();
    assert_eq!(filtered["groups"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["groups"][0]["requests"], 225); // unknown ingress active is filtered out.
    assert_eq!(filtered["daily"][0]["requests"], 225);
    assert_eq!(filtered["daily"][1]["requests"], 0);
    assert_eq!(filtered["performance"]["p50_latency_ms"], 212.0);
    assert!((filtered["performance"]["p95_latency_ms"].as_f64().unwrap() - 312.8).abs() < 0.00001);
    assert_eq!(filtered["performance"]["p50_first_token_ms"], 122.0);
    assert_eq!(filtered["options"], full["options"]);
    let missing = build(
        &mut data,
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
fn rolling_hours_use_five_minute_buckets_and_exclude_outside_requests() {
    for hours in [1, 5] {
        let (_dir, mut data) = fixture();
        let from = 100_000;
        let to = from + hours * 3600;
        for (id, started_at) in [
            ("before", from - 1),
            ("first", from),
            ("first-end", from + 299),
            ("second", from + 300),
            ("last", to - 1),
            ("after", to),
        ] {
            persist(&data, &record(id, started_at), Some("native"));
        }
        let boundaries: Vec<_> = (from..=to).step_by(300).collect();
        let result = build(
            &mut data,
            &json!({"from":from,"to":to,"day_starts":boundaries}),
        )
        .unwrap();
        assert_eq!(result["groups"][0]["requests"], 4);
        let buckets = result["daily"].as_array().unwrap();
        assert_eq!(buckets.len(), hours as usize * 12);
        assert_eq!(buckets[0]["requests"], 2);
        assert_eq!(buckets[1]["requests"], 1);
        assert_eq!(buckets[1]["start_at"], from + 300);
        assert_eq!(buckets.last().unwrap()["requests"], 1);
        assert_eq!(result["performance"]["p50_latency_ms"], 100.0);
    }
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
    let result = build(&mut data, &payload).unwrap();
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
        json!({"from":from,"to":from+86_400,"day_starts":[from,from+86_400],"status":"bogus"}),
    ] {
        assert!(build(&mut data, &invalid).is_err(), "accepted {invalid}");
    }
    assert_eq!(snapshot, data.connection.total_changes());
    assert_eq!(data.active.len(), 1);
}

#[test]
fn appended_and_late_completions_extend_aligned_history_without_double_counting() {
    let (_dir, data) = fixture();
    let store = crate::usage::UsageStore {
        data: std::sync::Mutex::new(data),
    };
    let from = 100_000;
    let day = 86_400;
    for (id, at, latency, first_token) in [
        ("before", from, 1000, Some(100)),
        ("middle", from + day, 2000, None),
        ("after", from + 2 * day, 5000, Some(500)),
    ] {
        let mut row = record(id, at);
        if id == "middle" {
            row.model = "unknown-model".into();
        }
        row.latency_ms = latency;
        row.first_token_latency_ms = first_token;
        store.record(row).unwrap();
    }
    let middle = json!({"from":from+day,"to":from+2*day,"day_starts":[from+day,from+2*day]});
    let initial = store.analytics(&middle).unwrap();
    assert_eq!(initial["daily"][0]["requests"], 1);
    assert!(initial["daily"][0]["total_usd"].is_null());
    let mut late = record("late", from + day + 1);
    late.latency_ms = 3000;
    late.first_token_latency_ms = Some(300);
    late.source_name = "新名称".into();
    store.start(&late);
    assert_eq!(store.analytics(&middle).unwrap()["daily"][0]["requests"], 2);
    store.record(late).unwrap();
    assert_eq!(store.analytics(&middle).unwrap()["daily"][0]["requests"], 2);
    let mut older = record("older", from + 1);
    older.source_name = "最终名称".into();
    older.latency_ms = 4000;
    older.first_token_latency_ms = Some(0);
    store.record(older).unwrap();
    store.record(record("outside", from + 3 * day)).unwrap();
    let expanded =
        json!({"from":from,"to":from+3*day,"day_starts":[from,from+day,from+2*day,from+3*day]});
    let result = store.analytics(&expanded).unwrap();
    let known = result["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["model"] == "gpt-6.1-sol")
        .unwrap();
    assert_eq!(known["requests"], 4);
    assert_eq!(known["source_name"], "最终名称");
    assert_eq!(known["active_requests"], 0);
    assert_eq!(known["input_tokens"], 400);
    assert_eq!(known["output_tokens"], 40);
    assert_eq!(known["cached_input_tokens"], 80);
    assert_eq!(known["priced_requests"], 4);
    assert_eq!(
        result["daily"]
            .as_array()
            .unwrap()
            .iter()
            .map(|day| day["requests"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [2, 2, 1]
    );
    assert!(result["daily"][1]["total_usd"].as_f64().unwrap() > 0.0);
    assert_eq!(result["performance"]["p50_latency_ms"], 3000.0);
    assert_eq!(result["performance"]["p95_latency_ms"], 4800.0);
    assert_eq!(result["performance"]["p50_first_token_ms"], 200.0);
    assert_eq!(result["performance"]["p95_first_token_ms"], 470.0);
    assert_eq!(result["options"]["sources"][0]["name"], "最终名称");
    let repeated = store.analytics(&expanded).unwrap();
    for field in ["groups", "daily", "performance", "options"] {
        assert_eq!(repeated[field], result[field]);
    }
}

#[test]
fn rolling_window_adds_new_rows_and_moves_previous_rows_between_buckets() {
    let (_dir, data) = fixture();
    let store = crate::usage::UsageStore {
        data: std::sync::Mutex::new(data),
    };
    let from = 100_000;
    let rolling = |start| json!({"from":start,"to":start+3600,"day_starts":(start..=start+3600).step_by(300).collect::<Vec<_>>()});
    for (id, offset, latency) in [
        ("first", 299, 100),
        ("boundary", 300, 200),
        ("second", 303, 300),
        ("future", 3600, 400),
    ] {
        let mut row = record(id, from + offset);
        row.latency_ms = latency;
        store.record(row).unwrap();
    }
    assert_eq!(store.analytics(&rolling(from)).unwrap()["daily"][1]["requests"], 2);
    let mut appended = record("late", from + 10);
    appended.latency_ms = 500;
    store.record(appended).unwrap();
    let shifted = store.analytics(&rolling(from + 3)).unwrap();
    assert_eq!(shifted["groups"][0]["requests"], 5);
    assert_eq!(shifted["daily"][0]["requests"], 3);
    assert_eq!(shifted["daily"][1]["requests"], 1);
    assert_eq!(shifted["daily"][11]["requests"], 1);
    assert_eq!(shifted["performance"]["p50_latency_ms"], 300.0);

    let rebucketed = store.analytics(&rolling(from + 6)).unwrap();
    assert_eq!(rebucketed["groups"][0]["requests"], 5);
    assert_eq!(rebucketed["daily"][0]["requests"], 4);
    assert_eq!(rebucketed["daily"][1]["requests"], 0);
    assert_eq!(rebucketed["daily"][1]["total_usd"], 0);
    assert_eq!(rebucketed["daily"][0]["start_at"], from + 6);
    assert_eq!(rebucketed["performance"]["p50_latency_ms"], 300.0);
}

#[test]
fn corrections_diagnostics_and_shifted_windows_do_not_reuse_stale_totals() {
    let (dir, data) = fixture();
    persist(&data, &record("legacy", 100_000), Some("native"));
    let store = crate::usage::UsageStore {
        data: std::sync::Mutex::new(data),
    };
    let payload =
        json!({"from":100_000,"to":186_400,"day_starts":[100_000,186_400],"ingress":"native"});
    assert_eq!(
        store.analytics(&payload).unwrap()["groups"][0]["requests"],
        1
    );
    let source = serde_json::from_value(
        json!({"id":"a","name":"来源 A","kind":"api","base_url":"http://127.0.0.1:1",
        "enabled":true,"priority":0,"proxy":{"mode":"direct"},"secret":{"api_key":"fixture"}}),
    )
    .unwrap();
    let capture = crate::usage::diagnostics::Capture::new(
        "legacy",
        100_000,
        &source,
        "fixture",
        crate::usage::Ingress::Ssh,
    );
    store.diagnostic(&capture, false).unwrap();
    let changed = store.analytics(&payload).unwrap();
    assert!(changed["groups"].as_array().unwrap().is_empty());
    assert_eq!(changed["options"]["ingresses"], json!(["ssh"]));
    let all = json!({"from":100_000,"to":186_400,"day_starts":[100_000,186_400]});
    store.analytics(&all).unwrap();
    let external = rusqlite::Connection::open(dir.path().join("usage.sqlite")).unwrap();
    external.execute("UPDATE usage SET metadata=json_set(metadata,'$.cost.total_usd',7.5,'$.cost.billing_basis','upstream_charge')", []).unwrap();
    let mut appended = record("append", 100_001);
    appended.model = "unknown-model".into();
    store.record(appended).unwrap();
    let corrected = store.analytics(&all).unwrap();
    assert_eq!(corrected["daily"][0]["total_usd"], 7.5);
    assert_eq!(corrected["daily"][0]["requests"], 2);
    assert_eq!(corrected["daily"][0]["upstream_priced_requests"], 1);
    store
        .data
        .lock()
        .unwrap()
        .connection
        .execute("DELETE FROM usage WHERE attempt=1", [])
        .unwrap();
    store.record(record("after-delete", 100_002)).unwrap();
    let after_delete = store.analytics(&all).unwrap();
    assert_eq!(after_delete["daily"][0]["requests"], 2);
    assert_eq!(after_delete["daily"][0]["upstream_priced_requests"], 0);
    let rolling = |from| json!({"from":from,"to":from+3600,"day_starts":(from..=from+3600).step_by(300).collect::<Vec<_>>()});
    store.analytics(&rolling(100_001)).unwrap();
    let shifted = store.analytics(&rolling(100_002)).unwrap();
    assert_eq!(shifted["groups"][0]["requests"], 1);
    assert_eq!(shifted["daily"][0]["requests"], 1);
    assert_eq!(shifted["options"]["models"], json!(["gpt-6.1-sol"]));
}

#[test]
fn same_name_costs_preserve_mixed_json_numbers_in_both_orders() {
    let (_dir, mut data) = fixture();
    for (id, model, name, cost) in [
        ("a", "fraction-first", " 同名 ", json!(0.25)),
        ("b", "fraction-first", "同名", json!(2)),
        ("c", "integer-first", " 同名 ", json!(2)),
        ("d", "integer-first", "同名", json!(0.25)),
    ] {
        let mut row = record(id, 100_000);
        row.source_id = id.into();
        row.source_name = name.into();
        row.model = model.into();
        let attempt = persist(&data, &row, None);
        data.connection.execute(
            "UPDATE usage SET metadata=json_set(metadata,'$.cost.total_usd',json(?1)) WHERE attempt=?2",
            rusqlite::params![cost.to_string(), attempt],
        ).unwrap();
    }
    let result = build(
        &mut data,
        &json!({"from":100_000,"to":186_400,"day_starts":[100_000,186_400]}),
    )
    .unwrap();
    let groups = result["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    for group in groups {
        assert_eq!(group["source_name"], "同名");
        assert_eq!(group["requests"], 2);
        assert_eq!(group["total_usd"], 2.25);
        assert!(group["source_id"].is_null());
        assert_eq!(group["source_ids"].as_array().unwrap().len(), 2);
    }
}

#[test]
fn high_cardinality_timings_stay_exact_after_appends_and_aligned_expansion() {
    let (dir, _) = fixture();
    let store = crate::usage::UsageStore::open(&dir.path().join("usage.sqlite")).unwrap();
    let from = 100_000;
    let day = 86_400;
    store
        .data
        .lock()
        .unwrap()
        .connection
        .execute_batch("BEGIN")
        .unwrap();
    for (id, at, latency) in [("before", from, 4099), ("after", from + 2 * day, 4100)] {
        let mut row = record(id, at);
        row.model = "unknown-model".into();
        row.latency_ms = latency;
        row.first_token_latency_ms = Some(latency + 10_000);
        store.record(row).unwrap();
    }
    for latency in 1..=4096 {
        let mut row = record(&format!("sample-{latency}"), from + day);
        row.model = "unknown-model".into();
        row.latency_ms = latency;
        row.first_token_latency_ms = Some(latency + 10_000);
        store.record(row).unwrap();
    }
    store
        .data
        .lock()
        .unwrap()
        .connection
        .execute_batch("COMMIT")
        .unwrap();
    let middle = json!({"from":from+day,"to":from+2*day,"day_starts":[from+day,from+2*day]});
    assert_eq!(
        store.analytics(&middle).unwrap()["performance"]["p50_latency_ms"],
        2048.5
    );
    for latency in [4097, 4098] {
        let mut row = record(&format!("sample-{latency}"), from + day);
        row.model = "unknown-model".into();
        row.latency_ms = latency;
        row.first_token_latency_ms = Some(latency + 10_000);
        store.record(row).unwrap();
        let result = store.analytics(&middle).unwrap();
        assert_eq!(
            result["performance"]["p50_latency_ms"],
            (latency + 1) as f64 / 2.0
        );
        assert!(
            (result["performance"]["p95_latency_ms"].as_f64().unwrap()
                - (1.0 + (latency - 1) as f64 * 0.95))
                .abs()
                < 0.00001
        );
        assert_eq!(
            result["performance"]["p50_first_token_ms"],
            10_000.0 + (latency + 1) as f64 / 2.0
        );
        assert!(
            (result["performance"]["p95_first_token_ms"]
                .as_f64()
                .unwrap()
                - (10_001.0 + (latency - 1) as f64 * 0.95))
                .abs()
                < 0.00001
        );
        let cold = crate::usage::UsageStore::open(&dir.path().join("usage.sqlite"))
            .unwrap()
            .analytics(&middle)
            .unwrap();
        for field in ["groups", "daily", "performance", "options"] {
            assert_eq!(result[field], cold[field]);
        }
    }
    let expanded =
        json!({"from":from,"to":from+3*day,"day_starts":[from,from+day,from+2*day,from+3*day]});
    let result = store.analytics(&expanded).unwrap();
    assert_eq!(result["performance"]["p50_latency_ms"], 2050.5);
    assert!((result["performance"]["p95_latency_ms"].as_f64().unwrap() - 3895.05).abs() < 0.00001);
    assert_eq!(result["performance"]["p50_first_token_ms"], 12050.5);
    assert!(
        (result["performance"]["p95_first_token_ms"]
            .as_f64()
            .unwrap()
            - 13895.05)
            .abs()
            < 0.00001
    );
    let cold = crate::usage::UsageStore::open(&dir.path().join("usage.sqlite"))
        .unwrap()
        .analytics(&expanded)
        .unwrap();
    for field in ["groups", "daily", "performance", "options"] {
        assert_eq!(result[field], cold[field]);
    }
    assert_eq!(
        store.analytics(&middle).unwrap()["performance"]["p50_latency_ms"],
        2049.5
    );
}
