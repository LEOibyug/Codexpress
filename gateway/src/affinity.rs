//! App-owned, hashed conversation affinity. Never contains client history or tokens.
use crate::model::{now, Source};
use anyhow::Result;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
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
struct Data {
    connection: Connection,
    entries: HashMap<String, Entry>,
    pending: HashMap<String, Pending>,
}
pub struct AffinityStore {
    data: Mutex<Data>,
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
        let mut data = self.data.lock().ok()?;
        let identity = source.identity();
        if let Some(eligible) = eligible {
            if let Some(entry) = key.and_then(|key| data.entries.get(key)) {
                if entry.source_id != source.id
                    && eligible.iter().any(|candidate| {
                        candidate.id == entry.source_id && candidate.identity() == entry.identity
                    })
                {
                    return None;
                }
            }
        }
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
        let storage_key = key
            .map(str::to_owned)
            .unwrap_or_else(|| format!("ephemeral:{}", uuid::Uuid::new_v4()));
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
            committed: false,
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
    committed: bool,
}
impl RouteLease {
    pub fn commit(&mut self) -> Result<()> {
        if !self.persistent || self.committed {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
