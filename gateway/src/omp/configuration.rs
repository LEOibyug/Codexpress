//! Native preferences are separate from connection ownership and patched by leaf.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Original {
    file: String,
    path: Vec<String>,
    before: Option<String>,
    applied: Option<String>,
}

fn stored(value: Option<&Value>) -> Result<Option<String>> {
    value
        .map(serde_yaml::to_string)
        .transpose()
        .map_err(Into::into)
}

fn decoded(value: &Option<String>) -> Result<Option<Value>> {
    value
        .as_deref()
        .map(serde_yaml::from_str)
        .transpose()
        .map_err(|_| anyhow::anyhow!("OMP 压缩配置快照损坏"))
}

pub(super) fn revision(models: &Document, settings: &Document) -> String {
    let mut hash = Sha256::new();
    for document in [models, settings] {
        let path = document.path.to_string_lossy();
        hash.update((path.len() as u64).to_le_bytes());
        hash.update(path.as_bytes());
        hash.update([u8::from(document.bytes.is_some())]);
        if let Some(bytes) = &document.bytes {
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
    }
    format!("{:x}", hash.finalize())
}

pub(super) fn migrate(snapshot: &mut Snapshot) -> Result<()> {
    if snapshot.version == 1 {
        let models = parse(&snapshot.models.applied)?;
        let settings = parse(&snapshot.settings.applied)?;
        snapshot.added_models = at(&models, &["providers", "codexpress", "models"])
            .and_then(Value::as_sequence)
            .cloned()
            .unwrap_or_default();
        for (selector, before) in std::mem::take(&mut snapshot.threshold_before) {
            let path = vec!["compaction".into(), "modelThresholds".into(), selector];
            let parts: Vec<_> = path.iter().map(String::as_str).collect();
            snapshot.native_before.push(Original {
                file: "config".into(),
                before,
                applied: stored(at(&settings, &parts))?,
                path,
            });
        }
        snapshot.version = 2;
    }
    Ok(())
}

pub(super) fn merge_models(
    models: &mut Value,
    generated: Value,
    added: &mut Vec<Value>,
) -> Result<()> {
    let provider = provider_mut(models)?;
    let rows = provider
        .entry(key("models"))
        .or_insert_with(|| Value::Sequence(Vec::new()));
    if rows.is_null() {
        *rows = Value::Sequence(Vec::new());
    }
    let rows = rows.as_sequence_mut().context("OMP 模型列表必须为数组")?;
    let ids: BTreeSet<String> = rows
        .iter()
        .filter_map(|row| at(row, &["id"]).and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    let Value::Sequence(generated) = generated else {
        bail!("OMP 模型目录无效")
    };
    for row in generated {
        let id = at(&row, &["id"])
            .and_then(Value::as_str)
            .context("OMP 模型 ID 无效")?;
        if !ids.contains(id) {
            added.push(row.clone());
            rows.push(row);
        }
    }
    Ok(())
}

fn provider_mut(models: &mut Value) -> Result<&mut Mapping> {
    models
        .as_mapping_mut()
        .and_then(|root| root.get_mut(key("providers")))
        .and_then(Value::as_mapping_mut)
        .and_then(|providers| providers.get_mut(key("codexpress")))
        .and_then(Value::as_mapping_mut)
        .context("OMP 供应商配置必须为对象")
}

pub(super) fn configure(
    state_dir: &Path,
    snapshot: &Snapshot,
    models: &Document,
    settings: &Document,
    payload: &Json,
) -> Result<Snapshot> {
    if payload["expected_revision"].as_str() != Some(revision(models, settings).as_str()) {
        bail!("OMP 配置已更改，请刷新后重新编辑；未修改文件");
    }
    let edits = preferences::edits(payload, &settings.value, &models.value)?;
    let mut updated = snapshot.clone();
    let mut models_after = models.value.clone();
    let mut settings_after = settings.value.clone();
    for edit in edits {
        let document = if edit.file == "config" {
            &mut settings_after
        } else {
            &mut models_after
        };
        let path: Vec<_> = edit.path.iter().map(String::as_str).collect();
        let original = stored(at(document, &path))?;
        let value = if edit.remove {
            None
        } else {
            Some(serde_yaml::to_value(
                edit.value.context("缺少 OMP 配置值")?,
            )?)
        };
        if at(document, &path) == value.as_ref() {
            continue;
        }
        put(document, &path, value)?;
        let applied = stored(at(document, &path))?;
        if let Some(owned) = updated
            .native_before
            .iter_mut()
            .find(|owned| owned.file == edit.file && owned.path == edit.path)
        {
            owned.applied = applied;
        } else {
            updated.native_before.push(Original {
                file: edit.file,
                path: edit.path,
                before: original,
                applied,
            });
        }
    }
    updated.models.applied =
        document::patch(models.bytes.as_deref(), &models.value, &models_after)?;
    updated.settings.applied =
        document::patch(settings.bytes.as_deref(), &settings.value, &settings_after)?;
    if read(&models.path, FILE_LIMIT)? != models.bytes
        || read(&settings.path, FILE_LIMIT)? != settings.bytes
    {
        bail!("OMP 配置在保存前发生变化，未修改文件");
    }
    write_files(&[
        (
            &models.path,
            models.bytes.as_deref(),
            Some(updated.models.applied.as_slice()),
        ),
        (
            &settings.path,
            settings.bytes.as_deref(),
            Some(updated.settings.applied.as_slice()),
        ),
    ])?;
    if let Err(error) = save_snapshot(state_dir, &updated) {
        write_files(&[
            (
                &models.path,
                Some(updated.models.applied.as_slice()),
                models.bytes.as_deref(),
            ),
            (
                &settings.path,
                Some(updated.settings.applied.as_slice()),
                settings.bytes.as_deref(),
            ),
        ])
        .context("OMP 压缩配置保存失败，且无法回滚")?;
        return Err(error);
    }
    Ok(updated)
}

pub(super) fn restore(snapshot: &Snapshot, models: &mut Value, settings: &mut Value) -> Result<()> {
    // Check every managed leaf before changing anything; unrelated preferences remain user-owned.
    let original_models = parse(snapshot.models.baseline.as_bytes())?;
    let original_settings = parse(snapshot.settings.baseline.as_bytes())?;
    for original in &snapshot.native_before {
        let document = if original.file == "config" {
            &*settings
        } else {
            &*models
        };
        let path: Vec<_> = original.path.iter().map(String::as_str).collect();
        if at(document, &path) != decoded(&original.applied)?.as_ref() {
            bail!("OMP 压缩配置已被其他程序修改；为保留更改，未恢复");
        }
    }
    for original in &snapshot.native_before {
        let document = if original.file == "config" {
            &mut *settings
        } else {
            &mut *models
        };
        let path: Vec<_> = original.path.iter().map(String::as_str).collect();
        put(document, &path, decoded(&original.before)?)?;
        let baseline = if original.file == "config" {
            &original_settings
        } else {
            &original_models
        };
        for length in (1..path.len()).rev() {
            prune(document, baseline, &[&path[..length]])?;
        }
    }
    Ok(())
}

pub(super) fn restore_provider(
    snapshot: &Snapshot,
    models: &mut Value,
    original: &Value,
) -> Result<()> {
    for field in ["api", "baseUrl", "apiKey", "auth"] {
        let path = &["providers", "codexpress", field];
        put(models, path, at(original, path).cloned())?;
    }
    let path = &["providers", "codexpress", "models"];
    let provider = provider_mut(models)?;
    if let Some(rows) = provider
        .get_mut(key("models"))
        .and_then(Value::as_sequence_mut)
    {
        // A row edited outside Codexpress is not ours to delete.
        let created: std::collections::HashSet<_> = snapshot.added_models.iter().collect();
        rows.retain(|row| !created.contains(row));
        if rows.is_empty() {
            if let Some(value) = at(original, path) {
                provider.insert(key("models"), value.clone());
            } else {
                provider.remove(key("models"));
            }
        }
    }
    prune(models, original, &[PROVIDER])
}
