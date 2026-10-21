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
            .append_pair("client_version", "0.162.1");
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

async fn fetch(source: &Source) -> Result<Vec<String>> {
    source.validate()?;
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
            .header("version", "0.162.1");
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
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("读取模型列表失败，请重试"))?
    {
        if body.len() + chunk.len() > BODY_LIMIT {
            bail!("供应商模型列表超过 4 MiB 限制");
        }
        body.extend_from_slice(&chunk);
    }
    parse_models(&body)
}

pub async fn list(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    let source = if let Some(draft) = payload.get("source") {
        let source: Source =
            serde_json::from_value(draft.clone()).map_err(|_| anyhow!("模型来源配置格式错误"))?;
        // Unsaved API drafts never mutate routing or credentials. Official tokens
        // can only be read from a saved source and the helper's refresh generation.
        if source.kind != "api" {
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
    Ok(json!({"models":fetch(&source).await?}))
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
    fn official_endpoint_matches_codex_catalog_request() {
        let source: Source = serde_json::from_value(json!({"id":"official","name":"Official","kind":"official","base_url":"https://chatgpt.com/backend-api/codex/","enabled":true,"priority":0,"proxy":{"mode":"explicit","url":"http://127.0.0.1:7890"}})).unwrap();
        assert_eq!(
            endpoint(&source).unwrap().as_str(),
            "https://chatgpt.com/backend-api/codex/models?client_version=0.162.1"
        );
    }
}
