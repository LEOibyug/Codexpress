use super::*;
use crate::usage::requests::tests::{fixture, persist, record};

fn capture(id: &str) -> Capture {
    let source: Source = serde_json::from_value(json!({
        "id":"fixture","name":"测试来源","kind":"api","base_url":"http://127.0.0.1:1",
        "enabled":true,"priority":0,"proxy":{"mode":"direct"},
        "secret":{"api_key":"fixture-secret-key","refresh_token":"fixture-refresh-token"}
    }))
    .unwrap();
    Capture::new(id, crate::model::now(), &source, "fixture-gateway-key")
}

fn stored(data: &StoreData, id: &str) -> Value {
    detail(data, &json!({"attempt_id":id,"body_limit":262144})).unwrap()["diagnostic"].clone()
}

#[test]
fn unicode_empty_tokens_and_existing_placeholders_are_redacted_without_panics() {
    let input = "错误 Bearer 秘密令牌，路径 https://example.test/?code=&state=oauth-state&key=\" Bearer [已隐藏]，后续";
    let output = redact_tokens(input);
    assert!(!output.contains("秘密令牌"));
    assert!(!output.contains("oauth-state"));
    assert!(!output.contains("[已隐藏]已隐藏"));
    assert_eq!(redact_tokens(&output), output);
    assert_eq!(redact_tokens("?code=&state=&key="), "?code=&state=&key=");
    let output = redact_tokens("Bearer   space-secret; bearer\ttab-secret\nBearer \t mixed-secret");
    for secret in ["space-secret", "tab-secret", "mixed-secret"] {
        assert!(!output.contains(secret));
    }
    let mut overlap = capture("overlap");
    overlap.secrets = vec!["private-long-secret".into(), "private".into()];
    assert_eq!(overlap.redact("private-long-secret"), HIDDEN);
    overlap.secrets = vec!["prefix[已隐藏]suffix".into()];
    assert_eq!(
        overlap.redact("prefix[已隐藏]suffix 后续"),
        format!("{HIDDEN} 后续")
    );
}

#[test]
fn failure_feedback_and_metadata_hide_credentials_without_capturing_success_content() {
    let (_dir, data) = fixture();
    persist(&data, &record("failed", crate::model::now()), None);
    let mut failed = capture("failed");
    failed.request(
        "native",
        "POST",
        "/v1/responses",
        "https://user:password@example.test/v1/responses?key=private",
        &[],
    );
    failed.response(400, &HeaderMap::new());
    failed.chunk(serde_json::to_string(&json!({"error":{"message":"中文错误 fixture-secret-key Bearer another-private-token https://example.test/?code=oauth-callback&state=oauth-state","access_token":"unrelated-access","nested":{"API-Key":"nested-secret"},"empty":null}})).unwrap().as_bytes());
    failed.complete();
    save(&data.connection, &failed, true).unwrap();
    let value = stored(&data, "failed");
    let text = value.to_string();
    for secret in [
        "fixture-secret-key",
        "another-private-token",
        "oauth-callback",
        "oauth-state",
        "unrelated-access",
        "nested-secret",
        "user:password",
        "key=private",
    ] {
        assert!(!text.contains(secret), "a credential escaped redaction");
    }
    assert!(text.contains("中文错误"));

    persist(&data, &record("success", crate::model::now()), None);
    let mut success = capture("success");
    success.response(200, &HeaderMap::new());
    success.chunk(b"PRIVATE-SUCCESS-CONTENT");
    success.complete();
    save(&data.connection, &success, true).unwrap();
    assert_eq!(stored(&data, "success")["body_text"], "");
}

#[test]
fn failed_events_preserve_errors_but_never_generated_output() {
    let (_dir, data) = fixture();
    persist(&data, &record("event", crate::model::now()), None);
    let mut event = capture("event");
    event.response(200, &HeaderMap::new());
    event.failed_event(&json!({"type":"response.failed","response":{"id":"response-fixture","status":"failed","output":[{"content":"PRIVATE-GENERATED-OUTPUT"}],"error":{"code":"server_error","message":"完整失败原因"}}}));
    event.chunk(b"PRIVATE-LATER-SUCCESS-DELTA");
    save(&data.connection, &event, true).unwrap();
    let value = stored(&data, "event");
    assert!(value["body_text"]
        .as_str()
        .unwrap()
        .contains("完整失败原因"));
    assert!(!value.to_string().contains("PRIVATE-GENERATED-OUTPUT"));
    assert!(!value.to_string().contains("PRIVATE-LATER-SUCCESS-DELTA"));
    assert_eq!(value["error_code"], "server_error");
    persist(&data, &record("json-error", crate::model::now()), None);
    let mut json_error = capture("json-error");
    json_error.response(200, &HeaderMap::new());
    json_error.failed_event(&json!({"error":{"code":"server_error","message":"HTTP 200 错误对象","details":{"why":"完整原因"}},"output":[{"text":"PRIVATE-SUCCESS"}]}));
    save(&data.connection, &json_error, true).unwrap();
    let value = stored(&data, "json-error");
    assert!(value["body_text"].as_str().unwrap().contains("完整原因"));
    assert!(!value.to_string().contains("PRIVATE-SUCCESS"));
}

#[test]
fn live_details_have_rates_and_costs_and_empty_legacy_ids_are_repairable() {
    use std::time::{Duration, Instant};
    let (_dir, mut data) = fixture();
    let mut live = record("live-detail", crate::model::now());
    live.status = "streaming".into();
    live.first_token_latency_ms = Some(100);
    live.token_details.stream_complete = false;
    data.active.insert(
        live.attempt_id.clone(),
        (live, Instant::now() - Duration::from_secs(2)),
    );
    let value = detail(&data, &json!({"attempt_id":"live-detail"})).unwrap();
    assert_eq!(value["record"]["active"], true);
    assert!(value["record"]["latency_ms"].as_u64().unwrap() >= 2000);
    assert!(value["record"]["generation_ms"].as_u64().unwrap() >= 1900);
    assert!(value["record"]["output_tps"].is_number());
    assert!(value["record"]["cost"].is_object());
    let row = persist(&data, &record("empty-id", crate::model::now()), None);
    data.connection
        .execute(
            "UPDATE usage SET metadata='{\"attempt_id\":\"\"}' WHERE attempt=?1",
            [row],
        )
        .unwrap();
    let value = detail(&data, &json!({"attempt_id":format!("legacy:{row}")})).unwrap();
    assert_eq!(value["record"]["attempt_id"], format!("legacy:{row}"));
    assert_eq!(value["related"][0]["attempt_id"], format!("legacy:{row}"));
}

#[test]
fn compressed_failure_bodies_are_decoded_before_redaction_and_bounded_after_cleanup() {
    use std::io::Write;
    let (_dir, data) = fixture();
    let message = json!({"error":{"message":"中文失败 fixture-secret-key Bearer hidden-bearer","refresh_token":"unknown-refresh"}}).to_string();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(message.as_bytes()).unwrap();
    let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    zlib.write_all(message.as_bytes()).unwrap();
    let mut raw = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
    raw.write_all(message.as_bytes()).unwrap();
    for (id, encoding, bytes) in [
        ("gzip", "gzip", gzip.finish().unwrap()),
        ("zlib", "deflate", zlib.finish().unwrap()),
        ("deflate", "deflate", raw.finish().unwrap()),
        (
            "zstd",
            "zstd",
            zstd::stream::encode_all(message.as_bytes(), 1).unwrap(),
        ),
    ] {
        persist(&data, &record(id, crate::model::now()), None);
        let mut event = capture(id);
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", encoding.parse().unwrap());
        event.response(503, &headers);
        event.chunk(&bytes);
        event.complete();
        save(&data.connection, &event, true).unwrap();
        let value = stored(&data, id);
        let text = value["body_text"].as_str().unwrap();
        assert!(text.contains("中文失败"));
        for secret in ["fixture-secret-key", "hidden-bearer", "unknown-refresh"] {
            assert!(!text.contains(secret));
        }
    }
    persist(&data, &record("bomb", crate::model::now()), None);
    let huge = "文".repeat(BODY_LIMIT / 3 + 100);
    let mut event = capture("bomb");
    let mut headers = HeaderMap::new();
    headers.insert("content-encoding", "zstd".parse().unwrap());
    event.response(503, &headers);
    event.chunk(&zstd::stream::encode_all(huge.as_bytes(), 1).unwrap());
    event.complete();
    save(&data.connection, &event, true).unwrap();
    let (meta, compressed): (String, Vec<u8>) = data
        .connection
        .query_row(
            "SELECT metadata,body FROM diagnostics WHERE attempt_id='bomb'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&meta).unwrap()["body_truncated"],
        true
    );
    assert!(
        zstd::stream::decode_all(compressed.as_slice())
            .unwrap()
            .len()
            <= BODY_LIMIT
    );
    let mut split = capture("split-secret");
    split.response(503, &headers);
    let plain = format!("{}fixture-secret-key", " ".repeat(BODY_LIMIT - 10));
    split.chunk(&zstd::stream::encode_all(plain.as_bytes(), 1).unwrap());
    split.complete();
    let (text, truncated, _) = split.error_text();
    assert!(truncated);
    assert!(!text.unwrap().contains("fixture-se"));
}

#[test]
fn raw_and_sanitized_limits_and_incomplete_compressed_feedback_are_explicit() {
    let (_dir, data) = fixture();
    for (id, bytes, complete) in [
        ("expanded", vec![b'x'; BODY_LIMIT], true),
        ("exact", vec![b'x'; BODY_LIMIT], false),
        ("oversized", vec![b'x'; BODY_LIMIT + 10], true),
    ] {
        persist(&data, &record(id, crate::model::now()), None);
        let mut event = capture(id);
        if id == "expanded" {
            event.secrets = vec!["x".into()];
        }
        event.response(500, &HeaderMap::new());
        event.chunk(&bytes);
        if complete {
            event.complete();
        }
        save(&data.connection, &event, true).unwrap();
        let (metadata, compressed): (String, Vec<u8>) = data
            .connection
            .query_row(
                "SELECT metadata,body FROM diagnostics WHERE attempt_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            zstd::stream::decode_all(compressed.as_slice())
                .unwrap()
                .len()
                <= BODY_LIMIT
        );
        assert_eq!(
            serde_json::from_str::<Value>(&metadata).unwrap()["body_truncated"],
            true
        );
    }
    persist(&data, &record("broken", crate::model::now()), None);
    let mut event = capture("broken");
    let mut headers = HeaderMap::new();
    headers.insert("content-encoding", "gzip".parse().unwrap());
    event.response(400, &headers);
    event.chunk(b"not gzip PRIVATE-BINARY");
    event.complete();
    save(&data.connection, &event, true).unwrap();
    let value = stored(&data, "broken");
    assert_eq!(value["body_text"], "");
    assert_eq!(value["body_complete"], false);
    assert!(value["body_decode_error"].is_string());

    assert!(!capture("unused")
        .redact("{\"api_key\":\"unknown-secret-without-end")
        .contains("unknown-secret"));
}

#[test]
fn body_retention_evicts_oldest_and_expired_feedback_and_metadata_updates_preserve_body() {
    let (_dir, data) = fixture();
    let now = crate::model::now();
    persist(&data, &record("retained", now), None);
    let mut event = capture("retained");
    event.response(500, &HeaderMap::new());
    event.chunk("错误详情".as_bytes());
    event.complete();
    save(&data.connection, &event, true).unwrap();
    save(&data.connection, &event, false).unwrap();
    assert_eq!(stored(&data, "retained")["body_text"], "错误详情");
    assert_eq!(
        stored(&data, "retained")["body_saved_bytes"],
        "错误详情".len()
    );
    data.connection
        .execute(
            "UPDATE diagnostics SET created_at=?1 WHERE attempt_id='retained'",
            [now - 31 * 86400],
        )
        .unwrap();
    initialize(&data.connection).unwrap();
    let value = stored(&data, "retained");
    assert_eq!(value["body_evicted"], true);
    assert_eq!(value["body_text"], "");

    // SQLite zeroblobs exercise the storage budget without allocating 128 MiB in Rust.
    for (id, age) in [("old", 2), ("new", 1)] {
        data.connection
            .execute(
                "INSERT INTO diagnostics VALUES(?1,?2,'{}',zeroblob(?3))",
                params![id, now - age, STORAGE_LIMIT / 2 + 1],
            )
            .unwrap();
    }
    prune(&data.connection).unwrap();
    let old: bool = data
        .connection
        .query_row(
            "SELECT body IS NULL FROM diagnostics WHERE attempt_id='old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let new: bool = data
        .connection
        .query_row(
            "SELECT body IS NULL FROM diagnostics WHERE attempt_id='new'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(old);
    assert!(!new);
    let total: i64 = data
        .connection
        .query_row(
            "SELECT COALESCE(SUM(length(body)),0) FROM diagnostics",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(total <= STORAGE_LIMIT);
}

#[test]
fn utf8_pages_make_progress_and_legacy_records_have_no_body() {
    let (_dir, data) = fixture();
    persist(&data, &record("unicode", crate::model::now()), None);
    let mut event = capture("unicode");
    event.response(400, &HeaderMap::new());
    event.chunk("中文🙂结束".as_bytes());
    save(&data.connection, &event, true).unwrap();
    let mut offset = 0;
    let mut text = String::new();
    loop {
        let value = detail(
            &data,
            &json!({"attempt_id":"unicode","body_offset":offset,"body_limit":1}),
        )
        .unwrap();
        let diagnostic = &value["diagnostic"];
        text.push_str(diagnostic["body_text"].as_str().unwrap());
        let Some(next) = diagnostic["body_next_offset"].as_u64() else {
            break;
        };
        assert!(next > offset, "a UTF-8 page must advance");
        offset = next;
    }
    assert_eq!(text, "中文🙂结束");
    assert!(detail(&data, &json!({"attempt_id":"unicode","body_offset":1})).is_err());
    assert!(detail(&data, &json!({"attempt_id":"unicode","body_limit":262145})).is_err());
    assert!(detail(
        &data,
        &json!({"attempt_id":"unicode","body_offset":u64::MAX})
    )
    .is_err());
    let row = persist(&data, &record("legacy", crate::model::now()), None);
    data.connection
        .execute("UPDATE usage SET metadata='{}' WHERE attempt=?1", [row])
        .unwrap();
    assert!(
        detail(&data, &json!({"attempt_id":format!("legacy:{row}")})).unwrap()["diagnostic"]
            .is_null()
    );
}
