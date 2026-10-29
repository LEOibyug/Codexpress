mod sync;

use crate::{config, model::AppState};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{atomic::Ordering, Arc},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

#[derive(Deserialize)]
struct Request {
    id: String,
    op: String,
    #[serde(default)]
    payload: Value,
}

async fn credentials(state: &Arc<AppState>) -> Value {
    let sources = state.sources.read().await;
    json!({"updated_secrets":sources.iter().filter(|r|r.source.kind=="official" && r.source.secret.get("access_token").and_then(Value::as_str).is_some_and(|s|!s.is_empty())).map(|r|json!({"source_id":r.source.id,"secret":r.source.secret})).collect::<Vec<_>>()})
}

pub async fn dispatch(state: &Arc<AppState>, op: &str, payload: Value) -> Result<Value> {
    match op {
        "status" => {
            let settings = state.settings.read().await.clone();
            let sources = state.sources.read().await.clone();
            state.manual_routing.refresh(&sources, &state.affinity);
            crate::routing::notify_quotas(state, &sources);
            let active = state.usage.active_by_source();
            let sources: Vec<_> = sources
                .iter()
                .map(|runtime| {
                    let mut value = serde_json::to_value(runtime).expect("source status");
                    if crate::routing::official_exhausted(runtime, &sources) {
                        value["health"] = json!("exhausted");
                        value["last_error"] = json!("官方额度已耗尽，等待额度恢复");
                    }
                    value["active_requests"] =
                        json!(active.get(&runtime.source.id).copied().unwrap_or(0));
                    value["identity_digest"] = json!(runtime.source.identity());
                    value["system_role_as_developer"] =
                        json!(state.role_capabilities.requires_developer(&runtime.source));
                    value["admission"] = crate::routing::admission_with_settings(
                        state, runtime, &sources, &settings,
                    );
                    value
                })
                .collect();
            Ok(
                json!({"running":true,"process_id":std::process::id(),"port":state.port,"base_url":format!("http://127.0.0.1:{}/v1",state.port),"mode":settings.mode,"fixed_source_id":settings.fixed_source_id,"load_balance":settings.load_balance,"manual_route":state.manual_routing.status(),"official_low_quota_percent":settings.official_low_quota_percent,"degraded_context_fallback":settings.degraded_context_fallback,"idle_return_minutes":settings.idle_return_minutes,"upstream_attempts":settings.upstream_attempts,"upstream_retry_delay_seconds":settings.upstream_retry_delay_seconds,"sources":sources,"inflight":state.inflight.load(Ordering::Relaxed),"connection_mode":config::connection_mode(&state.state_dir).unwrap_or("unknown")}),
            )
        }
        "credentials" => Ok(credentials(state).await),
        "routing.recover" | "routing.try" | "routing.cancel" => {
            crate::routing::manual::handle(state, op, payload).await
        }
        op if op.starts_with("credentials.") => {
            crate::credentials::handle(state, op, payload).await
        }
        "remote.status" => state.remote.status().await,
        "chatbot.status" => state.chatbot.status().await,
        "chatbot.key" => state.chatbot.key(),
        "chatbot.configure" => crate::chatbot::configure(state, payload).await,
        "images.status" => state.images.status().await,
        "images.key" => state.images.key(),
        "images.configure" => crate::images::configure(state, payload).await,
        "remote.start" => {
            let port = payload["port"]
                .as_u64()
                .filter(|p| (1..=65535).contains(p))
                .context("远端入口端口无效")? as u16;
            if port != state.port {
                bail!("远端与本机共用网关端口，请在设置中修改网关端口");
            }
            state.remote.status().await
        }
        "remote.stop" => bail!("远端与本机共用网关，请撤销对应机器的 Key 以停止接入"),
        "remote.revoke" => {
            state
                .remote
                .revoke(payload["id"].as_str().context("缺少机器标识")?)
                .await
        }
        "remote.compaction.source" => crate::remote::compaction_source(
            payload["config"].as_str().context("缺少远端模型配置")?,
        ),
        "remote.preview" => state
            .remote
            .prepare(serde_json::from_value(payload).context("远端预览参数无效")?),
        "history.status" => state
            .history
            .status()
            .map_err(|_| anyhow::anyhow!("无法读取原文缓存状态")),
        "history.configure" => {
            let enabled = payload["enabled"].as_bool().context("请指定是否保存原文")?;
            let max_bytes = payload["max_bytes"]
                .as_u64()
                .context("请填写有效缓存容量")?;
            let history = state.history.clone();
            tokio::task::spawn_blocking(move || history.configure(enabled, max_bytes))
                .await
                .context("原文缓存设置失败")?
        }
        "history.clear" => {
            let history = state.history.clone();
            tokio::task::spawn_blocking(move || history.clear())
                .await
                .context("原文缓存清理失败")?
        }
        "notifications.list" => Ok(json!({"events":state.notifications.list()})),
        "notifications.ack" => {
            let ids: Vec<String> =
                serde_json::from_value(payload["ids"].clone()).context("通知标识无效")?;
            state.notifications.acknowledge(&ids);
            Ok(json!({"acknowledged":true}))
        }
        "models.list" => crate::discovery::list(state, payload).await,
        "balance.query" => crate::balance::query(state, payload).await,
        "usage.reconcile" => Ok(crate::balance::billing::schedule(
            state,
            true,
            payload["source_id"].as_str(),
        )),
        "usage.reconcile.status" => Ok(crate::balance::billing::status(state)),
        "sync" => sync::handle(state, payload).await,
        "usage.analytics" | "usage.requests" | "usage.detail" => {
            crate::balance::billing::schedule(state, false, None);
            let usage = state.usage.clone();
            let operation = op.to_owned();
            tokio::task::spawn_blocking(move || match operation.as_str() {
                "usage.analytics" => usage.analytics(&payload),
                "usage.requests" => usage.requests(&payload),
                _ => usage.detail(&payload),
            })
            .await
            .context("用量查询失败")?
        }
        "usage" => {
            crate::balance::billing::schedule(state, false, None);
            state.usage.summary_filtered(
                payload
                    .get("days")
                    .and_then(Value::as_u64)
                    .unwrap_or(30)
                    .clamp(1, 3660) as u32,
                payload.get("source_id").and_then(Value::as_str),
                payload.get("model").and_then(Value::as_str),
            )
        }
        op if op.starts_with("config.") => config::handle(state, op, payload).await,
        op if op.starts_with("official.") => crate::official::handle(state, op, payload).await,
        _ => bail!("未知管理操作"),
    }
}

async fn serve_connection(state: Arc<AppState>, stream: UnixStream) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(reader).take(8 * 1024 * 1024 + 1);
    use tokio::io::AsyncReadExt;
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        reader.read_line(&mut line),
    )
    .await??;
    if line.len() > 8 * 1024 * 1024 || !line.ends_with('\n') {
        bail!("管理请求超出限制");
    }
    let request: Request = serde_json::from_str(&line).context("管理请求格式错误")?;
    let response = match dispatch(&state, &request.op, request.payload).await {
        Ok(data) => json!({"id":request.id,"ok":true,"data":data,"error":null}),
        Err(error) => json!({"id":request.id,"ok":false,"data":null,"error":error.to_string()}),
    };
    let mut data = serde_json::to_vec(&response)?;
    data.push(b'\n');
    writer.write_all(&data).await?;
    writer.shutdown().await?;
    Ok(())
}

pub async fn serve(state: Arc<AppState>) -> Result<()> {
    let socket = state.state_dir.join("control.sock");
    if socket.exists() {
        if UnixStream::connect(&socket).await.is_ok() {
            bail!("网关已运行");
        }
        std::fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            let _ = serve_connection(state, stream).await;
        });
    }
}
