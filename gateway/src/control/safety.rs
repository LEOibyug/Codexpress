use crate::model::{AppState, ProxyPolicy, Source};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

pub(super) fn fields(payload: &Value, allowed: &[&str]) -> Result<()> {
    let object = payload.as_object().context("参数必须为 JSON 对象")?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        bail!("包含不支持的参数");
    }
    Ok(())
}
pub(super) fn text<'a>(payload: &'a Value, key: &str) -> Result<&'a str> {
    payload[key]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .context("缺少有效标识")
}
pub(super) fn optional_text(payload: &Value, key: &str) -> Result<Option<String>> {
    match payload.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => text(payload, key).map(|s| Some(s.to_owned())),
    }
}
pub(super) fn contains_secret(value: &Value, key: &str) -> bool {
    match value {
        Value::String(s) => s == key,
        Value::Array(items) => items.iter().any(|v| contains_secret(v, key)),
        Value::Object(object) => object.values().any(|v| contains_secret(v, key)),
        _ => false,
    }
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Binding {
    pub source_id: String,
    identity: String,
    proxy: ProxyPolicy,
    pub account_id: Option<String>,
}
impl Binding {
    pub fn of(source: &Source) -> Self {
        Self {
            source_id: source.id.clone(),
            identity: source.identity(),
            proxy: source.proxy.clone(),
            account_id: source.official_account_id().map(str::to_owned),
        }
    }
    pub fn matches(&self, source: &Source) -> bool {
        self == &Self::of(source)
    }
    pub async fn current(&self, state: &Arc<AppState>) -> bool {
        state
            .sources
            .read()
            .await
            .iter()
            .any(|runtime| self.matches(&runtime.source))
    }
}

fn credential_strings(value: &Value, secrets: &mut Vec<String>) {
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            if matches!(
                key.as_str(),
                "api_key"
                    | "access_token"
                    | "refresh_token"
                    | "id_token"
                    | "cookie"
                    | "password"
                    | "client_secret"
            ) {
                if let Some(s) = value.as_str().filter(|s| !s.is_empty()) {
                    secrets.push(s.to_owned());
                }
            } else if value.is_object() {
                credential_strings(value, secrets);
            }
        }
    }
}
fn clean(value: &mut Value, secrets: &[String]) {
    match value {
        Value::Object(object) => {
            object.retain(|key, _| {
                ![
                    "secret",
                    "key",
                    "api_key",
                    "access_token",
                    "refresh_token",
                    "id_token",
                    "authorization",
                    "cookie",
                    "set-cookie",
                    "headers",
                    "body",
                    "request_body",
                    "response_body",
                    "error_body",
                    "error_message",
                    "last_error",
                    "error_param",
                    "error_code",
                    "error_kind",
                    "error",
                    "metadata",
                    "raw",
                    "raw_body",
                    "diagnostic",
                ]
                .iter()
                .any(|name| key.eq_ignore_ascii_case(name))
            });
            if let Some(proxy) = object.get_mut("proxy").and_then(Value::as_object_mut) {
                proxy.retain(|key, _| key == "mode");
            }
            for item in object.values_mut() {
                clean(item, secrets);
            }
        }
        Value::Array(items) => {
            for item in items {
                clean(item, secrets);
            }
        }
        Value::String(s) => {
            for secret in secrets {
                if s.contains(secret) {
                    *s = s.replace(secret, "[已隐藏]");
                }
            }
        }
        _ => {}
    }
}
/// Only normalized metadata crosses HTTP; upstream error text and known credentials never do.
pub(super) async fn expose(state: &Arc<AppState>, mut value: Value) -> Value {
    let mut secrets = vec![state.gateway_key.clone()];
    for value in [state.control.key(), state.chatbot.key(), state.images.key()]
        .into_iter()
        .flatten()
    {
        if let Some(key) = value["key"].as_str() {
            secrets.push(key.to_owned());
        }
    }
    for runtime in state.sources.read().await.iter() {
        credential_strings(&runtime.source.secret, &mut secrets);
    }
    secrets.retain(|s| !s.is_empty());
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    secrets.dedup();
    clean(&mut value, &secrets);
    value
}
