//! Local settlement boundaries: no provider calls, token polling or historical scans.
use super::{UsageRecord, UsageStore};
use crate::{balance::Snapshot, model::Source};
use anyhow::Result;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tokio::time::{Duration, Instant};

pub(crate) fn identity(source: &Source) -> String {
    let encoded = serde_json::to_vec(&(source.identity(), &source.proxy)).expect("billing identity");
    format!("{:x}", Sha256::digest(encoded))
}

#[derive(Clone, Copy)]
pub(super) struct Cost {
    usd: Option<f64>,
    estimated: bool,
}
impl Cost {
    pub(super) fn read(value: &Value) -> Self {
        Self {
            usd: (value["currency"] == "USD")
                .then(|| value["total_usd"].as_f64())
                .flatten()
                .filter(|v| v.is_finite() && *v >= 0.0),
            estimated: value["billing_basis"] != "upstream_charge",
        }
    }
}

pub(super) struct Ledger {
    identity: String,
    inflight: usize,
    settled: u64,
    acknowledged: u64,
    last_end: Option<Instant>,
    balance: Option<Snapshot>,
    watermark: i64,
    spend: f64,
    estimated: u64,
    unknown: u64,
}
impl Ledger {
    fn new(identity: String) -> Self {
        Self {
            identity,
            inflight: 0,
            settled: 0,
            acknowledged: 0,
            last_end: None,
            balance: None,
            watermark: 0,
            spend: 0.0,
            estimated: 0,
            unknown: 0,
        }
    }
    fn add(&mut self, cost: Cost) {
        if let Some(usd) = cost.usd {
            self.spend += usd;
            self.estimated += u64::from(cost.estimated);
        } else {
            self.unknown += 1;
        }
    }
    fn remove(&mut self, cost: Cost) {
        if let Some(usd) = cost.usd {
            self.spend -= usd;
            self.estimated = self.estimated.saturating_sub(u64::from(cost.estimated));
        } else {
            self.unknown = self.unknown.saturating_sub(1);
        }
    }
}

#[derive(Default)]
pub(super) struct Settlements {
    ledgers: HashMap<String, Ledger>,
    attempts: HashMap<String, (String, String)>,
}
impl Settlements {
    fn ensure(&mut self, source: &Source) -> Option<&mut Ledger> {
        if source.kind != "api" {
            return None;
        }
        let identity = identity(source);
        if !self.ledgers.contains_key(&source.id) && self.ledgers.len() >= 256 {
            let evict = self
                .ledgers
                .iter()
                .find(|(_, ledger)| ledger.inflight == 0 && ledger.settled == ledger.acknowledged)
                .map(|(id, _)| id.clone());
            let id = evict?;
            self.ledgers.remove(&id);
        }
        let ledger = self
            .ledgers
            .entry(source.id.clone())
            .or_insert_with(|| Ledger::new(identity.clone()));
        if ledger.identity != identity {
            *ledger = Ledger::new(identity);
        }
        Some(ledger)
    }
    pub(super) fn started(&mut self, source: &Source, attempt: &str) {
        let Some(ledger) = self.ensure(source) else {
            return;
        };
        ledger.inflight += 1;
        let identity = ledger.identity.clone();
        self.attempts
            .insert(attempt.to_owned(), (source.id.clone(), identity));
    }
    pub(super) fn finish(&mut self, record: &UsageRecord) -> Option<String> {
        let (source, identity) = self.attempts.remove(&record.attempt_id)?;
        if let Some(ledger) = self
            .ledgers
            .get_mut(&source)
            .filter(|l| l.identity == identity)
        {
            ledger.inflight = ledger.inflight.saturating_sub(1);
            ledger.settled = ledger.settled.saturating_add(1);
            ledger.last_end = Some(Instant::now());
        }
        Some(identity)
    }
    pub(super) fn charge(&mut self, source: &str, identity: &str, attempt: i64, cost: Cost) {
        if let Some(ledger) = self
            .ledgers
            .get_mut(source)
            .filter(|l| l.identity == identity && l.balance.is_some() && attempt > l.watermark)
        {
            ledger.add(cost);
        }
    }
    pub(super) fn correct(
        &mut self,
        source: &str,
        identity: &str,
        attempt: i64,
        old: Cost,
        new: Cost,
    ) {
        if let Some(ledger) = self
            .ledgers
            .get_mut(source)
            .filter(|l| l.identity == identity && l.balance.is_some() && attempt > l.watermark)
        {
            ledger.remove(old);
            ledger.add(new);
        }
    }
}

impl UsageStore {
    /// Bind lifecycle accounting to the exact credential, upstream and proxy used.
    pub fn start_for_source(&self, record: &UsageRecord, source: &Source) {
        if let Ok(mut data) = self.data.lock() {
            data.active.insert(
                record.attempt_id.clone(),
                (record.clone(), std::time::Instant::now()),
            );
            data.settlements.started(source, &record.attempt_id);
        }
    }
    pub(crate) fn billing_claim(&self, source: &Source, full: bool) -> bool {
        let Ok(mut data) = self.data.lock() else {
            return false;
        };
        let Some(ledger) = data.settlements.ensure(source) else {
            return false;
        };
        let pending = ledger.settled.saturating_sub(ledger.acknowledged);
        if !full
            && !(pending >= 50
                || (pending > 0
                    && ledger.inflight == 0
                    && ledger
                        .last_end
                        .is_some_and(|at| at.elapsed() >= Duration::from_secs(300))))
        {
            return false;
        }
        // Acknowledge the claimed boundary, never the later worker completion.
        // Failure waits for genuinely new traffic rather than retrying hot.
        ledger.acknowledged = ledger.settled;
        true
    }
    pub(crate) fn reset_balance(&self, source: &Source, snapshot: Snapshot) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        let watermark = data.last_attempt;
        if let Some(ledger) = data.settlements.ensure(source) {
            ledger.balance = Some(snapshot);
            ledger.watermark = watermark;
            ledger.spend = 0.0;
            ledger.estimated = 0;
            ledger.unknown = 0;
        }
        Ok(())
    }
    pub(crate) fn failed_balance(&self, source: &Source, checked_at: u64) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        if let Some(snapshot) = data
            .settlements
            .ensure(source)
            .and_then(|l| l.balance.as_mut())
        {
            snapshot.stale = true;
            snapshot.checked_at = checked_at;
        }
        Ok(())
    }
    pub(crate) fn cached_balance(&self, source: &Source) -> Result<Option<Snapshot>> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("用量数据库不可用"))?;
        let Some(ledger) = data.settlements.ensure(source) else {
            return Ok(None);
        };
        let Some(mut snapshot) = ledger.balance.clone() else {
            return Ok(None);
        };
        snapshot.projection_supported = snapshot.currency.as_deref() == Some("USD");
        if snapshot.projection_supported {
            snapshot.amount = snapshot.amount.map(|amount| amount - ledger.spend);
            snapshot.local_spend = Some(ledger.spend);
            snapshot.estimated = ledger.estimated > 0 || ledger.unknown > 0;
        }
        snapshot.unknown_spend_requests = ledger.unknown;
        Ok(Some(snapshot))
    }
}
