//! Repair only an explicit pre-generation rejection of a system input role.
use crate::model::Source;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap, fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt, path::PathBuf,
    sync::Mutex,
};

/// Derived source capabilities only: no credentials, messages or response bodies.
pub(crate) struct RoleCapabilities {
    path: PathBuf,
    sources: Mutex<BTreeMap<String, String>>,
}

impl RoleCapabilities {
    pub(crate) fn open(path: PathBuf) -> Self {
        let load = || -> anyhow::Result<BTreeMap<String, String>> {
            let file = match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(BTreeMap::new())
                }
                Err(error) => return Err(error.into()),
            };
            let meta = file.metadata()?;
            anyhow::ensure!(
                meta.is_file() && meta.len() <= 262144,
                "invalid capability cache"
            );
            let mut bytes = Vec::new();
            file.take(262145).read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= 262144, "capability cache too large");
            let value: Value = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(value["version"] == 1, "invalid capability version");
            let sources: BTreeMap<String, String> =
                serde_json::from_value(value["system_as_developer"].clone())?;
            anyhow::ensure!(
                sources.len() <= 1024
                    && sources
                        .iter()
                        .all(|(id, identity)| id.len() <= 256 && identity.len() == 64),
                "invalid capability entries"
            );
            Ok(sources)
        };
        let sources = load().unwrap_or_else(|_| {
            eprintln!("来源角色兼容标记无法读取，将重新识别");
            BTreeMap::new()
        });
        Self {
            path,
            sources: Mutex::new(sources),
        }
    }

    pub(crate) fn requires_developer(&self, source: &Source) -> bool {
        self.sources
            .lock()
            .expect("role capabilities")
            .get(&source.id)
            == Some(&source.identity())
    }

    pub(super) fn remember(&self, source: &Source) {
        let mut sources = self.sources.lock().expect("role capabilities");
        let identity = source.identity();
        if sources.get(&source.id) == Some(&identity) {
            return;
        }
        sources.insert(source.id.clone(), identity);
        while sources.len() > 1024 {
            sources.pop_first();
        }
        let bytes = serde_json::to_vec(&json!({"version":1,"system_as_developer":*sources}))
            .expect("capabilities");
        if crate::config::secure_write(&self.path, &bytes).is_err() {
            eprintln!("来源角色兼容标记未能保存，本次运行仍会使用已识别结果");
        }
    }
}

pub(super) fn has_system(body: &Value) -> bool {
    body["input"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["role"] == "system"))
}

fn role_index(param: &str) -> Option<usize> {
    param
        .strip_prefix("input[")
        .and_then(|p| p.strip_suffix("].role"))
        .or_else(|| {
            param
                .strip_prefix("input.")
                .and_then(|p| p.strip_suffix(".role"))
        })?
        .parse()
        .ok()
}

fn rejected(body: &Value, error: &Value) -> bool {
    let param = error["param"].as_str().unwrap_or("");
    let message = error["message"]
        .as_str()
        .or_else(|| error["detail"].as_str())
        .or_else(|| error.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if let Some(index) = role_index(param) {
        return body["input"]
            .get(index)
            .is_some_and(|item| item["role"] == "system")
            && (matches!(
                error["code"].as_str(),
                Some("unsupported_value" | "invalid_value" | "unsupported_role")
            ) || (message.contains("system")
                && (message.contains("unsupported")
                    || message.contains("not supported")
                    || message.contains("not allowed"))));
    }
    // Do not mistake validation of content or another field for a role error.
    if !param.is_empty() {
        return false;
    }
    [
        "system messages are not supported",
        "system messages are not allowed",
        "system role is not supported",
        "unsupported role: system",
        "unsupported role: 'system'",
    ]
    .iter()
    .any(|phrase| message.contains(phrase))
}

pub(super) fn adapt(body: &Value, rejection: &Value) -> Option<Value> {
    if !has_system(body) {
        return None;
    }
    let error = rejection.get("error").unwrap_or(rejection);
    let explicit = rejected(body, error)
        || rejection["detail"].as_array().is_some_and(|details| {
            details.iter().any(|item| {
                let Some(loc) = item["loc"].as_array() else {
                    return false;
                };
                let loc = loc.strip_prefix(&[json!("body")]).unwrap_or(loc);
                loc.len() == 3
                    && loc[0] == "input"
                    && loc[2] == "role"
                    && loc[1]
                        .as_u64()
                        .and_then(|index| usize::try_from(index).ok())
                        .is_some_and(|index| {
                            body["input"]
                                .get(index)
                                .is_some_and(|input| input["role"] == "system")
                        })
                    && item["input"] == "system"
                    && item["type"] == "literal_error"
                    && item["msg"]
                        .as_str()
                        .is_some_and(|msg| msg.contains("developer"))
            })
        });
    if !explicit {
        return None;
    }
    convert(body)
}

pub(super) fn convert(body: &Value) -> Option<Value> {
    if !has_system(body) {
        return None;
    }
    let mut repaired = body.clone();
    for item in repaired["input"].as_array_mut()? {
        if item["role"] == "system" {
            item["role"] = json!("developer");
        }
    }
    Some(repaired)
}
