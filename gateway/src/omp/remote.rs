//! Pure remote bundle rendering. Remote paths are identifiers, never local filesystem paths.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};

const NAMES: [&str; 6] = [
    "models.yml", "models.yaml", "models.json", "config.yml", "config.yaml", "settings.json",
];
// Leave room for the private IPC request/response envelope and operation name.
const BUNDLE_LIMIT: usize = 8 * 1024 * 1024 - 16 * 1024;
type Files = BTreeMap<String, Option<Vec<u8>>>;

fn bounded_json(value: &Json) -> Result<()> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            if self.0 > BUNDLE_LIMIT {
                return Err(std::io::Error::other("bundle limit"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    serde_json::to_writer(Counter(0), value)
        .map_err(|_| anyhow::anyhow!("远端 OMP 数据超过 8 MiB 管理通信限制，未修改配置"))
}

fn remote_dir(value: &Json) -> Result<PathBuf> {
    let text = value.as_str().context("缺少远端 OMP 配置目录")?;
    // Canonicalization belongs to the SSH reader: never inspect this path on the Mac.
    if text.len() > 4096 || !text.starts_with('/') || text.chars().any(char::is_control)
        || (text != "/" && text.split('/').skip(1).any(|part| matches!(part, "" | "." | "..")))
    {
        bail!("远端 OMP 目录须为规范绝对路径");
    }
    Ok(PathBuf::from(text))
}

fn decode(value: &Json, limit: usize) -> Result<Option<Vec<u8>>> {
    if value.is_null() { return Ok(None); }
    let encoded = value.as_str().context("远端 OMP 文件须为 base64 或 null")?;
    if encoded.len() > limit.div_ceil(3) * 4 {
        bail!("远端 OMP 文件超过大小限制，未修改配置");
    }
    let bytes = STANDARD.decode(encoded)
        .map_err(|_| anyhow::anyhow!("远端 OMP 文件 base64 无效，未修改配置"))?;
    if bytes.len() > limit { bail!("远端 OMP 文件超过大小限制，未修改配置"); }
    Ok(Some(bytes))
}

fn files(payload: &Json) -> Result<Files> {
    let files = payload["files"].as_object().context("缺少远端 OMP 文件集合")?;
    if files.len() != NAMES.len() || !NAMES.iter().all(|name| files.contains_key(*name)) {
        bail!("远端 OMP 文件集合必须完整包含六个受支持文件名");
    }
    NAMES.iter().map(|name| Ok(((*name).into(), decode(&files[*name], FILE_LIMIT)?))).collect()
}

fn choose_remote(home: &Path, files: &Files, stem: &str, legacy: &str) -> Result<Document> {
    for extension in ["yml", "yaml"] {
        let name = format!("{stem}.{extension}");
        if let Some(bytes) = &files[&name] {
            return Ok(Document { path: home.join(name), value: parse(bytes)?, bytes: Some(bytes.clone()) });
        }
    }
    Ok(Document {
        path: home.join(format!("{stem}.yml")), bytes: None,
        value: files[legacy].as_deref().map(parse).transpose()?.unwrap_or_else(mapping),
    })
}

fn snapshot(value: &Json) -> Result<Option<Snapshot>> {
    let Some(bytes) = decode(value, SNAPSHOT_LIMIT)? else { return Ok(None); };
    let mut snapshot: Snapshot = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("OMP 连接快照损坏，停止覆盖配置"))?;
    if ![1, 2].contains(&snapshot.version)
        || !["models.yml", "models.yaml"].contains(&snapshot.models.name.as_str())
        || !["config.yml", "config.yaml"].contains(&snapshot.settings.name.as_str())
        || snapshot.threshold_before.len() > 200 || snapshot.native_before.len() > 100_000
        || snapshot.status_plugin.is_some() || !snapshot.footer_before.is_empty()
    { bail!("远端 OMP 连接快照无效，停止覆盖配置"); }
    for file in [&snapshot.models, &snapshot.settings] {
        if file.original.as_ref().is_some_and(|bytes| bytes.len() > FILE_LIMIT)
            || file.applied.len() > FILE_LIMIT || file.baseline.len() > FILE_LIMIT
        { bail!("远端 OMP 连接快照超过大小限制"); }
        parse(file.baseline.as_bytes())?;
        parse(&file.applied)?;
    }
    configuration::migrate(&mut snapshot)?;
    Ok(Some(snapshot))
}

fn result(files: Files, snapshot: Option<&Snapshot>, summary: &str) -> Result<Json> {
    let files: BTreeMap<_, _> = files.into_iter()
        .map(|(name, bytes)| (name, bytes.map(|bytes| STANDARD.encode(bytes)))).collect();
    let snapshot = snapshot.map(|snapshot| {
        let bytes = serde_json::to_vec(snapshot)
            .map_err(|_| anyhow::anyhow!("无法生成远端 OMP 连接快照"))?;
        if bytes.len() > SNAPSHOT_LIMIT { bail!("远端 OMP 连接快照超过大小限制"); }
        Ok::<_, anyhow::Error>(STANDARD.encode(bytes))
    }).transpose()?;
    let result = json!({"files": files, "snapshot": snapshot, "summary": summary});
    bounded_json(&result)?;
    Ok(result)
}

pub(super) async fn handle(state: &Arc<AppState>, op: &str, payload: Json) -> Result<Json> {
    bounded_json(&payload)?;
    let home = remote_dir(&payload["agent_dir"])?;
    let mut files = files(&payload)?;
    let agent_db_exists = payload["agent_db_exists"].as_bool().context("缺少远端 OMP 迁移状态")?;
    let models = choose_remote(&home, &files, "models", "models.json")?;
    let settings = choose_remote(&home, &files, "config", "settings.json")?;
    if op == "omp.remote.read" {
        if settings.bytes.is_none() && agent_db_exists {
            bail!("OMP 尚有待迁移的旧配置；请先启动 OMP 完成配置迁移，生成 config.yml 后重试");
        }
        let selected = at(&settings.value, DEFAULT_MODEL)
            .and_then(Value::as_str)
            .and_then(|model| model.strip_prefix("codexpress/"))
            .filter(|model| valid_model(model));
        let catalog_model = payload["model_id"].as_str().filter(|model| valid_model(model)).or(selected);
        let ids = source_models(state, catalog_model.map(upstream_model)).await?;
        let mut catalog = models.value.clone();
        if at(&catalog, PROVIDER).is_none() { put(&mut catalog, PROVIDER, Some(mapping()))?; }
        configuration::merge_models(&mut catalog, model_metadata(&ids)?, None)?;
        let result = json!({
            "agent_dir": home, "model_id": selected,
            "configuration_revision": configuration::revision(&models, &settings),
            "configuration": preferences::configuration(&settings.value, &catalog)?,
        });
        bounded_json(&result)?;
        return Ok(result);
    }
    let previous = snapshot(payload.get("snapshot").context("缺少远端 OMP 连接快照字段")?)?;
    if let Some(snapshot) = &previous {
        same_home(snapshot, &home, &models, &settings)?;
        verify(snapshot, &models, &settings)?;
        configuration::verify_native(snapshot, &models.value, &settings.value)?;
    }
    if op == "omp.remote.restore" {
        if let Some(snapshot) = previous {
            let (models_after, settings_after) = restore_documents(&snapshot, &models, &settings)?;
            files.insert(snapshot.models.name.clone(), models_after);
            files.insert(snapshot.settings.name.clone(), settings_after);
            return result(files, None, "已生成恢复配置，保留其他更改；写入后请重启远端 OMP");
        }
        return result(files, None, "没有需要恢复的远端 OMP 连接");
    }
    if settings.bytes.is_none() && agent_db_exists {
        bail!("OMP 尚有待迁移的旧配置；请先启动 OMP 完成配置迁移，生成 config.yml 后重试");
    }
    let selected = payload["model_id"].as_str().filter(|model| valid_model(model))
        .context("请提供有效的 OMP 默认模型 ID")?;
    let host = payload["host"].as_str().context("缺少远端可访问的本机地址")?;
    crate::remote::validate_host(host)?;
    // The shared chatbot listener currently binds IPv4 only.
    if host.contains(':') { bail!("聊天客户端接入目前仅支持 IPv4 地址或可解析为 IPv4 的主机名"); }
    let _guard = state.config_lock.lock().await;
    let access = state.chatbot.status().await?;
    if access["enabled"] != true || access["running"] != true {
        bail!("请先启用并启动聊天客户端接入");
    }
    let port = access["port"].as_u64().filter(|port| (1..=65535).contains(port))
        .context("聊天客户端端口无效")?;
    let key = state.chatbot.key()?["key"].as_str().context("聊天客户端密钥不可用")?.to_owned();
    let base_url = format!("http://{host}:{port}/v1");
    let selected_model = upstream_model(selected);
    verify_model_access(&settings.value, selected_model)?;
    let ids = source_models(state, Some(selected_model)).await?;
    let (mut snapshot, mut models_after, mut settings_after) = prepare_attachment(
        &home, &models, &settings, &previous, selected, &ids, (&base_url, &key),
    )?;
    let mut configuration_changed = false;
    if let Some(edits) = payload.get("configuration_edits") {
        let edits = preferences::edits(&json!({"edits": edits}), &settings_after, &models_after)?;
        for edit in &edits {
            let document = if edit.file == "config" { &settings_after } else { &models_after };
            let current = edit.path.iter().try_fold(document, |value, field| {
                value.as_mapping()?.get(super::key(field))
            });
            let proposed = edit.value.as_ref().map(serde_yaml::to_value).transpose()?;
            if current != proposed.as_ref() {
                configuration_changed = true;
                break;
            }
        }
        configuration::apply_edits(&mut snapshot, &mut models_after, &mut settings_after, edits)?;
    }
    let configuration = preferences::configuration(&settings_after, &models_after)?;
    snapshot.models.applied = document::patch(models.bytes.as_deref(), &models.value, &models_after)?;
    snapshot.settings.applied = document::patch(settings.bytes.as_deref(), &settings.value, &settings_after)?;
    if snapshot.models.applied.len() > FILE_LIMIT || snapshot.settings.applied.len() > FILE_LIMIT {
        bail!("远端 OMP 配置超过 4 MiB，未修改配置");
    }
    files.insert(snapshot.models.name.clone(), Some(snapshot.models.applied.clone()));
    files.insert(snapshot.settings.name.clone(), Some(snapshot.settings.applied.clone()));
    let summary = format!("默认模型 = codexpress/{selected}\nbaseUrl = {base_url}\n协议 = OpenAI Responses\nAPI Key = 已设置（与聊天客户端共用）\nOMP 配置 = {}\n写入后请重启远端 OMP", if configuration_changed { "已更改" } else { "未更改" });
    let mut result = result(files, Some(&snapshot), &summary)?;
    result["base_url"] = json!(base_url);
    result["key"] = json!(key);
    result["configuration"] = configuration;
    result["configuration_revision"] = json!(configuration::revision(
        &Document { path: models.path, bytes: Some(snapshot.models.applied), value: models_after },
        &Document { path: settings.path, bytes: Some(snapshot.settings.applied), value: settings_after },
    ));
    bounded_json(&result)?;
    Ok(result)
}
