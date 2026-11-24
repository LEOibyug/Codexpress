use crate::{
    model::{AppState, Source},
    official, transport,
};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc, time::Duration};

const BODY_LIMIT: usize = 4 * 1024 * 1024;
const MODEL_LIMIT: usize = 10_000;

fn endpoint(source: &Source) -> Result<url::Url> {
    let base = source.base_url.trim_end_matches('/');
    let base = base.strip_suffix("/responses").unwrap_or(base);
    let mut url =
        url::Url::parse(&format!("{base}/models")).map_err(|_| anyhow!("模型列表地址无效"))?;
    if source.kind == "official" {
        url.query_pairs_mut()
            .append_pair("client_version", official::CODEX_CLIENT_VERSION);
    }
    Ok(url)
}

fn parse_models(body: &[u8]) -> Result<Vec<String>> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| anyhow!("供应商返回的模型列表不是有效 JSON"))?;
    let entries = value
        .get("data")
        .or_else(|| value.get("models"))
        .unwrap_or(&value)
        .as_array()
        .context("无法识别供应商的模型列表格式")?;
    if entries.len() > MODEL_LIMIT {
        bail!("供应商返回的模型数量过多");
    }
    let mut models = BTreeSet::new();
    for entry in entries {
        let id = entry
            .as_str()
            .or_else(|| entry.get("id").and_then(Value::as_str))
            .or_else(|| entry.get("slug").and_then(Value::as_str))
            .context("供应商模型列表缺少模型 ID")?;
        if id.is_empty()
            || id.len() > 256
            || id.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            bail!("供应商返回了无效的模型 ID");
        }
        models.insert(id.to_owned());
    }
    if models.is_empty() {
        bail!("供应商未返回可用模型，可重试或手动填写");
    }
    Ok(models.into_iter().collect())
}

fn is_openai_responses_model(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    let excluded_prefixes = [
        "whisper-",
        "tts-",
        "text-embedding-",
        "omni-moderation-",
        "text-moderation-",
        "dall-e-",
        "gpt-image-",
        "chatgpt-image-",
        "sora-",
        "babbage-",
        "davinci-",
        "gpt-realtime",
        "gpt-audio",
    ];
    if excluded_prefixes.iter().any(|prefix| lower.starts_with(prefix)) {
        return false;
    }
    if lower.ends_with("-instruct")
        || lower.ends_with("-audio")
        || lower.contains("-realtime")
        || lower.contains("-transcribe")
        || lower.contains("-tts")
        || lower.contains("-audio-")
    {
        return false;
    }
    true
}

async fn fetch(source: &Source) -> Result<(Vec<String>, Option<Value>)> {
    source.validate()?;
    if source.kind == "google" {
        return fetch_google(source).await;
    }
    let mut request = transport::client(source)?
        .get(endpoint(source)?)
        .timeout(Duration::from_secs(25))
        .header("accept", "application/json")
        .header("accept-encoding", "identity");
    let credential = if source.kind == "official" {
        "access_token"
    } else {
        "api_key"
    };
    let token = source
        .secret
        .get(credential)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .context("请先登录此账号")?;
    request = request.bearer_auth(token);
    if source.kind == "official" {
        let account = source
            .account_id
            .as_deref()
            .or_else(|| source.secret.get("account_id").and_then(Value::as_str))
            .filter(|s| !s.is_empty())
            .context("官方账号身份缺失，请重新登录")?;
        request = request
            .header("chatgpt-account-id", account)
            .header("originator", "codex_cli_rs")
            .header("version", official::CODEX_CLIENT_VERSION);
    }
    let mut response = request
        .send()
        .await
        .map_err(|e| anyhow!("获取模型列表{}", transport::failure_reason(&e)))?;
    if !response.status().is_success() {
        bail!(
            "获取模型列表失败（HTTP {}），请检查密钥或供应商接口",
            response.status().as_u16()
        );
    }
    if response
        .content_length()
        .is_some_and(|n| n > BODY_LIMIT as u64)
    {
        bail!("供应商模型列表超过 4 MiB 限制");
    }
    let body = transport::read_limited(&mut response, BODY_LIMIT)
        .await
        .map_err(|error| match error {
            transport::BodyReadError::TooLarge => anyhow!("供应商模型列表超过 4 MiB 限制"),
            transport::BodyReadError::ReadFailed => anyhow!("读取模型列表失败，请重试"),
        })?;
    let mut models = parse_models(&body)?;
    if source.kind == "openai" {
        models.retain(|id| is_openai_responses_model(id));
        if models.is_empty() {
            bail!("OpenAI API Platform 未返回可用文本模型，可重试或手动填写");
        }
    }
    let catalog = if source.kind == "official" {
        serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|value| {
                let entries = value.get("models")?.as_array()?;
                entries
                    .iter()
                    .all(|entry| entry["slug"].as_str().is_some())
                    .then(|| json!({"models":entries}))
            })
    } else {
        None
    };
    Ok((models, catalog))
}

fn google_endpoint(source: &Source, page_token: Option<&str>) -> Result<url::Url> {
    let base = source.base_url.trim_end_matches('/');
    let base = base.strip_suffix("/models").unwrap_or(base);
    let mut url =
        url::Url::parse(&format!("{base}/models")).map_err(|_| anyhow!("Google AI Studio 模型列表地址无效"))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("pageSize", "1000");
        if let Some(token) = page_token.filter(|t| !t.is_empty()) {
            query.append_pair("pageToken", token);
        }
    }
    Ok(url)
}

fn parse_google_models_page(
    body: &[u8],
    models: &mut BTreeSet<String>,
    catalog_entries: &mut Vec<Value>,
) -> Result<Option<String>> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| anyhow!("Google AI Studio 返回的模型列表不是有效 JSON"))?;
    let entries = value
        .get("models")
        .and_then(Value::as_array)
        .context("无法识别 Google AI Studio 的模型列表格式")?;
    for entry in entries {
        if let Some(methods) = entry.get("supportedGenerationMethods").and_then(Value::as_array) {
            if !methods.iter().any(|m| m.as_str() == Some("generateContent")) {
                continue;
            }
        }
        let raw_id = entry
            .get("name")
            .or_else(|| entry.get("id"))
            .and_then(Value::as_str)
            .context("Google AI Studio 模型列表缺少模型名称")?;
        let id = raw_id.strip_prefix("models/").unwrap_or(raw_id);
        if id.is_empty()
            || id.len() > 256
            || id.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            bail!("Google AI Studio 返回了无效的模型 ID");
        }
        if models.insert(id.to_owned()) {
            if models.len() > MODEL_LIMIT {
                bail!("供应商返回的模型数量过多");
            }
            let mut item = serde_json::Map::new();
            item.insert("slug".into(), json!(id));
            if let Some(name) = entry.get("displayName").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                item.insert("display_name".into(), json!(name));
            }
            if let Some(input_limit) = entry.get("inputTokenLimit").and_then(Value::as_i64).filter(|n| *n >= 2) {
                item.insert("context_window".into(), json!(input_limit));
                item.insert("max_context_window".into(), json!(input_limit));
            }
            if let Some(output_limit) = entry.get("outputTokenLimit").and_then(Value::as_i64).filter(|n| *n >= 1) {
                item.insert("max_output_tokens".into(), json!(output_limit));
            }
            if id.starts_with("gemini-") {
                item.insert("input_modalities".into(), json!(["text", "image"]));
                if !id.contains("-image") && !id.contains("-tts") {
                    if (id.starts_with("gemini-3") || id.starts_with("gemini-4")) && id.contains("-pro") {
                        item.insert("supported_reasoning_levels".into(), json!(["low", "high"]));
                    } else if id.starts_with("gemini-2.5-")
                        || id.starts_with("gemini-3")
                        || id.starts_with("gemini-4")
                    {
                        item.insert(
                            "supported_reasoning_levels".into(),
                            json!(["minimal", "low", "medium", "high"]),
                        );
                    }
                }
            }
            catalog_entries.push(Value::Object(item));
        }
    }
    Ok(value
        .get("nextPageToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned))
}

async fn fetch_google(source: &Source) -> Result<(Vec<String>, Option<Value>)> {
    let client = transport::client(source)?;
    let key = source
        .secret
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .context("请填写 Google AI Studio API 密钥")?;
    let mut models = BTreeSet::new();
    let mut catalog_entries = Vec::new();
    let mut page_token: Option<String> = None;

    for _ in 0..10 {
        let mut response = client
            .get(google_endpoint(source, page_token.as_deref())?)
            .timeout(Duration::from_secs(25))
            .header("accept", "application/json")
            .header("accept-encoding", "identity")
            .header("x-goog-api-key", key)
            .send()
            .await
            .map_err(|e| anyhow!("获取模型列表{}", transport::failure_reason(&e)))?;
        if !response.status().is_success() {
            bail!(
                "获取 Google AI Studio 模型列表失败（HTTP {}），请检查密钥或网络",
                response.status().as_u16()
            );
        }
        if response
            .content_length()
            .is_some_and(|n| n > BODY_LIMIT as u64)
        {
            bail!("供应商模型列表超过 4 MiB 限制");
        }
        let body = transport::read_limited(&mut response, BODY_LIMIT)
            .await
            .map_err(|error| match error {
                transport::BodyReadError::TooLarge => anyhow!("供应商模型列表超过 4 MiB 限制"),
                transport::BodyReadError::ReadFailed => anyhow!("读取模型列表失败，请重试"),
            })?;
        page_token = parse_google_models_page(&body, &mut models, &mut catalog_entries)?;
        if page_token.is_none() {
            break;
        }
    }

    if models.is_empty() {
        bail!("Google AI Studio 未返回可用文本模型，可重试或手动填写");
    }
    Ok((
        models.into_iter().collect(),
        Some(json!({"models": catalog_entries})),
    ))
}

pub async fn list(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    let source = if let Some(draft) = payload.get("source") {
        let source: Source =
            serde_json::from_value(draft.clone()).map_err(|_| anyhow!("模型来源配置格式错误"))?;
        // Unsaved API/Google/OpenAI drafts never mutate routing or credentials. Official
        // tokens can only be read from a saved source and the helper's refresh generation.
        if !matches!(source.kind.as_str(), "api" | "google" | "openai") {
            bail!("官方模型列表须使用已保存的来源");
        }
        source
    } else {
        let id = payload
            .get("source_id")
            .and_then(Value::as_str)
            .context("缺少来源标识")?;
        let source = state
            .sources
            .read()
            .await
            .iter()
            .find(|r| r.source.id == id)
            .map(|r| r.source.clone())
            .context("来源不存在")?;
        if source.kind == "official" {
            official::ensure_access(state, id).await?
        } else {
            source
        }
    };
    let (models, catalog) = fetch(&source).await?;
    if let Some(catalog) = catalog {
        let sources = state.sources.read().await;
        if sources.iter().any(|current| {
            current.source.enabled
                && current.source.id == source.id
                && current.source.identity() == source.identity()
                && current.source.proxy == source.proxy
        }) {
            let filename = if source.kind == "google" {
                "model-catalogs/google-latest.json"
            } else {
                "model-catalogs/official-latest.json"
            };
            let path = state.state_dir.join(filename);
            let bytes = serde_json::to_vec(&catalog)?;
            if std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
                crate::config::secure_write(&path, &bytes)?;
            }
        }
    }
    Ok(json!({"models":models}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_common_catalogs_and_rejects_unusable_responses() {
        for value in [
            json!({"data":[{"id":"z"},{"id":"a"},{"id":"z"}]}),
            json!({"models":[{"slug":"z"},{"slug":"a"}]}),
            json!(["z", "a"]),
        ] {
            assert_eq!(
                parse_models(&serde_json::to_vec(&value).unwrap()).unwrap(),
                ["a", "z"]
            );
        }
        for value in [
            json!({"data":[]}),
            json!({"data":[{"id":"bad\nmodel"}]}),
            json!({"error":"PRIVATE"}),
            json!({"data":[{}]}),
        ] {
            let error = parse_models(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .to_string();
            assert!(!error.contains("PRIVATE"));
        }
    }

    #[test]
    fn official_catalog_version_meets_new_model_compatibility_floor() {
        let source: Source = serde_json::from_value(json!({"id":"official","name":"Official","kind":"official","base_url":"https://chatgpt.com/backend-api/codex/","enabled":true,"priority":0,"proxy":{"mode":"explicit","url":"http://127.0.0.1:7890"}})).unwrap();
        let url = endpoint(&source).unwrap();
        assert_eq!(url.path(), "/backend-api/codex/models");
        let version = url.query_pairs().find(|(key, _)| key == "client_version").unwrap().1;
        let version: Vec<u32> = version.split('.').map(|part| part.parse().unwrap()).collect();
        assert_eq!(version.len(), 3);
        assert!(version.as_slice() >= [0, 166, 0].as_slice(), "6.2 官方目录要求至少 0.166.0 客户端协议");
    }

    #[test]
    fn google_model_page_parses_limits_modalities_and_thinking_levels() {
        let page = json!({
            "models": [
                {
                    "name": "models/gemini-2.5-pro",
                    "displayName": "Gemini 2.5 Pro",
                    "inputTokenLimit": 1048576,
                    "outputTokenLimit": 65536,
                    "supportedGenerationMethods": ["generateContent", "countTokens"]
                },
                {
                    "name": "models/gemini-3.1-pro-preview",
                    "displayName": "Gemini 3.1 Pro Preview",
                    "inputTokenLimit": 1048576,
                    "outputTokenLimit": 65536,
                    "supportedGenerationMethods": ["generateContent"]
                },
                {
                    "name": "models/text-embedding-004",
                    "displayName": "Embedding",
                    "supportedGenerationMethods": ["embedContent"]
                }
            ],
            "nextPageToken": "page-2"
        });
        let mut models = BTreeSet::new();
        let mut catalog = Vec::new();
        let next = parse_google_models_page(&serde_json::to_vec(&page).unwrap(), &mut models, &mut catalog).unwrap();
        assert_eq!(next.as_deref(), Some("page-2"));
        assert_eq!(models.into_iter().collect::<Vec<_>>(), ["gemini-2.5-pro", "gemini-3.1-pro-preview"]);
        let p25 = catalog.iter().find(|item| item["slug"] == "gemini-2.5-pro").unwrap();
        assert_eq!(p25["display_name"], "Gemini 2.5 Pro");
        assert_eq!(p25["context_window"], 1048576);
        assert_eq!(p25["max_context_window"], 1048576);
        assert_eq!(p25["max_output_tokens"], 65536);
        assert_eq!(p25["input_modalities"], json!(["text", "image"]));
        assert_eq!(p25["supported_reasoning_levels"], json!(["minimal", "low", "medium", "high"]));
        let p31 = catalog.iter().find(|item| item["slug"] == "gemini-3.1-pro-preview").unwrap();
        assert_eq!(p31["supported_reasoning_levels"], json!(["low", "high"]));
    }

    #[test]
    fn openai_platform_filter_keeps_responses_models_and_drops_non_text_endpoints() {
        for kept in ["gpt-5.4", "gpt-4o", "o3", "o4-mini", "codex-mini-latest"] {
            assert!(is_openai_responses_model(kept), "{kept} should be kept");
        }
        for dropped in [
            "whisper-1",
            "tts-1",
            "text-embedding-3-small",
            "dall-e-3",
            "gpt-image-1",
            "sora-2",
            "omni-moderation-latest",
            "babbage-002",
            "gpt-3.5-turbo-instruct",
            "gpt-4o-realtime-preview",
            "gpt-4o-audio-preview",
        ] {
            assert!(!is_openai_responses_model(dropped), "{dropped} should be filtered out");
        }
    }
}
