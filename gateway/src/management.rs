use crate::{
    config,
    model::{AppState, RoutingSettings, Source, SourceRuntime},
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
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
            let sources: Vec<_> = sources
                .iter()
                .map(|runtime| {
                    let mut value = serde_json::to_value(runtime).expect("source status");
                    value["admission"] = crate::routing::admission(
                        state,
                        runtime,
                        &sources,
                        settings.official_low_quota_percent,
                    );
                    value
                })
                .collect();
            Ok(
                json!({"running":true,"process_id":std::process::id(),"port":state.port,"base_url":format!("http://127.0.0.1:{}/v1",state.port),"mode":settings.mode,"fixed_source_id":settings.fixed_source_id,"official_low_quota_percent":settings.official_low_quota_percent,"sources":sources,"inflight":state.inflight.load(Ordering::Relaxed),"connection_mode":config::connection_mode(&state.state_dir).unwrap_or("unknown")}),
            )
        }
        "credentials" => Ok(credentials(state).await),
        "models.list" => crate::discovery::list(state, payload).await,
        "sync" => {
            let sources: Vec<Source> =
                serde_json::from_value(payload.get("sources").cloned().context("缺少来源列表")?)
                    .context("来源配置格式错误")?;
            let mode = payload
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("auto");
            let fixed = payload
                .get("fixed_source_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let threshold = match payload.get("official_low_quota_percent") {
                None => 10,
                Some(value) => value
                    .as_u64()
                    .filter(|v| (1..=99).contains(v))
                    .ok_or_else(|| anyhow::anyhow!("官方余量阈值须为1–99的整数"))?
                    as u8,
            };
            if !["auto", "fixed"].contains(&mode) {
                bail!("未知路由模式");
            }
            if sources.len() > 200 {
                bail!("最多支持 200 个来源");
            }
            let mut ids = HashSet::new();
            for source in &sources {
                source.validate()?;
                if !ids.insert(source.id.clone()) {
                    bail!("来源标识不能重复");
                }
            }
            if mode == "fixed"
                && !sources
                    .iter()
                    .any(|s| Some(&s.id) == fixed.as_ref() && s.enabled)
            {
                bail!("请选择已启用的固定来源");
            }
            let mut current = state.sources.write().await;
            let mut next = Vec::new();
            let mut invalidated = HashSet::new();
            for mut source in sources {
                if let Some(previous) = current.iter().find(|r| r.source.id == source.id) {
                    if source.kind == "official" && previous.source.kind == "official" {
                        // The helper owns the refresh generation for this process. Never
                        // overwrite a newer refresh token with the UI's stale snapshot.
                        if previous
                            .source
                            .secret
                            .get("access_token")
                            .and_then(Value::as_str)
                            .is_some_and(|s| !s.is_empty())
                        {
                            source.secret = previous.source.secret.clone();
                            source.account_id = previous.source.account_id.clone();
                        }
                    }
                    if source.secret == previous.source.secret
                        && source.proxy == previous.source.proxy
                        && source.base_url == previous.source.base_url
                        && source.kind == previous.source.kind
                    {
                        let mut kept = previous.clone();
                        kept.source = source;
                        next.push(kept);
                        continue;
                    }
                    if source.base_url != previous.source.base_url
                        || source.kind != previous.source.kind
                        || source.secret != previous.source.secret
                        || source.account_id != previous.source.account_id
                    {
                        invalidated.insert(source.id.clone());
                    }
                }
                next.push(SourceRuntime::new(source));
            }
            state.affinity.retain_identities(
                &next
                    .iter()
                    .map(|runtime| runtime.source.clone())
                    .collect::<Vec<_>>(),
            )?;
            *current = next;
            // Keep the source write lock through binding invalidation. Relay
            // readers and binding writers use sources -> bindings consistently.
            state
                .bindings
                .lock()
                .await
                .retain(|_, source| ids.contains(source) && !invalidated.contains(source));
            drop(current);
            *state.settings.write().await = RoutingSettings {
                mode: mode.into(),
                fixed_source_id: fixed,
                official_low_quota_percent: threshold,
            };
            Ok(credentials(state).await)
        }
        "usage" => state.usage.summary_for(
            payload
                .get("days")
                .and_then(Value::as_u64)
                .unwrap_or(30)
                .clamp(1, 3660) as u32,
            payload.get("source_id").and_then(Value::as_str),
        ),
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
