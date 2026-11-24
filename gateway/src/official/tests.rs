//! These tests run only against a local simulated explicit proxy and temporary state.
use crate::{credentials, model, official, routing};
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use model::{AppState, ProxyPolicy, Source, SourceRuntime};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Mutex;

#[derive(Default)]
struct Fixture {
    posts: AtomicUsize,
    tokens: AtomicUsize,
    reads: AtomicUsize,
    quota_reads: AtomicUsize,
    quota_delay_ms: AtomicUsize,
    reflect_redemption: std::sync::atomic::AtomicBool,
    invalid_credits: std::sync::atomic::AtomicBool,
    token_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    credit_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    credit_started: tokio::sync::Notify,
    inference_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    inference_started: tokio::sync::Notify,
    uncertain: bool,
    headers: Mutex<Vec<String>>,
    quota: Mutex<Option<Value>>,
    inventory: Mutex<Option<Value>>,
    consumed: Mutex<Vec<String>>,
    token_response: Mutex<Option<Value>>,
}
async fn fixture(uncertain: bool) -> (String, Arc<Fixture>, tokio::task::JoinHandle<()>) {
    let data = Arc::new(Fixture {
        uncertain,
        ..Default::default()
    });
    async fn inventory(State(s): State<Arc<Fixture>>) -> Json<Value> {
        s.reads.fetch_add(1, Ordering::SeqCst);
        let custom = s.inventory.lock().await.clone();
        let inventory = if let Some(mut inventory) = custom {
            if s.reflect_redemption.load(Ordering::SeqCst) {
                let consumed = s.consumed.lock().await;
                for card in inventory["credits"].as_array_mut().unwrap() {
                    if consumed.iter().any(|id| card["id"].as_str() == Some(id.as_str())) {
                        card["status"] = json!("redeemed");
                    }
                }
            }
            inventory
        } else if s.invalid_credits.load(Ordering::SeqCst) {
            json!([])
        } else if s.reflect_redemption.load(Ordering::SeqCst) && s.posts.load(Ordering::SeqCst) > 0
        {
            json!({"available_count":0,"credits":[{"id":"card-a","status":"redeemed","expires_at":"2099-01-01T00:00:00Z","reset_type":"codex_rate_limits"}]})
        } else {
            json!({"available_count":1,"credits":[{"id":"card-a","title":"模拟卡","status":"available","expires_at":"2099-01-01T00:00:00Z","reset_type":"codex_rate_limits"}]})
        };
        let gate = s.credit_gate.lock().await.clone();
        if let Some(gate) = gate {
            s.credit_started.notify_one();
            gate.notified().await;
        }
        Json(inventory)
    }
    async fn usage(State(s): State<Arc<Fixture>>) -> Json<Value> {
        s.quota_reads.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(
            s.quota_delay_ms.load(Ordering::SeqCst) as u64,
        ))
        .await;
        if let Some(value) = s.quota.lock().await.clone() {
            return Json(value);
        }
        Json(
            json!({"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":10,"reset_at":2000000000,"limit_window_seconds":18000},"secondary_window":{"used_percent":20,"reset_at":2000500000,"limit_window_seconds":604800}}}),
        )
    }
    async fn consume(
        State(s): State<Arc<Fixture>>,
        headers: axum::http::HeaderMap,
        Json(payload): Json<Value>,
    ) -> Json<Value> {
        s.consumed.lock().await.push(payload["credit_id"].as_str().unwrap().to_owned());
        assert!(payload["redeem_request_id"].is_string());
        s.posts.fetch_add(1, Ordering::SeqCst);
        s.headers.lock().await.push(
            headers
                .get("ChatGPT-Account-ID")
                .unwrap()
                .to_str()
                .unwrap()
                .into(),
        );
        if s.uncertain {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        Json(json!({"code":"reset","windows_reset":2}))
    }
    async fn token(State(s): State<Arc<Fixture>>) -> Json<Value> {
        let gate = s.token_gate.lock().await.clone();
        s.tokens.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = gate {
            gate.notified().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if let Some(value) = s.token_response.lock().await.clone() {
            return Json(value);
        }
        Json(
            json!({"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600,"id_token":"eyJhbGciOiJub25lIn0.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjb3VudC1hIn19."}),
        )
    }
    let router = Router::new()
        .route("/credits", get(inventory))
        .route("/usage", get(usage))
        .route("/consume", post(consume))
        .route("/token", post(token))
        .fallback(
            |State(s): State<Arc<Fixture>>, method: axum::http::Method| async move {
                if method != axum::http::Method::CONNECT {
                    return axum::http::StatusCode::NOT_FOUND;
                }
                let gate = s.inference_gate.lock().await.clone();
                if let Some(gate) = gate {
                    // Hold only the simulated proxy handshake; never tunnel to an upstream.
                    s.inference_started.notify_one();
                    gate.notified().await;
                }
                axum::http::StatusCode::BAD_GATEWAY
            },
        )
        .with_state(data.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{address}"), data, task)
}
async fn setup(proxy: &str) -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::new(directory.path().to_path_buf(), 0, "local-fixture".into()).unwrap();
    for (id, account) in [("official-a", "account-a"), ("official-b", "account-b")] {
        state.sources.write().await.push(SourceRuntime::new(Source {id:id.into(),name:id.into(),kind:"official".into(),show_in_cmux:true,is_subscription:false,base_url:"https://chatgpt.com/backend-api/codex".into(),enabled:true,priority:0,max_active_conversations:None,active_conversation_window_seconds:15,proxy:ProxyPolicy {mode:"explicit".into(),url:Some(proxy.into())},models:vec![],account_id:Some(account.into()),secret:json!({"access_token":"fixture-access","refresh_token":"fixture-refresh","expires_at":model::now()+3600,"account_id":account})}));
    }
    (directory, state)
}

async fn elapse(seconds: u64) {
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(seconds)).await;
    tokio::time::resume();
}

#[test]
fn unknown_fields_stay_unknown_and_credit_dates_are_normalized() {
    let quota=official::fixture::parse_quota(&json!({"plan_type":"future_plan","rate_limit":{"primary_window":{"used_percent":null,"reset_at":"unknown"},"secondary_window":{"used_percent":75}}})).unwrap();
    assert_eq!(quota["plan_type"], "future_plan");
    assert!(quota["windows"][0]["used_percent"].is_null());
    assert!(quota["windows"][0]["reset_at"].is_null());
    let credits=official::fixture::parse_credits(&json!({"credits":[{"id":"x","status":"future","expires_at":"2026-10-20T08:00:00.123+08:00"},{"title":"missing"}]})).unwrap();
    assert!(credits["available_count"].is_null());
    assert_eq!(credits["credits"][0]["status"], "future");
    assert_eq!(credits["credits"][0]["expires_at"], 1792454400u64);
    assert!(credits["credits"][1]["id"].is_null());
    assert_eq!(credits["credits"][1]["status"], "unknown");
}

#[tokio::test]
async fn frequent_quota_refresh_retains_weekly_data_and_does_not_poll_credits() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    let first = official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid")
        .await
        .unwrap();
    official::fixture::selected_snapshots_at(&state, "official-a", "http://fixture.invalid", false).await.unwrap();
    assert_eq!(counters.quota_reads.load(Ordering::SeqCst), 2);
    assert_eq!(counters.reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        first["quota"]["windows"][1]["window_seconds"].as_f64(),
        Some(604800.0)
    );
    assert_eq!(
        first["quota"]["windows"][1]["used_percent"].as_f64(),
        Some(20.0)
    );
    assert_eq!(first["quota"]["windows"][1]["reset_at"], 2000500000);
    let saved_credits = first["credits"].clone();
    *counters.quota.lock().await = Some(json!({"rate_limit":{
        "primary_window":{"used_percent":35,"limit_window_seconds":18000},
        "secondary_window":{"used_percent":70,"limit_window_seconds":604800,"reset_at":2000600000}
    }}));
    let refreshed = official::fixture::selected_snapshots_at(
        &state,
        "official-a",
        "http://fixture.invalid",
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        refreshed["quota"]["windows"][1]["used_percent"].as_f64(),
        Some(70.0)
    );
    assert_eq!(refreshed["quota"]["windows"][1]["reset_at"], 2000600000);
    assert_eq!(refreshed["credits"], saved_credits);
    assert_eq!(counters.reads.load(Ordering::SeqCst), 1);
    assert_eq!(counters.quota_reads.load(Ordering::SeqCst), 3);
    // A failed quota parse preserves the last weekly snapshot and card state.
    *counters.quota.lock().await = Some(json!([]));
    let failed = official::fixture::selected_snapshots_at(
        &state,
        "official-a",
        "http://fixture.invalid",
        false,
    )
    .await
    .unwrap();
    assert_eq!(failed["quota"]["stale"], true);
    assert_eq!(
        failed["quota"]["windows"][1]["used_percent"].as_f64(),
        Some(70.0)
    );
    assert_eq!(failed["credits"], saved_credits);
    assert_eq!(counters.reads.load(Ordering::SeqCst), 1);
    official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid")
        .await
        .unwrap();
    assert_eq!(counters.reads.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn account_queries_deduplicate_and_publish_quota_before_slow_cards() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    {
        let mut sources = state.sources.write().await;
        sources[0].quota_exhausted = true;
        sources[0].health = "exhausted".into();
        let mut duplicate = sources[0].clone();
        duplicate.source.id = "duplicate-a".into();
        sources[1].credits = json!({"credits":[{"id":"account-b-card"}]});
        sources.push(duplicate);
    }
    counters.quota_delay_ms.store(50, Ordering::SeqCst);
    let gate = Arc::new(tokio::sync::Notify::new());
    *counters.credit_gate.lock().await = Some(gate.clone());
    let copy = state.clone();
    let queries = tokio::spawn(async move {
        tokio::join!(
            official::fixture::snapshots_at(&copy, "official-a", "http://fixture.invalid"),
            official::fixture::snapshots_at(&copy, "duplicate-a", "http://fixture.invalid"),
        )
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        counters.credit_started.notified(),
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let sources = state.sources.read().await;
            if !routing::official_exhausted(&sources[0], &sources) {
                assert!(!routing::official_exhausted(&sources[2], &sources));
                assert!(sources[0].credits.is_null());
                break;
            }
            drop(sources);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!queries.is_finished());
    assert_eq!(counters.quota_reads.load(Ordering::SeqCst), 1);
    assert_eq!(counters.reads.load(Ordering::SeqCst), 1);
    gate.notify_one();
    let (first, second) = queries.await.unwrap();
    assert_eq!(first.unwrap(), second.unwrap());
    {
        let sources = state.sources.read().await;
        assert_eq!(sources[0].credits, sources[2].credits);
        assert_eq!(sources[1].credits["credits"][0]["id"], "account-b-card");
    }
    *counters.credit_gate.lock().await = None;
    counters.reflect_redemption.store(true, Ordering::SeqCst);
    let old_inventory_gate = Arc::new(tokio::sync::Notify::new());
    *counters.credit_gate.lock().await = Some(old_inventory_gate.clone());
    let copy = state.clone();
    let old_inventory = tokio::spawn(async move {
        official::fixture::snapshots_at(&copy, "official-a", "http://fixture.invalid")
            .await
            .unwrap()
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        counters.credit_started.notified(),
    )
    .await
    .unwrap();
    *counters.credit_gate.lock().await = None;
    let payload = json!({"source_id":"official-a","expected_account_id":"account-a","credit_id":"card-a","operation_id":"alias-reset","confirmed":true});
    let copy = state.clone();
    let mut reset = tokio::spawn(async move {
        official::fixture::consume_at(&copy, &payload, "http://fixture.invalid")
            .await
            .unwrap()
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut reset)
            .await
            .is_err()
    );
    assert_eq!(counters.posts.load(Ordering::SeqCst), 0);
    old_inventory_gate.notify_one();
    old_inventory.await.unwrap();
    let result = reset.await.unwrap();
    assert_eq!(result["code"], "reset");
    {
        let sources = state.sources.read().await;
        assert_eq!(sources[0].credits["credits"][0]["status"], "redeemed");
        assert_eq!(sources[0].credits, sources[2].credits);
        assert_eq!(sources[1].credits["credits"][0]["id"], "account-b-card");
    }
    counters.invalid_credits.store(true, Ordering::SeqCst);
    official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid")
        .await
        .unwrap();
    let sources = state.sources.read().await;
    assert_eq!(sources[0].credits["stale"], true);
    assert_eq!(sources[2].credits["stale"], true);
    assert_eq!(sources[2].credits["credits"][0]["status"], "redeemed");
    task.abort();
}

#[tokio::test]
async fn fresh_quota_recovers_only_the_proxy_failure_observed_before_query() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    routing::mark_failure(&state, "official-a", 0, None).await;
    official::fixture::selected_snapshots_at(&state, "official-a", "http://fixture.invalid", false)
        .await
        .unwrap();
    {
        let sources = state.sources.read().await;
        assert_eq!(sources[0].health, "ready");
        assert!(sources[0].last_error.is_none());
        assert!(sources[0].cooldown_until.is_none());
    }
    for status in [401, 429] {
        routing::mark_failure(&state, "official-a", status, None).await;
        let before = state.sources.read().await[0].health.clone();
        official::fixture::selected_snapshots_at(
            &state,
            "official-a",
            "http://fixture.invalid",
            false,
        )
        .await
        .unwrap();
        assert_eq!(state.sources.read().await[0].health, before);
    }
    routing::mark_failure(&state, "official-a", 0, None).await;
    *counters.quota.lock().await = Some(json!([]));
    official::fixture::selected_snapshots_at(&state, "official-a", "http://fixture.invalid", false)
        .await
        .unwrap();
    assert_eq!(state.sources.read().await[0].health, "proxy_error");
    *counters.quota.lock().await = None;
    counters.quota_delay_ms.store(80, Ordering::SeqCst);
    let prior = counters.quota_reads.load(Ordering::SeqCst);
    elapse(60).await;
    let copy = state.clone();
    let query = tokio::spawn(async move {
        official::fixture::selected_snapshots_at(
            &copy,
            "official-a",
            "http://fixture.invalid",
            false,
        )
        .await
        .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while counters.quota_reads.load(Ordering::SeqCst) == prior {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    routing::mark_failure(&state, "official-a", 0, None).await;
    query.await.unwrap();
    assert_eq!(state.sources.read().await[0].health, "proxy_error");
    task.abort();
}

#[tokio::test]
async fn login_strictly_binds_source_and_state_before_any_exchange() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    let started = official::handle(
        &state,
        "official.login.start",
        json!({"source_id":"official-a"}),
    )
    .await
    .unwrap();
    let flow = started["flow_id"].as_str().unwrap();
    let authorization = url::Url::parse(started["auth_url"].as_str().unwrap()).unwrap();
    let nonce = authorization
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    let callback = format!("http://localhost:1455/auth/callback?code=fixture-code&state={nonce}");
    assert!(official::fixture::complete_at(
        &state,
        "official-b",
        flow,
        &callback,
        "http://fixture.invalid"
    )
    .await
    .is_err());
    for callback in [
        "http://localhost:1455/auth/callback?code=x",
        "http://localhost:1455/auth/callback?code=x&state=wrong",
        "http://evil.invalid:1455/auth/callback?code=x&state=wrong",
    ] {
        assert!(official::fixture::complete_at(
            &state,
            "official-a",
            flow,
            callback,
            "http://fixture.invalid"
        )
        .await
        .is_err());
    }
    assert_eq!(counters.tokens.load(Ordering::SeqCst), 0);
    let connected = official::fixture::complete_at(
        &state,
        "official-a",
        flow,
        &callback,
        "http://fixture.invalid",
    )
    .await
    .unwrap();
    assert_eq!(connected["secret"]["access_token"], "new-access");
    assert_eq!(counters.tokens.load(Ordering::SeqCst), 1);
    let status = official::handle(
        &state,
        "official.login.status",
        json!({"source_id":"official-a","flow_id":flow}),
    )
    .await
    .unwrap();
    assert_eq!(status["state"], "complete");
    assert!(official::handle(
        &state,
        "official.login.status",
        json!({"source_id":"official-b","flow_id":flow})
    )
    .await
    .is_err());
    let mut rotated = connected["secret"].clone();
    rotated["access_token"] = json!("newer-access");
    rotated["refresh_token"] = json!("newer-refresh");
    official::fixture::install_secret(&state, "official-a", rotated)
        .await
        .unwrap();
    let replay = official::fixture::complete_at(
        &state,
        "official-a",
        flow,
        &callback,
        "http://fixture.invalid",
    )
    .await
    .unwrap();
    assert_eq!(replay["secret"]["access_token"], "newer-access");
    let mut switched = state.sources.read().await[1].source.secret.clone();
    switched["account_id"] = json!("account-c");
    official::fixture::install_secret(&state, "official-a", switched)
        .await
        .unwrap();
    assert!(official::fixture::complete_at(
        &state,
        "official-a",
        flow,
        &callback,
        "http://fixture.invalid",
    )
    .await
    .is_err());
    assert_eq!(counters.tokens.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn concurrent_refresh_rotates_once_and_never_writes_tokens_to_disk() {
    let (proxy, counters, task) = fixture(false).await;
    let (dir, state) = setup(&proxy).await;
    state.sources.write().await[0].source.secret["expires_at"] = json!(1);
    let mut duplicate = state.sources.read().await[0].clone();
    duplicate.source.id = "duplicate-a".into();
    state.sources.write().await.push(duplicate);
    let (a, b) = tokio::join!(
        official::fixture::refresh_at(&state, "official-a", "http://fixture.invalid"),
        official::fixture::refresh_at(&state, "duplicate-a", "http://fixture.invalid")
    );
    assert_eq!(a.unwrap().secret["access_token"], "new-access");
    assert_eq!(b.unwrap().secret["refresh_token"], "new-refresh");
    assert_eq!(counters.tokens.load(Ordering::SeqCst), 1);
    assert_eq!(
        state.sources.read().await[1].source.secret["refresh_token"],
        "fixture-refresh"
    );
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let bytes = std::fs::read(path).unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("new-refresh"));
        }
    }
    task.abort();
}

#[tokio::test]
async fn refreshed_access_identity_cannot_hide_behind_a_retained_id_token() {
    use base64::Engine;

    let (proxy, counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({"https://api.openai.com/auth":{"chatgpt_account_id":"account-b"}})).unwrap(),
    );
    *counters.token_response.lock().await = Some(json!({
        "access_token":format!("eyJhbGciOiJub25lIn0.{claims}."),
        "refresh_token":"replacement-refresh",
        "expires_in":3600
    }));
    let before = {
        let mut sources = state.sources.write().await;
        sources[0].source.secret["expires_at"] = json!(1);
        sources[0].source.secret["id_token"] = json!("eyJhbGciOiJub25lIn0.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjb3VudC1hIn19.");
        sources[0].source.secret.clone()
    };

    assert!(official::fixture::refresh_at(&state, "official-a", "http://fixture.invalid").await.is_err());
    let sources = state.sources.read().await;
    assert_eq!(sources[0].source.secret, before);
    assert_eq!(sources[0].source.account_id.as_deref(), Some("account-a"));
    assert_eq!(counters.tokens.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn concurrent_logins_for_one_account_install_only_one_source() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, original) = setup(&proxy).await;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(CredentialFixture::default());
    let state = AppState::new_with_credential_store(
        directory.path().to_path_buf(),
        0,
        "fixture".into(),
        Some(store.clone()),
    )
    .unwrap();
    *store.owner.lock().unwrap() = Arc::downgrade(&state);
    *state.sources.write().await = original.sources.read().await.clone();
    for runtime in state.sources.write().await.iter_mut() {
        runtime.source.account_id = None;
        runtime.source.secret = json!({});
        runtime.health = "auth_required".into();
    }
    let mut logins = Vec::new();
    for id in ["official-a", "official-b"] {
        let started = official::handle(&state, "official.login.start", json!({"source_id":id}))
            .await
            .unwrap();
        let authorization = url::Url::parse(started["auth_url"].as_str().unwrap()).unwrap();
        let nonce = authorization
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        logins.push((
            started["flow_id"].as_str().unwrap().to_owned(),
            format!("http://localhost:1455/auth/callback?code=fixture-code&state={nonce}"),
        ));
    }
    let (first, second) = tokio::join!(
        official::fixture::complete_at(
            &state,
            "official-a",
            &logins[0].0,
            &logins[0].1,
            "http://fixture.invalid"
        ),
        official::fixture::complete_at(
            &state,
            "official-b",
            &logins[1].0,
            &logins[1].1,
            "http://fixture.invalid"
        ),
    );
    assert_ne!(first.is_ok(), second.is_ok());
    let sources = state.sources.read().await;
    let winner = sources
        .iter()
        .find(|runtime| runtime.source.account_id.is_some())
        .unwrap();
    let rejected = if first.is_err() {
        first.unwrap_err()
    } else {
        second.unwrap_err()
    }
    .to_string();
    assert!(rejected.contains(&winner.source.name));
    assert!(!rejected.contains("account-a"));
    assert_eq!(
        sources
            .iter()
            .filter(|runtime| runtime.source.account_id.is_some())
            .count(),
        1
    );
    let unchanged = sources
        .iter()
        .find(|runtime| runtime.source.account_id.is_none())
        .unwrap();
    assert_eq!(unchanged.source.secret, json!({}));
    assert_eq!(unchanged.health, "auth_required");
    assert_eq!(store.saved.lock().unwrap().len(), 1);
    assert_eq!(counters.tokens.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn replaced_login_cannot_install_after_its_token_exchange_started() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, original) = setup(&proxy).await;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(CredentialFixture::default());
    let state = AppState::new_with_credential_store(
        directory.path().to_path_buf(),
        0,
        "fixture".into(),
        Some(store.clone()),
    )
    .unwrap();
    *store.owner.lock().unwrap() = Arc::downgrade(&state);
    *state.sources.write().await = original.sources.read().await.clone();
    let previous_secret = state.sources.read().await[0].source.secret.clone();
    let callback = |started: &Value| {
        let authorization = url::Url::parse(started["auth_url"].as_str().unwrap()).unwrap();
        let nonce = authorization
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        format!("http://localhost:1455/auth/callback?code=fixture-code&state={nonce}")
    };
    let started = official::handle(
        &state,
        "official.login.start",
        json!({"source_id":"official-a"}),
    )
    .await
    .unwrap();
    let old_flow = started["flow_id"].as_str().unwrap().to_owned();
    let old_callback = callback(&started);
    let gate = Arc::new(tokio::sync::Notify::new());
    *counters.token_gate.lock().await = Some(gate.clone());
    let copy = state.clone();
    let pending = tokio::spawn(async move {
        official::fixture::complete_at(
            &copy,
            "official-a",
            &old_flow,
            &old_callback,
            "http://fixture.invalid",
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while counters.tokens.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let replacement = official::handle(
        &state,
        "official.login.start",
        json!({"source_id":"official-a"}),
    )
    .await
    .unwrap();
    *counters.token_gate.lock().await = None;
    gate.notify_one();
    assert!(pending.await.unwrap().is_err());
    assert!(store.saved.lock().unwrap().is_empty());
    assert_eq!(state.sources.read().await[0].source.secret, previous_secret);
    let new_flow = replacement["flow_id"].as_str().unwrap();
    assert_eq!(
        official::handle(
            &state,
            "official.login.status",
            json!({"source_id":"official-a","flow_id":new_flow})
        )
        .await
        .unwrap()["state"],
        "pending"
    );
    let connected = official::fixture::complete_at(
        &state,
        "official-a",
        new_flow,
        &callback(&replacement),
        "http://fixture.invalid",
    )
    .await
    .unwrap();
    assert_eq!(connected["connected"], true);
    assert_eq!(store.saved.lock().unwrap().len(), 1);
    assert_eq!(counters.tokens.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn cancelled_login_cannot_persist_its_inflight_token_exchange() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, original) = setup(&proxy).await;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(CredentialFixture::default());
    let state = AppState::new_with_credential_store(
        directory.path().to_path_buf(),
        0,
        "fixture".into(),
        Some(store.clone()),
    )
    .unwrap();
    *store.owner.lock().unwrap() = Arc::downgrade(&state);
    *state.sources.write().await = original.sources.read().await.clone();
    let previous_secret = state.sources.read().await[0].source.secret.clone();
    let started = official::handle(
        &state,
        "official.login.start",
        json!({"source_id":"official-a"}),
    )
    .await
    .unwrap();
    let flow = started["flow_id"].as_str().unwrap().to_owned();
    let payload = json!({"source_id":"official-a","flow_id":flow});
    let authorization = url::Url::parse(started["auth_url"].as_str().unwrap()).unwrap();
    let nonce = authorization
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    let callback = format!("http://localhost:1455/auth/callback?code=fixture-code&state={nonce}");
    let gate = Arc::new(tokio::sync::Notify::new());
    *counters.token_gate.lock().await = Some(gate.clone());
    let copy = state.clone();
    let pending = tokio::spawn(async move {
        official::fixture::complete_at(
            &copy,
            "official-a",
            &flow,
            &callback,
            "http://fixture.invalid",
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while counters.tokens.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(official::handle(
        &state,
        "official.login.cancel",
        json!({"source_id":"official-b","flow_id":payload["flow_id"]})
    )
    .await
    .is_err());
    assert_eq!(
        official::handle(&state, "official.login.cancel", payload.clone())
            .await
            .unwrap()["cancelled"],
        true
    );
    assert_eq!(
        official::handle(&state, "official.login.cancel", payload.clone())
            .await
            .unwrap()["cancelled"],
        false
    );
    gate.notify_one();
    assert!(pending.await.unwrap().is_err());
    assert!(store.saved.lock().unwrap().is_empty());
    assert_eq!(state.sources.read().await[0].source.secret, previous_secret);
    assert!(official::handle(&state, "official.login.status", payload)
        .await
        .is_err());
    assert_eq!(counters.tokens.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn confirmed_reset_is_single_submission_and_clears_only_exhaustion() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    state.sources.write().await[0].health = "exhausted".into();
    let payload = json!({"source_id":"official-a","expected_account_id":"account-a","credit_id":"card-a","operation_id":"stable-op","confirmed":true});
    let (first, second) = tokio::join!(
        official::fixture::consume_at(&state, &payload, "http://fixture.invalid"),
        official::fixture::consume_at(&state, &payload, "http://fixture.invalid")
    );
    assert_eq!(first.unwrap()["code"], "reset");
    assert_eq!(second.unwrap()["code"], "reset");
    assert_eq!(counters.posts.load(Ordering::SeqCst), 1);
    assert_eq!(state.sources.read().await[0].health, "ready");
    assert_eq!(*counters.headers.lock().await, vec!["account-a"]);
    let wrong = json!({"source_id":"official-b","expected_account_id":"account-b","credit_id":"card-a","operation_id":"stable-op","confirmed":true});
    assert!(
        official::fixture::consume_at(&state, &wrong, "http://fixture.invalid")
            .await
            .is_err()
    );
    let unconfirmed = json!({"source_id":"official-a","expected_account_id":"account-a","credit_id":"card-a","operation_id":"no-confirm","automatic":true});
    assert!(
        official::fixture::consume_at(&state, &unconfirmed, "http://fixture.invalid")
            .await
            .is_err()
    );
    state.sources.write().await[0].health = "proxy_error".into();
    let again = json!({"source_id":"official-a","expected_account_id":"account-a","credit_id":"card-a","operation_id":"other-card-op","confirmed":true});
    official::fixture::consume_at(&state, &again, "http://fixture.invalid")
        .await
        .unwrap();
    assert_eq!(state.sources.read().await[0].health, "proxy_error");
    task.abort();
}

#[tokio::test]
async fn timed_out_reset_remains_pending_across_restart_and_new_operation_ids() {
    let (proxy, counters, task) = fixture(true).await;
    let (dir, state) = setup(&proxy).await;
    let payload = json!({"source_id":"official-a","expected_account_id":"account-a","credit_id":"card-a","operation_id":"uncertain-op","confirmed":true});
    let first = official::fixture::consume_at(&state, &payload, "http://fixture.invalid")
        .await
        .unwrap();
    assert_eq!(first["code"], "pending_verification");
    let second = official::fixture::consume_at(&state, &payload, "http://fixture.invalid")
        .await
        .unwrap();
    assert_eq!(second["code"], "pending_verification");
    let restored = AppState::new(dir.path().to_path_buf(), 0, "fixture".into()).unwrap();
    *restored.sources.write().await = state.sources.read().await.clone();
    let third = official::fixture::consume_at(&restored, &payload, "http://fixture.invalid")
        .await
        .unwrap();
    assert_eq!(third["code"], "pending_verification");
    let mut new_payload = payload.clone();
    new_payload["operation_id"] = json!("new-op");
    assert!(
        official::fixture::consume_at(&restored, &new_payload, "http://fixture.invalid")
            .await
            .is_err()
    );
    assert_eq!(counters.posts.load(Ordering::SeqCst), 1);
    assert!(counters.reads.load(Ordering::SeqCst) >= 4);
    let operation_dir = dir.path().join("official-operations");
    let bytes = std::fs::read(
        std::fs::read_dir(operation_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path(),
    )
    .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("pending_verification"));
    assert!(!text.contains("fixture-access"));
    assert!(!text.contains("fixture-refresh"));
    task.abort();
}

#[tokio::test]
async fn uncertain_reset_is_scoped_to_its_account_after_source_relogin() {
    let (proxy, counters, task) = fixture(true).await;
    let (_dir, state) = setup(&proxy).await;
    let mut payload = json!({"source_id":"official-a","expected_account_id":"account-a","credit_id":"card-a","operation_id":"prior-account-op","confirmed":true});
    assert_eq!(
        official::fixture::consume_at(&state, &payload, "http://fixture.invalid")
            .await
            .unwrap()["code"],
        "pending_verification"
    );
    // A second source for the same account remains blocked by the durable intent.
    let mut duplicate = state.sources.read().await[0].clone();
    duplicate.source.id = "duplicate-a".into();
    state.sources.write().await.push(duplicate);
    payload["source_id"] = json!("duplicate-a");
    payload["operation_id"] = json!("duplicate-account-op");
    assert!(
        official::fixture::consume_at(&state, &payload, "http://fixture.invalid")
            .await
            .is_err()
    );
    assert_eq!(counters.posts.load(Ordering::SeqCst), 1);

    // Card IDs can overlap across accounts; the old account's pending operation
    // must not prevent the newly confirmed account from using its own card.
    let mut switched = state.sources.read().await[1].source.secret.clone();
    switched["account_id"] = json!("account-c");
    official::fixture::install_secret(&state, "official-a", switched)
        .await
        .unwrap();
    payload["source_id"] = json!("official-a");
    payload["expected_account_id"] = json!("account-c");
    payload["operation_id"] = json!("new-account-op");
    assert_eq!(
        official::fixture::consume_at(&state, &payload, "http://fixture.invalid")
            .await
            .unwrap()["code"],
        "pending_verification"
    );
    assert_eq!(counters.posts.load(Ordering::SeqCst), 2);
    assert_eq!(
        *counters.headers.lock().await,
        vec!["account-a", "account-c"]
    );
    task.abort();
}

#[tokio::test]
async fn new_account_does_not_inherit_previous_account_cooldown_or_inventory() {
    let (proxy, _counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    {
        let mut sources = state.sources.write().await;
        sources[0].health = "cooldown".into();
        sources[0].cooldown_until = Some(model::now() + 1000);
        sources[0].last_error = Some("来源限流".into());
        sources[0].quota_exhausted = true;
        sources[0].quota = json!({"windows":[{"used_percent":100}]});
        sources[0].credits = json!({"credits":[{"id":"old-account-card"}]});
    }
    let mut switched = state.sources.read().await[1].source.secret.clone();
    switched["account_id"] = json!("account-c");
    official::fixture::install_secret(&state, "official-a", switched)
        .await
        .unwrap();
    let sources = state.sources.read().await;
    assert_eq!(sources[0].health, "ready");
    assert!(sources[0].cooldown_until.is_none());
    assert!(sources[0].last_error.is_none());
    assert!(!sources[0].quota_exhausted);
    assert!(sources[0].quota.is_null());
    assert!(sources[0].credits.is_null());
    task.abort();
}

#[tokio::test]
async fn fresh_quota_recovers_natural_reset_but_preserves_unknown_and_other_restrictions() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    {
        let mut sources = state.sources.write().await;
        sources[0].health = "exhausted".into();
        sources[0].cooldown_until = None;
        sources[0].source.enabled = false;
    }
    official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid")
        .await
        .unwrap();
    {
        let sources = state.sources.read().await;
        assert_eq!(sources[0].health, "ready");
        assert!(!sources[0].source.enabled);
    }
    for health in ["proxy_error", "auth_required", "cooldown"] {
        state.sources.write().await[0].health = health.into();
        official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid")
            .await
            .unwrap();
        assert_eq!(state.sources.read().await[0].health, health);
    }
    let unknowns = [
        json!({"rate_limit":{"primary_window":{"used_percent":null}},"additional_rate_limits":[{"limit_name":"unknown","rate_limit":{"primary_window":{"used_percent":null}}}]}),
        json!({"rate_limit":{"primary_window":{"used_percent":10}},"additional_rate_limits":[{"rate_limit":{}}]}),
        json!({"rate_limit":{"primary_window":{"used_percent":10}},"additional_rate_limits":"changed-schema"}),
        json!({"rate_limit":{}}),
    ];
    for quota in unknowns {
        state.sources.write().await[0].health = "exhausted".into();
        *counters.quota.lock().await = Some(quota);
        official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid")
            .await
            .unwrap();
        assert_eq!(state.sources.read().await[0].health, "exhausted");
    }
    // Passing the advertised reset timestamp is never recovery evidence.
    {
        let mut sources = state.sources.write().await;
        sources[0].cooldown_until = Some(model::now() - 1);
        sources[0].quota = json!({"stale":true,"windows":[{"name":"primary","used_percent":100,"reset_at":model::now()-1}]});
    }
    assert!(routing::candidates(&state, "m", None)
        .await
        .iter()
        .all(|source| source.id != "official-a"));
    // A failed quota query also retains the known zero after that timestamp.
    official::fixture::snapshots_at(&state, "official-a", "http://127.0.0.1:0")
        .await
        .unwrap();
    assert!(routing::official_exhausted(
        &state.sources.read().await[0],
        &state.sources.read().await
    ));
    let reset = model::now() + 300;
    *counters.quota.lock().await = Some(
        json!({"rate_limit":{"primary_window":{"used_percent":100,"reset_at":reset},"secondary_window":{"used_percent":100,"reset_at":reset+500}}}),
    );
    elapse(60).await;
    official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid")
        .await
        .unwrap();
    {
        let sources = state.sources.read().await;
        assert_eq!(sources[0].health, "exhausted");
        assert_eq!(sources[0].cooldown_until, Some(reset + 500));
    }
    *counters.quota.lock().await = Some(
        json!({"rate_limit":{"primary_window":{"used_percent":15},"secondary_window":{"used_percent":20}}}),
    );
    official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid")
        .await
        .unwrap();
    assert_eq!(state.sources.read().await[0].health, "ready");
    // Unknown quotas never introduce exhaustion for a source which was usable.
    *counters.quota.lock().await =
        Some(json!({"rate_limit":{"primary_window":{"used_percent":null}}}));
    official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid")
        .await
        .unwrap();
    assert_eq!(state.sources.read().await[0].health, "ready");
    task.abort();
}

#[tokio::test]
async fn new_exhaustion_outlives_old_failures_and_older_quota_queries() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    let duplicate = state.sources.read().await[0].source.clone();
    let mut duplicate = SourceRuntime::new(Source {
        id: "duplicate".into(),
        ..duplicate
    });
    duplicate.health = "exhausted".into();
    state.sources.write().await.push(duplicate);
    let original = state.sources.read().await[0].source.clone();
    routing::mark_rejection(&state, &original, Some("usage_limit_reached"), Some(1)).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    *counters.credit_gate.lock().await = Some(gate.clone());
    let revision = state.sources.read().await[0].health_revision;
    let copy = state.clone();
    let query = tokio::spawn(async move {
        official::fixture::snapshots_at(&copy, "official-a", "http://fixture.invalid")
            .await
            .unwrap()
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        counters.credit_started.notified(),
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while state.sources.read().await[0].health_revision == revision {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Quota was already read as positive; a newer rejection must win, even if
    // old network/auth completions subsequently change the health label.
    let duplicate = state
        .sources
        .read()
        .await
        .iter()
        .find(|r| r.source.id == "duplicate")
        .unwrap()
        .source
        .clone();
    routing::mark_rejection(&state, &duplicate, Some("usage_limit_reached"), Some(1)).await;
    routing::mark_failure(&state, "official-a", 500, None).await;
    let source = state.sources.read().await[0].source.clone();
    routing::mark_auth_preflight_failure(&state, &source).await;
    routing::mark_ready(&state, &source).await;
    gate.notify_one();
    query.await.unwrap();
    {
        let sources = state.sources.read().await;
        assert!(routing::official_exhausted(&sources[0], &sources));
        assert!(routing::official_exhausted(&sources[2], &sources));
    }
    // Even when already latched, a later zero query invalidates an older
    // positive query that is still waiting for its credit lookup.
    let revision = state.sources.read().await[0].health_revision;
    let copy = state.clone();
    let query = tokio::spawn(async move {
        official::fixture::snapshots_at(&copy, "official-a", "http://fixture.invalid")
            .await
            .unwrap()
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        counters.credit_started.notified(),
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while state.sources.read().await[0].health_revision == revision {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    *counters.quota.lock().await = Some(
        json!({"rate_limit":{"primary_window":{"used_percent":20},"secondary_window":{"used_percent":100}}}),
    );
    official::fixture::selected_snapshots_at(&state, "official-a", "http://fixture.invalid", false)
        .await
        .unwrap();
    gate.notify_one();
    query.await.unwrap();
    assert!(routing::official_exhausted(
        &state.sources.read().await[0],
        &state.sources.read().await
    ));
    *counters.credit_gate.lock().await = None;
    *counters.quota.lock().await =
        Some(json!({"rate_limit":{"primary_window":{"used_percent":20}}}));
    official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid")
        .await
        .unwrap();
    assert_eq!(
        routing::account_remaining(&state.sources.read().await[0], &state.sources.read().await),
        Some(0.0)
    );
    assert!(routing::official_exhausted(
        &state.sources.read().await[0],
        &state.sources.read().await
    ));
    *counters.quota.lock().await = Some(
        json!({"rate_limit":{"primary_window":{"used_percent":20},"secondary_window":{"used_percent":30}},"additional_rate_limits":[{"limit_name":"other-model","rate_limit":{"primary_window":{"used_percent":100}}}]}),
    );
    official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid")
        .await
        .unwrap();
    {
        let sources = state.sources.read().await;
        assert!(!routing::official_exhausted(&sources[0], &sources));
        assert!(!routing::official_exhausted(&sources[2], &sources));
        assert_eq!(sources[2].health, "ready");
    }
    task.abort();
}

#[derive(Default)]
struct CredentialFixture {
    owner: std::sync::Mutex<std::sync::Weak<AppState>>,
    saved: std::sync::Mutex<Vec<(String, Value)>>,
    fail: std::sync::atomic::AtomicBool,
}
impl credentials::CredentialStore for CredentialFixture {
    fn save(&self, id: &str, secret: &Value) -> anyhow::Result<()> {
        // install must hold the runtime write lock while durability is established.
        let state = self.owner.lock().unwrap().upgrade().unwrap();
        assert!(state.sources.try_read().is_err());
        if self.fail.load(Ordering::SeqCst) {
            anyhow::bail!("fixture-store-error-with-sensitive-detail");
        }
        self.saved.lock().unwrap().push((id.into(), secret.clone()));
        Ok(())
    }
}
#[tokio::test]
async fn token_persistence_precedes_publication_and_failure_blocks_further_use() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, original) = setup(&proxy).await;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(CredentialFixture::default());
    let state = AppState::new_with_credential_store(
        directory.path().to_path_buf(),
        0,
        "fixture".into(),
        Some(store.clone()),
    )
    .unwrap();
    *store.owner.lock().unwrap() = Arc::downgrade(&state);
    *state.sources.write().await = original.sources.read().await.clone();
    state.sources.write().await[0].source.secret["expires_at"] = json!(1);
    state
        .bindings
        .lock()
        .await
        .insert("prior-response".into(), "official-a".into());
    let refreshed = official::fixture::refresh_at(&state, "official-a", "http://fixture.invalid")
        .await
        .unwrap();
    assert_eq!(store.saved.lock().unwrap()[0].1, refreshed.secret);
    assert_eq!(
        state
            .bindings
            .lock()
            .await
            .get("prior-response")
            .map(String::as_str),
        Some("official-a")
    );
    let old = refreshed.secret.clone();
    state.sources.write().await[0].source.secret["expires_at"] = json!(1);
    store.fail.store(true, Ordering::SeqCst);
    let error = official::fixture::refresh_at(&state, "official-a", "http://fixture.invalid")
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("保存失败"));
    assert!(!error.contains("sensitive-detail"));
    {
        let sources = state.sources.read().await;
        assert_eq!(sources[0].source.secret["generation"], old["generation"]);
        assert_eq!(sources[0].health, "auth_required");
    }
    let token_requests = counters.tokens.load(Ordering::SeqCst);
    assert!(
        official::fixture::refresh_at(&state, "official-a", "http://fixture.invalid")
            .await
            .is_err()
    );
    assert_eq!(counters.tokens.load(Ordering::SeqCst), token_requests);
    store.fail.store(false, Ordering::SeqCst);
    {
        let mut sources = state.sources.write().await;
        sources[0].quota = json!({"windows":[{"used_percent":100}]});
        sources[0].credits = json!({"credits":[{"id":"previous-account-card"}]});
        sources[0].health = "exhausted".into();
        sources[0].cooldown_until = Some(model::now() + 1000);
    }
    let mut switched = old;
    switched["account_id"] = json!("account-new");
    official::fixture::install_secret(&state, "official-a", switched)
        .await
        .unwrap();
    assert!(!state.bindings.lock().await.contains_key("prior-response"));
    {
        let sources = state.sources.read().await;
        assert!(sources[0].quota.is_null());
        assert!(sources[0].credits.is_null());
        assert_eq!(sources[0].health, "ready");
        assert_eq!(sources[0].cooldown_until, None);
    }
    task.abort();
}

#[tokio::test]
async fn consume_requires_the_explicitly_confirmed_account_before_network_dispatch() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    let mut payload = json!({"source_id":"official-a","credit_id":"card-a","operation_id":"account-confirm","confirmed":true});
    assert!(
        official::fixture::consume_at(&state, &payload, "http://fixture.invalid")
            .await
            .is_err()
    );
    payload["expected_account_id"] = json!("account-b");
    assert!(
        official::fixture::consume_at(&state, &payload, "http://fixture.invalid")
            .await
            .is_err()
    );
    assert_eq!(counters.posts.load(Ordering::SeqCst), 0);
    assert_eq!(counters.reads.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
async fn missing_low_quota_windows_never_reopen_admission_without_known_recovery() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    *counters.quota.lock().await = Some(
        json!({"rate_limit":{"primary_window":{"used_percent":20},"secondary_window":{"used_percent":100,"limit_window_seconds":604800}}}),
    );
    official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid")
        .await
        .unwrap();
    // Make the old observation deterministic without waiting for a clock tick.
    {
        let mut sources = state.sources.write().await;
        sources[0].quota["fetched_at"] = json!(100);
        for window in sources[0].quota["windows"].as_array_mut().unwrap() {
            window["observed_at"] = json!(100);
        }
    }
    for unknown in [
        json!({}),
        json!({"rate_limit":{"primary_window":{"used_percent":20},"secondary_window":{"used_percent":null}}}),
    ] {
        *counters.quota.lock().await = Some(unknown);
        let result =
            official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid")
                .await
                .unwrap();
        assert_eq!(result["quota"]["stale"], true);
        let retained = result["quota"]["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|window| window["name"] == "secondary")
            .unwrap();
        assert_eq!(retained["observed_at"], 100);
        assert_eq!(
            routing::remaining(&state.sources.read().await[0]),
            Some(0.0)
        );
    }
    {
        let mut sources = state.sources.write().await;
        let mut recovered = sources[0].clone();
        recovered.source.id = "same-account-recovered".into();
        recovered.quota = json!({"fetched_at":101,"windows":[{"name":"secondary","used_percent":50,"observed_at":101}]});
        sources.push(recovered);
        assert_eq!(
            routing::account_remaining(&sources[0], &sources),
            Some(50.0)
        );
    }
    *counters.quota.lock().await = Some(json!({}));
    official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid")
        .await
        .unwrap();
    {
        let sources = state.sources.read().await;
        assert_eq!(
            routing::account_remaining(&sources[0], &sources),
            Some(50.0)
        );
    }
    *counters.quota.lock().await = Some(
        json!({"rate_limit":{"primary_window":{"used_percent":20},"secondary_window":{"used_percent":30}}}),
    );
    {
        let mut sources = state.sources.write().await;
        for runtime in sources
            .iter_mut()
            .filter(|r| r.source.account_id.as_deref() == Some("account-a"))
        {
            runtime.health = "exhausted".into();
            runtime.last_error = Some("来源额度已耗尽".into());
        }
        sources[1].health = "auth_required".into();
    }
    let result = official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid")
        .await
        .unwrap();
    assert_eq!(result["quota"]["stale"], false);
    assert_eq!(
        routing::remaining(&state.sources.read().await[0]),
        Some(70.0)
    );
    {
        let sources = state.sources.read().await;
        for runtime in sources
            .iter()
            .filter(|r| r.source.account_id.as_deref() == Some("account-a"))
        {
            assert_eq!(runtime.health, "ready");
            assert!(!routing::official_exhausted(runtime, &sources));
        }
        assert_eq!(sources[1].health, "auth_required");
    }
    task.abort();
}

#[tokio::test]
async fn quota_and_credit_failures_retry_immediately_and_recover_independently() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    let first = official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid").await.unwrap();
    *counters.quota.lock().await = Some(json!([]));
    counters.invalid_credits.store(true, Ordering::SeqCst);
    for index in 0..6 {
        let reads = (counters.quota_reads.load(Ordering::SeqCst), counters.reads.load(Ordering::SeqCst));
        let failed = official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid").await.unwrap();
        for kind in ["quota", "credits"] {
            assert_eq!(failed[kind]["failure_count"], index + 1);
            assert_eq!(failed[kind]["stale"], true);
            assert_eq!(failed[kind]["fetched_at"], first[kind]["fetched_at"]);
            assert!(failed[kind]["next_retry_at"].is_null());
        }
        assert_eq!(failed["quota"]["windows"], first["quota"]["windows"]);
        assert_eq!(failed["credits"]["available_count"], 1);
        assert_eq!((counters.quota_reads.load(Ordering::SeqCst), counters.reads.load(Ordering::SeqCst)), (reads.0 + 1, reads.1 + 1));
    }
    *counters.quota.lock().await = None;
    let recovered = official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid").await.unwrap();
    assert_eq!(recovered["quota"]["failure_count"], 0);
    assert!(recovered["quota"]["next_retry_at"].is_null());
    assert_eq!(recovered["quota"]["stale"], false);
    assert_eq!(recovered["credits"]["stale"], true);
    assert_eq!(recovered["credits"]["failure_count"], 7);
    assert!(recovered["credits"]["next_retry_at"].is_null());
    counters.invalid_credits.store(false, Ordering::SeqCst);
    let recovered = official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid").await.unwrap();
    assert_eq!(recovered["credits"]["failure_count"], 0);
    assert!(recovered["credits"]["next_retry_at"].is_null());
    assert_eq!(recovered["credits"]["stale"], false);
    task.abort();
}

#[tokio::test]
async fn failed_token_refresh_is_shared_before_quota_and_credit_requests() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    state.sources.write().await[0].source.secret["expires_at"] = json!(1);
    *counters.token_response.lock().await = Some(json!({}));
    let failed = official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid").await.unwrap();
    assert_eq!(failed["quota"]["failure_count"], 1);
    assert_eq!(failed["credits"]["failure_count"], 1);
    assert_eq!(counters.tokens.load(Ordering::SeqCst), 1);
    assert_eq!(counters.quota_reads.load(Ordering::SeqCst), 0);
    assert_eq!(counters.reads.load(Ordering::SeqCst), 0);
    let retried = official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid").await.unwrap();
    for kind in ["quota", "credits"] {
        assert_eq!(retried[kind]["failure_count"], 2);
        assert!(retried[kind]["next_retry_at"].is_null());
        assert_eq!(retried[kind]["stale"], true);
    }
    assert_eq!(counters.quota_reads.load(Ordering::SeqCst), 0);
    assert_eq!(counters.reads.load(Ordering::SeqCst), 0);
    assert_eq!(counters.tokens.load(Ordering::SeqCst), 1);
    elapse(60).await;
    *counters.token_response.lock().await = None;
    let recovered = official::fixture::snapshots_at(&state, "official-a", "http://fixture.invalid").await.unwrap();
    assert_eq!(recovered["quota"]["stale"], false);
    assert_eq!(recovered["credits"]["stale"], false);
    assert_eq!(counters.tokens.load(Ordering::SeqCst), 2);
    task.abort();
}

fn weekly_usage(used: u64) -> Value {
    json!({"rate_limit":{
        "primary_window":{"used_percent":10,"limit_window_seconds":18000,"reset_at":2000000000},
        "secondary_window":{"used_percent":used,"limit_window_seconds":604800,"reset_at":2000500000}
    }})
}

fn auto_inventory() -> Value {
    json!({"credits":[
        {"id":"no-expiry","status":"available","expires_at":null},
        {"id":"card-b","status":"available","expires_at":"2099-01-01T00:00:00Z"},
        {"id":"unknown-expiry","status":"available"},
        {"id":"expired","status":"available","expires_at":1},
        {"id":"card-a","status":"available","expires_at":"2099-01-01T00:00:00Z"}
    ]})
}

async fn reject_and_wait(state: &Arc<AppState>, id: &str) {
    let ticket = state.quota_recovery.try_admit().unwrap();
    let source = state.sources.read().await.iter().find(|r| r.source.id == id).unwrap().source.clone();
    official::fixture::quota_rejected(state, &source, ticket).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), state.quota_recovery.wait_idle()).await.unwrap();
}

#[tokio::test]
async fn weekly_auto_reset_requires_opt_in_and_a_complete_current_main_week() {
    let (proxy, counters, task) = fixture(false).await;
    let mut short = weekly_usage(20);
    short["rate_limit"]["primary_window"]["used_percent"] = json!(100);
    let mut review = weekly_usage(20);
    review["additional_rate_limits"] = json!([{"limit_name":"review","rate_limit":{
        "primary_window":{"used_percent":100,"limit_window_seconds":604800,"reset_at":2000500000}
    }}]);
    let mut unknown = weekly_usage(100);
    unknown["rate_limit"]["secondary_window"]["used_percent"] = Value::Null;
    let mut expired = weekly_usage(100);
    expired["rate_limit"]["secondary_window"]["reset_at"] = json!(1);
    let mut incomplete = weekly_usage(100);
    incomplete["additional_rate_limits"] = json!([{}]);
    for (enrolled, quota) in [
        ("official-a", weekly_usage(100)), ("account-a", short), ("account-a", review),
        ("account-a", unknown), ("account-a", expired), ("account-a", incomplete), ("account-a", json!({})),
    ] {
        let (_dir, state) = setup(&proxy).await;
        state.settings.write().await.auto_reset_weekly_accounts = vec![enrolled.into()];
        state.sources.write().await[0].quota = official::fixture::parse_quota(&weekly_usage(100)).unwrap();
        *counters.quota.lock().await = Some(quota);
        reject_and_wait(&state, "official-a").await;
    }
    assert_eq!(counters.posts.load(Ordering::SeqCst), 0);
    assert_eq!(counters.reads.load(Ordering::SeqCst), 0);
    assert_eq!(counters.quota_reads.load(Ordering::SeqCst), 7);
    task.abort();
}

#[tokio::test]
async fn weekly_auto_reset_is_account_durable_and_rearms_after_observed_recovery() {
    let (proxy, counters, task) = fixture(false).await;
    let (dir, state) = setup(&proxy).await;
    counters.reflect_redemption.store(true, Ordering::SeqCst);
    *counters.inventory.lock().await = Some(auto_inventory());
    *counters.quota.lock().await = Some(weekly_usage(100));
    official::fixture::selected_snapshots_at(&state, "official-a", "http://fixture.invalid", false)
        .await.unwrap();
    assert_eq!(counters.posts.load(Ordering::SeqCst), 0);
    state.settings.write().await.auto_reset_weekly_accounts = vec!["account-a".into()];
    let mut alias = state.sources.read().await[0].clone();
    alias.source.id = "alias-a".into();
    state.sources.write().await.push(alias);
    let (a, b) = tokio::join!(
        official::fixture::selected_snapshots_at(&state, "official-a", "http://fixture.invalid", false),
        official::fixture::selected_snapshots_at(&state, "alias-a", "http://fixture.invalid", false)
    );
    a.unwrap();
    b.unwrap();
    // Observing exhaustion, including on an enrolled account, cannot use a card.
    assert_eq!(counters.posts.load(Ordering::SeqCst), 0);
    reject_and_wait(&state, "official-a").await;
    assert_eq!(*counters.consumed.lock().await, vec!["card-a"]);
    let restored = AppState::new(dir.path().to_path_buf(), 0, "fixture".into()).unwrap();
    *restored.sources.write().await = state.sources.read().await.clone();
    restored.settings.write().await.auto_reset_weekly_accounts = vec!["account-a".into()];
    reject_and_wait(&restored, "alias-a").await;
    assert_eq!(counters.posts.load(Ordering::SeqCst), 1);
    *counters.quota.lock().await = Some(weekly_usage(20));
    official::fixture::selected_snapshots_at(&restored, "alias-a", "http://fixture.invalid", false)
        .await.unwrap();
    assert!(!restored.sources.read().await[0].quota_exhausted);
    // reset_at deliberately stays unchanged: a recovered account may exhaust again.
    *counters.quota.lock().await = Some(weekly_usage(100));
    reject_and_wait(&restored, "official-a").await;
    assert_eq!(*counters.consumed.lock().await, vec!["card-a", "card-b"]);
    task.abort();
}

#[tokio::test]
async fn weekly_auto_reset_unknown_post_waits_for_recovery_and_never_replays_its_card() {
    for manual in [false, true] {
        let (proxy, counters, task) = fixture(true).await;
        let (dir, state) = setup(&proxy).await;
        state.settings.write().await.auto_reset_weekly_accounts = vec!["account-a".into()];
        *counters.inventory.lock().await = Some(auto_inventory());
        *counters.quota.lock().await = Some(weekly_usage(100));
        if manual {
            let payload = json!({"source_id":"official-a","expected_account_id":"account-a",
                "credit_id":"card-a","operation_id":"manual-uncertain","confirmed":true});
            official::fixture::consume_at(&state, &payload, "http://fixture.invalid").await.unwrap();
        } else {
            reject_and_wait(&state, "official-a").await;
            let source = state.sources.read().await[0].source.clone();
            assert!(state.quota_recovery.unconfirmed(&source));
        }
        let restored = AppState::new(dir.path().to_path_buf(), 0, "fixture".into()).unwrap();
        *restored.sources.write().await = state.sources.read().await.clone();
        restored.settings.write().await.auto_reset_weekly_accounts = vec!["account-a".into()];
        reject_and_wait(&restored, "official-a").await;
        assert_eq!(*counters.consumed.lock().await, vec!["card-a"], "manual={manual}");
        *counters.quota.lock().await = Some(weekly_usage(20));
        official::fixture::selected_snapshots_at(&restored, "official-a", "http://fixture.invalid", false)
            .await.unwrap();
        *counters.quota.lock().await = Some(weekly_usage(100));
        reject_and_wait(&restored, "official-a").await;
        assert_eq!(*counters.consumed.lock().await, vec!["card-a", "card-b"], "manual={manual}");
        task.abort();
    }
}

#[tokio::test]
async fn weekly_auto_reset_rechecks_authorization_after_inventory_preflight() {
    for change in ["setting", "disabled", "binding", "account", "quota"] {
        let (proxy, counters, task) = fixture(false).await;
        let (_dir, state) = setup(&proxy).await;
        state.settings.write().await.auto_reset_weekly_accounts = vec!["account-a".into()];
        *counters.quota.lock().await = Some(weekly_usage(100));
        let gate = Arc::new(tokio::sync::Notify::new());
        *counters.credit_gate.lock().await = Some(gate.clone());
        let source = state.sources.read().await[0].source.clone();
        let ticket = state.quota_recovery.try_admit().unwrap();
        official::fixture::quota_rejected(&state, &source, ticket).await;
        counters.credit_started.notified().await;
        match change {
            "setting" => state.settings.write().await.auto_reset_weekly_accounts.clear(),
            "disabled" => state.sources.write().await[0].source.enabled = false,
            "binding" => state.sources.write().await[0].source.secret["access_token"] = json!("changed"),
            "account" => {
                let mut sources = state.sources.write().await;
                sources[0].source.account_id = Some("changed".into());
                sources[0].source.secret["account_id"] = json!("changed");
            }
            "quota" => state.sources.write().await[0].quota["stale"] = json!(true),
            _ => unreachable!(),
        }
        gate.notify_one();
        state.quota_recovery.wait_idle().await;
        assert_eq!(counters.posts.load(Ordering::SeqCst), 0, "{change}");
        task.abort();
    }
}

#[tokio::test]
async fn weekly_auto_reset_empty_inventory_releases_the_gate_with_a_hard_block() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    state.settings.write().await.auto_reset_weekly_accounts = vec!["account-a".into()];
    *counters.inventory.lock().await = Some(json!({"credits":[]}));
    *counters.quota.lock().await = Some(weekly_usage(100));
    reject_and_wait(&state, "official-a").await;
    let sources = state.sources.read().await;
    assert!(routing::official_exhausted(&sources[0], &sources));
    assert!(!state.quota_recovery.unconfirmed(&sources[0].source));
    assert_eq!(counters.reads.load(Ordering::SeqCst), 1);
    assert_eq!(counters.posts.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
async fn week_only_rejection_uses_inventory_first_and_waits_for_confirmed_recovery() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    state.settings.write().await.auto_reset_weekly_accounts = vec!["account-a".into()];
    let week_only = |used| json!({"rate_limit":{"primary_window":{
        "used_percent":used,"limit_window_seconds":604800,"reset_at":2000500000
    }}});
    let original = {
        let mut sources = state.sources.write().await;
        sources[0].quota = official::fixture::parse_quota(&week_only(75)).unwrap();
        let mut alias = sources[0].clone();
        alias.source.id = "alias-a".into();
        let original = sources[0].source.clone();
        sources.push(alias);
        original
    };
    *counters.quota.lock().await = Some(week_only(100));
    let inventory_gate = Arc::new(tokio::sync::Notify::new());
    *counters.credit_gate.lock().await = Some(inventory_gate.clone());
    let ticket = state.quota_recovery.try_admit().unwrap();
    official::fixture::quota_rejected(&state, &original, ticket).await;
    tokio::time::timeout(std::time::Duration::from_secs(1), counters.credit_started.notified()).await.unwrap();
    assert!(state.quota_recovery.try_admit().is_none());
    assert_eq!(counters.quota_reads.load(Ordering::SeqCst), 0);

    let alias = state.sources.read().await[2].source.clone();
    official::fixture::quota_rejected(&state, &alias, ticket).await;
    inventory_gate.notify_one();
    tokio::time::timeout(std::time::Duration::from_millis(500), async {
        while counters.quota_reads.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }).await.unwrap();
    assert_eq!(counters.posts.load(Ordering::SeqCst), 1);
    assert!(state.quota_recovery.try_admit().is_none());
    assert!(state.quota_recovery.unconfirmed(&original));
    // A positive short window alone does not prove the observed week recovered.
    *counters.quota.lock().await = Some(json!({"rate_limit":{"primary_window":{
        "used_percent":0,"limit_window_seconds":18000,"reset_at":2000500000
    }}}));
    let reads = counters.quota_reads.load(Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_millis(300), async {
        while counters.quota_reads.load(Ordering::SeqCst) < reads + 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }).await.unwrap();
    assert!(state.quota_recovery.try_admit().is_none());
    *counters.quota.lock().await = Some(week_only(20));
    tokio::time::timeout(std::time::Duration::from_secs(1), state.quota_recovery.wait_idle()).await.unwrap();
    assert!(!state.quota_recovery.unconfirmed(&original));
    assert_ne!(state.quota_recovery.try_admit(), Some(ticket));
    official::fixture::quota_rejected(&state, &alias, ticket).await;
    assert!(state.quota_recovery.try_admit().is_some());
    let sources = state.sources.read().await;
    assert!(!routing::official_exhausted(&sources[0], &sources));
    assert_eq!(counters.reads.load(Ordering::SeqCst), 1);
    task.abort();
}
