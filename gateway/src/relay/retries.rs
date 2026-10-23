//! Source-local retries for explicit transient failures before output starts.
use crate::model::{RoutingSettings, Source};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

pub(super) fn transient_status(status: u16) -> bool {
    // A timeout (408/504) can mean inference was submitted; do not replay it.
    matches!(status, 500 | 502 | 503)
}

pub(super) fn permanent_code(value: &Value) -> Option<&str> {
    let error = value.get("error").unwrap_or(value);
    let codes = [error["code"].as_str(), error["type"].as_str()];
    // Account-wide refusals take precedence over generic request classifications.
    codes
        .into_iter()
        .flatten()
        .find(|code| {
            matches!(
                *code,
                "insufficient_quota"
                    | "billing_hard_limit_reached"
                    | "usage_limit_reached"
                    | "quota_exceeded"
                    | "invalid_api_key"
                    | "token_expired"
                    | "account_deactivated"
                    | "authentication_error"
            )
        })
        .or_else(|| {
            codes.into_iter().flatten().find(|code| {
                matches!(
                    *code,
                    "insufficient_quota"
                        | "billing_hard_limit_reached"
                        | "usage_limit_reached"
                        | "quota_exceeded"
                        | "invalid_api_key"
                        | "token_expired"
                        | "account_deactivated"
                        | "authentication_error"
                        | "invalid_request_error"
                        | "invalid_value"
                        | "unsupported_value"
                        | "unsupported_parameter"
                        | "unknown_parameter"
                        | "missing_required_parameter"
                        | "context_length_exceeded"
                        | "model_not_found"
                        | "permission_denied"
                        | "rate_limit_exceeded"
                        | "invalid_encrypted_content"
                        | "previous_response_not_found"
                )
            })
        })
}

type SourceKey = (String, String);

/// Lives only for one authenticated client request, including internal replans.
#[derive(Clone, Default)]
pub(super) struct RequestRetries(Arc<Mutex<HashMap<SourceKey, u8>>>);

impl RequestRetries {
    fn key(source: &Source) -> SourceKey {
        (source.id.clone(), source.identity())
    }

    pub(super) fn exhausted(&self, source: &Source, limit: u8) -> bool {
        self.0
            .lock()
            .expect("retry state")
            .get(&Self::key(source))
            .copied()
            .unwrap_or(0)
            >= limit.clamp(1, 10)
    }

    pub(super) fn block(&self, source: &Source) {
        self.0
            .lock()
            .expect("retry state")
            .insert(Self::key(source), u8::MAX);
    }
}

pub(super) struct UpstreamRetries {
    request: RequestRetries,
    key: SourceKey,
    limit: u8,
    delay: Duration,
}

impl UpstreamRetries {
    pub(super) fn new(
        request: &RequestRetries,
        source: &Source,
        settings: &RoutingSettings,
    ) -> Self {
        Self {
            request: request.clone(),
            key: RequestRetries::key(source),
            limit: settings.upstream_attempts.clamp(1, 10),
            delay: Duration::from_secs(settings.upstream_retry_delay_seconds.min(300)),
        }
    }

    pub(super) fn extra_attempts(&self) -> usize {
        usize::from(self.limit - 1)
    }

    pub(super) fn failed(&self) -> Option<Duration> {
        let mut failures = self.request.0.lock().expect("retry state");
        let count = failures.entry(self.key.clone()).or_default();
        *count = count.saturating_add(1);
        if *count >= self.limit {
            return None;
        }
        eprintln!(
            "upstream_retry_wait unix={} retry={} delay_seconds={}",
            crate::model::now(),
            count,
            self.delay.as_secs()
        );
        Some(self.delay)
    }
}
