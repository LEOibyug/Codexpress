use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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
    pub auto_compact_token_limit: i64,
    #[serde(default)]
    pub context_window: Option<i64>,
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
        if !(1..=1_000_000_000).contains(&rule.auto_compact_token_limit)
            || rule
                .context_window
                .is_some_and(|n| !(2..=1_000_000_000).contains(&n))
        {
            bail!("压缩阈值须为正整数，上下文窗口至少为 2 Token，最大支持十亿 Token");
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
    validate(rules)?;
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
        serde_json::from_str(include_str!("../resources/codex-models-0.162.1.json"))?
    };
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
            .context("模型目录缺少模型 ID")?;
        if !names.insert(slug.to_owned()) {
            bail!("模型目录存在重复模型 ID");
        }
        // Move existing root defaults into metadata before removing root overrides.
        if let Some(window) = original
            .get("model_context_window")
            .and_then(|v| v.as_integer())
        {
            let max = model
                .get("max_context_window")
                .and_then(Value::as_i64)
                .unwrap_or(window);
            model["context_window"] = json!(window.min(max));
        }
        if let Some(limit) = original
            .get("model_auto_compact_token_limit")
            .and_then(|v| v.as_integer())
        {
            model["auto_compact_token_limit"] = json!(limit);
        }
    }
    for rule in rules {
        let index = if let Some(index) = models.iter().position(|m| m["slug"] == rule.model) {
            index
        } else {
            // Preserve Codex's existing model-family metadata for versioned/namespaced IDs.
            let suffix = rule.model.split_once('/').map(|(_, suffix)| suffix);
            let matching = |name: &str| {
                models
                    .iter()
                    .filter(|m| {
                        m["slug"]
                            .as_str()
                            .is_some_and(|slug| name.starts_with(slug))
                    })
                    .max_by_key(|m| m["slug"].as_str().map(str::len).unwrap_or(0))
                    .cloned()
            };
            let inherited = matching(&rule.model).or_else(|| suffix.and_then(matching));
            // Match Codex's conservative fallback for third-party model IDs.
            let mut metadata = inherited.unwrap_or_else(|| json!({
                "slug":rule.model,"display_name":rule.model,"description":null,
                "supported_reasoning_levels":[],"shell_type":"unified_exec",
                "visibility":"list","supported_in_api":true,"priority":99,
                "availability_nux":null,"upgrade":null,"support_verbosity":false,
                "default_verbosity":null,"apply_patch_tool_type":null,
                "truncation_policy":{"mode":"bytes","limit":10000},
                "context_window":272000,"max_context_window":272000,
                "effective_context_window_percent":95,"experimental_supported_tools":[],
                "model_messages":{"instructions_template":include_str!("../resources/codex-prompt-0.162.1.md")}
            }));
            metadata["slug"] = json!(rule.model);
            metadata["display_name"] = json!(rule.model);
            models.push(metadata);
            models.len() - 1
        };
        let model = &mut models[index];
        if let Some(window) = rule.context_window {
            model["context_window"] = json!(window);
            // An explicit window is a user assertion about their upstream's capacity.
            model["max_context_window"] = json!(window);
        }
        let window = model
            .get("context_window")
            .and_then(Value::as_i64)
            .or_else(|| model.get("max_context_window").and_then(Value::as_i64));
        if window
            .is_some_and(|window| rule.auto_compact_token_limit > window.saturating_mul(9) / 10)
        {
            bail!(
                "模型 {} 的压缩阈值超过上下文窗口的 90%；请降低阈值或填写正确的上下文窗口",
                rule.model
            );
        }
        model["auto_compact_token_limit"] = json!(rule.auto_compact_token_limit);
    }
    // Cache envelopes may contain authentication/cache identity; export model metadata only.
    Ok(json!({"models":models}))
}
