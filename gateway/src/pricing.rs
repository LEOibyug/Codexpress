//! Versioned reference token prices. Never represents a provider invoice.
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::OnceLock;

#[derive(Deserialize)]
struct Catalog {
    revision: String,
    models: Vec<Price>,
}
#[derive(Deserialize)]
struct Price {
    id: String,
    input: f64,
    cached_input: f64,
    output: f64,
    cache_write_30m: Option<f64>,
    cache_write_12h: Option<f64>,
    long_context_over: Option<u64>,
    long_input_multiplier: Option<f64>,
    long_output_multiplier: Option<f64>,
    fast_multiplier: Option<f64>,
    ultrafast_multiplier: Option<f64>,
    discount_multiplier: Option<f64>,
    #[serde(default)]
    session_scope: bool,
}
pub fn catalog() -> Value {
    serde_json::from_str(include_str!("../resources/openai-prices.json")).expect("bundled prices")
}
fn prices() -> &'static Catalog {
    static PRICES: OnceLock<Catalog> = OnceLock::new();
    PRICES.get_or_init(|| {
        serde_json::from_str(include_str!("../resources/openai-prices.json"))
            .expect("bundled prices")
    })
}
fn matches_model(model: &str, id: &str) -> bool {
    if model == id {
        return true;
    }
    // Only explicit date snapshots, never prefix guesses such as gpt-5.4-pro.
    model
        .strip_prefix(id)
        .and_then(|s| s.strip_prefix('-'))
        .is_some_and(|date| {
            date.len() == 10
                && date.bytes().enumerate().all(|(i, c)| {
                    if matches!(i, 4 | 7) {
                        c == b'-'
                    } else {
                        c.is_ascii_digit()
                    }
                })
        })
}
fn dollars(tokens: u64, per_million: f64, multiplier: f64) -> f64 {
    // Integer picodollars retain fractional-cent cache rates without rounding
    // each component to a displayed currency precision.
    let rate = (per_million * multiplier * 1_000_000.0).round() as u128;
    (u128::from(tokens) * rate).min(i64::MAX as u128) as f64 / 1_000_000_000_000.0
}

pub fn estimate(record: &Value, historical: bool) -> Value {
    let details = &record["token_details"];
    let model = details["billing_model"]
        .as_str()
        .or_else(|| record["model"].as_str())
        .unwrap_or("");
    let model = model.strip_prefix("openai/").unwrap_or(model);
    let official = details["source_kind"] == "official";
    let subscription = details["is_subscription"] == true;
    let basis = if official {
        "official_subscription_reference"
    } else if subscription {
        "subscription_reference"
    } else {
        "api_reference"
    };
    let base = json!({"revision":prices().revision,"currency":"USD","historical":historical,
        "billing_basis":basis,"source_type_known":details["source_kind"].is_string()});
    let unavailable = |status: &str| {
        let mut cost = base.clone();
        cost["status"] = json!(status);
        cost
    };
    let Some(price) = prices().models.iter().find(|p| matches_model(model, &p.id)) else {
        return unavailable("unknown_model");
    };
    let tier = details["service_tier"].as_str().unwrap_or("default");
    let multiplier = match tier {
        "default" | "standard" | "auto" => Some(1.0),
        "priority" | "fast" => price.fast_multiplier,
        "ultrafast" => price.ultrafast_multiplier,
        "flex" | "batch" => price.discount_multiplier,
        _ => None,
    };
    let Some(multiplier) = multiplier else {
        return unavailable("unknown_tier");
    };
    let input_known = details["input_estimated"] == false && details["legacy"] != true
        || details["input_available"] == true
        || record["complete"] == true;
    let output_known = details["output_available"] == true || record["complete"] == true;
    if !input_known && !output_known && record["status"] != "success" {
        return unavailable("unobserved");
    }
    let input = record["input_tokens"].as_u64().unwrap_or(0);
    let output = record["output_tokens"].as_u64().unwrap_or(0);
    let read = record["cached_input_tokens"]
        .as_u64()
        .unwrap_or(0)
        .min(input);
    let write = details["cache_write_input_tokens"]
        .as_u64()
        .unwrap_or(0)
        .min(input - read);
    let uncached = input - read - write;
    let long = price
        .long_context_over
        .is_some_and(|threshold| input > threshold);
    // Official Sol exemption is documented for subscription usage. Other API
    // providers can explicitly opt into subscription reference pricing.
    let waived = subscription || official && price.id == "gpt-6.1-sol";
    let long_priced = long && !waived;
    if long_priced && matches!(tier, "priority" | "fast") && price.session_scope {
        return unavailable("unknown_tier");
    }
    let input_multiplier = multiplier
        * if long_priced {
            price.long_input_multiplier.unwrap_or(1.0)
        } else {
            1.0
        };
    let output_multiplier = multiplier
        * if long_priced {
            price.long_output_multiplier.unwrap_or(1.0)
        } else {
            1.0
        };
    let write_price = match details["cache_write_retention"].as_str() {
        None | Some("in_memory" | "30m") => price.cache_write_30m,
        Some("12h") => price.cache_write_12h,
        _ => None,
    };
    let input_usd = dollars(uncached, price.input, input_multiplier);
    let cached_usd = dollars(read, price.cached_input, input_multiplier);
    let write_usd = write_price
        .map(|p| dollars(write, p, input_multiplier))
        .unwrap_or(0.0);
    let output_usd = if output_known {
        dollars(output, price.output, output_multiplier)
    } else {
        0.0
    };
    let estimated = details["input_estimated"] == true
        || details["output_estimated"] == true
        || details["cached_input_reported"] != true && input > 0
        || details["legacy"] == true
        || historical
        || price.session_scope && !waived;
    let partial = !output_known || !input_known && input == 0 || write > 0 && write_price.is_none();
    json!({"revision":prices().revision,"currency":"USD","status":if partial {"partial"}else if estimated {"estimated"}else {"reported"},
        "historical":historical,"price_model":price.id,"service_tier":tier,"long_context":long,"long_context_waived":long && waived,
        "billing_basis":basis,"source_type_known":details["source_kind"].is_string(),
        "session_scope_estimate":price.session_scope && !waived,"uncached_input_tokens":uncached,"cached_input_tokens":read,"cache_write_tokens":write,
        "input_usd":input_usd,"cached_input_usd":cached_usd,"cache_write_usd":write_usd,"output_usd":output_usd,
        "total_usd":input_usd+cached_usd+write_usd+output_usd,
        "cache_savings_usd":dollars(read,price.input-price.cached_input,input_multiplier),
        "rates":{"input":price.input*input_multiplier,"cached_input":price.cached_input*input_multiplier,"output":price.output*output_multiplier,"cache_write":write_price.map(|p|p*input_multiplier)},
        "unpriced_cache_write_tokens":if write_price.is_none(){write}else{0}})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(input: u64, cached: u64) -> Value {
        json!({"model":"gpt-6.1-sol","status":"success","input_tokens":input,"cached_input_tokens":cached,"output_tokens":1000,"complete":true,
            "token_details":{"input_estimated":false,"output_estimated":false,"output_available":true,"cached_input_reported":true}})
    }
    #[test]
    fn full_request_long_context_boundary_and_cache_costs_are_not_double_counted() {
        let short = estimate(&record(272000, 200000), false);
        assert_eq!(short["long_context"], false);
        assert!((short["total_usd"].as_f64().unwrap() - 0.174).abs() < 1e-9);
        let long = estimate(&record(272001, 200000), false);
        assert_eq!(long["long_context"], true);
        assert!((long["total_usd"].as_f64().unwrap() - 0.343004).abs() < 1e-9);
        assert_eq!(long["rates"]["output"], 15.0);
        let mut write = record(10000, 5000);
        write["token_details"]["cache_write_input_tokens"] = json!(2000);
        write["token_details"]["cache_write_retention"] = json!("12h");
        write["token_details"]["service_tier"] = json!("priority");
        let cost = estimate(&write, false);
        assert_eq!(cost["uncached_input_tokens"], 3000);
        assert!((cost["total_usd"].as_f64().unwrap() - 0.053).abs() < 1e-9);
    }
    #[test]
    fn unknown_prices_and_missing_usage_do_not_become_free_requests() {
        let mut value = record(100, 50);
        value["model"] = json!("gpt-6.1-sol-unverified");
        assert_eq!(estimate(&value, false)["status"], "unknown_model");
        value["model"] = json!("openai/gpt-6.1-sol-2026-10-01");
        assert!(estimate(&value, false)["total_usd"].is_number());
        value["token_details"]["service_tier"] = json!("unknown");
        assert_eq!(estimate(&value, false)["status"], "unknown_tier");
        value["token_details"] = json!({"input_estimated":true});
        value["status"] = json!("failed");
        value["complete"] = json!(false);
        assert_eq!(estimate(&value, false)["status"], "unobserved");
        value["status"] = json!("success");
        assert_eq!(estimate(&value, true)["status"], "partial");
    }

    #[test]
    fn subscription_exemption_is_captured_per_request_without_changing_api_prices() {
        let mut value = record(512000, 400000);
        value["token_details"]["source_kind"] = json!("api");
        value["token_details"]["is_subscription"] = json!(false);
        let api = estimate(&value, false);
        assert_eq!(api["rates"]["input"], 4.0);
        assert_eq!(api["long_context_waived"], false);
        value["token_details"]["is_subscription"] = json!(true);
        let subscription = estimate(&value, false);
        assert_eq!(subscription["billing_basis"], "subscription_reference");
        assert_eq!(subscription["long_context_waived"], true);
        assert_eq!(subscription["rates"]["input"], 2.0);
        assert_eq!(subscription["rates"]["output"], 10.0);
        value["model"] = json!("gpt-5.4");
        value["token_details"]["service_tier"] = json!("priority");
        assert!(estimate(&value, false)["total_usd"].is_number());
        assert_eq!(estimate(&value, false)["session_scope_estimate"], false);
        value["model"] = json!("gpt-6.1-sol");
        value["token_details"]["is_subscription"] = json!(false);
        value["token_details"]["source_kind"] = json!("official");
        assert_eq!(estimate(&value, false)["long_context_waived"], true);
        value["model"] = json!("gpt-6-astra");
        assert_eq!(estimate(&value, false)["long_context_waived"], false);
        value["token_details"]
            .as_object_mut()
            .unwrap()
            .remove("source_kind");
        assert_eq!(estimate(&value, true)["source_type_known"], false);
        assert_eq!(estimate(&value, true)["billing_basis"], "api_reference");
    }
}
