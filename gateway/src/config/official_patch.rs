use super::Snapshot;
use anyhow::Result;
use toml_edit::{DocumentMut, Item, Table};

pub(super) const OFFICIAL_KEYS: [&str; 4] = [
    "openai_base_url",
    "chatgpt_base_url",
    "forced_login_method",
    "respect_system_proxy",
];
pub const OFFICIAL_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const OFFICIAL_SERVICE_URL: &str = "https://chatgpt.com/backend-api/";

pub(super) fn official_fields_version(doc: &DocumentMut, schema: u32) -> String {
    let mut fields = DocumentMut::new();
    for key in OFFICIAL_KEYS {
        if schema < 2 && key == "respect_system_proxy" {
            continue;
        }
        if let Some(item) = doc.get(key) {
            fields[key] = item.clone();
        }
    }
    if schema >= 3 {
        if let Some(item) = doc.get("model_providers") {
            fields["model_providers"] = item.clone();
        }
    } else if schema >= 2 {
        if let Some(item) = doc.get("model_providers").and_then(|p| p.get("openai")) {
            fields["model_providers"] = Item::Table(Table::new());
            fields["model_providers"]["openai"] = item.clone();
        }
    }
    fields.to_string()
}

pub(super) fn official_fields(doc: &DocumentMut) -> String {
    official_fields_version(doc, 3)
}

pub(super) fn restore_official_fields(doc: &mut DocumentMut, snapshot: &Snapshot) -> Result<()> {
    if let Some(before) = &snapshot.official_before {
        let original = before.parse::<DocumentMut>()?;
        for key in OFFICIAL_KEYS {
            if snapshot.official_schema < 2 && key == "respect_system_proxy" {
                continue;
            }
            match original.get(key) {
                Some(item) => {
                    doc[key] = item.clone();
                }
                None => {
                    doc.remove(key);
                }
            }
        }
        if snapshot.official_schema >= 3 {
            if let Some(item) = original.get("model_providers") {
                doc["model_providers"] = item.clone();
            } else {
                doc.remove("model_providers");
            }
        } else if snapshot.official_schema >= 2 {
            if let Some(item) = original
                .get("model_providers")
                .and_then(|p| p.get("openai"))
            {
                if doc.get("model_providers").is_none() {
                    let mut providers = Table::new();
                    providers.set_implicit(true);
                    doc["model_providers"] = Item::Table(providers);
                }
                doc["model_providers"]["openai"] = item.clone();
            } else if let Some(table) = doc
                .get_mut("model_providers")
                .and_then(Item::as_table_like_mut)
            {
                table.remove("openai");
            }
        }
    }
    Ok(())
}
