use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, fs, path::Path};
use toml_edit::DocumentMut;

pub const MANAGED_KEYS: [&str; 3] = [
    "model_catalog_json",
    "model_auto_compact_token_limit",
    "model_context_window",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCompactionRule {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact_token_limit: Option<i64>,
    #[serde(default)]
    pub context_window: Option<i64>,
}

pub(crate) fn revision(rules: &[ModelCompactionRule]) -> String {
    // Rule order does not affect budgets. Hash a canonical, length-delimited
    // snapshot only when publishing configuration, never on a polling request.
    let mut ordered: Vec<_> = rules.iter().filter(|rule| rule.auto_compact_token_limit.is_some() || rule.context_window.is_some()).collect();
    ordered.sort_unstable_by(|left, right| left.model.cmp(&right.model));
    let mut hash = Sha256::new();
    hash.update(b"codexpress-omp-budgets-v2");
    for rule in ordered {
        hash.update((rule.model.len() as u64).to_le_bytes());
        hash.update(rule.model.as_bytes());
        hash.update([u8::from(rule.auto_compact_token_limit.is_some())]);
        if let Some(limit) = rule.auto_compact_token_limit {
            hash.update(limit.to_le_bytes());
        }
        hash.update([u8::from(rule.context_window.is_some())]);
        if let Some(window) = rule.context_window {
            hash.update(window.to_le_bytes());
        }
    }
    format!("{:x}", hash.finalize())
}

pub fn validate(rules: &[ModelCompactionRule]) -> Result<()> {
    if rules.len() > 200 {
        bail!("最多支持 200 条模型压缩规则");
    }
    let mut names = HashSet::new();
    for rule in rules {
        if rule.model.is_empty()
            || rule.model.len() > 256
            || rule.model.chars().any(char::is_whitespace)
            || !names.insert(&rule.model)
        {
            bail!("请填写不含空白且不重复的模型 ID");
        }
        validate_budget(&rule.model, rule.context_window, rule.auto_compact_token_limit)?;
    }
    Ok(())
}

pub(crate) fn validate_budget(model: &str, window: Option<i64>, limit: Option<i64>) -> Result<()> {
    if limit.is_some_and(|n| !(1..=1_000_000_000).contains(&n))
        || window.is_some_and(|n| !(2..=1_000_000_000).contains(&n))
    {
        bail!("压缩阈值须为正整数，上下文窗口至少为 2 Token，最大支持十亿 Token");
    }
    if let (Some(limit), Some(window)) = (limit, window) {
        if limit > window.saturating_mul(9) / 10 {
            bail!("模型 {} 的压缩阈值不能超过最大输入的 90%", model);
        }
    }
    Ok(())
}

fn load_catalog(path: &Path) -> Result<Value> {
    let data = fs::read(path).context("无法读取所选模型目录，未更改配置")?;
    if data.len() > 16 * 1024 * 1024 {
        bail!("模型目录过大，未更改配置");
    }
    serde_json::from_slice(&data).context("模型目录不是有效的 JSON，未更改配置")
}

/// Only invoked by an explicit preview/apply against the selected CODEX_HOME.
pub fn catalog(
    home: &Path,
    original: &DocumentMut,
    rules: &[ModelCompactionRule],
) -> Result<Value> {
    catalog_with_additions(home, original, rules, None)
}

pub fn catalog_with_additions(
    home: &Path,
    original: &DocumentMut,
    rules: &[ModelCompactionRule],
    additions: Option<&Value>,
) -> Result<Value> {
    validate(rules)?;
    let bundled = original.get("model_catalog_json").is_none() && !home.join("models_cache.json").exists();
    let mut catalog = if let Some(path) = original.get("model_catalog_json") {
        let path = path
            .as_str()
            .context("model_catalog_json 必须是路径字符串")?;
        let path = Path::new(path);
        load_catalog(&if path.is_absolute() {
            path.to_path_buf()
        } else {
            home.join(path)
        })?
    } else if home.join("models_cache.json").exists() {
        load_catalog(&home.join("models_cache.json"))?
    } else {
        bundled_catalog()?
    };
    let managed_catalog = original.get("model_catalog_json").and_then(|value| value.as_str()).is_some_and(|path| {
        let path = Path::new(path);
        path.parent().and_then(Path::file_name).is_some_and(|name| name == "model-catalogs")
            || path.file_name().is_some_and(|name| name == ".codexpress-model-catalog.json")
    });
    if managed_catalog {
        // Older generated catalogs did not distinguish original defaults from
        // effective overrides. Recover only budget defaults from trusted local
        // source metadata, retaining the user's instructions and capabilities.
        let defaults = bundled_catalog()?;
        if let Some(models) = catalog["models"].as_array_mut() {
            for model in models {
                let bundled = defaults["models"].as_array().and_then(|models| models.iter().find(|entry| entry["slug"] == model["slug"]));
                let discovered = additions.and_then(|catalog| catalog["models"].as_array()).and_then(|models| models.iter().find(|entry| entry["slug"] == model["slug"]));
                if bundled.is_some() || discovered.is_some() {
                    for (target, field) in [("default_context_window", "context_window"),
                        ("default_max_context_window", "max_context_window"),
                        ("default_auto_compact_token_limit", "auto_compact_token_limit")] {
                        model[target] = discovered.and_then(|entry| entry.get(field))
                            .or_else(|| bundled.and_then(|entry| entry.get(field))).cloned().unwrap_or(Value::Null);
                    }
                }
            }
        }
    }
    if let Some(additions) = additions.and_then(|value| value["models"].as_array()) {
        if let Some(models) = catalog["models"].as_array_mut() {
            let mut names: HashSet<String> = models
                .iter()
                .filter_map(|model| model["slug"].as_str().map(str::to_owned))
                .collect();
            for model in additions {
                if let Some(slug) = model["slug"].as_str() {
                    if names.insert(slug.to_owned()) {
                        models.push(model.clone());
                    } else if bundled {
                        if let Some(existing) = models.iter_mut().find(|entry| entry["slug"] == slug).and_then(Value::as_object_mut) {
                            if let Some(fields) = model.as_object() {
                                // Refresh capabilities without replacing the bundled
                                // compatibility instructions during explicit sync.
                                existing.extend(fields.iter().filter(|(key, _)| !matches!(key.as_str(), "base_instructions" | "model_messages"))
                                    .map(|(key, value)| (key.clone(), value.clone())));
                            }
                        }
                    }
                }
            }
        }
    }
    catalog_from_value(catalog, original, rules)
}

pub fn bundled_catalog() -> Result<Value> {
    let mut catalog: Value = serde_json::from_str(include_str!("../resources/codex-models-0.162.1.json"))?;
    let supplement: Value = serde_json::from_str(include_str!("../resources/codex-model-capabilities.json"))?;
    let models = catalog["models"].as_array_mut().context("内置模型目录缺少 models")?;
    for capability in supplement["models"].as_array().into_iter().flatten() {
        let id = capability["slug"].as_str().context("内置模型能力缺少模型 ID")?;
        let index = if let Some(index) = models.iter().position(|model| model["slug"] == id) {
            index
        } else {
            models.push(metadata_for_model(models, id));
            models.len() - 1
        };
        if let (Some(model), Some(capability)) = (models[index].as_object_mut(), capability.as_object()) {
            model.extend(capability.clone());
        }
    }
    Ok(catalog)
}

/// Apply rules to supplied metadata without opening any local Codex files.
pub fn catalog_from_value(
    mut catalog: Value,
    original: &DocumentMut,
    rules: &[ModelCompactionRule],
) -> Result<Value> {
    validate(rules)?;
    let models = catalog
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .filter(|models| !models.is_empty())
        .context("模型目录必须包含非空 models 列表")?;
    let mut names = HashSet::new();
    for model in models.iter_mut() {
        let slug = model
            .get("slug")
            .and_then(Value::as_str)
            .context("模型目录缺少模型 ID")?.to_owned();
        if !names.insert(slug.to_owned()) {
            bail!("模型目录存在重复模型 ID");
        }
        preserve_defaults(model);
        // Explicit Codex sync migrates root overrides, but never increases the
        // catalog's original default capacity.
        let root_rule = ModelCompactionRule {
            model: slug,
            context_window: original.get("model_context_window").and_then(|v| v.as_integer()),
            auto_compact_token_limit: original.get("model_auto_compact_token_limit").and_then(|v| v.as_integer()),
        };
        apply_budget(model, Some(&root_rule));
    }
    for rule in rules {
        if rule.context_window.is_none() && rule.auto_compact_token_limit.is_none() { continue; }
        let index = if let Some(index) = models.iter().position(|m| m["slug"] == rule.model) {
            index
        } else {
            // Preserve Codex's existing model-family metadata for versioned/namespaced IDs.
            let metadata = metadata_for_model(models, &rule.model);
            models.push(metadata);
            models.len() - 1
        };
        let model = &mut models[index];
        apply_budget(model, Some(rule));
    }
    // Older Codex clients deserialize every catalog entry and require the
    // pre-model_messages instruction field. Retain modern metadata verbatim;
    // supply only the missing legacy representation, including unruled models.
    for model in models.iter_mut() {
        if model
            .get("base_instructions")
            .and_then(Value::as_str)
            .is_none()
        {
            let instructions = model
                .pointer("/model_messages/instructions_template")
                .and_then(Value::as_str)
                .unwrap_or(include_str!("../resources/codex-prompt-0.162.1.md"));
            model["base_instructions"] = json!(instructions);
        }
    }
    // Cache envelopes may contain authentication/cache identity; export model metadata only.
    Ok(json!({"models":models}))
}

pub(crate) fn preserve_defaults(model: &mut Value) {
    for (default, field) in [
        ("default_context_window", "context_window"),
        ("default_max_context_window", "max_context_window"),
        ("default_auto_compact_token_limit", "auto_compact_token_limit"),
    ] {
        if model.get(default).is_none() {
            model[default] = model.get(field).cloned().unwrap_or(Value::Null);
        }
    }
}

pub(crate) fn apply_budget(model: &mut Value, rule: Option<&ModelCompactionRule>) {
    preserve_defaults(model);
    let default_window = model["default_max_context_window"].as_i64()
        .or_else(|| model["default_context_window"].as_i64());
    let window = match (rule.and_then(|rule| rule.context_window), default_window) {
        (Some(window), Some(default)) => Some(window.min(default)),
        (window, default) => window.or(default),
    };
    if let Some(window) = window {
        model["context_window"] = json!(window);
        // Explicit reductions cannot exceed the source's original maximum.
        model["max_context_window"] = json!(window);
    }
    let limit = rule.and_then(|rule| rule.auto_compact_token_limit)
        .or_else(|| model["default_auto_compact_token_limit"].as_i64());
    if let Some(limit) = limit {
        model["auto_compact_token_limit"] = json!(window.map_or(limit, |window| limit.min(window.saturating_mul(9) / 10)));
    } else {
        model.as_object_mut().map(|model| model.remove("auto_compact_token_limit"));
    }
}

pub(crate) fn metadata_for_model(models: &[Value], id: &str) -> Value {
    let matching = |name: &str| {
        models.iter().filter(|model| {
            model["slug"].as_str().is_some_and(|slug| name.starts_with(slug))
        }).max_by_key(|model| model["slug"].as_str().map(str::len).unwrap_or(0)).cloned()
    };
    let inherited = matching(id).or_else(|| id.split_once('/').and_then(|(_, suffix)| matching(suffix)));
    let mut metadata = inherited.unwrap_or_else(|| json!({
        "slug":id,"display_name":id,"description":null,
        "supported_reasoning_levels":[],"shell_type":"unified_exec",
        "visibility":"list","supported_in_api":true,"priority":99,
        "availability_nux":null,"upgrade":null,"support_verbosity":false,
        "default_verbosity":null,"apply_patch_tool_type":null,
        "truncation_policy":{"mode":"bytes","limit":10000},
        "context_window":272000,"max_context_window":272000,
        "effective_context_window_percent":95,"experimental_supported_tools":[],
        "model_messages":{"instructions_template":include_str!("../resources/codex-prompt-0.162.1.md")}
    }));
    metadata["slug"] = json!(id);
    metadata["display_name"] = json!(id);
    metadata
}
