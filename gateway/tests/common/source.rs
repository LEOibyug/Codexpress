use codexpress_gateway::model::{ProxyPolicy, Source};
use serde_json::json;

pub fn api_source(id: &str, name: &str, base: &str, api_key: &str) -> Source {
    Source {
        id: id.into(),
        name: name.into(),
        kind: "api".into(),
        is_subscription: false,
        base_url: base.into(),
        enabled: true,
        priority: 0,
        max_active_conversations: None,
        active_conversation_window_seconds: 15,
        proxy: ProxyPolicy {
            mode: "direct".into(),
            url: None,
        },
        models: vec![],
        account_id: None,
        secret: json!({"api_key": api_key}),
    }
}
