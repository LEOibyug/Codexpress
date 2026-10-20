use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    sync::Mutex,
};

#[derive(Default)]
struct Data {
    events: VecDeque<Value>,
    exhausted: HashSet<String>,
    fallback: HashSet<String>,
}
#[derive(Default)]
pub struct Notifications {
    data: Mutex<Data>,
}
impl Notifications {
    pub fn fallback(&self, key: &str) {
        if let Ok(mut data) = self.data.lock() {
            if data.fallback.len() > 10000 {
                data.fallback.clear();
            }
            if !data.fallback.insert(key.into()) {
                return;
            }
            data.events.push_back(json!({"id":uuid::Uuid::new_v4(),"kind":"context_fallback","title":"已尝试恢复对话","body":"网关已移除本次请求中的加密上下文及旧状态引用。部分隐藏推理或压缩摘要可能缺失，本地对话记录保留。"}));
            while data.events.len() > 100 {
                data.events.pop_front();
            }
        }
    }
    pub fn quota(&self, identity: &str, name: &str, exhausted: bool) {
        if let Ok(mut data) = self.data.lock() {
            if !exhausted {
                data.exhausted.remove(identity);
                return;
            }
            if !data.exhausted.insert(identity.into()) {
                return;
            }
            data.events.push_back(json!({"id":uuid::Uuid::new_v4(),"kind":"official_exhausted","title":"官方来源额度已耗尽","body":format!("{name} 暂停分配请求；可用的新请求将按路由规则选择其他来源。额度恢复后重新参与分配。" )}));
            while data.events.len() > 100 {
                data.events.pop_front();
            }
        }
    }
    pub fn list(&self) -> Vec<Value> {
        self.data
            .lock()
            .map(|d| d.events.iter().cloned().collect())
            .unwrap_or_default()
    }
    pub fn acknowledge(&self, ids: &[String]) {
        if let Ok(mut d) = self.data.lock() {
            d.events.retain(|e| {
                e["id"]
                    .as_str()
                    .is_none_or(|id| !ids.iter().any(|s| s == id))
            });
        }
    }
}
