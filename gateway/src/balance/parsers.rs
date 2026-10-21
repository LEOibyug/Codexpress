use super::{Amount, Reply};
use serde_json::Value;

pub(super) fn data(reply: &Reply) -> Option<&Value> {
    let value = &reply.json;
    if reply.status != 200
        || !value.is_object()
        || value["ok"] == false
        || value["success"] == false
        || value["code"] == false
        || value["status"] == false
        || value.get("error").is_some_and(|e| !e.is_null())
    {
        return None;
    }
    Some(value.get("data").filter(|v| v.is_object()).unwrap_or(value))
}
fn number(value: &Value) -> Option<f64> {
    let n = value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())?;
    (n.is_finite() && n.abs() < 1e15).then_some(n)
}
fn amount(value: f64, currency: &str, scope: &'static str) -> Option<Amount> {
    let currency = currency.trim().to_ascii_uppercase();
    (value.is_finite() && currency.len() == 3 && currency.bytes().all(|b| b.is_ascii_uppercase()))
        .then_some(Amount {
            value,
            currency,
            scope,
        })
}
fn remaining(total: &Value, used: &Value) -> Option<f64> {
    let total = number(total)?;
    let used = number(used)?;
    (total >= 0.0 && used >= 0.0).then_some((total - used).max(0.0))
}
pub(super) fn cch(reply: &Reply) -> Option<Amount> {
    let d = data(reply)?;
    // Prefer account-wide total quota. Never substitute a daily/weekly window.
    if let Some(left) = number(&d["userLimitTotalUsd"])
        .filter(|n| *n > 0.0)
        .and_then(|_| remaining(&d["userLimitTotalUsd"], &d["userCurrentTotalUsd"]))
    {
        return amount(left, "USD", "account");
    }
    number(&d["keyLimitTotalUsd"]).filter(|n| *n > 0.0)?;
    amount(
        remaining(&d["keyLimitTotalUsd"], &d["keyCurrentTotalUsd"])?,
        "USD",
        "key",
    )
}
pub(super) fn new_api_token(reply: &Reply) -> Option<Amount> {
    let d = data(reply)?;
    if d["unlimited_quota"] == true || number(&d["total_granted"]).is_some_and(|v| v < 0.0) {
        return None;
    }
    let value = number(&d["total_available"])
        .or_else(|| remaining(&d["total_granted"], &d["total_used"]))?;
    // New API / One API token-usage fields are quota units, not dollars.
    amount(value.max(0.0) / 500_000.0, "USD", "key")
}
pub(super) fn new_api_account(reply: &Reply) -> Option<Amount> {
    let d = data(reply)?;
    // Account shape must be recognized; do not treat arbitrary 'quota' as money.
    number(&d["used_quota"])?;
    amount(number(&d["quota"])?.max(0.0) / 500_000.0, "USD", "account")
}
pub(super) fn sub2api(reply: &Reply) -> Option<Amount> {
    let d = data(reply)?;
    if d["isValid"] != true {
        return None;
    }
    if let Some(value) = number(&d["balance"]) {
        return amount(value, "USD", "account");
    }
    if d["mode"] != "quota_limited" {
        return None;
    }
    let q = &d["quota"];
    // Only a total key quota is portable as a balance; subscription remaining
    // is the tightest period limit and must not appear as a wallet amount.
    let value = number(&q["remaining"]).or_else(|| remaining(&q["limit"], &q["used"]))?;
    amount(value.max(0.0), "USD", "key")
}
pub(super) fn credits(reply: &Reply, credit_grants: bool) -> Option<Amount> {
    let d = data(reply)?;
    let value = number(&d["total_available"])
        .or_else(|| number(&d["balance"]))
        .or_else(|| remaining(&d["total_granted"], &d["total_used"]))
        .or_else(|| {
            credit_grants
                .then(|| remaining(&d["total"], &d["used"]))
                .flatten()
        })?;
    // hard_limit_usd is frequently a monthly cap or an unlimited sentinel.
    // Never subtract a date-range usage response to manufacture total credit.
    amount(value, d["currency"].as_str().unwrap_or("USD"), "account")
}
pub(super) fn deepseek(reply: &Reply) -> Option<Amount> {
    let d = data(reply)?;
    let wallets = d["balance_infos"].as_array()?;
    let mut amounts = wallets
        .iter()
        .map(|w| {
            amount(
                number(&w["total_balance"])?,
                w["currency"].as_str()?,
                "account",
            )
        })
        .collect::<Option<Vec<_>>>()?;
    if amounts.iter().any(|a| a.value != 0.0) {
        amounts.retain(|a| a.value != 0.0);
    }
    let mut first = amounts.first()?.clone();
    // Different currencies cannot be summed without an exchange-rate source.
    if amounts.iter().any(|a| a.currency != first.currency) {
        return None;
    }
    first.value = amounts.iter().map(|a| a.value).sum();
    Some(first)
}
pub(super) fn moonshot(reply: &Reply, usd: bool) -> Option<Amount> {
    let d = data(reply)?;
    if reply.json["status"] != true {
        return None;
    }
    amount(
        number(&d["available_balance"])?,
        d["currency"]
            .as_str()
            .unwrap_or(if usd { "USD" } else { "CNY" }),
        "account",
    )
}
pub(super) fn siliconflow(reply: &Reply, usd: bool) -> Option<Amount> {
    let d = data(reply)?;
    if reply
        .json
        .get("code")
        .is_some_and(|c| !matches!(c.as_i64(), Some(0 | 20000)))
    {
        return None;
    }
    amount(
        number(&d["totalBalance"])?,
        d["currency"]
            .as_str()
            .unwrap_or(if usd { "USD" } else { "CNY" }),
        "account",
    )
}
pub(super) fn openrouter(reply: &Reply) -> Option<Amount> {
    let d = data(reply)?;
    amount(
        remaining(&d["total_credits"], &d["total_usage"])?,
        "USD",
        "account",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn reply(json: Value) -> Reply {
        Reply {
            status: 200,
            json,
            session: None,
        }
    }
    #[test]
    fn normalizes_used_total_and_rejects_period_limits_missing_fields_and_unlimited() {
        assert_eq!(
            cch(&reply(
                json!({"userLimitTotalUsd":"100","userCurrentTotalUsd":"12.5"})
            ))
            .unwrap()
            .value,
            87.5
        );
        assert!(cch(&reply(
            json!({"userLimitDailyUsd":100,"userCurrentDailyUsd":12})
        ))
        .is_none());
        assert!(cch(&reply(json!({"userLimitTotalUsd":100}))).is_none());
        assert!(cch(&reply(
            json!({"userLimitTotalUsd":0,"userCurrentTotalUsd":0})
        ))
        .is_none());
        assert!(new_api_token(&reply(
            json!({"data":{"total_available":9999,"unlimited_quota":true}})
        ))
        .is_none());
        assert_eq!(
            new_api_token(&reply(
                json!({"data":{"total_granted":5_000_000,"total_used":2_500_000}})
            ))
            .unwrap()
            .value,
            5.0
        );
        assert!(credits(
            &reply(json!({"hard_limit_usd":1000000,"total_usage":1})),
            false
        )
        .is_none());
        assert_eq!(
            credits(&reply(json!({"total":100,"used":12.25})), true)
                .unwrap()
                .value,
            87.75
        );
        assert!(sub2api(&reply(json!({"isValid":true,"remaining":99}))).is_none());
        assert_eq!(
            sub2api(&reply(
                json!({"isValid":true,"mode":"quota_limited","quota":{"limit":100,"used":12}})
            ))
            .unwrap()
            .value,
            88.0
        );
        assert_eq!(
            openrouter(&reply(
                json!({"data":{"total_credits":50,"total_usage":12}})
            ))
            .unwrap()
            .value,
            38.0
        );
        assert!(credits(&reply(json!({"balance":"NaN"})), true).is_none());
        assert!(deepseek(&reply(json!({"balance_infos":[{"currency":"USD","total_balance":1},{"currency":"CNY","total_balance":1}]}))).is_none());
    }
}
