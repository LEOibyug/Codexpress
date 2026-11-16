//! Bounded refresh operations. Read-only HTTP queries never schedule upstream work.
use super::safety::{self, Binding};
use crate::model::{now, AppState};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{collections::VecDeque, sync::Arc};

const MAX_OPERATIONS: usize = 128;
const MAX_RUNNING: usize = 8;
struct Operation {
    id: String,
    kind: String,
    created_at: u64,
    status: &'static str,
    bindings: Vec<Binding>,
    include_credits: bool,
    results: Vec<Value>,
}
impl Operation {
    fn view(&self) -> Value {
        json!({"id":self.id,"kind":self.kind,"created_at":self.created_at,"status":self.status,"result":{"sources":self.results}})
    }
}
#[derive(Default)]
pub(super) struct Operations {
    entries: VecDeque<Operation>,
}

pub(super) async fn start(state: &Arc<AppState>, kind: &str, payload: Value) -> Result<Value> {
    safety::fields(
        &payload,
        if kind == "quotas" {
            &["source_id", "include_credits"]
        } else {
            &["source_id"]
        },
    )?;
    let source_id = safety::optional_text(&payload, "source_id")?;
    let include_credits = match payload.get("include_credits") {
        None => true,
        Some(value) => value.as_bool().context("include_credits 必须为布尔值")?,
    };
    let sources = state.sources.read().await;
    if source_id
        .as_ref()
        .is_some_and(|id| !sources.iter().any(|runtime| &runtime.source.id == id))
    {
        bail!("来源不存在");
    }
    let selected: Vec<_> = sources
        .iter()
        .filter(|runtime| {
            runtime.source.kind
                == if kind == "balances" {
                    "api"
                } else {
                    "official"
                }
                && source_id.as_ref().is_none_or(|id| &runtime.source.id == id)
        })
        .map(|runtime| runtime.source.clone())
        .collect();
    let bindings: Vec<_> = selected.iter().map(Binding::of).collect();
    if source_id.is_some() && bindings.is_empty() {
        bail!("所选来源不支持此刷新操作");
    }
    drop(sources);
    let id = uuid::Uuid::new_v4().to_string();
    let response = {
        let mut store = state
            .control
            .operations
            .lock()
            .map_err(|_| anyhow::anyhow!("刷新队列忙碌"))?;
        if let Some(operation) = store.entries.iter().find(|operation| {
            operation.status == "running"
                && operation.kind == kind
                && operation.bindings == bindings
                && operation.include_credits == include_credits
        }) {
            return Ok(operation.view());
        }
        if store
            .entries
            .iter()
            .filter(|op| op.status == "running")
            .count()
            >= MAX_RUNNING
        {
            bail!("刷新任务过多，请稍后重试");
        }
        if store.entries.len() >= MAX_OPERATIONS {
            let index = store
                .entries
                .iter()
                .position(|op| op.status != "running")
                .context("刷新队列已满")?;
            store.entries.remove(index);
        }
        let operation = Operation {
            id: id.clone(),
            kind: kind.to_owned(),
            created_at: now(),
            status: "running",
            bindings: bindings.clone(),
            include_credits,
            results: Vec::new(),
        };
        let response = operation.view();
        store.entries.push_back(operation);
        response
    };
    let state = state.clone();
    let kind = kind.to_owned();
    tokio::spawn(async move {
        let mut results = Vec::new();
        let mut failed = false;
        for (binding, source) in bindings.into_iter().zip(selected) {
            let outcome = if !binding.current(&state).await {
                None
            } else {
                let result = if kind == "balances" {
                    crate::balance::query_for_source(&state, &source, true).await
                } else {
                    crate::official::refresh_for_source(&state, &source, include_credits).await
                };
                if binding.current(&state).await {
                    Some(result)
                } else {
                    None
                }
            };
            let entry = match outcome {
                Some(Ok(result)) => {
                    let fresh = if kind == "balances" {
                        result["status"] == "available" && result["stale"] == false
                    } else {
                        result["quota"]["stale"] == false
                            && result["quota"]["error"].is_null()
                            && (!include_credits
                                || result["credits"]["stale"] == false
                                    && result["credits"]["error"].is_null())
                    };
                    if fresh {
                        json!({"source_id":binding.source_id,"status":"completed","result":result})
                    } else {
                        failed = true;
                        let next_retry_at = if kind == "balances" {
                            result["next_retry_at"].as_u64()
                        } else {
                            [
                                result["quota"]["next_retry_at"].as_u64(),
                                if include_credits {
                                    result["credits"]["next_retry_at"].as_u64()
                                } else {
                                    None
                                },
                            ]
                            .into_iter()
                            .flatten()
                            .min()
                        };
                        json!({"source_id":binding.source_id,"status":"failed","code":"upstream_refresh_failed","next_retry_at":next_retry_at,"result":null})
                    }
                }
                Some(Err(_)) => {
                    failed = true;
                    json!({"source_id":binding.source_id,"status":"failed","code":"refresh_failed","result":null})
                }
                None => {
                    failed = true;
                    json!({"source_id":binding.source_id,"status":"failed","code":"source_changed","result":null})
                }
            };
            results.push(entry);
        }
        if let Ok(mut store) = state.control.operations.lock() {
            if let Some(operation) = store.entries.iter_mut().find(|op| op.id == id) {
                operation.results = results;
                operation.status = if failed { "failed" } else { "completed" };
            }
        };
    });
    Ok(response)
}
pub(super) async fn get(state: &Arc<AppState>, id: &str) -> Result<Option<Value>> {
    let id = uuid::Uuid::parse_str(id)
        .context("操作标识必须为 UUID")?
        .to_string();
    let sources = state.sources.read().await;
    let store = state
        .control
        .operations
        .lock()
        .map_err(|_| anyhow::anyhow!("刷新队列忙碌"))?;
    let Some(operation) = store.entries.iter().find(|operation| operation.id == id) else {
        return Ok(None);
    };
    let mut view = operation.view();
    let mut stale = false;
    if let Some(results) = view["result"]["sources"].as_array_mut() {
        for result in results {
            if operation
                .bindings
                .iter()
                .find(|binding| result["source_id"] == binding.source_id)
                .is_some_and(|binding| {
                    !sources
                        .iter()
                        .any(|runtime| binding.matches(&runtime.source))
                })
            {
                *result = json!({"source_id":result["source_id"],"status":"failed","code":"source_changed","result":null});
                stale = true;
            }
        }
    }
    if stale {
        view["status"] = json!("failed");
    }
    Ok(Some(view))
}
