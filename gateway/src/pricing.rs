//! Provider-reported charges, with versioned reference prices as a fallback.
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
fn price_for_model(model: &str) -> Option<&'static Price> {
    let model = model.strip_prefix("openai/").unwrap_or(model);
    prices().models.iter().find(|p| matches_model(model, &p.id))
}

/// Standard API-reference USD rates per million tokens, not billed balances.
pub(crate) fn model_cost(model: &str) -> Option<Value> {
    let price = price_for_model(model)?;
    Some(json!({
        "input": price.input,
        "output": price.output,
        "cacheRead": price.cached_input,
        "cacheWrite": price.cache_write_30m,
    }))
}

fn dollars(tokens: u64, per_million: f64, multiplier: f64) -> f64 {
    // Integer picodollars retain fractional-cent cache rates without rounding
    // each component to a displayed currency precision.
    let rate = (per_million * multiplier * 1_000_000.0).round() as u128;
    (u128::from(tokens) * rate).min(i64::MAX as u128) as f64 / 1_000_000_000_000.0
}

pub fn response_charge(response: &Value) -> Option<crate::usage::ReportedCost> {
    let usage = &response["usage"];
    let value = usage
        .get("cost_usd")
        .or_else(|| response.get("cost_usd"))
        .or_else(|| {
            (usage["currency"]
                .as_str()
                .is_some_and(|c| c.eq_ignore_ascii_case("USD")))
            .then(|| usage.get("cost"))
            .flatten()
        })?;
    let usd = value.as_f64().or_else(|| value.as_str()?.parse().ok())?;
    (usd.is_finite() && usd >= 0.0).then(|| crate::usage::ReportedCost {
        usd,
        method: "response".into(),
        record_id: None,
    })
}

pub fn estimate(record: &Value, historical: bool) -> Value {
    let details = &record["token_details"];
    let model = details["billing_model"]
        .as_str()
        .or_else(|| record["model"].as_str())
        .unwrap_or("");
    let official = details["source_kind"] == "official";
    let subscription = details["is_subscription"] == true;
    if details["source_kind"] == "api" {
        let charge = &details["upstream_cost"];
        if let Some(usd) = charge["usd"]
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
        {
            return json!({"currency":"USD","status":"upstream_reported","historical":false,
                "billing_basis":"upstream_charge","source_type_known":true,"total_usd":usd,
                "method":charge["method"],"upstream_record_id":charge["record_id"]});
        }
    }
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
    let Some(price) = price_for_model(model) else {
        return unavailable("unknown_model");
    };
    let gpt_62 = matches!(price.id.as_str(), "gpt-6.2-sol" | "gpt-6.2-astra");
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
    // New Codex credit references keep cache writes within ordinary input;
    // preserve the existing older-model reference behavior.
    let subscription_pricing = gpt_62 && (official || subscription);
    let uncached = input - read - if subscription_pricing { 0 } else { write };
    let long = price
        .long_context_over
        .is_some_and(|threshold| input > threshold);
    // Only 6.1 Sol has a documented official subscription exemption. Preserve
    // older opt-in references, but do not infer an exemption for 6.2.
    let waived = !gpt_62 && (subscription || official && price.id == "gpt-6.1-sol");
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
    let write_price = if subscription_pricing {
        Some(0.0)
    } else {
        match details["cache_write_retention"].as_str() {
            None | Some("in_memory" | "30m") => price.cache_write_30m,
            Some("12h") => price.cache_write_12h,
            _ => None,
        }
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
    fn terra_prices_cache_writes_long_context_and_supported_service_tiers() {
        let mut value = record(272000, 200000);
        value["model"] = json!("gpt-6.1-terra");
        value["token_details"]["cache_write_input_tokens"] = json!(2000);
        value["token_details"]["source_kind"] = json!("api");
        let standard = estimate(&value, false);
        assert_eq!(standard["long_context"], false);
        assert!((standard["total_usd"].as_f64().unwrap() - 0.0875).abs() < 1e-9);
        value["input_tokens"] = json!(272001);
        let long = estimate(&value, false);
        assert_eq!(long["rates"]["input"], 2.0);
        assert_eq!(long["rates"]["cached_input"], 0.1);
        assert_eq!(long["rates"]["output"], 7.5);
        assert!((long["total_usd"].as_f64().unwrap() - 0.172502).abs() < 1e-9);
        value["token_details"]["is_subscription"] = json!(true);
        value["token_details"]["cache_write_retention"] = json!("12h");
        value["token_details"]["service_tier"] = json!("fast");
        let fast = estimate(&value, false);
        assert_eq!(fast["long_context_waived"], true);
        assert_eq!(fast["rates"]["input"], 2.0);
        assert_eq!(fast["rates"]["cache_write"], 5.0);
        assert_eq!(fast["rates"]["output"], 10.0);
        value["token_details"]["service_tier"] = json!("flex");
        let flex = estimate(&value, false);
        assert_eq!(flex["rates"]["input"], 0.5);
        assert_eq!(flex["rates"]["output"], 2.5);
        value["token_details"]["service_tier"] = json!("ultrafast");
        assert_eq!(estimate(&value, false)["status"], "unknown_tier");
    }
    #[test]
    fn gpt_62_prices_full_requests_at_the_long_context_boundary_and_supported_tiers() {
        for (model, short_total, long_total, retained_total, fast_total, batch_total) in [
            ("gpt-6.2-sol", 0.175, 0.345004, 0.355004, 0.690008, 0.172502),
            ("gpt-6.2-astra", 0.7, 1.380016, 1.420016, 2.760032, 0.690008),
        ] {
            let mut value = record(272000, 200000);
            value["model"] = json!(model);
            value["token_details"]["source_kind"] = json!("api");
            value["token_details"]["cache_write_input_tokens"] = json!(2000);
            let short = estimate(&value, false);
            assert_eq!(short["long_context"], false);
            assert!((short["total_usd"].as_f64().unwrap() - short_total).abs() < 1e-9);
            value["input_tokens"] = json!(272001);
            let long = estimate(&value, false);
            assert_eq!(long["long_context"], true);
            assert!((long["total_usd"].as_f64().unwrap() - long_total).abs() < 1e-9);
            value["token_details"]["cache_write_retention"] = json!("12h");
            let retained = estimate(&value, false);
            assert!((retained["total_usd"].as_f64().unwrap() - retained_total).abs() < 1e-9);
            value["token_details"]["cache_write_retention"] = json!("30m");
            for (tier, total) in [("fast", fast_total), ("batch", batch_total)] {
                value["token_details"]["service_tier"] = json!(tier);
                let cost = estimate(&value, false);
                assert!((cost["total_usd"].as_f64().unwrap() - total).abs() < 1e-9);
            }
            // The promotional Ultrafast rate depends on date and eligibility;
            // neither a perpetual 3x nor a perpetual 6x is a known API rate.
            value["token_details"]["service_tier"] = json!("ultrafast");
            assert_eq!(estimate(&value, false)["status"], "unknown_tier");
        }
    }

    #[test]
    fn gpt_62_subscription_references_keep_the_surcharge_without_a_cache_write_premium() {
        for (model, total) in [("gpt-6.2-sol", 0.343004), ("gpt-6.2-astra", 1.372016)] {
            let mut value = record(272001, 200000);
            value["model"] = json!(model);
            value["token_details"]["cache_write_input_tokens"] = json!(2000);
            value["token_details"]["cache_write_retention"] = json!("12h");
            for (source, subscription) in [("official", false), ("api", true)] {
                value["token_details"]["source_kind"] = json!(source);
                value["token_details"]["is_subscription"] = json!(subscription);
                let cost = estimate(&value, false);
                assert_eq!(cost["long_context_waived"], false);
                assert_eq!(cost["uncached_input_tokens"], 72001);
                assert_eq!(cost["cache_write_usd"], 0.0);
                assert!((cost["total_usd"].as_f64().unwrap() - total).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn model_metadata_costs_use_standard_api_rates_and_explicit_snapshot_aliases() {
        assert_eq!(
            model_cost("openai/gpt-6.2-sol-2026-11-18"),
            Some(json!({"input":2.0,"output":10.0,"cacheRead":0.1,"cacheWrite":2.5}))
        );
        assert_eq!(
            model_cost("gpt-6.2-astra"),
            Some(json!({"input":8.0,"output":40.0,"cacheRead":0.4,"cacheWrite":10.0}))
        );
        assert!(model_cost("gpt-6.2-sol-unverified").is_none());
        assert!(model_cost("provider-custom-model").is_none());
    }

    #[test]
    fn upstream_charge_overrides_reference_rates_even_for_unknown_models_and_zero_cost() {
        let mut value = record(512000, 400000);
        value["model"] = json!("provider-custom-model");
        value["token_details"]["source_kind"] = json!("api");
        value["token_details"]["upstream_cost"] =
            json!({"usd":0.15652124,"method":"cch_usage","record_id":"42"});
        let cost = estimate(&value, false);
        assert_eq!(cost["total_usd"], 0.15652124);
        assert_eq!(cost["billing_basis"], "upstream_charge");
        assert_eq!(cost["status"], "upstream_reported");
        assert!(cost["rates"].is_null());
        assert!(cost["input_usd"].is_null());
        value["token_details"]["upstream_cost"]["usd"] = json!(0);
        assert_eq!(estimate(&value, false)["total_usd"], 0.0);
        value["token_details"]["upstream_cost"]["usd"] = json!(-1);
        assert_eq!(estimate(&value, false)["status"], "unknown_model");
        value["token_details"]["upstream_cost"]["usd"] = json!(1);
        value["model"] = json!("gpt-6.1-sol");
        value["token_details"]["source_kind"] = json!("official");
        assert_eq!(
            estimate(&value, false)["billing_basis"],
            "official_subscription_reference"
        );
    }
    #[test]
    fn ambiguous_currency_and_invalid_response_charges_do_not_replace_reference_prices() {
        for usage in [
            json!({"cost":1}),
            json!({"cost":1,"currency":"CNY"}),
            json!({"cost_usd":-1}),
            json!({"cost_usd":"NaN"}),
        ] {
            assert!(response_charge(&json!({"usage":usage})).is_none());
        }
        assert_eq!(
            response_charge(&json!({"usage":{"cost":0,"currency":"USD"}}))
                .unwrap()
                .usd,
            0.0
        );
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
