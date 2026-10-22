use super::{snapshot_load, Snapshot};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use toml_edit::{value, DocumentMut, Item, Table};

#[derive(Serialize, Deserialize)]
pub(super) struct ProviderAliases {
    before: String,
    pub(super) applied: String,
    #[serde(default)]
    origins: Vec<ProviderOrigin>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderOrigin {
    pub provider_id: String,
    pub source_id: String,
    pub source_identity: String,
    pub base_url: String,
    #[serde(default)]
    pub(super) credential_digest: String,
}

pub fn normalized_base(raw: &str) -> String {
    raw.trim_end_matches('/')
        .strip_suffix("/responses")
        .unwrap_or(raw.trim_end_matches('/'))
        .to_owned()
}

pub fn confirmed_origin(state_dir: &Path, credential: &str) -> Result<Option<ProviderOrigin>> {
    let digest = format!("{:x}", Sha256::digest(credential.as_bytes()));
    Ok(snapshot_load(state_dir)?
        .filter(|s| s.applied_provider == "codexpress")
        .and_then(|s| s.gateway_aliases)
        .and_then(|a| {
            a.origins
                .into_iter()
                .find(|origin| origin.credential_digest == digest)
        }))
}

fn origin_credential(key: &str, origin: &ProviderOrigin) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                key,
                &origin.provider_id,
                &origin.source_id,
                &origin.source_identity
            ))
            .expect("strings")
        )
    )
}

pub(super) fn alias_tables(doc: &DocumentMut) -> Option<&dyn toml_edit::TableLike> {
    doc.get("model_providers").and_then(Item::as_table_like)
}

pub(super) fn verify_gateway_aliases(doc: &DocumentMut, snapshot: &Snapshot) -> Result<()> {
    if let Some(aliases) = &snapshot.gateway_aliases {
        let applied = aliases.applied.parse::<DocumentMut>()?;
        if let Some(providers) = alias_tables(&applied) {
            for (id, expected) in providers.iter() {
                if alias_tables(doc)
                    .and_then(|p| p.get(id))
                    .map(ToString::to_string)
                    != Some(expected.to_string())
                {
                    bail!("已有对话的供应商连接已被其他程序修改，未覆盖；请先检查供应商配置。");
                }
            }
        }
    }
    Ok(())
}

pub(super) fn restore_gateway_aliases(doc: &mut DocumentMut, snapshot: &Snapshot) -> Result<()> {
    if let Some(aliases) = &snapshot.gateway_aliases {
        let before = aliases.before.parse::<DocumentMut>()?;
        if let Some(providers) = alias_tables(&before) {
            for (id, item) in providers.iter() {
                doc["model_providers"][id] = item.clone();
            }
        }
    }
    Ok(())
}

/// Resume can retain a custom provider ID. Redirect its current definition,
/// keeping the conversation itself untouched and the original table restorable.
pub(super) fn prepare_gateway_aliases(
    doc: &mut DocumentMut,
    port: u16,
    key: &str,
    origins: &[ProviderOrigin],
) -> Result<Option<ProviderAliases>> {
    let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
    else {
        return Ok(None);
    };
    let mut before = DocumentMut::new();
    let mut applied = DocumentMut::new();
    before["model_providers"] = Item::Table(Table::new());
    applied["model_providers"] = Item::Table(Table::new());
    let mut confirmed = Vec::new();
    for (id, item) in providers.iter_mut() {
        let id = id.get();
        if matches!(id, "codexpress" | "openai" | "ollama" | "lmstudio") {
            continue;
        }
        let Some(origin) = origins.iter().find(|origin| origin.provider_id == id) else {
            continue;
        };
        if item
            .get("base_url")
            .and_then(Item::as_str)
            .map(normalized_base)
            .as_deref()
            != Some(origin.base_url.as_str())
        {
            bail!("已有供应商与所选来源的地址不一致，未接入；请重新确认原来源。");
        }
        let credential = origin_credential(key, origin);
        before["model_providers"][id] = item.clone();
        *item = Item::Table(provider_table(port, &credential));
        applied["model_providers"][id] = item.clone();
        let mut origin = origin.clone();
        origin.credential_digest = format!("{:x}", Sha256::digest(credential.as_bytes()));
        confirmed.push(origin);
    }
    if confirmed.len() != origins.len() {
        bail!("原供应商不存在或不支持接入，请重新选择已有对话来源");
    }
    Ok(alias_tables(&before)
        .filter(|p| !p.is_empty())
        .map(|_| ProviderAliases {
            before: before.to_string(),
            applied: applied.to_string(),
            origins: confirmed,
        }))
}

pub(super) fn provider_table(port: u16, key: &str) -> Table {
    let mut table = Table::new();
    table["name"] = value("Codexpress");
    table["base_url"] = value(format!("http://127.0.0.1:{port}/v1"));
    table["wire_api"] = value("responses");
    table["requires_openai_auth"] = value(false);
    table["experimental_bearer_token"] = value(key);
    table["supports_websockets"] = value(false);
    table
}
