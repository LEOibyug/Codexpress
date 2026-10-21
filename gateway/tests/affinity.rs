use codexpress_gateway::{
    affinity, management,
    model::{AppState, Source, SourceRuntime},
    routing,
};
use serde_json::json;
use std::sync::Arc;

fn source(id: &str, kind: &str) -> Source {
    serde_json::from_value(json!({"id":id,"name":id,"kind":kind,"base_url":if kind=="official" {"https://chatgpt.com/backend-api/codex"} else {"http://127.0.0.1:1/v1"},"enabled":true,"priority":if kind=="official" {0} else {1},"proxy":{"mode":if kind=="official" {"explicit"}else{"direct"},"url":"http://127.0.0.1:1"},"models":[],"account_id":if kind=="official" {Some("fixture-account")}else{None},"secret":{"api_key":"PRIVATE-FIXTURE-KEY","access_token":"PRIVATE-OFFICIAL-TOKEN"}})).unwrap()
}
async fn state() -> (tempfile::TempDir, Arc<AppState>, Source, Source) {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().to_owned(), 0, "fixture".into()).unwrap();
    let official = source("official", "official");
    let api = source("api", "api");
    *state.sources.write().await = vec![
        SourceRuntime::new(official.clone()),
        SourceRuntime::new(api.clone()),
    ];
    (dir, state, official, api)
}
async fn quota(state: &Arc<AppState>, used: f64) {
    state.sources.write().await[0].quota = json!({"windows":[{"name":"primary","used_percent":20},{"name":"secondary","used_percent":used}],"stale":false,"fetched_at":codexpress_gateway::model::now()});
}

#[tokio::test]
async fn low_quota_keeps_existing_conversation_and_reserves_only_one_new_conversation_atomically() {
    let (_dir, state, official, api) = state().await;
    quota(&state, 90.0).await;
    let first = affinity::key("thread-a", "model");
    let second = affinity::key("thread-b", "model");
    let mut lease = routing::reserve(&state, &official, Some(&first), false)
        .await
        .unwrap();
    assert!(routing::reserve(&state, &official, Some(&second), false)
        .await
        .is_none());
    assert!(routing::reserve(&state, &official, Some(&first), false)
        .await
        .is_some());
    lease.commit().unwrap();
    drop(lease);
    assert_eq!(state.affinity.active_count(&official), 1);
    assert!(routing::reserve(&state, &official, Some(&first), false)
        .await
        .is_some());
    assert!(routing::reserve(&state, &official, Some(&second), false)
        .await
        .is_none());
    assert!(routing::reserve(&state, &api, Some(&second), false)
        .await
        .is_some());
    let status = management::dispatch(&state, "status", json!({}))
        .await
        .unwrap();
    assert_eq!(
        status["sources"][0]["admission"]["routing_note"],
        "仅续接已有对话"
    );
    quota(&state, 100.0).await;
    assert!(routing::reserve(&state, &official, Some(&first), true)
        .await
        .is_none());
    let candidates = routing::candidates_with_session(&state, "model", None, Some(&first)).await;
    assert_eq!(
        candidates.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        vec!["api"]
    );
    let mut migrated = routing::reserve(&state, &api, Some(&first), false)
        .await
        .unwrap();
    migrated.commit().unwrap();
    drop(migrated);
    quota(&state, 0.0).await;
    assert_eq!(
        routing::candidates_with_session(&state, "model", None, Some(&first)).await[0].id,
        "api"
    );
    assert_eq!(
        routing::candidates_with_session(
            &state,
            "model",
            None,
            Some(&affinity::key("new-thread", "model"))
        )
        .await[0]
            .id,
        "official"
    );
}

#[tokio::test]
async fn simultaneous_new_threads_cannot_all_enter_low_quota_official_source() {
    let (_dir, state, official, _) = state().await;
    quota(&state, 95.0).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(12));
    let mut tasks = Vec::new();
    for index in 0..12 {
        let state = state.clone();
        let official = official.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            routing::reserve(
                &state,
                &official,
                Some(&affinity::key(&format!("thread-{index}"), "m")),
                false,
            )
            .await
        }));
    }
    let mut leases = Vec::new();
    for task in tasks {
        if let Some(lease) = task.await.unwrap() {
            leases.push(lease)
        }
    }
    assert_eq!(leases.len(), 1);
    drop(leases);
    assert_eq!(state.affinity.active_count(&official), 0);
}

#[tokio::test]
async fn affinity_survives_restart_is_model_scoped_and_ignores_changed_credentials() {
    let (dir, state, official, api) = state().await;
    let key = affinity::key("PRIVATE-THREAD-IDENTIFIER", "model-a");
    let mut lease = routing::reserve(&state, &api, Some(&key), false)
        .await
        .unwrap();
    lease.commit().unwrap();
    drop(lease);
    drop(state);
    let restored = AppState::new(dir.path().to_owned(), 0, "fixture".into()).unwrap();
    *restored.sources.write().await = vec![
        SourceRuntime::new(official.clone()),
        SourceRuntime::new(api.clone()),
    ];
    assert_eq!(
        routing::candidates_with_session(&restored, "model-a", None, Some(&key)).await[0].id,
        "api"
    );
    assert_eq!(
        routing::candidates_with_session(
            &restored,
            "model-b",
            None,
            Some(&affinity::key("PRIVATE-THREAD-IDENTIFIER", "model-b"))
        )
        .await[0]
            .id,
        "official"
    );
    restored.sources.write().await[1].source.secret = json!({"api_key":"rotated"});
    assert_eq!(
        routing::candidates_with_session(&restored, "model-a", None, Some(&key)).await[0].id,
        "official"
    );
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
        for secret in [
            "PRIVATE-THREAD-IDENTIFIER",
            "PRIVATE-FIXTURE-KEY",
            "PRIVATE-OFFICIAL-TOKEN",
        ] {
            assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()));
        }
    }
}

#[tokio::test]
async fn unknown_or_other_model_quota_is_not_zero_and_fixed_mode_never_falls_back() {
    let (_dir, state, official, _) = state().await;
    state.sources.write().await[0].quota = json!({"windows":[{"name":"primary","used_percent":null},{"name":"other-model:secondary","used_percent":100}]});
    assert!(routing::remaining(&state.sources.read().await[0]).is_none());
    assert!(routing::reserve(&state, &official, Some("fixture"), false)
        .await
        .is_some());
    quota(&state, 100.0).await;
    {
        let mut settings = state.settings.write().await;
        settings.mode = "fixed".into();
        settings.fixed_source_id = Some(official.id);
    }
    assert!(routing::candidates(&state, "m", None).await.is_empty());
}

#[tokio::test]
async fn idle_return_requires_recovered_quota_and_failed_attempt_keeps_confirmed_affinity() {
    let (dir, state, official, api) = state().await;
    let key = affinity::key("long-idle-thread", "m");
    let mut lease = routing::reserve(&state, &api, Some(&key), false)
        .await
        .unwrap();
    lease.commit().unwrap();
    drop(lease);
    drop(state);
    let db = rusqlite::Connection::open(dir.path().join("affinity.sqlite")).unwrap();
    db.execute(
        "UPDATE affinity SET last_seen=?1",
        [codexpress_gateway::model::now() - 86400 * 50],
    )
    .unwrap();
    drop(db);
    let restored = AppState::new(dir.path().to_owned(), 0, "fixture".into()).unwrap();
    *restored.sources.write().await = vec![
        SourceRuntime::new(official.clone()),
        SourceRuntime::new(api.clone()),
    ];
    assert_eq!(restored.affinity.active_count(&api), 0);
    assert_eq!(
        routing::candidates_with_session(&restored, "m", None, Some(&key)).await[0].id,
        "api"
    );
    // Unknown/zero quota must never reclaim an API conversation.
    assert!(routing::reserve_idle_return(&restored, "m", &key)
        .await
        .is_none());
    quota(&restored, 100.0).await;
    assert!(routing::reserve_idle_return(&restored, "m", &key)
        .await
        .is_none());
    quota(&restored, 20.0).await;
    restored.settings.write().await.mode = "fixed".into();
    assert!(routing::reserve_idle_return(&restored, "m", &key)
        .await
        .is_none());
    restored.settings.write().await.mode = "auto".into();
    // Even a confirmed small recovery cannot take quota reserved for active work.
    quota(&restored, 95.0).await;
    let active = routing::reserve(&restored, &official, Some("other-active-thread"), false)
        .await
        .unwrap();
    assert!(routing::reserve_idle_return(&restored, "m", &key)
        .await
        .is_none());
    drop(active);
    quota(&restored, 20.0).await;
    let (target, pending) = routing::reserve_idle_return(&restored, "m", &key)
        .await
        .unwrap();
    assert_eq!(target.id, official.id);
    assert!(routing::reserve(&restored, &api, Some(&key), false)
        .await
        .is_none());
    drop(pending);
    assert_eq!(
        routing::candidates_with_session(&restored, "m", None, Some(&key)).await[0].id,
        "api"
    );
    let (_, mut lease) = routing::reserve_idle_return(&restored, "m", &key)
        .await
        .unwrap();
    lease.commit().unwrap();
    drop(lease);
    assert_eq!(
        restored
            .affinity
            .preferred(&key, &[official, api])
            .as_deref(),
        Some("official")
    );
}

#[tokio::test]
async fn idle_return_uses_current_priority_for_both_official_and_api_conversations() {
    for from_official in [true, false] {
        let (dir, state, mut official, mut api) = state().await;
        official.priority = 1;
        api.priority = 0;
        let mut low = api.clone();
        low.id = "low-api".into();
        low.priority = 2;
        low.secret = json!({"api_key":"low-fixture"});
        let from = if from_official {
            official.clone()
        } else {
            low.clone()
        };
        *state.sources.write().await = vec![
            SourceRuntime::new(official.clone()),
            SourceRuntime::new(api.clone()),
            SourceRuntime::new(low.clone()),
        ];
        let key = affinity::key("idle-priority-only", "m");
        let mut lease = routing::reserve(&state, &from, Some(&key), false)
            .await
            .unwrap();
        lease.commit().unwrap();
        drop(lease);
        drop(state);
        let db = rusqlite::Connection::open(dir.path().join("affinity.sqlite")).unwrap();
        db.execute(
            "UPDATE affinity SET last_seen=?1",
            [codexpress_gateway::model::now() - 86400],
        )
        .unwrap();
        drop(db);
        let restored = AppState::new(dir.path().to_owned(), 0, "fixture".into()).unwrap();
        *restored.sources.write().await = vec![
            SourceRuntime::new(official.clone()),
            SourceRuntime::new(api.clone()),
            SourceRuntime::new(low),
        ];
        quota(&restored, 20.0).await;
        assert_eq!(
            routing::candidates(&restored, "m", None).await[0].id,
            api.id
        );
        let (target, lease) = routing::reserve_idle_return(&restored, "m", &key)
            .await
            .unwrap();
        assert_eq!(target.id, api.id);
        assert_eq!(target.kind, "api");
        drop(lease);
        restored.sources.write().await[1].source.enabled = false;
        assert!(routing::candidates(&restored, "m", None)
            .await
            .iter()
            .all(|s| s.id != api.id));
        let next = routing::reserve_idle_return(&restored, "m", &key).await;
        if from_official {
            assert!(next.is_none());
        } else {
            assert_eq!(next.as_ref().unwrap().0.id, official.id);
        }
        drop(next);
        restored.sources.write().await[1].source.enabled = true;
        restored.sources.write().await[1].source.priority = 3;
        let next = routing::reserve_idle_return(&restored, "m", &key).await;
        if from_official {
            assert!(next.is_none());
        } else {
            assert_eq!(next.as_ref().unwrap().0.id, official.id);
        }
        drop(next);
        restored.sources.write().await[1].source.priority = 0;
        restored.settings.write().await.mode = "fixed".into();
        assert!(routing::reserve_idle_return(&restored, "m", &key)
            .await
            .is_none());
    }
}

#[tokio::test]
async fn stale_candidates_cannot_undo_migration_but_model_allowlist_changes_can_migrate() {
    let (_dir, state, official, api) = state().await;
    let key = affinity::key("stale-candidates", "m");
    let old_candidates = routing::candidates_with_session(&state, "m", None, Some(&key)).await;
    let mut migrated = routing::reserve(&state, &api, Some(&key), false)
        .await
        .unwrap();
    migrated.commit().unwrap();
    drop(migrated);
    assert_eq!(old_candidates[0].id, official.id);
    assert!(
        routing::reserve_for_model(&state, &old_candidates[0], Some(&key), false, "m")
            .await
            .is_none()
    );
    assert!(
        routing::reserve_for_model(&state, &api, Some(&key), false, "m")
            .await
            .is_some()
    );
    state.sources.write().await[1].source.models = vec!["another-model".into()];
    assert!(
        routing::reserve_for_model(&state, &official, Some(&key), false, "m")
            .await
            .is_some()
    );
}

#[tokio::test]
async fn duplicate_official_sources_share_account_low_quota_admission() {
    let (_dir, state, official, _) = state().await;
    quota(&state, 95.0).await;
    let mut duplicate = official.clone();
    duplicate.id = "same-account-copy".into();
    let mut runtime = state.sources.read().await[0].clone();
    runtime.source = duplicate.clone();
    state.sources.write().await.push(runtime);
    let lease = routing::reserve(
        &state,
        &official,
        Some(&affinity::key("first-thread", "m")),
        false,
    )
    .await
    .unwrap();
    assert!(routing::reserve(
        &state,
        &duplicate,
        Some(&affinity::key("new-thread", "m")),
        false
    )
    .await
    .is_none());
    drop(lease);
}

#[tokio::test]
async fn account_quota_uses_latest_known_windows_and_auth_failures_keep_auth_required() {
    let (_dir, state, official, _) = state().await;
    let mut duplicate = official.clone();
    duplicate.id = "duplicate".into();
    let mut copy = SourceRuntime::new(duplicate.clone());
    copy.quota = json!({"fetched_at":100,"windows":[{"name":"secondary","used_percent":100}]});
    state.sources.write().await.push(copy);
    assert!(routing::reserve(&state, &official, Some("new"), false)
        .await
        .is_none());
    state.sources.write().await[0].quota =
        json!({"fetched_at":101,"windows":[{"name":"secondary","used_percent":50}]});
    assert!(routing::reserve(&state, &official, Some("new"), false)
        .await
        .is_some());
    {
        let mut sources = state.sources.write().await;
        sources[0].health = "auth_required".into();
        sources[0].last_error = Some("persistence blocked".into());
    }
    routing::mark_auth_preflight_failure(&state, &official).await;
    assert_eq!(state.sources.read().await[0].health, "auth_required");
    assert_eq!(
        state.sources.read().await[0].last_error.as_deref(),
        Some("persistence blocked")
    );
}
