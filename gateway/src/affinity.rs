//! App-owned, hashed conversation affinity. Never contains client history or tokens.
use crate::model::{now, Source};
use anyhow::Result;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const ACTIVE_SECONDS: u64 = 30 * 60;
const MAX_ENTRIES: usize = 100_000;

#[derive(Clone)]
struct Entry {
    source_id: String,
    identity: String,
    last_seen: u64,
}
struct Pending {
    source_id: String,
    identity: String,
    users: usize,
}
#[derive(Clone, Hash, PartialEq, Eq)]
struct CapacityKey {
    source_id: String,
    identity: String,
    conversation: String,
}
struct CapacityActivity {
    users: usize,
    last_released: Option<Instant>,
}
impl CapacityActivity {
    fn active(&self, timestamp: Instant, seconds: u64) -> bool {
        self.users > 0
            || self.last_released.is_some_and(|released| {
                timestamp.saturating_duration_since(released) < Duration::from_secs(seconds)
            })
    }
}
struct Data {
    connection: Connection,
    entries: HashMap<String, Entry>,
    pending: HashMap<String, Pending>,
    // Capacity has a short, monotonic activity window. Affinity and low-quota
    // protection retain their independent, persisted 30-minute semantics.
    capacity: HashMap<CapacityKey, CapacityActivity>,
}
pub struct AffinityStore {
    data: Mutex<Data>,
}
enum Reservation<'a> {
    Normal {
        continuation: bool,
        eligible: Option<&'a [Source]>,
    },
    IdleReturn {
        from: &'a Source,
        seconds: u64,
    },
}

pub fn key(session: &str, model: &str) -> String {
    let encoded = serde_json::to_vec(&(session, model)).expect("string tuple");
    format!("{:x}", Sha256::digest(encoded))
}

impl AffinityStore {
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        let connection = Connection::open(path)?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
            CREATE TABLE IF NOT EXISTS affinity (key TEXT PRIMARY KEY, source_id TEXT NOT NULL, identity TEXT NOT NULL, last_seen INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS affinity_activity ON affinity(last_seen);")?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS context_exclusions (key TEXT PRIMARY KEY, hashes TEXT NOT NULL, last_seen INTEGER NOT NULL);")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        let entries = {
            let mut statement = connection.prepare("SELECT key,source_id,identity,last_seen FROM affinity ORDER BY last_seen DESC LIMIT 100000")?;
            let entries = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        Entry {
                            source_id: row.get(1)?,
                            identity: row.get(2)?,
                            last_seen: row.get(3)?,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<HashMap<_, _>>>()?;
            entries
        };
        connection.execute("DELETE FROM affinity WHERE key NOT IN (SELECT key FROM affinity ORDER BY last_seen DESC LIMIT 100000)",[])?;
        Ok(Arc::new(Self {
            data: Mutex::new(Data {
                connection,
                entries,
                pending: HashMap::new(),
                capacity: HashMap::new(),
            }),
        }))
    }
    pub fn preferred(&self, key: &str, sources: &[Source]) -> Option<String> {
        let data = self.data.lock().ok()?;
        let (id, identity) = if let Some(pending) = data.pending.get(key) {
            (&pending.source_id, &pending.identity)
        } else {
            let entry = data.entries.get(key)?;
            (&entry.source_id, &entry.identity)
        };
        sources
            .iter()
            .find(|source| &source.id == id && source.identity() == *identity)
            .map(|source| source.id.clone())
    }
    pub fn preferred_within(&self, key: &str, sources: &[Source], seconds: u64) -> Option<String> {
        let data = self.data.lock().ok()?;
        let (id, identity) = if let Some(pending) = data.pending.get(key) {
            (&pending.source_id, &pending.identity)
        } else {
            let entry = data.entries.get(key)?;
            if seconds > 0 && now().saturating_sub(entry.last_seen) >= seconds {
                return None;
            }
            (&entry.source_id, &entry.identity)
        };
        sources
            .iter()
            .find(|source| &source.id == id && source.identity() == *identity)
            .map(|s| s.id.clone())
    }
    pub fn exclusions(&self, key: &str) -> HashSet<String> {
        let Ok(data) = self.data.lock() else {
            return HashSet::new();
        };
        data.connection
            .query_row(
                "SELECT hashes FROM context_exclusions WHERE key=?1",
                [key],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }
    pub fn confirm_exclusions(&self, key: &str, hashes: &HashSet<String>) -> Result<()> {
        self.update_exclusions(key, hashes, &HashSet::new())
    }
    pub fn confirm_acceptance(
        &self,
        key: &str,
        hashes: &HashSet<String>,
        restored: &HashSet<String>,
    ) -> Result<()> {
        self.update_exclusions(key, hashes, restored)
    }
    fn update_exclusions(
        &self,
        key: &str,
        hashes: &HashSet<String>,
        restored: &HashSet<String>,
    ) -> Result<()> {
        let data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("上下文状态不可用"))?;
        let mut merged: HashSet<String> = data
            .connection
            .query_row(
                "SELECT hashes FROM context_exclusions WHERE key=?1",
                [key],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        merged.retain(|key| !restored.contains(key));
        merged.extend(hashes.iter().cloned());
        if merged.len() > 10000 {
            anyhow::bail!("上下文降级记录超过限制");
        }
        data.connection.execute("INSERT INTO context_exclusions(key,hashes,last_seen) VALUES (?1,?2,?3) ON CONFLICT(key) DO UPDATE SET hashes=excluded.hashes,last_seen=excluded.last_seen",params![key,serde_json::to_string(&merged)?,now()])?;
        data.connection.execute("DELETE FROM context_exclusions WHERE key NOT IN (SELECT key FROM context_exclusions ORDER BY last_seen DESC LIMIT 100000)",[])?;
        Ok(())
    }
    pub fn active_count(&self, source: &Source) -> usize {
        let Ok(data) = self.data.lock() else { return 0 };
        active_keys(&data, source, now()).len()
    }
    pub fn protected_active_count(&self, source: &Source) -> usize {
        let Ok(data) = self.data.lock() else { return 0 };
        active_identity_keys(&data, source, now()).len()
    }
    pub fn capacity_status(&self, source: &Source) -> (usize, bool) {
        let Ok(data) = self.data.lock() else {
            return (0, false);
        };
        let active = capacity_count(&data, source, Instant::now());
        (
            active,
            source
                .max_active_conversations
                .is_none_or(|limit| active < limit as usize),
        )
    }
    pub fn order_by_load(&self, sources: &mut [Source]) {
        let Ok(data) = self.data.lock() else { return };
        let timestamp = Instant::now();
        let counts: HashMap<_, _> = sources
            .iter()
            .map(|source| (source.id.clone(), capacity_count(&data, source, timestamp)))
            .collect();
        sources.sort_by(|a, b| {
            counts[&a.id]
                .cmp(&counts[&b.id])
                .then(a.priority.cmp(&b.priority))
                .then(a.id.cmp(&b.id))
        });
    }
    /// Candidate filtering is advisory; reserve repeats this check and takes
    /// the slot while holding the same mutex as every competing reservation.
    pub fn capacity_available(&self, key: Option<&str>, source: &Source) -> bool {
        let Ok(data) = self.data.lock() else {
            return false;
        };
        capacity_allows(&data, key, source, Instant::now())
    }
    /// Check low-quota admission and reserve before any network await. Callers
    /// hold sources.read so identity cannot change between validation/reservation.
    pub fn reserve(
        self: &Arc<Self>,
        key: Option<&str>,
        source: &Source,
        continuation: bool,
        low: bool,
        eligible: Option<&[Source]>,
    ) -> Option<RouteLease> {
        self.reserve_with_policy(
            key,
            source,
            low,
            Reservation::Normal {
                continuation,
                eligible,
            },
        )
    }
    pub fn reserve_idle_return(
        self: &Arc<Self>,
        key: &str,
        from: &Source,
        source: &Source,
        low: bool,
        seconds: u64,
    ) -> Option<RouteLease> {
        self.reserve_with_policy(
            Some(key),
            source,
            low,
            Reservation::IdleReturn { from, seconds },
        )
    }
    fn reserve_with_policy(
        self: &Arc<Self>,
        key: Option<&str>,
        source: &Source,
        low: bool,
        policy: Reservation<'_>,
    ) -> Option<RouteLease> {
        let mut data = self.data.lock().ok()?;
        let identity = source.identity();
        let continuation = match policy {
            Reservation::Normal {
                continuation,
                eligible,
            } => {
                if let Some(eligible) = eligible {
                    if let Some(entry) = key.and_then(|key| data.entries.get(key)) {
                        if entry.source_id != source.id
                            && eligible.iter().any(|candidate| {
                                candidate.id == entry.source_id
                                    && candidate.identity() == entry.identity
                                    && capacity_allows(&data, key, candidate, Instant::now())
                            })
                        {
                            return None;
                        }
                    }
                }
                continuation
            }
            Reservation::IdleReturn { from, seconds } => {
                let key = key?;
                let entry = data.entries.get(key)?;
                if seconds == 0
                    || source.priority >= from.priority
                    || entry.source_id != from.id
                    || entry.identity != from.identity()
                    || data.pending.contains_key(key)
                    || now().saturating_sub(entry.last_seen) < seconds
                {
                    return None;
                }
                false
            }
        };
        let known = key
            .and_then(|key| data.entries.get(key))
            .is_some_and(|entry| {
                entry.identity == identity
                    && (entry.source_id == source.id || source.kind == "official")
            });
        let pending = key.and_then(|key| data.pending.get(key));
        if pending
            .is_some_and(|pending| pending.source_id != source.id || pending.identity != identity)
        {
            return None;
        }
        if low
            && !known
            && !continuation
            && pending.is_none()
            && !active_identity_keys(&data, source, now()).is_empty()
        {
            return None;
        }
        let timestamp = Instant::now();
        // Every configured window is at most five minutes. Keep live requests
        // regardless of age; only completed, globally expired slots are pruned.
        data.capacity
            .retain(|_, activity| activity.active(timestamp, 300));
        if !capacity_allows(&data, key, source, timestamp) {
            return None;
        }
        let storage_key = key
            .map(str::to_owned)
            .unwrap_or_else(|| format!("ephemeral:{}", uuid::Uuid::new_v4()));
        let capacity_key = CapacityKey {
            source_id: source.id.clone(),
            identity: identity.clone(),
            conversation: storage_key.clone(),
        };
        data.capacity
            .entry(capacity_key.clone())
            .and_modify(|activity| activity.users += 1)
            .or_insert(CapacityActivity {
                users: 1,
                last_released: None,
            });
        data.pending
            .entry(storage_key.clone())
            .and_modify(|pending| pending.users += 1)
            .or_insert(Pending {
                source_id: source.id.clone(),
                identity: identity.clone(),
                users: 1,
            });
        Some(RouteLease {
            store: self.clone(),
            key: storage_key,
            persistent: key.is_some(),
            source_id: source.id.clone(),
            identity,
            capacity_key,
            committed: false,
            retain_capacity: false,
        })
    }
    pub fn retain_identities(&self, sources: &[Source]) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("会话路由存储不可用"))?;
        let identities: HashMap<_, _> = sources
            .iter()
            .map(|source| (source.id.clone(), source.identity()))
            .collect();
        let deleted: Vec<_> = data
            .entries
            .iter()
            .filter(|(_, entry)| identities.get(&entry.source_id) != Some(&entry.identity))
            .map(|(key, _)| key.clone())
            .collect();
        for key in deleted {
            data.connection
                .execute("DELETE FROM affinity WHERE key=?1", [&key])?;
            data.entries.remove(&key);
        }
        // Keep live leases: a relogin cannot make an old pending request appear
        // to belong to the new identity; relay checks the identity at commit.
        Ok(())
    }
}
fn capacity_count(data: &Data, source: &Source, timestamp: Instant) -> usize {
    let identity = source.identity();
    data.capacity
        .iter()
        .filter(|(key, activity)| {
            key.source_id == source.id
                // Replacing credentials cannot hide a request still using
                // this source. Completed activity belongs only to its identity.
                && (key.identity == identity || activity.users > 0)
                && activity.active(timestamp, source.active_conversation_window_seconds)
        })
        .count()
}
fn capacity_allows(
    data: &Data,
    conversation: Option<&str>,
    source: &Source,
    timestamp: Instant,
) -> bool {
    let Some(limit) = source.max_active_conversations else {
        return true;
    };
    let active = capacity_count(data, source, timestamp);
    let identity = source.identity();
    let existing = conversation.is_some_and(|conversation| {
        data.capacity
            .get(&CapacityKey {
                source_id: source.id.clone(),
                identity,
                conversation: conversation.to_owned(),
            })
            .is_some_and(|activity| {
                // Sharing an inflight request never adds a conversation.
                // A retained idle slot may resume only after a lowered limit
                // accommodates all current activity, so it cannot stay above
                // the new limit by alternating requests indefinitely.
                activity.users > 0
                    || (active <= limit as usize
                        && activity.active(timestamp, source.active_conversation_window_seconds))
            })
    });
    existing || active < limit as usize
}
fn active_keys(data: &Data, source: &Source, timestamp: u64) -> HashSet<String> {
    let identity = source.identity();
    let mut keys: HashSet<_> = data
        .entries
        .iter()
        .filter(|(_, entry)| {
            entry.source_id == source.id
                && entry.identity == identity
                && timestamp.saturating_sub(entry.last_seen) <= ACTIVE_SECONDS
        })
        .map(|(key, _)| key.clone())
        .collect();
    keys.extend(
        data.pending
            .iter()
            .filter(|(_, pending)| pending.source_id == source.id && pending.identity == identity)
            .map(|(key, _)| key.clone()),
    );
    keys
}
fn active_identity_keys(data: &Data, source: &Source, timestamp: u64) -> HashSet<String> {
    if source.kind != "official" {
        return active_keys(data, source, timestamp);
    }
    let identity = source.identity();
    let mut keys: HashSet<_> = data
        .entries
        .iter()
        .filter(|(_, entry)| {
            entry.identity == identity
                && timestamp.saturating_sub(entry.last_seen) <= ACTIVE_SECONDS
        })
        .map(|(key, _)| key.clone())
        .collect();
    keys.extend(
        data.pending
            .iter()
            .filter(|(_, pending)| pending.identity == identity)
            .map(|(key, _)| key.clone()),
    );
    keys
}

pub struct RouteLease {
    store: Arc<AffinityStore>,
    key: String,
    persistent: bool,
    source_id: String,
    identity: String,
    capacity_key: CapacityKey,
    committed: bool,
    retain_capacity: bool,
}
impl RouteLease {
    pub fn commit(&mut self) -> Result<()> {
        if self.committed {
            return Ok(());
        }
        // A successful HTTP response still consumed an activity slot if the
        // separate durable affinity write cannot be completed.
        self.retain_capacity = true;
        if !self.persistent {
            self.committed = true;
            return Ok(());
        }
        let mut data = self
            .store
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("会话路由存储不可用"))?;
        let timestamp = now();
        data.connection.execute("INSERT INTO affinity(key,source_id,identity,last_seen) VALUES (?1,?2,?3,?4) ON CONFLICT(key) DO UPDATE SET source_id=excluded.source_id,identity=excluded.identity,last_seen=excluded.last_seen",params![self.key,self.source_id,self.identity,timestamp])?;
        data.entries.insert(
            self.key.clone(),
            Entry {
                source_id: self.source_id.clone(),
                identity: self.identity.clone(),
                last_seen: timestamp,
            },
        );
        if data.entries.len() > MAX_ENTRIES {
            let oldest = data
                .entries
                .iter()
                .filter(|(key, _)| !data.pending.contains_key(*key))
                .min_by_key(|(_, entry)| entry.last_seen)
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest {
                data.connection
                    .execute("DELETE FROM affinity WHERE key=?1", [&key])?;
                data.entries.remove(&key);
            }
        }
        self.committed = true;
        Ok(())
    }
}
impl Drop for RouteLease {
    fn drop(&mut self) {
        if let Ok(mut data) = self.store.data.lock() {
            if self.committed
                && data.entries.get(&self.key).is_some_and(|entry| {
                    entry.source_id == self.source_id && entry.identity == self.identity
                })
            {
                let timestamp = now();
                if data.connection.execute("UPDATE affinity SET last_seen=?1 WHERE key=?2 AND source_id=?3 AND identity=?4",params![timestamp,self.key,self.source_id,self.identity]).is_err() {eprintln!("会话活动时间保存失败");}
                if let Some(entry) = data.entries.get_mut(&self.key) {
                    entry.last_seen = timestamp;
                }
            }
            if let Some(pending) = data.pending.get_mut(&self.key) {
                pending.users = pending.users.saturating_sub(1);
                if pending.users == 0 {
                    data.pending.remove(&self.key);
                }
            }
            if let Some(activity) = data.capacity.get_mut(&self.capacity_key) {
                activity.users = activity.users.saturating_sub(1);
                if self.retain_capacity {
                    activity.last_released = Some(Instant::now());
                }
                // A cancelled plan or rejected attempt never creates a new
                // retained conversation without a successful response.
                if activity.users == 0 && activity.last_released.is_none() {
                    data.capacity.remove(&self.capacity_key);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn capacity_source() -> Source {
        serde_json::from_value(serde_json::json!({"id":"limited","name":"limited","kind":"api","base_url":"http://127.0.0.1:1","enabled":true,"priority":0,"max_active_conversations":1,"active_conversation_window_seconds":1,"proxy":{"mode":"direct"},"secret":{"api_key":"fixture"}})).unwrap()
    }

    #[test]
    fn capacity_never_expires_inflight_and_releases_after_its_independent_idle_window() {
        let directory = tempfile::tempdir().unwrap();
        let store = AffinityStore::open(&directory.path().join("affinity.sqlite")).unwrap();
        let mut source = capacity_source();
        let key = key("long-stream", "m");
        let mut stream = store
            .reserve(Some(&key), &source, false, false, None)
            .unwrap();
        stream.commit().unwrap();
        {
            let mut data = store.data.lock().unwrap();
            data.entries.get_mut(&key).unwrap().last_seen = now() - ACTIVE_SECONDS - 1;
            data.capacity
                .get_mut(&stream.capacity_key)
                .unwrap()
                .last_released = Some(Instant::now() - Duration::from_secs(3600));
        }
        // A smaller window and elapsed wall/monotonic time cannot expire a
        // live request. The same conversation can still share its live slot.
        source.active_conversation_window_seconds = 1;
        assert_eq!(store.capacity_status(&source), (1, false));
        assert!(store.reserve(None, &source, true, false, None).is_none());
        let mut parallel = store
            .reserve(Some(&key), &source, true, false, None)
            .unwrap();
        parallel.commit().unwrap();
        let capacity_key = stream.capacity_key.clone();
        drop(stream);
        assert_eq!(store.capacity_status(&source), (1, false));
        drop(parallel);
        assert_eq!(store.capacity_status(&source), (1, false));
        assert!(store.reserve(None, &source, false, false, None).is_none());
        {
            let mut data = store.data.lock().unwrap();
            data.capacity.get_mut(&capacity_key).unwrap().last_released =
                Some(Instant::now() - Duration::from_secs(2));
        }
        assert_eq!(store.capacity_status(&source), (0, true));
        // The 30-minute affinity/low-quota activity remains intact.
        assert_eq!(store.active_count(&source), 1);
        let occupied = store
            .reserve(Some("other"), &source, false, false, None)
            .unwrap();
        assert!(store
            .reserve(Some(&key), &source, true, false, None)
            .is_none());
        drop(occupied);
    }

    #[test]
    fn successful_anonymous_activity_has_a_short_tail_but_cancelled_plans_do_not() {
        let directory = tempfile::tempdir().unwrap();
        let store = AffinityStore::open(&directory.path().join("affinity.sqlite")).unwrap();
        let source = capacity_source();
        let planned = store.reserve(None, &source, false, false, None).unwrap();
        drop(planned);
        assert_eq!(store.capacity_status(&source), (0, true));
        let mut request = store.reserve(None, &source, false, false, None).unwrap();
        let capacity_key = request.capacity_key.clone();
        request.commit().unwrap();
        drop(request);
        assert_eq!(store.capacity_status(&source), (1, false));
        assert_eq!(store.active_count(&source), 0);
        store
            .data
            .lock()
            .unwrap()
            .capacity
            .get_mut(&capacity_key)
            .unwrap()
            .last_released = Some(Instant::now() - Duration::from_secs(2));
        assert!(store.reserve(None, &source, false, false, None).is_some());
    }

    #[test]
    fn lowering_capacity_blocks_retained_slots_but_preserves_inflight_sharing() {
        let directory = tempfile::tempdir().unwrap();
        let store = AffinityStore::open(&directory.path().join("affinity.sqlite")).unwrap();
        let mut source = capacity_source();
        source.max_active_conversations = Some(2);
        source.active_conversation_window_seconds = 15;
        let first_key = key("first", "m");
        let second_key = key("second", "m");
        let mut first = store
            .reserve(Some(&first_key), &source, false, false, None)
            .unwrap();
        let mut second = store
            .reserve(Some(&second_key), &source, false, false, None)
            .unwrap();
        let second_capacity_key = second.capacity_key.clone();
        first.commit().unwrap();
        second.commit().unwrap();
        drop((first, second));

        source.max_active_conversations = Some(1);
        assert_eq!(store.capacity_status(&source), (2, false));
        for conversation in [&first_key, &second_key] {
            assert!(!store.capacity_available(Some(conversation), &source));
            assert!(store
                .reserve(Some(conversation), &source, true, false, None)
                .is_none());
        }

        source.max_active_conversations = Some(2);
        let mut inflight = store
            .reserve(Some(&first_key), &source, true, false, None)
            .unwrap();
        inflight.commit().unwrap();
        source.max_active_conversations = Some(1);
        let mut parallel = store
            .reserve(Some(&first_key), &source, true, false, None)
            .expect("inflight conversation keeps sharing its existing slot");
        parallel.commit().unwrap();
        assert!(store
            .reserve(Some(&second_key), &source, true, false, None)
            .is_none());
        drop(inflight);
        assert!(store.capacity_available(Some(&first_key), &source));
        drop(parallel);
        assert!(!store.capacity_available(Some(&first_key), &source));

        store
            .data
            .lock()
            .unwrap()
            .capacity
            .get_mut(&second_capacity_key)
            .unwrap()
            .last_released = Some(Instant::now() - Duration::from_secs(16));
        assert_eq!(store.capacity_status(&source), (1, false));
        assert!(store
            .reserve(Some(&first_key), &source, true, false, None)
            .is_some());
        assert!(store
            .reserve(Some(&second_key), &source, true, false, None)
            .is_none());
    }

    #[test]
    fn idle_return_obeys_target_capacity_and_identity_changes_keep_live_requests_counted() {
        let directory = tempfile::tempdir().unwrap();
        let store = AffinityStore::open(&directory.path().join("affinity.sqlite")).unwrap();
        let target = capacity_source();
        let mut from = target.clone();
        from.id = "fallback".into();
        from.priority = 1;
        let key = key("idle-return", "m");
        let mut old = store
            .reserve(Some(&key), &from, false, false, None)
            .unwrap();
        old.commit().unwrap();
        drop(old);
        store
            .data
            .lock()
            .unwrap()
            .entries
            .get_mut(&key)
            .unwrap()
            .last_seen = now() - 60;
        let busy = store
            .reserve(Some("target-busy"), &target, false, false, None)
            .unwrap();
        assert!(store
            .reserve_idle_return(&key, &from, &target, false, 30)
            .is_none());
        let mut replacement = target.clone();
        replacement.secret = serde_json::json!({"api_key":"changed"});
        store
            .retain_identities(&[from.clone(), replacement.clone()])
            .unwrap();
        assert_eq!(store.capacity_status(&replacement), (1, false));
        assert!(store
            .reserve(
                Some("new-identity-thread"),
                &replacement,
                false,
                false,
                None
            )
            .is_none());
        assert!(store
            .reserve(Some("target-busy"), &replacement, true, false, None)
            .is_none());
        drop(busy);
        let returning = store
            .reserve_idle_return(&key, &from, &replacement, false, 30)
            .unwrap();
        assert_eq!(store.capacity_status(&replacement), (1, false));
        drop(returning);
    }

    #[test]
    fn idle_return_waits_for_stream_end_and_restarts_protection_from_release() {
        let directory = tempfile::tempdir().unwrap();
        let store = AffinityStore::open(&directory.path().join("affinity.sqlite")).unwrap();
        let from: Source = serde_json::from_value(serde_json::json!({"id":"api","name":"api","kind":"api","base_url":"http://127.0.0.1:1","enabled":true,"priority":1,"proxy":{"mode":"direct"},"secret":{"api_key":"fixture"}})).unwrap();
        let mut target = from.clone();
        target.id = "preferred".into();
        target.priority = 0;
        let key = key("busy-thread", "m");
        let mut stream = store
            .reserve(Some(&key), &from, false, false, None)
            .unwrap();
        stream.commit().unwrap();
        store
            .data
            .lock()
            .unwrap()
            .entries
            .get_mut(&key)
            .unwrap()
            .last_seen = now() - 3600;
        assert_eq!(
            store
                .preferred_within(&key, &[from.clone(), target.clone()], 300)
                .as_deref(),
            Some("api")
        );
        assert!(store
            .reserve_idle_return(&key, &from, &target, false, 1800)
            .is_none());
        drop(stream);
        assert!(store
            .reserve_idle_return(&key, &from, &target, false, 1800)
            .is_none());
        store
            .data
            .lock()
            .unwrap()
            .entries
            .get_mut(&key)
            .unwrap()
            .last_seen = now() - 1800;
        assert!(store
            .preferred_within(&key, &[from.clone(), target.clone()], 300)
            .is_none());
        assert!(store
            .reserve_idle_return(&key, &from, &target, false, 0)
            .is_none());
        let returning = store
            .reserve_idle_return(&key, &from, &target, false, 1800)
            .unwrap();
        assert!(store
            .reserve(Some(&key), &from, false, false, None)
            .is_none());
        drop(returning);
        assert_eq!(
            store.preferred(&key, &[from, target]).as_deref(),
            Some("api")
        );
    }
    #[test]
    fn releasing_long_lived_stream_touches_only_its_still_current_identity() {
        let directory = tempfile::tempdir().unwrap();
        let store = AffinityStore::open(&directory.path().join("affinity.sqlite")).unwrap();
        let source:Source=serde_json::from_value(serde_json::json!({"id":"a","name":"a","kind":"api","base_url":"http://127.0.0.1:1","enabled":true,"priority":0,"proxy":{"mode":"direct"},"secret":{"api_key":"fixture"}})).unwrap();
        let key = key("thread", "m");
        let mut lease = store
            .reserve(Some(&key), &source, false, false, None)
            .unwrap();
        lease.commit().unwrap();
        {
            let mut data = store.data.lock().unwrap();
            data.entries.get_mut(&key).unwrap().last_seen = now() - ACTIVE_SECONDS - 100;
        }
        assert_eq!(store.active_count(&source), 1);
        drop(lease);
        assert_eq!(store.active_count(&source), 1);
        let after = store.data.lock().unwrap().entries[&key].last_seen;
        assert!(now().saturating_sub(after) < 5);
        let mut old = store
            .reserve(Some(&key), &source, false, false, None)
            .unwrap();
        old.commit().unwrap();
        let mut replacement = source.clone();
        replacement.secret = serde_json::json!({"api_key":"changed"});
        store.retain_identities(&[replacement]).unwrap();
        drop(old);
        assert!(store.data.lock().unwrap().entries.is_empty());
    }
}
