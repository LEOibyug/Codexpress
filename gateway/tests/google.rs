#[path = "common/server.rs"]
mod server;
#[path = "common/source.rs"]
mod source_fixture;

use axum::{
    body::{to_bytes, Body},
    extract::{Path as AxumPath, Request},
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use codexpress_gateway::{
    chatbot, management,
    model::{AppState, Source},
    relay,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tower::ServiceExt;

fn source_value(source: &Source) -> Value {
    let mut value = serde_json::to_value(source).unwrap();
    value["secret"] = source.secret.clone();
    value
}

fn google_source(id: &str, name: &str, base: &str, api_key: &str, models: Vec<String>) -> Source {
    let mut source = source_fixture::api_source(id, name, base, api_key);
    source.kind = "google".into();
    source.models = models;
    source
}

fn openai_source(id: &str, name: &str, base: &str, api_key: &str, models: Vec<String>) -> Source {
    let mut source = source_fixture::api_source(id, name, base, api_key);
    source.kind = "openai".into();
    source.models = models;
    source
}

#[derive(Clone)]
struct CapturedUpstream {
    path: String,
    body: Value,
}

#[tokio::test]
async fn google_source_translates_responses_chat_omp_tools_and_thinking_and_records_usage() {
    let captured: Arc<Mutex<Vec<CapturedUpstream>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_ref = captured.clone();

    let upstream = Router::new().route(
        "/v1beta/models/{spec}",
        post(move |AxumPath(spec): AxumPath<String>, headers: HeaderMap, body: axum::body::Bytes| {
            let captured = captured_ref.clone();
            async move {
                let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                assert_eq!(headers["x-goog-api-key"], "PRIVATE-GOOGLE-KEY");
                assert!(!headers.contains_key("authorization"));
                captured.lock().unwrap().push(CapturedUpstream {
                    path: spec.clone(),
                    body: parsed.clone(),
                });

                if spec == "gemini-2.5-pro:generateContent" {
                    // Multi-turn tool call check
                    let has_tool_result = parsed["contents"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|turn| {
                            turn["parts"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|p| p.get("functionResponse").is_some())
                        });
                    if !has_tool_result {
                        return (
                            [("content-type", "application/json")],
                            json!({
                                "modelVersion": "gemini-2.5-pro",
                                "candidates": [{
                                    "content": {
                                        "role": "model",
                                        "parts": [
                                            {"text": "Let me think.", "thought": true},
                                            {
                                                "functionCall": {"name": "lookup_weather", "args": {"city": "Tokyo"}},
                                                "thoughtSignature": "sig-secret-turn-1"
                                            }
                                        ]
                                    },
                                    "finishReason": "STOP"
                                }],
                                "usageMetadata": {
                                    "promptTokenCount": 100,
                                    "cachedContentTokenCount": 20,
                                    "candidatesTokenCount": 30,
                                    "thoughtsTokenCount": 12,
                                    "totalTokenCount": 142
                                }
                            })
                            .to_string(),
                        )
                            .into_response();
                    }
                    return (
                        [("content-type", "application/json")],
                        json!({
                            "modelVersion": "gemini-2.5-pro",
                            "candidates": [{
                                "content": {
                                    "role": "model",
                                    "parts": [{"text": "It is sunny in Tokyo."}]
                                },
                                "finishReason": "STOP"
                            }],
                            "usageMetadata": {
                                "promptTokenCount": 140,
                                "candidatesTokenCount": 10,
                                "thoughtsTokenCount": 5,
                                "totalTokenCount": 155
                            }
                        })
                        .to_string(),
                    )
                        .into_response();
                }

                if spec.starts_with("gemini-2.0-flash:") {
                    if spec.ends_with(":streamGenerateContent") {
                        let sse = "data: {\"modelVersion\":\"gemini-2.0-flash\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello 2.0\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":4,\"totalTokenCount\":16}}\n\n";
                        return ([("content-type", "text/event-stream")], sse).into_response();
                    }
                    return (
                        [("content-type", "application/json")],
                        json!({
                            "modelVersion": "gemini-2.0-flash",
                            "candidates": [{
                                "content": {"role": "model", "parts": [{"text": "Hello 2.0"}]},
                                "finishReason": "STOP"
                            }],
                            "usageMetadata": {
                                "promptTokenCount": 12,
                                "candidatesTokenCount": 4,
                                "totalTokenCount": 16
                            }
                        })
                        .to_string(),
                    )
                        .into_response();
                }

                if spec.ends_with(":streamGenerateContent") {
                    let sse = concat!(
                        "data: {\"modelVersion\":\"gemini-3-flash-preview\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Thinking...\",\"thought\":true}]}}]}\r\n\r\n",
                        "data: {\"modelVersion\":\"gemini-3-flash-preview\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello Gemini stream!\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":50,\"cachedContentTokenCount\":10,\"candidatesTokenCount\":20,\"thoughtsTokenCount\":7,\"totalTokenCount\":77}}\r\n\r\n"
                    );
                    return ([("content-type", "text/event-stream")], sse).into_response();
                }

                StatusCode::NOT_FOUND.into_response()
            }
        }),
    );

    use axum::response::IntoResponse;
    let (base, task) = server::serve(upstream).await;
    let dir = tempfile::tempdir_in(".").unwrap();
    let state = AppState::new(dir.path().to_owned(), 17863, "native-secret".into()).unwrap();
    let g_source = google_source(
        "google-1",
        "Google AI Studio",
        &format!("{base}/v1beta"),
        "PRIVATE-GOOGLE-KEY",
        vec![
            "gemini-2.5-pro".into(),
            "gemini-3-flash-preview".into(),
            "gemini-2.0-flash".into(),
            "disabled-gemini".into(),
        ],
    );
    let policies = json!([
        {"model": "disabled-gemini", "source_ids": [], "clients": ["codex", "omp", "chatbot"]}
    ]);
    management::dispatch(
        &state,
        "sync",
        json!({"sources": [source_value(&g_source)], "model_policies": policies}),
    )
    .await
    .unwrap();
    let binding = management::dispatch(&state, "omp.binding", json!({})).await.unwrap();
    let key = binding["key"].as_str().unwrap().to_owned();
    let chat = chatbot::router(state.clone());
    let native = relay::router(state.clone());

    // /omp/v1/models advertises remote_compaction: false for Google-only models, and omits disabled-gemini.
    let omp_models_res = chat
        .clone()
        .oneshot(
            Request::builder()
                .uri("/omp/v1/models")
                .header("host", "127.0.0.1:17865")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(omp_models_res.status(), StatusCode::OK);
    let omp_models: Value =
        serde_json::from_slice(&to_bytes(omp_models_res.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert!(!omp_models["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "disabled-gemini"));
    let g3 = omp_models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "gemini-3-flash-preview")
        .unwrap();
    assert_eq!(g3["remote_compaction"], json!(false));

    // /omp/v1/capabilities returns remote_compaction_v2: false immediately for Google sources.
    let cap_res = chat
        .clone()
        .oneshot(
            Request::builder()
                .uri("/omp/v1/capabilities?model=gemini-3-flash-preview&session_id=s1")
                .header("host", "127.0.0.1:17865")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cap_res.status(), StatusCode::OK);
    let cap: Value =
        serde_json::from_slice(&to_bytes(cap_res.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(cap["remote_compaction_v2"], json!(false));

    // Compaction endpoints return 400 unsupported_compaction without hitting upstream.
    for (path, body) in [
        ("/omp/v1/responses/compact", json!({"model":"gemini-2.5-pro","input":[]})),
        ("/omp/v1/responses", json!({"model":"gemini-2.5-pro","input":[{"type":"compaction_trigger"}]})),
    ] {
        let res = chat
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("host", "127.0.0.1:17865")
                    .header("authorization", format!("Bearer {key}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let err: Value =
            serde_json::from_slice(&to_bytes(res.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(err["error"]["code"], "unsupported_compaction");
    }
    assert!(captured.lock().unwrap().is_empty());

    // 1) Native /v1/responses non-streaming multi-turn tool call + thoughtSignature re-attachment + thinkingBudget
    let turn1_req = json!({
        "model": "gemini-2.5-pro",
        "stream": false,
        "instructions": "Be concise.",
        "reasoning": {"effort": "high"},
        "tools": [{
            "type": "function",
            "name": "lookup_weather",
            "description": "Get weather",
            "parameters": {
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"],
                "additionalProperties": false
            }
        }],
        "input": [{"role": "user", "content": "Weather in Tokyo?"}]
    });
    let turn1_res = native
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("host", "127.0.0.1:17863")
                .header("authorization", "Bearer native-secret")
                .header("content-type", "application/json")
                .body(Body::from(turn1_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(turn1_res.status(), StatusCode::OK);
    let turn1_json: Value =
        serde_json::from_slice(&to_bytes(turn1_res.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(turn1_json["usage"]["output_tokens"], 42);
    assert_eq!(turn1_json["usage"]["output_tokens_details"]["reasoning_tokens"], 12);
    let fc_item = turn1_json["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .unwrap();
    let call_id = fc_item["call_id"].as_str().unwrap().to_owned();
    assert_eq!(fc_item["name"], "lookup_weather");

    // Verify Gemini request sanitized schema and set thinkingBudget for 2.5
    {
        let log = captured.lock().unwrap();
        let req1 = &log[0];
        assert_eq!(req1.path, "gemini-2.5-pro:generateContent");
        let decl = &req1.body["tools"][0]["functionDeclarations"][0];
        assert_eq!(decl["parameters"]["type"], "OBJECT");
        assert_eq!(decl["parameters"]["properties"]["city"]["type"], "STRING");
        assert!(decl["parameters"].get("$schema").is_none());
        assert!(decl["parameters"].get("additionalProperties").is_none());
        assert_eq!(req1.body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 24576);
    }

    // Turn 2 with function_call + function_call_output re-attaches cached thoughtSignature
    let turn2_req = json!({
        "model": "gemini-2.5-pro",
        "stream": false,
        "input": [
            {"role": "user", "content": "Weather in Tokyo?"},
            {"type": "function_call", "call_id": call_id, "name": "lookup_weather", "arguments": "{\"city\":\"Tokyo\"}"},
            {"type": "function_call_output", "call_id": call_id, "output": "{\"temp\":\"22C\"}"}
        ]
    });
    let turn2_res = native
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("host", "127.0.0.1:17863")
                .header("authorization", "Bearer native-secret")
                .header("content-type", "application/json")
                .body(Body::from(turn2_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(turn2_res.status(), StatusCode::OK);
    {
        let log = captured.lock().unwrap();
        let req2 = &log[1];
        assert_eq!(
            req2.body["contents"][1]["parts"][0]["thoughtSignature"],
            "sig-secret-turn-1"
        );
        assert_eq!(req2.body["contents"][2]["role"], "user");
        assert_eq!(
            req2.body["contents"][2]["parts"][0]["functionResponse"]["name"],
            "lookup_weather"
        );
    }

    // 2) OMP streaming /omp/v1/responses with Gemini 3 thinkingLevel + reasoning/text/usage translation
    let omp_res = chat
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/omp/v1/responses")
                .header("host", "127.0.0.1:17865")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .header("x-session-id", "omp-gemini-session")
                .body(Body::from(
                    json!({
                        "model": "gemini-3-flash-preview",
                        "stream": true,
                        "reasoning": {"effort": "medium"},
                        "input": [{"role": "user", "content": "Hi"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(omp_res.status(), StatusCode::OK);
    let omp_sse = String::from_utf8(
        to_bytes(omp_res.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(omp_sse.contains("response.reasoning_summary_text.delta"));
    assert!(omp_sse.contains("Hello Gemini stream!"));
    assert!(omp_sse.contains("response.completed"));
    {
        let log = captured.lock().unwrap();
        let req3 = &log[2];
        assert_eq!(req3.path, "gemini-3-flash-preview:streamGenerateContent");
        assert_eq!(req3.body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "medium");
        assert!(req3.body["generationConfig"]["thinkingConfig"].get("thinkingBudget").is_none());
    }

    // 3) Chatbot /v1/chat/completions non-streaming & streaming with Gemini 2.0 (omits thinkingConfig)
    let chat_non_stream = chat
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("host", "127.0.0.1:17865")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-2.0-flash",
                        "stream": false,
                        "messages": [{"role": "user", "content": "Say hi"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(chat_non_stream.status(), StatusCode::OK);
    let chat_json: Value = serde_json::from_slice(
        &to_bytes(chat_non_stream.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(chat_json["choices"][0]["message"]["content"], "Hello 2.0");
    {
        let log = captured.lock().unwrap();
        let req4 = &log[3];
        assert!(req4.body["generationConfig"].get("thinkingConfig").is_none());
    }

    let chat_stream = chat
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("host", "127.0.0.1:17865")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gemini-3-flash-preview",
                        "stream": true,
                        "messages": [{"role": "user", "content": "Stream chat"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(chat_stream.status(), StatusCode::OK);
    let chat_sse = String::from_utf8(
        to_bytes(chat_stream.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(chat_sse.contains("Hello Gemini stream!"));
    assert!(chat_sse.contains("[DONE]"));

    // Verify usage recorded source_kind == "google"
    let now = codexpress_gateway::model::now();
    let recent = state
        .usage
        .requests(&json!({"from": now - 3600, "to": now + 3600, "source_id": "google-1"}))
        .unwrap();
    assert_eq!(recent["total"], 5);
    assert!(recent["records"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["token_details"]["source_kind"] == "google"));

    // Verify disabled-gemini cannot bypass to upstream
    let before_disabled = captured.lock().unwrap().len();
    let blocked = native
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("host", "127.0.0.1:17863")
                .header("authorization", "Bearer native-secret")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model": "disabled-gemini", "input": "hello"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(blocked.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(captured.lock().unwrap().len(), before_disabled);

    task.abort();
}

#[tokio::test]
async fn openai_platform_source_sanitizes_codex_only_fields_and_rejects_balance_query() {
    let captured_bodies: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_ref = captured_bodies.clone();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_ref = hits.clone();

    let upstream = Router::new()
        .route(
            "/v1/responses",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let captured = captured_ref.clone();
                let hits = hits_ref.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(headers["authorization"], "Bearer PRIVATE-OPENAI-KEY");
                    let parsed: Value = serde_json::from_slice(&body).unwrap();
                    captured.lock().unwrap().push(("/v1/responses".into(), parsed));
                    Json(json!({
                        "id": "resp_openai_1",
                        "object": "response",
                        "status": "completed",
                        "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"OpenAI OK"}]}],
                        "usage": {"input_tokens": 10, "output_tokens": 3}
                    }))
                }
            }),
        )
        .route(
            "/v1/responses/compact",
            post(|| async {
                Json(json!({
                    "id": "cmp_1",
                    "object": "response.compaction",
                    "output": [{"type": "compaction", "encrypted_content": "opaque-openai"}]
                }))
            }),
        );

    let (base, task) = server::serve(upstream).await;
    let dir = tempfile::tempdir_in(".").unwrap();
    let state = AppState::new(dir.path().to_owned(), 17863, "native-secret".into()).unwrap();
    let oa_source = openai_source(
        "openai-1",
        "OpenAI API Platform",
        &format!("{base}/v1"),
        "PRIVATE-OPENAI-KEY",
        vec!["gpt-5.4".into()],
    );
    let g_source = google_source(
        "google-1",
        "Google AI Studio",
        "https://127.0.0.1:1/v1beta",
        "PRIVATE-GOOGLE-KEY",
        vec!["gemini-2.5-pro".into()],
    );
    management::dispatch(
        &state,
        "sync",
        json!({"sources": [source_value(&oa_source), source_value(&g_source)]}),
    )
    .await
    .unwrap();
    let binding = management::dispatch(&state, "omp.binding", json!({})).await.unwrap();
    let key = binding["key"].as_str().unwrap().to_owned();
    let chat = chatbot::router(state.clone());

    // /omp/v1/models omits remote_compaction (not false) for OpenAI API Platform models, and sets false for Google-only.
    let models_res = chat
        .clone()
        .oneshot(
            Request::builder()
                .uri("/omp/v1/models")
                .header("host", "127.0.0.1:17865")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let models_json: Value =
        serde_json::from_slice(&to_bytes(models_res.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    let gpt = models_json["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "gpt-5.4")
        .unwrap();
    assert!(gpt.get("remote_compaction").is_none());
    let gem = models_json["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "gemini-2.5-pro")
        .unwrap();
    assert_eq!(gem["remote_compaction"], json!(false));

    // /omp/v1/codex/responses strips client_metadata, access_programs, and stream_options.reasoning_summary_delivery
    let omp_req = json!({
        "model": "gpt-5.4",
        "stream": false,
        "input": "hello",
        "client_metadata": {"session_id": "omp-sess", "codexpress_session_id": "omp-sess"},
        "access_programs": {"cyber": "enabled"},
        "stream_options": {"reasoning_summary_delivery": "sequential_cutoff", "include_usage": true}
    });
    let omp_res = chat
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/omp/v1/codex/responses")
                .header("host", "127.0.0.1:17865")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(omp_req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(omp_res.status(), StatusCode::OK);
    {
        let recorded = captured_bodies.lock().unwrap();
        let (_, sent) = &recorded[0];
        assert!(sent.get("client_metadata").is_none());
        assert!(sent.get("access_programs").is_none());
        assert!(sent["stream_options"].get("reasoning_summary_delivery").is_none());
        assert_eq!(sent["stream_options"]["include_usage"], json!(true));
    }

    // /v1/responses/compact and /v1/chat/completions also work against OpenAI Platform
    let compact_res = chat
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses/compact")
                .header("host", "127.0.0.1:17865")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"model": "gpt-5.4", "input": []}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(compact_res.status(), StatusCode::OK);

    let chat_res = chat
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("host", "127.0.0.1:17865")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model": "gpt-5.4", "stream": false, "messages": [{"role":"user","content":"ping"}]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(chat_res.status(), StatusCode::OK);
    let chat_body: Value =
        serde_json::from_slice(&to_bytes(chat_res.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(chat_body["choices"][0]["message"]["content"], "OpenAI OK");

    // balance.query rejects both openai and google sources
    for id in ["openai-1", "google-1"] {
        assert!(
            management::dispatch(&state, "balance.query", json!({"source_id": id}))
                .await
                .is_err()
        );
    }

    task.abort();
}
