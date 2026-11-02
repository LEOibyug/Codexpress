//! Native inline hook-status layout for OMP 18.15.0.
//!
//! Preset data and merge rules are pinned to:
//! https://github.com/can1357/oh-my-pi/blob/v18.15.0/packages/tui/src/status-line/presets.ts
//! https://github.com/can1357/oh-my-pi/blob/v18.15.0/packages/tui/src/status-line/schema.ts
//! https://github.com/can1357/oh-my-pi/blob/v18.15.0/packages/tui/src/status-line/component.ts
//! The native `status` segment consumes setStatus; showHookStatus controls only
//! its additional standalone row, not that segment.
use anyhow::{bail, Context, Result};
use serde_yaml::{Mapping, Value};
use std::collections::BTreeMap;

struct Preset {
    left: &'static [&'static str],
    right: &'static [&'static str],
    separator: &'static str,
    thinking: Option<bool>,
    path: Option<(u64, bool)>,
    git: Option<(bool, bool, bool)>,
    seconds: Option<bool>,
}

fn preset(name: &str) -> Result<Preset> {
    let mut value = Preset {
        left: &[],
        right: &[],
        separator: "powerline-thin",
        thinking: None,
        path: None,
        git: None,
        seconds: None,
    };
    match name {
        "default" => {
            value.left = &[
                "pi",
                "vim",
                "model",
                "mode",
                "collab",
                "stream",
                "path",
                "git",
                "pr",
                "context_pct",
                "cost",
            ];
            value.right = &["session_name"];
            value.thinking = Some(true);
            value.path = Some((40, true));
            value.git = Some((true, true, true));
        }
        "minimal" => {
            value.left = &["vim", "path", "git"];
            value.right = &["session_name", "mode", "context_pct"];
            value.separator = "slash";
            value.path = Some((30, false));
            value.git = Some((false, false, false));
        }
        "compact" => {
            value.left = &["vim", "model", "mode", "git", "pr"];
            value.right = &["session_name", "cost", "context_pct"];
            value.thinking = Some(false);
            value.git = Some((true, true, false));
        }
        "full" | "nerd" => {
            value.left = if name == "full" {
                &[
                    "pi",
                    "vim",
                    "hostname",
                    "model",
                    "mode",
                    "path",
                    "git",
                    "pr",
                    "subagents",
                ]
            } else {
                &[
                    "pi",
                    "vim",
                    "hostname",
                    "model",
                    "mode",
                    "path",
                    "git",
                    "pr",
                    "session",
                    "subagents",
                ]
            };
            value.right = if name == "full" {
                &[
                    "session_name",
                    "cache_hit",
                    "token_in",
                    "token_out",
                    "token_rate",
                    "cache_read",
                    "cost",
                    "context_pct",
                    "time_spent",
                    "time",
                ]
            } else {
                &[
                    "session_name",
                    "token_in",
                    "token_out",
                    "cache_read",
                    "cache_write",
                    "token_rate",
                    "cost",
                    "context_pct",
                    "context_total",
                    "time_spent",
                    "time",
                ]
            };
            value.separator = "powerline";
            value.thinking = Some(true);
            value.path = Some((if name == "full" { 50 } else { 60 }, false));
            value.git = Some((true, true, true));
            value.seconds = Some(name == "nerd");
        }
        "ascii" | "custom" => {
            value.left = &["vim", "model", "mode", "path", "git", "pr"];
            value.right = &["session_name", "token_total", "cost", "context_pct"];
            if name == "ascii" {
                value.separator = "ascii";
                value.thinking = Some(true);
                value.path = Some((40, false));
                value.git = Some((true, true, true));
            }
        }
        _ => bail!("OMP 状态栏预设不受支持，未修改布局"),
    }
    Ok(value)
}

fn string(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn option_map(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Mapping(
        entries
            .into_iter()
            .map(|(key, value)| (string(key), value))
            .collect(),
    )
}

fn segments(value: Option<&Value>, defaults: &[&str]) -> Result<Vec<Value>> {
    match value {
        None | Some(Value::Null) => Ok(defaults.iter().map(|value| string(value)).collect()),
        Some(Value::Sequence(values)) if values.iter().all(|value| value.as_str().is_some()) => {
            Ok(values.clone())
        }
        _ => bail!("OMP 状态栏分段必须为字符串数组，未修改布局"),
    }
}

pub(super) fn values(settings: &Value) -> Result<BTreeMap<String, Value>> {
    let root = settings
        .as_mapping()
        .context("OMP 配置必须为对象，未修改布局")?;
    let status = match root.get(string("statusLine")) {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_mapping()
                .context("OMP 状态栏配置必须为对象，未修改布局")?,
        ),
    };
    let field = |name: &str| status.and_then(|status| status.get(string(name)));
    let name = match field("preset") {
        None | Some(Value::Null) => "default",
        Some(value) => value
            .as_str()
            .context("OMP 状态栏预设必须为字符串，未修改布局")?,
    };
    let defaults = preset(name)?;
    // OMP ignores explicit segment arrays unless the selected preset is custom.
    let mut left = segments(
        if name == "custom" {
            field("leftSegments")
        } else {
            None
        },
        defaults.left,
    )?;
    let mut right = segments(
        if name == "custom" {
            field("rightSegments")
        } else {
            None
        },
        defaults.right,
    )?;
    let mut context_seen = false;
    for group in [&mut left, &mut right] {
        group.retain(|value| match value.as_str() {
            Some("status" | "context_total") => false,
            Some("context_pct") => {
                let keep = !context_seen;
                context_seen = true;
                keep
            }
            _ => true,
        });
    }
    if !context_seen {
        left.push(string("context_pct"));
    }
    left.push(string("status"));

    let mut options = Mapping::new();
    if let Some(thinking) = defaults.thinking {
        options.insert(
            string("model"),
            option_map([("showThinkingLevel", Value::Bool(thinking))]),
        );
    }
    if let Some((max_length, strip_work_prefix)) = defaults.path {
        let mut path = Mapping::new();
        path.insert(string("abbreviate"), Value::Bool(true));
        path.insert(string("maxLength"), Value::Number(max_length.into()));
        if strip_work_prefix {
            path.insert(string("stripWorkPrefix"), Value::Bool(true));
        }
        options.insert(string("path"), Value::Mapping(path));
    }
    if let Some((staged, unstaged, untracked)) = defaults.git {
        options.insert(
            string("git"),
            option_map([
                ("showBranch", Value::Bool(true)),
                ("showStaged", Value::Bool(staged)),
                ("showUnstaged", Value::Bool(unstaged)),
                ("showUntracked", Value::Bool(untracked)),
            ]),
        );
    }
    if let Some(seconds) = defaults.seconds {
        options.insert(
            string("time"),
            option_map([
                ("format", string("24h")),
                ("showSeconds", Value::Bool(seconds)),
            ]),
        );
    }
    if let Some(value) = field("segmentOptions").filter(|value| !value.is_null()) {
        let explicit = value
            .as_mapping()
            .context("OMP 状态栏分段选项必须为对象，未修改布局")?;
        for (segment, value) in explicit {
            if value.is_null() {
                options
                    .entry(segment.clone())
                    .or_insert_with(|| Value::Mapping(Mapping::new()));
                continue;
            }
            let fields = value
                .as_mapping()
                .context("OMP 状态栏分段选项必须为对象，未修改布局")?;
            let target = options
                .entry(segment.clone())
                .or_insert_with(|| Value::Mapping(Mapping::new()));
            let target = target
                .as_mapping_mut()
                .context("OMP 状态栏分段选项无效，未修改布局")?;
            for (key, value) in fields {
                target.insert(key.clone(), value.clone());
            }
        }
    }
    let separator = match field("separator") {
        None | Some(Value::Null) => string(defaults.separator),
        Some(value) if value.as_str().is_some() => value.clone(),
        _ => bail!("OMP 状态栏分隔符必须为字符串，未修改布局"),
    };
    Ok(BTreeMap::from([
        ("preset".to_owned(), string("custom")),
        ("leftSegments".to_owned(), Value::Sequence(left)),
        ("rightSegments".to_owned(), Value::Sequence(right)),
        ("separator".to_owned(), separator),
        ("segmentOptions".to_owned(), Value::Mapping(options)),
        ("contextLine".to_owned(), string("off")),
        ("showHookStatus".to_owned(), Value::Bool(false)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(input: &str) -> BTreeMap<String, Value> {
        values(&serde_yaml::from_str(input).unwrap()).unwrap()
    }

    #[test]
    fn custom_layout_keeps_order_and_unknown_segments_without_duplicate_status_or_context() {
        let result = plan("statusLine:\n  preset: custom\n  leftSegments: [model, context_pct, custom_fact, status, context_total]\n  rightSegments: [session_name, status, context_pct, time]\n  separator: pipe\n  segmentOptions:\n    path: {maxLength: 19}\n    custom_fact: {enabled: true}\n");
        assert_eq!(
            result["leftSegments"],
            serde_yaml::from_str::<Value>("[model, context_pct, custom_fact, status]").unwrap()
        );
        assert_eq!(
            result["rightSegments"],
            serde_yaml::from_str::<Value>("[session_name, time]").unwrap()
        );
        assert_eq!(result["separator"], string("pipe"));
        assert_eq!(
            result["segmentOptions"]["path"]["maxLength"].as_u64(),
            Some(19)
        );
        assert_eq!(
            result["segmentOptions"]["custom_fact"]["enabled"].as_bool(),
            Some(true)
        );
        assert!(result["segmentOptions"]["path"]["abbreviate"].is_null());
        assert_eq!(result["showHookStatus"], Value::Bool(false));
    }

    #[test]
    fn nondefault_preset_retains_effective_defaults_and_merges_user_options() {
        let result = plan("statusLine:\n  preset: nerd\n  leftSegments: [ignored_for_noncustom]\n  separator: slash\n  segmentOptions:\n    path: {maxLength: 22}\n    git: {showUntracked: false}\n");
        assert_eq!(result["separator"], string("slash"));
        assert_eq!(
            result["segmentOptions"]["path"]["maxLength"].as_u64(),
            Some(22)
        );
        assert_eq!(
            result["segmentOptions"]["path"]["abbreviate"].as_bool(),
            Some(true)
        );
        assert_eq!(
            result["segmentOptions"]["git"]["showUntracked"].as_bool(),
            Some(false)
        );
        assert_eq!(
            result["segmentOptions"]["git"]["showStaged"].as_bool(),
            Some(true)
        );
        assert_eq!(
            result["segmentOptions"]["time"]["showSeconds"].as_bool(),
            Some(true)
        );
        assert_eq!(result["rightSegments"], serde_yaml::from_str::<Value>("[session_name, token_in, token_out, cache_read, cache_write, token_rate, cost, context_pct, time_spent, time]").unwrap());
        assert_eq!(
            result["leftSegments"].as_sequence().unwrap().last(),
            Some(&string("status"))
        );
        assert!(!result["leftSegments"]
            .as_sequence()
            .unwrap()
            .contains(&string("ignored_for_noncustom")));
    }

    #[test]
    fn missing_context_is_added_and_unknown_presets_fail_closed() {
        let result = plan(
            "statusLine: {preset: custom, leftSegments: [model], rightSegments: [session_name]}",
        );
        assert_eq!(
            result["leftSegments"],
            serde_yaml::from_str::<Value>("[model, context_pct, status]").unwrap()
        );
        assert!(values(&serde_yaml::from_str("statusLine: {preset: future}").unwrap()).is_err());
        assert!(values(
            &serde_yaml::from_str("statusLine: {preset: custom, leftSegments: model}").unwrap()
        )
        .is_err());
    }
}
