//! Leaf-only management of the stored OMP 18.15 configuration.
//! Native references: config/compaction-threshold.ts, session/context-settings.ts,
//! and config/models-config-schema-bundle.ts at oh-my-pi v18.15.0.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Map, Value as Json};
use serde_yaml::Value;
use std::collections::BTreeSet;

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
const CONFIG_KEYS: &[&str] = &[
    "enabled",
    "midTurnEnabled",
    "methodOrder",
    "thresholdTokens",
    "thresholdPercent",
    "modelThresholdsEnabled",
    "reserveTokens",
    "keepRecentTokens",
    "autoContinue",
    "asyncEnabled",
    "idleEnabled",
    "idleThresholdTokens",
    "idleTimeoutSeconds",
    "remoteStreamingV2Enabled",
    "v2RetainedMessageBudget",
    "handoffSaveToDisk",
    "supersedeReads",
    "dropUseless",
    "experimentalContextManagement",
];
const MODEL_KEYS: &[&str] = &[
    "contextWindow",
    "maxContextWindow",
    "maxTokens",
    "omitMaxOutputTokens",
    "compactionModel",
];
const REMOTE_KEYS: &[&str] = &[
    "enabled",
    "api",
    "endpoint",
    "model",
    "v2Endpoint",
    "streamingEndpoint",
    "v2StreamingEnabled",
];

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Edit {
    pub file: String,
    pub path: Vec<String>,
    // serde's ordinary Option deserializer conflates explicit null and absence.
    #[serde(default, deserialize_with = "present_value")]
    pub value: Option<Json>,
    #[serde(default)]
    pub remove: bool,
}

fn present_value<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Json>, D::Error> {
    Json::deserialize(deserializer).map(Some)
}

#[derive(Clone, Copy)]
enum Leaf<'a> {
    Config(&'a str),
    Threshold,
    Model(&'a str),
    Remote(&'a str),
}

fn safe_integer(value: &Json, positive: bool) -> bool {
    value.as_f64().is_some_and(|number| {
        number.is_finite()
            && number.fract() == 0.0
            && number.abs() <= MAX_SAFE_INTEGER
            && (!positive || number > 0.0)
    })
}

fn nonempty(value: &Json) -> bool {
    value.as_str().is_some_and(|text| !text.is_empty())
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn selector_valid(selector: &str) -> bool {
    let Some(slash) = selector.find('/') else {
        return false;
    };
    slash > 0
        && slash < selector.len() - 1
        && selector
            .find('*')
            .is_none_or(|star| star == selector.len() - 1)
}

fn threshold_valid(value: &Json) -> bool {
    if value.is_null() || safe_integer(value, true) {
        return true;
    }
    let Some(text) = value.as_str().map(str::trim) else {
        return false;
    };
    if let Some(fixed) = text.strip_prefix('f').or_else(|| text.strip_prefix('F')) {
        return digits(fixed)
            && fixed.parse::<f64>().is_ok_and(|number| {
                number > 0.0 && number <= MAX_SAFE_INTEGER && number.fract() == 0.0
            });
    }
    let Some(percent) = text.strip_suffix('%') else {
        return false;
    };
    let valid_decimal = match percent.split_once('.') {
        Some((whole, fraction)) => digits(whole) && digits(fraction),
        None => digits(percent),
    };
    valid_decimal
        && percent
            .parse::<f64>()
            .is_ok_and(|number| number > 0.0 && number <= 100.0)
}

fn config_valid(field: &str, value: &Json) -> bool {
    match field {
        "enabled"
        | "midTurnEnabled"
        | "modelThresholdsEnabled"
        | "autoContinue"
        | "asyncEnabled"
        | "idleEnabled"
        | "remoteStreamingV2Enabled"
        | "handoffSaveToDisk"
        | "supersedeReads"
        | "dropUseless"
        | "experimentalContextManagement" => value.is_boolean(),
        "methodOrder" => value.as_array().is_some_and(|methods| {
            let mut seen = BTreeSet::new();
            methods.iter().all(|method| {
                method.as_str().is_some_and(|method| {
                    matches!(
                        method,
                        "remote" | "snapcompact" | "handoff" | "shake" | "soft"
                    ) && seen.insert(method)
                })
            })
        }),
        "thresholdTokens" => safe_integer(value, false),
        // Native global percentages are finite numbers, including nonpositive sentinels.
        // The runtime clamps positive values to 1–99%; it does not impose Codex's 90% rule.
        "thresholdPercent" => value
            .as_f64()
            .is_some_and(|number| number.is_finite() && number.abs() <= MAX_SAFE_INTEGER),
        "reserveTokens"
        | "keepRecentTokens"
        | "idleThresholdTokens"
        | "idleTimeoutSeconds"
        | "v2RetainedMessageBudget" => value
            .as_f64()
            .is_some_and(|number| number.is_finite() && number.abs() <= MAX_SAFE_INTEGER),
        _ => false,
    }
}

fn model_valid(field: &str, value: &Json) -> bool {
    match field {
        "contextWindow" | "maxContextWindow" | "maxTokens" => safe_integer(value, true),
        "omitMaxOutputTokens" => value.is_boolean(),
        "compactionModel" => nonempty(value),
        _ => false,
    }
}

fn remote_valid(field: &str, value: &Json) -> bool {
    match field {
        "enabled" | "v2StreamingEnabled" => value.is_boolean(),
        // Only the native Responses family can replay compaction through this gateway.
        "api" => value.as_str().is_some_and(|api| {
            matches!(
                api,
                "openai-responses" | "openai-codex-responses" | "azure-openai-responses"
            )
        }),
        "endpoint" | "model" | "v2Endpoint" | "streamingEndpoint" => nonempty(value),
        _ => false,
    }
}

fn valid(leaf: Leaf<'_>, value: &Json) -> bool {
    match leaf {
        Leaf::Config(field) => config_valid(field, value),
        Leaf::Threshold => threshold_valid(value),
        Leaf::Model(field) => model_valid(field, value),
        Leaf::Remote(field) => remote_valid(field, value),
    }
}

fn provider(models: &Value) -> Option<&Value> {
    super::at(models, &["providers", "codexpress"])
}

fn base_model<'a>(models: &'a Value, id: &str) -> Option<&'a Value> {
    super::at(provider(models)?, &["models"])?
        .as_sequence()?
        .iter()
        .find(|model| super::at(model, &["id"]).and_then(Value::as_str) == Some(id))
}

fn model_ids(models: &Value) -> Vec<&str> {
    let Some(provider) = provider(models) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    let mut seen = BTreeSet::new();
    if let Some(rows) = super::at(provider, &["models"]).and_then(Value::as_sequence) {
        for row in rows {
            if let Some(id) = super::at(row, &["id"]).and_then(Value::as_str) {
                if !id.is_empty() && seen.insert(id) {
                    ids.push(id);
                }
            }
        }
    }
    if let Some(overrides) = super::at(provider, &["modelOverrides"]).and_then(Value::as_mapping) {
        for key in overrides.keys() {
            if let Some(id) = key.as_str() {
                if !id.is_empty() && seen.insert(id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

fn leaf<'a>(edit: &'a Edit, ids: &[&str]) -> Result<Leaf<'a>> {
    let path = &edit.path;
    match (edit.file.as_str(), path.len()) {
        ("config", 2) if path[0] == "compaction" && CONFIG_KEYS.contains(&path[1].as_str()) => {
            Ok(Leaf::Config(&path[1]))
        }
        ("config", 3)
            if path[0] == "compaction"
                && path[1] == "modelThresholds"
                && selector_valid(&path[2]) =>
        {
            Ok(Leaf::Threshold)
        }
        ("models", 4)
            if path[0] == "providers"
                && path[1] == "codexpress"
                && path[2] == "remoteCompaction"
                && REMOTE_KEYS.contains(&path[3].as_str()) =>
        {
            Ok(Leaf::Remote(&path[3]))
        }
        ("models", 5)
            if path[0] == "providers"
                && path[1] == "codexpress"
                && path[2] == "modelOverrides"
                && ids.contains(&path[3].as_str())
                && MODEL_KEYS.contains(&path[4].as_str()) =>
        {
            Ok(Leaf::Model(&path[4]))
        }
        ("models", 6)
            if path[0] == "providers"
                && path[1] == "codexpress"
                && path[2] == "modelOverrides"
                && ids.contains(&path[3].as_str())
                && path[4] == "remoteCompaction"
                && REMOTE_KEYS.contains(&path[5].as_str()) =>
        {
            Ok(Leaf::Remote(&path[5]))
        }
        _ => bail!("不支持的 OMP 配置字段，未修改文件"),
    }
}

fn check_parents(document: &Value, path: &[String]) -> Result<()> {
    let mut current = document;
    for component in &path[..path.len() - 1] {
        if current.is_null() {
            return Ok(());
        }
        let mapping = current
            .as_mapping()
            .context("OMP 配置字段必须为对象，未修改文件")?;
        let Some(next) = mapping.get(Value::String(component.clone())) else {
            return Ok(());
        };
        current = next;
    }
    if !current.is_null() && !current.is_mapping() {
        bail!("OMP 配置字段必须为对象，未修改文件");
    }
    Ok(())
}

fn final_capacity(models: &Value, edits: &[Edit], id: &str, field: &str) -> Result<Option<f64>> {
    let base = base_model(models, id).and_then(|model| super::at(model, &[field]));
    let stored = super::at(
        models,
        &["providers", "codexpress", "modelOverrides", id, field],
    );
    let edited = edits.iter().find(|edit| {
        edit.file == "models"
            && edit.path.len() == 5
            && edit.path[2] == "modelOverrides"
            && edit.path[3] == id
            && edit.path[4] == field
    });
    let number = if let Some(edit) = edited.filter(|edit| !edit.remove) {
        edit.value.as_ref().and_then(Json::as_f64)
    } else {
        let inherited = if edited.is_some() {
            base
        } else {
            stored.or(base)
        };
        if let Some(value) = inherited {
            let json = serde_json::to_value(value).context("无法读取 OMP 模型容量")?;
            if !safe_integer(&json, true) {
                bail!("OMP 模型容量必须为正安全整数，未修改文件");
            }
            json.as_f64()
        } else {
            None
        }
    };
    Ok(number)
}

pub(super) fn edits(payload: &Json, settings: &Value, models: &Value) -> Result<Vec<Edit>> {
    let raw = payload.get("edits").context("缺少 OMP 配置修改列表")?;
    let edits = Vec::<Edit>::deserialize(raw)
        .map_err(|_| anyhow::anyhow!("OMP 配置修改列表无效，未修改文件"))?;
    let ids = model_ids(models);
    let mut seen = BTreeSet::new();
    let mut capacity_models = BTreeSet::new();
    for edit in &edits {
        let leaf = leaf(edit, &ids)?;
        if !seen.insert((edit.file.as_str(), edit.path.as_slice())) {
            bail!("OMP 配置字段重复，未修改文件");
        }
        match (&edit.value, edit.remove) {
            (None, true) => {}
            (Some(value), false) if valid(leaf, value) => {}
            _ => bail!("OMP 配置值无效：请提供 value 或 remove:true，未修改文件"),
        }
        check_parents(
            if edit.file == "config" {
                settings
            } else {
                models
            },
            &edit.path,
        )?;
        if matches!(leaf, Leaf::Model("contextWindow" | "maxContextWindow")) {
            capacity_models.insert(edit.path[3].as_str());
        }
    }
    // Validate the final batch, not intermediate edits: both sides may change together.
    for id in capacity_models {
        if let (Some(context), Some(maximum)) = (
            final_capacity(models, &edits, id, "contextWindow")?,
            final_capacity(models, &edits, id, "maxContextWindow")?,
        ) {
            if maximum < context {
                bail!("OMP maxContextWindow 不能小于继承后的 contextWindow，未修改文件");
            }
        }
    }
    Ok(edits)
}

fn safe_fields(
    value: Option<&Value>,
    fields: &[&str],
    kind: fn(&str, &Json) -> bool,
) -> Map<String, Json> {
    let mut result = Map::new();
    let Some(value) = value else { return result };
    for field in fields {
        if let Some(stored) = super::at(value, &[field]) {
            if let Ok(json) = serde_json::to_value(stored) {
                if kind(field, &json) {
                    result.insert((*field).into(), json);
                }
            }
        }
    }
    result
}

fn model_view(value: Option<&Value>) -> Json {
    let mut result = safe_fields(value, MODEL_KEYS, model_valid);
    let remote = value.and_then(|value| super::at(value, &["remoteCompaction"]));
    if remote.is_some_and(Value::is_mapping) {
        result.insert(
            "remoteCompaction".into(),
            Json::Object(safe_fields(remote, REMOTE_KEYS, remote_valid)),
        );
    }
    Json::Object(result)
}

pub(super) fn configuration(settings: &Value, models: &Value) -> Result<Json> {
    let stored = super::at(settings, &["compaction"]);
    let mut compaction = safe_fields(stored, CONFIG_KEYS, config_valid);
    if let Some(thresholds) = stored
        .and_then(|value| super::at(value, &["modelThresholds"]))
        .and_then(Value::as_mapping)
    {
        let mut safe = Map::new();
        for (key, value) in thresholds {
            if let Some(selector) = key.as_str().filter(|selector| selector_valid(selector)) {
                if let Ok(value) = serde_json::to_value(value) {
                    if threshold_valid(&value) {
                        safe.insert(selector.into(), value);
                    }
                }
            }
        }
        compaction.insert("modelThresholds".into(), Json::Object(safe));
    }
    let remote = provider(models).and_then(|value| super::at(value, &["remoteCompaction"]));
    let rows: Vec<Json> = model_ids(models).into_iter().map(|id| {
        json!({
            "id": id,
            "base": model_view(base_model(models, id)),
            "overrides": model_view(super::at(models, &["providers", "codexpress", "modelOverrides", id])),
        })
    }).collect();
    Ok(json!({
        "compaction": compaction,
        "providerRemoteCompaction": safe_fields(remote, REMOTE_KEYS, remote_valid),
        "models": rows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(text: &str) -> Value {
        serde_yaml::from_str(text).unwrap()
    }

    #[test]
    fn threshold_forms_null_and_removal_are_native_and_distinct() {
        let settings = yaml("compaction: {modelThresholds: {codexpress/old: legacy}}\n");
        let models = yaml("{}\n");
        for value in [
            json!(90000),
            json!("f95000"),
            json!(" F95000 "),
            json!("0.5%"),
            json!("100%"),
            Json::Null,
        ] {
            let result = edits(&json!({"edits":[{"file":"config","path":["compaction","modelThresholds","codexpress/*"],"value":value}]}), &settings, &models).unwrap();
            assert_eq!(result[0].value.as_ref(), Some(&value));
            assert!(!result[0].remove);
        }
        let removed = edits(&json!({"edits":[{"file":"config","path":["compaction","modelThresholds","codexpress/*"],"remove":true}]}), &settings, &models).unwrap();
        assert!(removed[0].value.is_none());
        for value in [
            json!(0),
            json!(1.5),
            json!("f90k"),
            json!("f0"),
            json!("0%"),
            json!("101%"),
            json!("1e1%"),
            json!(9_007_199_254_740_992_u64),
        ] {
            assert!(!threshold_valid(&value));
        }
        for selector in [
            "",
            "codexpress/",
            "/model",
            "cod*express/model",
            "codexpress/**",
        ] {
            assert!(!selector_valid(selector));
        }
    }

    #[test]
    fn capacities_validate_inheritance_removal_and_the_whole_batch() {
        let settings = yaml("{}\n");
        let models = yaml("providers:\n  codexpress:\n    models: [{id: m, contextWindow: 100, maxContextWindow: 200}]\n    modelOverrides: {m: {contextWindow: 150, maxContextWindow: 300}}\n");
        let change = |field: &str, value: Json| json!({"file":"models","path":["providers","codexpress","modelOverrides","m",field],"value":value});
        let remove = |field: &str| json!({"file":"models","path":["providers","codexpress","modelOverrides","m",field],"remove":true});
        assert!(edits(
            &json!({"edits":[change("contextWindow", json!(400))]}),
            &settings,
            &models
        )
        .is_err());
        assert!(edits(&json!({"edits":[change("contextWindow", json!(400)), change("maxContextWindow", json!(500))]}), &settings, &models).is_ok());
        assert!(edits(
            &json!({"edits":[change("contextWindow", json!(250)), remove("maxContextWindow")]}),
            &settings,
            &models
        )
        .is_err());
        assert!(edits(
            &json!({"edits":[remove("contextWindow"), change("maxContextWindow", json!(100))]}),
            &settings,
            &models
        )
        .is_ok());
        // Output capacity is independent of the input context window.
        assert!(edits(
            &json!({"edits":[change("maxTokens", json!(9_007_199_254_740_991_u64))]}),
            &settings,
            &models
        )
        .is_ok());
        assert!(edits(
            &json!({"edits":[change("contextWindow", json!(9_007_199_254_740_992_u64))]}),
            &settings,
            &models
        )
        .is_err());
        assert!(config_valid("thresholdTokens", &json!(-1)));
        assert!(config_valid("thresholdPercent", &json!(1000.5)));
    }

    #[test]
    fn unsafe_paths_duplicate_or_ambiguous_edits_fail() {
        let empty = yaml("{}\n");
        let change = json!({"file":"config","path":["compaction","enabled"],"value":true});
        assert!(edits(&json!({"edits":[change.clone(), change]}), &empty, &empty).is_err());
        for edit in [
            json!({"file":"models","path":["providers","codexpress"],"value":{}}),
            json!({"file":"models","path":["providers","codexpress","apiKey"],"value":"secret"}),
            json!({"file":"config","path":["compaction","protectTokens"],"value":100}),
            json!({"file":"config","path":["compaction","enabled"]}),
            json!({"file":"config","path":["compaction","modelThresholds","codexpress/m"],"value":null,"remove":true}),
            json!({"file":"models","path":["providers","codexpress","remoteCompaction","api"],"value":"responses"}),
        ] {
            assert!(edits(&json!({"edits":[edit]}), &empty, &empty).is_err());
        }
    }

    #[test]
    fn configuration_excludes_keys_headers_and_unknown_nested_values() {
        let settings = yaml("apiKey: SECRET\ncompaction:\n  enabled: true\n  future: SECRET\n  modelThresholds: {codexpress/*: null}\n");
        let models = yaml("providers:\n  codexpress:\n    apiKey: SECRET\n    headers: {Authorization: SECRET}\n    remoteCompaction: {enabled: true, apiKey: SECRET}\n    models:\n      - {id: m, contextWindow: 100, headers: {Authorization: SECRET}, remoteCompaction: {v2StreamingEnabled: true, headers: SECRET}}\n    modelOverrides:\n      m: {maxTokens: 500, headers: {Authorization: SECRET}}\n      extra: {compactionModel: summary, future: SECRET}\n  unrelated:\n    models: [{id: secret-model}]\n");
        let view = configuration(&settings, &models).unwrap();
        assert_eq!(
            view,
            json!({
                "compaction":{"enabled":true,"modelThresholds":{"codexpress/*":null}},
                "providerRemoteCompaction":{"enabled":true},
                "models":[
                    {"id":"m","base":{"contextWindow":100,"remoteCompaction":{"v2StreamingEnabled":true}},"overrides":{"maxTokens":500}},
                    {"id":"extra","base":{},"overrides":{"compactionModel":"summary"}}
                ]
            })
        );
        assert!(!view.to_string().contains("SECRET"));
        assert!(view["compaction"].get("reserveTokens").is_none());
    }
}
