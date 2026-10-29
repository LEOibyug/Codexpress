use super::Snapshot;
use crate::compaction::{self, ModelCompactionRule};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use toml_edit::{value, DocumentMut};

pub(super) fn compaction_text(doc: &DocumentMut) -> String {
    let mut fields = DocumentMut::new();
    for key in compaction::MANAGED_KEYS {
        if let Some(item) = doc.get(key) {
            fields[key] = item.clone();
        }
    }
    fields.to_string()
}

pub(super) fn restore_compaction(doc: &mut DocumentMut, original: &DocumentMut) {
    for key in compaction::MANAGED_KEYS {
        match original.get(key) {
            Some(item) => {
                doc[key] = item.clone();
            }
            None => {
                doc.remove(key);
            }
        }
    }
}

pub(super) fn prepare_compaction(
    doc: &mut DocumentMut,
    home: &Path,
    state_dir: &Path,
    snapshot: Option<&Snapshot>,
    rules: &[ModelCompactionRule],
) -> Result<Option<(PathBuf, Vec<u8>)>> {
    compaction::validate(rules)?;
    let original = match snapshot.filter(|s| s.applied_compaction.is_some()) {
        Some(snapshot) => snapshot
            .compaction_before
            .as_ref()
            .unwrap_or(&snapshot.before)
            .parse::<DocumentMut>()?,
        None => doc.clone(),
    };
    if rules.is_empty() {
        if snapshot.is_some_and(|s| s.applied_compaction.is_some()) {
            restore_compaction(doc, &original);
        }
        return Ok(None);
    }
    let additions = std::fs::read(state_dir.join("model-catalogs/official-latest.json"))
        .ok()
        .filter(|bytes| bytes.len() <= 4 * 1024 * 1024)
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let bytes = serde_json::to_vec_pretty(&compaction::catalog_with_additions(
        home,
        &original,
        rules,
        additions.as_ref(),
    )?)?;
    let path = state_dir
        .join("model-catalogs")
        .join(format!("{:x}.json", Sha256::digest(&bytes)));
    doc["model_catalog_json"] = value(path.to_str().context("模型目录路径无效")?);
    doc.remove("model_auto_compact_token_limit");
    doc.remove("model_context_window");
    Ok(Some((path, bytes)))
}
