//! Persist only hashes of discarded wire state, never conversation content.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub fn digest(prefix: &str, text: &str) -> String {
    format!("{prefix}:{:x}", Sha256::digest(text.as_bytes()))
}
pub fn accepted_state(identity: &str, key: &str) -> String {
    digest(
        "accepted",
        &serde_json::to_string(&(identity, key)).expect("strings"),
    )
}
pub fn keys(value: &Value, turn: Option<&str>) -> HashSet<String> {
    let mut result = HashSet::new();
    if let Some(items) = value["input"].as_array() {
        for item in items {
            if let Some(value) = item["encrypted_content"].as_str() {
                result.insert(digest("encrypted", value));
            }
        }
    }
    for name in ["previous_response_id", "conversation"] {
        if let Some(text) = value[name].as_str().or_else(|| value[name]["id"].as_str()) {
            result.insert(digest(name, text));
        }
    }
    if let Some(turn) = turn {
        result.insert(digest("turn", turn));
    }
    result
}

pub fn apply(value: &mut Value, turn: Option<&str>, excluded: &HashSet<String>) -> (bool, bool) {
    let mut changed = false;
    if let Some(items) = value.get_mut("input").and_then(Value::as_array_mut) {
        items.retain_mut(|item| {
            let drop = item["encrypted_content"]
                .as_str()
                .is_some_and(|text| excluded.contains(&digest("encrypted", text)));
            if !drop {
                return true;
            }
            changed = true;
            if matches!(item["type"].as_str(), Some("reasoning" | "compaction")) {
                return false;
            }
            if let Some(object) = item.as_object_mut() {
                object.remove("encrypted_content");
            }
            true
        });
    }
    for name in ["previous_response_id", "conversation"] {
        let drop = value[name]
            .as_str()
            .or_else(|| value[name]["id"].as_str())
            .is_some_and(|text| excluded.contains(&digest(name, text)));
        if drop {
            value.as_object_mut().expect("object").remove(name);
            changed = true;
        }
    }
    let remove_turn = turn.is_some_and(|text| excluded.contains(&digest("turn", text)));
    (changed, remove_turn)
}
