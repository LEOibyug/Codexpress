//! Durable approval queue. A UUID is never reused or silently evicted; uncertain
//! submissions remain terminal here and are never replayed after restart.
use super::safety::{self, Binding};
use crate::{
    config::secure_write,
    model::{now, AppState},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_REQUESTS: usize = 1024;
const CONFIRM_SECONDS: u64 = 15 * 60;
#[derive(Clone, Serialize, Deserialize)]
struct Job {
    id: String,
    binding: Binding,
    requested_credit_id: Option<String>,
    credit_id: Option<String>,
    created_at: u64,
    status: String,
    result: Value,
}
impl Job {
    fn view(&self) -> Value {
        json!({"id":self.id,"source_id":self.binding.source_id,"account_id":self.binding.account_id,"credit_id":self.credit_id,"created_at":self.created_at,"expires_at":self.created_at.saturating_add(CONFIRM_SECONDS),"status":self.status,"result":self.result})
    }
    fn waiting(&self) -> bool {
        matches!(self.status.as_str(), "pending_confirmation" | "presented")
    }
}
pub(super) struct Queue {
    path: PathBuf,
    jobs: Vec<Job>,
}
impl Queue {
    pub fn open(directory: &Path) -> Result<Self> {
        let path = directory.join("control-reset-requests.json");
        let jobs: Vec<Job> = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() && meta.len() <= 4 * 1024 * 1024 => {
                serde_json::from_slice(&std::fs::read(&path)?)
                    .context("管理重置请求记录损坏，禁止提交")?
            }
            Ok(_) => bail!("管理重置请求记录无效，禁止提交"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(_) => bail!("无法读取管理重置请求记录"),
        };
        let mut ids = HashSet::new();
        if jobs.len() > MAX_REQUESTS
            || jobs.iter().any(|job| {
                uuid::Uuid::parse_str(&job.id).is_err()
                    || !ids.insert(job.id.clone())
                    || !matches!(
                        job.status.as_str(),
                        "pending_confirmation"
                            | "presented"
                            | "pending_verification"
                            | "completed"
                            | "cancelled"
                            | "failed"
                    )
            })
        {
            bail!("管理重置请求记录无效，禁止提交");
        }
        let mut queue = Self { path, jobs };
        let mut next = queue.jobs.clone();
        for job in &mut next {
            if job.status == "presented" {
                job.status = "pending_confirmation".into();
            }
        }
        queue.commit(next)?;
        queue.expire()?;
        Ok(queue)
    }
    fn commit(&mut self, jobs: Vec<Job>) -> Result<()> {
        let bytes = serde_json::to_vec(&jobs)?;
        if bytes.len() > 4 * 1024 * 1024 {
            bail!("管理重置请求记录容量已满，拒绝覆盖已有记录");
        }
        secure_write(&self.path, &bytes)
            .map_err(|_| anyhow::anyhow!("无法保存管理重置请求，未允许提交"))?;
        if let Some(parent) = self.path.parent() {
            std::fs::File::open(parent)
                .and_then(|file| file.sync_all())
                .map_err(|_| anyhow::anyhow!("无法落盘管理重置请求，禁止重试消费"))?;
        }
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        self.jobs = jobs;
        Ok(())
    }
    fn expire(&mut self) -> Result<()> {
        if !self
            .jobs
            .iter()
            .any(|job| job.waiting() && now().saturating_sub(job.created_at) >= CONFIRM_SECONDS)
        {
            return Ok(());
        }
        let mut next = self.jobs.clone();
        for job in &mut next {
            if job.waiting() && now().saturating_sub(job.created_at) >= CONFIRM_SECONDS {
                job.status = "cancelled".into();
                job.result = json!({"code":"expired"});
            }
        }
        self.commit(next)
    }
}
fn queue(state: &Arc<AppState>) -> Result<std::sync::MutexGuard<'_, Queue>> {
    state
        .control
        .resets
        .lock()
        .map_err(|_| anyhow::anyhow!("管理重置队列忙碌"))
}
fn uuid(payload: &Value, field: &str) -> Result<String> {
    let id = safety::text(payload, field)?;
    Ok(uuid::Uuid::parse_str(id)
        .context("请求标识必须为 UUID")?
        .to_string())
}
async fn official_binding(state: &Arc<AppState>, id: &str) -> Result<Binding> {
    let sources = state.sources.read().await;
    let source = &sources
        .iter()
        .find(|r| r.source.id == id)
        .context("官方来源不存在")?
        .source;
    if source.kind != "official"
        || source.official_account_id().is_none()
        || source.proxy.mode != "explicit"
    {
        bail!("请选择已登录、指定代理的官方来源");
    }
    source.validate_official_account()?;
    Ok(Binding::of(source))
}
pub(super) async fn enqueue(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    safety::fields(&payload, &["request_id", "source_id", "credit_id"])?;
    let id = uuid(&payload, "request_id")?;
    let source_id = safety::text(&payload, "source_id")?;
    let credit_id = safety::optional_text(&payload, "credit_id")?;
    let binding = official_binding(state, source_id).await?;
    let mut queue = queue(state)?;
    queue.expire()?;
    if let Some(job) = queue.jobs.iter().find(|job| job.id == id) {
        if job.binding != binding || job.requested_credit_id != credit_id {
            bail!("请求标识已绑定其他来源、账号或重置卡");
        }
        return Ok(job.view());
    }
    if queue.jobs.len() >= MAX_REQUESTS {
        bail!("重置请求记录已满，拒绝新请求；已有待确认请求不会被删除");
    }
    let job = Job {
        id,
        binding,
        requested_credit_id: credit_id.clone(),
        credit_id,
        created_at: now(),
        status: "pending_confirmation".into(),
        result: Value::Null,
    };
    let value = job.view();
    let mut next = queue.jobs.clone();
    next.push(job);
    queue.commit(next)?;
    Ok(value)
}
pub(super) fn get(state: &Arc<AppState>, id: &str) -> Result<Option<Value>> {
    let id = uuid::Uuid::parse_str(id)
        .context("请求标识必须为 UUID")?
        .to_string();
    let mut queue = queue(state)?;
    queue.expire()?;
    Ok(queue.jobs.iter().find(|job| job.id == id).map(Job::view))
}
pub(super) async fn pending(state: &Arc<AppState>) -> Result<Value> {
    let sources = state.sources.read().await;
    let mut queue = queue(state)?;
    queue.expire()?;
    let mut next = queue.jobs.clone();
    let mut changed = false;
    for job in &mut next {
        if job.waiting()
            && !sources
                .iter()
                .any(|runtime| job.binding.matches(&runtime.source))
        {
            job.status = "cancelled".into();
            job.result = json!({"code":"source_changed"});
            changed = true;
        }
    }
    if changed {
        queue.commit(next)?;
    }
    Ok(
        json!({"requests":queue.jobs.iter().filter(|job|job.status == "pending_confirmation").map(Job::view).collect::<Vec<_>>()}),
    )
}
pub(super) async fn present(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    safety::fields(&payload, &["id"])?;
    let id = uuid(&payload, "id")?;
    let sources = state.sources.read().await;
    let mut queue = queue(state)?;
    queue.expire()?;
    let mut next = queue.jobs.clone();
    let job = next
        .iter_mut()
        .find(|job| job.id == id)
        .context("重置请求不存在")?;
    if job.waiting()
        && !sources
            .iter()
            .any(|runtime| job.binding.matches(&runtime.source))
    {
        job.status = "cancelled".into();
        job.result = json!({"code":"source_changed"});
    }
    let claimed = job.status == "pending_confirmation";
    if claimed {
        job.status = "presented".into();
    }
    let mut value = job.view();
    value["claimed"] = json!(claimed);
    queue.commit(next)?;
    Ok(value)
}
pub(super) async fn cancel(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    safety::fields(&payload, &["id"])?;
    let id = uuid(&payload, "id")?;
    let mut queue = queue(state)?;
    queue.expire()?;
    let mut next = queue.jobs.clone();
    let job = next
        .iter_mut()
        .find(|job| job.id == id)
        .context("重置请求不存在")?;
    if job.waiting() {
        job.status = "cancelled".into();
        job.result = json!({"code":"cancelled"});
    }
    let value = job.view();
    queue.commit(next)?;
    Ok(value)
}
pub(super) async fn confirm(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    safety::fields(
        &payload,
        &[
            "id",
            "source_id",
            "credit_id",
            "expected_account_id",
            "operation_id",
            "confirmed",
        ],
    )?;
    let id = uuid(&payload, "id")?;
    if payload["confirmed"] != true || safety::text(&payload, "operation_id")? != id {
        bail!("必须在本机确认此请求，操作标识必须一致");
    }
    let source_id = safety::text(&payload, "source_id")?;
    let credit_id = safety::text(&payload, "credit_id")?;
    let account_id = safety::text(&payload, "expected_account_id")?;
    let _routing = state.routing_sync.lock().await;
    let binding = official_binding(state, source_id).await?;
    {
        let mut queue = queue(state)?;
        queue.expire()?;
        let mut next = queue.jobs.clone();
        let job = next
            .iter_mut()
            .find(|job| job.id == id)
            .context("重置请求不存在")?;
        if job.binding != binding
            || job.binding.source_id != source_id
            || job.binding.account_id.as_deref() != Some(account_id)
            || job
                .credit_id
                .as_deref()
                .is_some_and(|credit| credit != credit_id)
        {
            bail!("来源、账号或重置卡已变化，不能提交");
        }
        if matches!(
            job.status.as_str(),
            "completed" | "pending_verification" | "failed"
        ) {
            return Ok(job.result.clone());
        }
        if job.status != "presented" {
            bail!("请求尚未在本机展示确认或已取消");
        }
        job.credit_id = Some(credit_id.to_owned());
        job.status = "pending_verification".into();
        job.result =
            json!({"code":"pending_verification","windows_reset":null,"quota":null,"credits":null});
        queue.commit(next)?;
    }
    // The pending marker precedes every call; no cancellation or duplicate
    // confirmation can reopen this UUID once the operation may reach upstream.
    let outcome = crate::official::handle(state, "official.credits.consume", json!({"source_id":source_id,"credit_id":credit_id,"expected_account_id":account_id,"operation_id":id,"confirmed":true})).await;
    let (status, result) = match outcome {
        Ok(value) => (
            if matches!(
                value["code"].as_str(),
                Some("unknown" | "pending_verification")
            ) {
                "pending_verification"
            } else {
                "completed"
            },
            value,
        ),
        Err(_) => (
            "failed",
            json!({"code":"failed","windows_reset":null,"quota":null,"credits":null}),
        ),
    };
    let mut queue = queue(state)?;
    let mut next = queue.jobs.clone();
    let job = next
        .iter_mut()
        .find(|job| job.id == id)
        .context("重置请求记录丢失，禁止重试")?;
    job.status = status.into();
    job.result = result.clone();
    if queue.commit(next).is_err() {
        return Ok(
            json!({"code":"pending_verification","windows_reset":null,"quota":null,"credits":null}),
        );
    }
    Ok(result)
}
