#[path = "common/server.rs"]
mod server;
#[path = "common/source.rs"]
mod source_fixture;

use axum::{body::{to_bytes, Body}, extract::Request, http::StatusCode, routing::{get, post}, Json, Router};
use codexpress_gateway::{chatbot, management, model::{AppState, Source}, relay};
use serde_json::{json, Value};
use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};
use tower::ServiceExt;

fn request(port: u16, key: &str, path: &str, model: Option<&str>, thread: Option<&str>) -> Request {
    let mut request = Request::builder().uri(path)
        .method(if model.is_some() { "POST" } else { "GET" })
        .header("host", format!("127.0.0.1:{port}"))
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        // These untrusted headers must not select the policy's client kind.
        .header("x-codexpress-client", "omp").header("x-client-kind", "omp");
    if let Some(thread) = thread { request = request.header("thread-id", thread); }
    request.body(model.map(|model| Body::from(json!({"model":model,"input":"hello"}).to_string())).unwrap_or_else(Body::empty)).unwrap()
}

async fn output(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 4 * 1024 * 1024).await.unwrap()).unwrap()
}

fn source_value(source: &Source) -> Value {
    let mut value = serde_json::to_value(source).unwrap();
    value["secret"] = source.secret.clone();
    value
}

#[tokio::test]
async fn client_catalogs_and_dispatch_share_model_policies_without_losing_native_metadata() {
    let hits = Arc::new(AtomicUsize::new(0));
    let mut origins = Vec::new();
    let mut tasks = Vec::new();
    for id in ["a", "b"] {
        let hits = hits.clone();
        let (base, task) = server::serve(Router::new().route("/responses", post(move || {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                Json(json!({"id":format!("response-{id}"),"object":"response","status":"completed","output":[],
                    "usage":{"input_tokens":1,"output_tokens":1}}))
            }
        }))).await;
        origins.push(base);
        tasks.push(task);
    }
    let dir = tempfile::tempdir_in(".").unwrap();
    let state = AppState::new(dir.path().into(), 17863, "fixture-native".into()).unwrap();
    let mut a = source_fixture::api_source("a", "A", &origins[0], "fixture-upstream-a");
    a.priority = 0;
    a.models = ["codex-only", "omp-only", "route-a", "route-b"].map(str::to_owned).to_vec();
    let mut b = source_fixture::api_source("b", "B", &origins[1], "fixture-upstream-b");
    b.priority = 1;
    b.models = ["chat-only", "omp-only", "route-a", "route-b", "blocked-source"].map(str::to_owned).to_vec();
    let sources = vec![source_value(&a), source_value(&b)];
    let mut policies = json!([
        {"model":"codex-only","clients":["codex"]},
        {"model":"chat-only","clients":["chatbot"]},
        {"model":"omp-only","clients":["omp"]},
        {"model":"route-a","source_ids":["b","a"],"clients":["codex"]},
        {"model":"route-b","source_ids":["a","b"],"clients":["codex"]},
        {"model":"unsupported","source_ids":["a"],"clients":["codex","omp","chatbot"]},
        {"model":"blocked-source","source_ids":["a"],"clients":["codex","omp","chatbot"]}
    ]);
    let rules = json!([{"model":"omp-only","context_window":800000,"auto_compact_token_limit":250000}]);
    management::dispatch(&state, "sync", json!({"sources":sources,"load_balance":true,
        "model_policies":policies,"model_compaction":rules})).await.unwrap();
    std::fs::create_dir_all(dir.path().join("model-catalogs")).unwrap();
    std::fs::write(dir.path().join("model-catalogs/official-latest.json"), serde_json::to_vec(&json!({"models":[
        {"slug":"codex-only","display_name":"Native Codex","supported_reasoning_levels":[{"effort":"high","description":"native"}],
            "experimental_supported_tools":["image_generation"],"model_messages":{"instructions_template":"native instructions"},
            "context_window":200000,"max_context_window":400000,"native_future_field":{"preserved":true}},
        {"slug":"omp-only","context_window":700000,"max_context_window":900000,"input_modalities":["text","image"],
            "supported_reasoning_levels":[{"effort":"high"}],"experimental_supported_tools":["native-tool"]}
    ]})).unwrap()).unwrap();
    let binding = management::dispatch(&state, "omp.binding", json!({})).await.unwrap();
    let key = binding["key"].as_str().unwrap();
    let chat = chatbot::router(state.clone());
    let native = relay::router(state.clone());
    for (path, expected) in [("/v1/models", "chat-only"), ("/omp/v1/models", "omp-only")] {
        let response = chat.clone().oneshot(request(17865, key, path, None, None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let result = output(response).await;
        assert_eq!(result["data"].as_array().unwrap().len(), 1);
        assert_eq!(result["data"][0]["id"], expected);
        if expected == "omp-only" {
            assert_eq!(result["data"], binding["models"]);
            assert_eq!(result["data"][0]["context_window"], 800000);
            assert_eq!(result["data"][0]["max_context_window"], 900000);
            assert_eq!(result["data"][0]["auto_compact_token_limit"], 250000);
            assert_eq!(result["data"][0]["experimental_supported_tools"], json!(["native-tool"]));
        }
    }
    let response = native.clone().oneshot(request(17863, "fixture-native", "/v1/models", None, None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let catalog = output(response).await;
    assert!(catalog.get("data").is_none());
    assert_eq!(catalog["models"].as_array().unwrap().len(), 3);
    let metadata = catalog["models"].as_array().unwrap().iter().find(|entry| entry["slug"] == "codex-only").unwrap();
    assert_eq!(metadata["native_future_field"], json!({"preserved":true}));
    assert_eq!(metadata["supported_reasoning_levels"][0]["effort"], "high");
    assert_eq!(metadata["experimental_supported_tools"], json!(["image_generation"]));
    assert_eq!(metadata["base_instructions"], "native instructions");
    state.sources.write().await[0].health = "cooldown".into();
    state.sources.write().await[0].cooldown_until = Some(codexpress_gateway::model::now() + 3600);
    let cooling = native.clone().oneshot(request(17863, "fixture-native", "/v1/models", None, None)).await.unwrap();
    assert_eq!(output(cooling).await, catalog);
    state.sources.write().await[0].health = "ready".into();
    state.sources.write().await[0].cooldown_until = None;
    // Per-model order is effective even with global load balancing enabled.
    for (router, port, token, path, model, expected, ingress) in [
        (native.clone(), 17863, "fixture-native", "/v1/responses", "route-a", Some("b"), "native"),
        (native.clone(), 17863, "fixture-native", "/v1/responses", "route-b", Some("a"), "native"),
        (native.clone(), 17863, "fixture-native", "/v1/responses", "codex-only", Some("a"), "native"),
        (chat.clone(), 17865, key, "/v1/responses", "chat-only", Some("b"), "chatbot"),
        (chat.clone(), 17865, key, "/omp/v1/responses", "omp-only", None, "omp"),
    ] {
        let response = router.oneshot(request(port, token, path, Some(model), Some(model))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        if ingress == "native" { assert_eq!(response.headers()["x-codexpress-source-id"], expected.unwrap()); }
        output(response).await;
        let now = codexpress_gateway::model::now();
        let records = state.usage.requests(&json!({"from":now-3600,"to":now+3600,"model":model,"ingress":ingress})).unwrap();
        assert_eq!(records["total"], 1);
        assert_eq!(records["records"][0]["ingress"], ingress);
        if ingress == "chatbot" { assert_eq!(records["records"][0]["source_id"], expected.unwrap()); }
    }
    let from = codexpress_gateway::model::now() - 3600;
    let analytics = state.usage.analytics(&json!({"from":from,"to":from+86400,"day_starts":[from,from+86400],"ingress":"omp"})).unwrap();
    assert_eq!(analytics["groups"][0]["requests"], 1);
    assert!(analytics["options"]["ingresses"].as_array().unwrap().contains(&json!("omp")));
    let submitted = hits.load(Ordering::SeqCst);
    policies[0]["clients"] = json!([]);
    management::dispatch(&state, "sync", json!({"sources":sources,"load_balance":true,
        "model_policies":policies,"model_compaction":rules})).await.unwrap();
    // Existing session state and the native compact endpoint cannot bypass a
    // newly disabled client. Nor can a caller forge the other client's model.
    for (is_native, path, model, thread) in [
        (true, "/v1/responses", "codex-only", Some("codex-only")),
        (true, "/v1/responses/compact", "codex-only", Some("codex-only")),
        (true, "/v1/responses", "omp-only", None),
        (false, "/v1/responses", "omp-only", None),
        (false, "/omp/v1/responses", "chat-only", None),
        (false, "/omp/v1/responses/compact", "chat-only", None),
    ] {
        let (router, port, token) = if is_native { (native.clone(), 17863, "fixture-native") } else { (chat.clone(), 17865, key) };
        let response = router.oneshot(request(port, token, path, Some(model), thread)).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error = output(response).await;
        assert_eq!(error["error"]["code"], "model_not_enabled_for_client");
        assert_eq!(error["error"]["param"], "model");
    }
    assert_eq!(hits.load(Ordering::SeqCst), submitted);
    for policy in policies.as_array_mut().unwrap() { policy["clients"] = json!([]); }
    management::dispatch(&state, "sync", json!({"sources":sources,"model_policies":policies,"model_compaction":rules})).await.unwrap();
    for (router, port, token, path, field) in [
        (native, 17863, "fixture-native", "/v1/models", "models"),
        (chat.clone(), 17865, key, "/v1/models", "data"),
        (chat, 17865, key, "/omp/v1/models", "data"),
    ] {
        let response = router.oneshot(request(port, token, path, None, None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(output(response).await[field], json!([]));
    }
    for task in tasks { task.abort(); }
}

#[tokio::test]
async fn discovery_does_not_narrow_an_unrestricted_sources_explicit_models() {
    let (base, task) = server::serve(Router::new()
        .route("/models", get(|| async { Json(json!({"data":[{"id":"advertised"}]})) }))
        .route("/responses", post(|| async { Json(json!({"id":"fixture-response","status":"completed","output":[]})) })))
        .await;
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().into(), 17863, "fixture-native".into()).unwrap();
    let mut source = source_fixture::api_source("wildcard", "Wildcard", &base, "fixture-upstream");
    source.models.clear();
    let policies = json!([{"model":"configured","source_ids":["wildcard"],"clients":["codex"]}]);
    management::dispatch(&state, "sync", json!({"sources":[source_value(&source)],"model_policies":policies})).await.unwrap();
    let router = relay::router(state.clone());
    let catalog = output(router.clone().oneshot(request(17863, "fixture-native", "/v1/models", None, None)).await.unwrap()).await;
    assert!(catalog["models"].as_array().unwrap().iter().any(|entry| entry["slug"] == "configured"));
    let response = router.clone().oneshot(request(17863, "fixture-native", "/v1/responses", Some("configured"), None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    source.models = vec!["advertised".into()];
    management::dispatch(&state, "sync", json!({"sources":[source_value(&source)],"model_policies":policies})).await.unwrap();
    let catalog = output(router.clone().oneshot(request(17863, "fixture-native", "/v1/models", None, None)).await.unwrap()).await;
    assert!(!catalog["models"].as_array().unwrap().iter().any(|entry| entry["slug"] == "configured"));
    source.base_url = "http://127.0.0.1:1".into();
    source.models.clear();
    management::dispatch(&state, "sync", json!({"sources":[source_value(&source)]})).await.unwrap();
    let response = router.oneshot(request(17863, "fixture-native", "/v1/models", None, None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    task.abort();
}

#[tokio::test]
async fn managed_local_and_remote_codex_catalogs_apply_and_remove_explicit_policies() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex");
    let state_dir = dir.path().join("app");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    let state = AppState::new(state_dir, 17863, "fixture-native".into()).unwrap();
    let mut source = source_fixture::api_source("a", "A", "http://127.0.0.1:9", "fixture-upstream");
    source.models = ["kept", "hidden", "source-blocked", "untouched"].map(str::to_owned).to_vec();
    let catalog = json!({"models":source.models.iter().map(|id| json!({"slug":id,"context_window":100000})).collect::<Vec<_>>()});
    let original = serde_json::to_vec(&catalog).unwrap();
    std::fs::write(home.join("own.json"), &original).unwrap();
    let config = "model_catalog_json='own.json'\n";
    std::fs::write(home.join("config.toml"), config).unwrap();
    let rules = json!([{"model":"kept","context_window":100000,"auto_compact_token_limit":20000},
        {"model":"hidden","context_window":100000,"auto_compact_token_limit":20000}]);
    let sources = json!([source_value(&source)]);
    management::dispatch(&state, "sync", json!({"sources":sources,"model_compaction":rules,"model_policies":[
        {"model":"hidden","clients":["omp"]},
        {"model":"source-blocked","source_ids":["removed"]}
    ]})).await.unwrap();
    let payload = json!({"codex_home":home,"model_compaction":rules});
    let preview = management::dispatch(&state, "config.preview", payload.clone()).await.unwrap();
    assert_eq!(preview["model_limits"].as_array().unwrap().iter().map(|entry| entry["model"].as_str().unwrap()).collect::<Vec<_>>(), ["kept"]);
    management::dispatch(&state, "config.attach", payload.clone()).await.unwrap();
    let active = std::fs::read_to_string(home.join("config.toml")).unwrap().parse::<toml_edit::DocumentMut>().unwrap();
    let active: Value = serde_json::from_slice(&std::fs::read(active["model_catalog_json"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(active["models"].as_array().unwrap().iter().map(|entry| entry["slug"].as_str().unwrap()).collect::<Vec<_>>(), ["kept", "untouched"]);
    let remote = management::dispatch(&state, "remote.preview", json!({"id":"fixture-models","name":"Fixture", "host":"192.0.2.10",
        "config":config,"codex_home":"/home/fixture/.codex","catalog":catalog,"model_compaction":rules})).await.unwrap();
    let remote: Value = serde_json::from_str(remote["catalog"].as_str().unwrap()).unwrap();
    assert_eq!(remote["models"].as_array().unwrap().iter().map(|entry| entry["slug"].as_str().unwrap()).collect::<Vec<_>>(), ["kept", "untouched"]);
    management::dispatch(&state, "sync", json!({"sources":sources,"model_compaction":rules})).await.unwrap();
    management::dispatch(&state, "config.attach", payload).await.unwrap();
    let restored = std::fs::read_to_string(home.join("config.toml")).unwrap().parse::<toml_edit::DocumentMut>().unwrap();
    let restored: Value = serde_json::from_slice(&std::fs::read(restored["model_catalog_json"].as_str().unwrap()).unwrap()).unwrap();
    for id in ["hidden", "source-blocked"] {
        assert!(restored["models"].as_array().unwrap().iter().any(|entry| entry["slug"] == id));
    }
    assert_eq!(std::fs::read(home.join("own.json")).unwrap(), original);
}
