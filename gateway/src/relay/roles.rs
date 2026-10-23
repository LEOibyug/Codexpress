//! Repair only an explicit pre-generation rejection of a system input role.
use serde_json::{json, Value};

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
                && (message.contains("unsupported") || message.contains("not supported"))));
    }
    // Do not mistake validation of content or another field for a role error.
    if !param.is_empty() {
        return false;
    }
    [
        "system messages are not supported",
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
    let mut repaired = body.clone();
    for item in repaired["input"].as_array_mut()? {
        if item["role"] == "system" {
            item["role"] = json!("developer");
        }
    }
    Some(repaired)
}
