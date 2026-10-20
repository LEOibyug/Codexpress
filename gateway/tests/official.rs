//! These tests run only against a local simulated explicit proxy and temporary state.
use codexpress_gateway::{model, transport};
#[allow(dead_code)]
#[path = "../src/official.rs"]
mod official;
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
    uncertain: bool,
    headers: Mutex<Vec<String>>,
}
async fn fixture(uncertain: bool) -> (String, Arc<Fixture>, tokio::task::JoinHandle<()>) {
    let data = Arc::new(Fixture {
        uncertain,
        ..Default::default()
    });
    async fn inventory(State(s): State<Arc<Fixture>>) -> Json<Value> {
        s.reads.fetch_add(1, Ordering::SeqCst);
        Json(
            json!({"available_count":1,"credits":[{"id":"card-a","title":"模拟卡","status":"available","expires_at":"2099-01-01T00:00:00Z","reset_type":"codex_rate_limits"}]}),
        )
    }
    async fn usage() -> Json<Value> {
        Json(
            json!({"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":10,"reset_at":2000000000,"limit_window_seconds":18000},"secondary_window":{"used_percent":20}}}),
        )
    }
    async fn consume(
        State(s): State<Arc<Fixture>>,
        headers: axum::http::HeaderMap,
        Json(payload): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(payload["credit_id"], "card-a");
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
        s.tokens.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        Json(
            json!({"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600,"id_token":"eyJhbGciOiJub25lIn0.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjb3VudC1hIn19."}),
        )
    }
    let router = Router::new()
        .route("/credits", get(inventory))
        .route("/usage", get(usage))
        .route("/consume", post(consume))
        .route("/token", post(token))
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
        state.sources.write().await.push(SourceRuntime::new(Source {id:id.into(),name:id.into(),kind:"official".into(),base_url:"https://chatgpt.com/backend-api/codex".into(),enabled:true,priority:0,proxy:ProxyPolicy {mode:"explicit".into(),url:Some(proxy.into())},models:vec![],account_id:Some(account.into()),secret:json!({"access_token":"fixture-access","refresh_token":"fixture-refresh","expires_at":model::now()+3600,"account_id":account})}));
    }
    (directory, state)
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
    task.abort();
}

#[tokio::test]
async fn concurrent_refresh_rotates_once_and_never_writes_tokens_to_disk() {
    let (proxy, counters, task) = fixture(false).await;
    let (dir, state) = setup(&proxy).await;
    state.sources.write().await[0].source.secret["expires_at"] = json!(1);
    let (a, b) = tokio::join!(
        official::fixture::refresh_at(&state, "official-a", "http://fixture.invalid"),
        official::fixture::refresh_at(&state, "official-a", "http://fixture.invalid")
    );
    assert_eq!(a.unwrap().secret["access_token"], "new-access");
    assert_eq!(b.unwrap().secret["refresh_token"], "new-refresh");
    assert_eq!(counters.tokens.load(Ordering::SeqCst), 1);
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
async fn confirmed_reset_is_single_submission_and_clears_only_exhaustion() {
    let (proxy, counters, task) = fixture(false).await;
    let (_dir, state) = setup(&proxy).await;
    state.sources.write().await[0].health = "exhausted".into();
    let payload = json!({"source_id":"official-a","credit_id":"card-a","operation_id":"stable-op","confirmed":true});
    let (first, second) = tokio::join!(
        official::fixture::consume_at(&state, &payload, "http://fixture.invalid"),
        official::fixture::consume_at(&state, &payload, "http://fixture.invalid")
    );
    assert_eq!(first.unwrap()["code"], "reset");
    assert_eq!(second.unwrap()["code"], "reset");
    assert_eq!(counters.posts.load(Ordering::SeqCst), 1);
    assert_eq!(state.sources.read().await[0].health, "ready");
    assert_eq!(*counters.headers.lock().await, vec!["account-a"]);
    let wrong = json!({"source_id":"official-b","credit_id":"card-a","operation_id":"stable-op","confirmed":true});
    assert!(
        official::fixture::consume_at(&state, &wrong, "http://fixture.invalid")
            .await
            .is_err()
    );
    let unconfirmed =
        json!({"source_id":"official-a","credit_id":"card-a","operation_id":"no-confirm"});
    assert!(
        official::fixture::consume_at(&state, &unconfirmed, "http://fixture.invalid")
            .await
            .is_err()
    );
    state.sources.write().await[0].health = "proxy_error".into();
    let again = json!({"source_id":"official-a","credit_id":"card-a","operation_id":"other-card-op","confirmed":true});
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
    let payload = json!({"source_id":"official-a","credit_id":"card-a","operation_id":"uncertain-op","confirmed":true});
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
