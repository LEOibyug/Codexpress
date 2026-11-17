//! Dispatch admission and one recovery worker for each rejected subscription.
use super::{account, auto_reset, same_account_binding, snapshots, Endpoints};
use crate::model::{AppState, Source};
use anyhow::Result;
use std::{collections::HashMap, sync::{Arc, Mutex}, time::Duration};
use tokio::sync::{oneshot, Notify};

#[cfg(not(test))]
const DEADLINE: Duration = Duration::from_secs(15);
#[cfg(test)]
const DEADLINE: Duration = Duration::from_millis(900);
#[cfg(not(test))]
const POLL_INTERVAL: Duration = Duration::from_millis(500);
#[cfg(test)]
const POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Default)]
struct Admission {
    epoch: u64,
    pending: usize,
    accounts: HashMap<String, AccountRecovery>,
}

struct AccountRecovery {
    rejected_through: u64,
    pending: bool,
    unconfirmed: bool,
}

#[derive(Default)]
pub(crate) struct QuotaRecovery {
    admission: Mutex<Admission>,
    idle: Notify,
}

impl QuotaRecovery {
    pub(crate) fn try_admit(&self) -> Option<u64> {
        let state = self.admission.lock().unwrap();
        (state.pending == 0).then_some(state.epoch)
    }

    pub(crate) async fn wait_idle(&self) {
        loop {
            let idle = self.idle.notified();
            if self.try_admit().is_some() {
                return;
            }
            idle.await;
        }
    }

    pub(crate) fn unconfirmed(&self, source: &Source) -> bool {
        if source.kind != "official" { return false; }
        self.admission.lock().unwrap().accounts.get(&source.identity())
            .is_some_and(|account| account.pending || account.unconfirmed)
    }

    pub(super) fn confirm(&self, source: &Source, from_worker: bool) {
        if let Some(account) = self.admission.lock().unwrap().accounts.get_mut(&source.identity()) {
            if from_worker || !account.pending {
                account.unconfirmed = false;
            }
        }
    }
}

struct Work(Arc<AppState>, String);

impl Work {
    fn begin(state: &Arc<AppState>, key: String, ticket: u64) -> Option<Self> {
        let mut admission = state.quota_recovery.admission.lock().unwrap();
        if admission.accounts.get(&key).is_some_and(|account| ticket <= account.rejected_through) {
            return None;
        }
        // All tickets issued before this latch (including more recent tickets
        // than this reporter's) are superseded for this account alone.
        let epoch = admission.epoch;
        admission.accounts.insert(key.clone(), AccountRecovery { rejected_through: epoch, pending: true, unconfirmed: true });
        admission.pending += 1;
        Some(Self(state.clone(), key))
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let gate = &self.0.quota_recovery;
        let mut admission = gate.admission.lock().unwrap();
        admission.accounts.get_mut(&self.1).unwrap().pending = false;
        admission.pending -= 1;
        if admission.pending == 0 {
            admission.epoch += 1;
            gate.idle.notify_waiters();
        }
    }
}

pub(super) async fn rejected(
    state: &Arc<AppState>, src: &Source, ticket: u64, endpoints: Endpoints<'static>,
) {
    if src.kind != "official" { return; }
    let key = src.identity();
    let Some(gate) = Work::begin(state, key, ticket) else { return; };
    let (published, wait_published) = oneshot::channel();
    let src = src.clone();
    let deadline = tokio::time::Instant::now() + DEADLINE;
    tokio::spawn(async move {
        let _gate = gate;
        let state = &_gate.0;
        // Keep the latch alive and publish the rejection even if its client
        // disconnects while the sources lock is contended.
        crate::routing::mark_rejection(state, &src, Some("usage_limit_reached"), None).await;
        let _ = published.send(());
        let Some(_work) = state.maintenance.enter() else { return; };
        match tokio::time::timeout_at(deadline, recover(state, &src, endpoints)).await {
            Ok(Ok(true)) => state.quota_recovery.confirm(&src, true),
            Ok(Err(error)) => eprintln!("官方额度恢复失败：{error}"),
            _ => {}
        }
    });
    let _ = wait_published.await;
}

// True means the account has recovered or we have a definitive fallback.
async fn recover(state: &Arc<AppState>, attempted: &Source, endpoints: Endpoints<'_>) -> Result<bool> {
    let Some(account_id) = account(attempted) else { return Ok(true) };
    let (mut current, infer_revision) = {
        let sources = state.sources.read().await;
        let Some(runtime) = sources.iter().find(|r| r.source.enabled && same_account_binding(&r.source, attempted))
        else { return Ok(true) };
        let infer = auto_reset::week_only(&runtime.quota).filter(|reset_at| sources.iter().all(|alias| {
            alias.source.id == runtime.source.id || account(&alias.source) != Some(account_id)
                || alias.quota.is_null() || auto_reset::week_only(&alias.quota) == Some(*reset_at)
        }));
        (runtime.source.clone(), infer.map(|_| runtime.health_revision))
    };
    if infer_revision.is_none() {
        snapshots::refresh_quota(state, &current, endpoints, true).await?;
        let refreshed = super::source(state, &current.id).await?;
        if !same_account_binding(&refreshed, &current) { return Ok(true); }
        current = refreshed;
    }
    let enrolled = state.settings.read().await.auto_reset_weekly(&current);
    let outcome = if enrolled {
        auto_reset::consider(state, &current, endpoints, infer_revision).await?
    } else {
        None
    };
    let Some(outcome) = outcome else {
        if infer_revision.is_some() { return Ok(true); }
        let sources = state.sources.read().await;
        return Ok(sources.iter().find(|r| same_account_binding(&r.source, &current))
            .is_some_and(|r| snapshots::confirmed_main_windows(&r.quota).is_some()));
    };
    if outcome["code"] == "no_available_credit" { return Ok(true); }
    // POST success is not quota recovery. Re-read immediately and retain the
    // account block through eventually-consistent or unconfirmed responses.
    loop {
        snapshots::refresh_quota(state, &current, endpoints, true).await?;
        let (exhausted, confirmed) = {
            let sources = state.sources.read().await;
            let Some(runtime) = sources.iter().find(|r| r.source.enabled && same_account_binding(&r.source, &current))
            else { return Ok(true) };
            (crate::routing::official_exhausted(runtime, &sources), snapshots::confirmed_main_windows(&runtime.quota).is_some())
        };
        if !exhausted { return Ok(true); }
        if matches!(outcome["code"].as_str(), Some("no_credit" | "nothing_to_reset")) {
            return Ok(confirmed);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
