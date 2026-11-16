use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Merge only output aggregates, after each caller has selected names and filtered IDs.
pub(super) fn merge<'a>(
    groups: impl IntoIterator<Item = &'a Value>,
    by_model: bool,
) -> Vec<Value> {
    let mut indices = BTreeMap::new();
    let mut rows: Vec<(Value, BTreeSet<&str>)> = Vec::new();
    for group in groups {
        let name = group["source_name"].as_str().unwrap_or("").trim();
        let id = group["source_id"].as_str().unwrap_or("");
        let key = (
            !name.is_empty(),
            if name.is_empty() { id } else { name },
            by_model.then(|| group["model"].as_str().unwrap_or("")),
        );
        let index = *indices.entry(key).or_insert_with(|| {
            let mut row = json!({"source_name":name});
            if by_model {
                row["model"] = group["model"].clone();
            }
            let index = rows.len();
            rows.push((row, BTreeSet::new()));
            index
        });
        let row = &mut rows[index].0;
        for (field, value) in group.as_object().expect("aggregate") {
            if let Some(number) = value.as_u64() {
                row[field] = json!(row[field].as_u64().unwrap_or(0).saturating_add(number));
            } else if let Some(number) = value.as_f64() {
                row[field] = json!(row[field].as_f64().unwrap_or(0.0) + number);
            } else if value.is_null() && row.get(field).is_none() {
                row[field] = Value::Null;
            }
        }
        rows[index].1.insert(id);
    }
    rows.into_iter()
        .map(|(mut row, ids)| {
            row["source_id"] = if ids.len() == 1 {
                json!(ids.first())
            } else {
                Value::Null
            };
            row["source_ids"] = json!(ids);
            row
        })
        .collect()
}
