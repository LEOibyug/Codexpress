//! Owner-only recovery cache. Bodies live here, never in logs or usage metadata.
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, ErrorCode, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub const DEFAULT_LIMIT: u64 = 5_000_000_000;
pub const MIN_LIMIT: u64 = 64_000_000;
pub const MAX_LIMIT: u64 = 1_000_000_000_000;
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub struct HistoryConfig {
    pub enabled: bool,
    pub max_bytes: u64,
}
impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_bytes: DEFAULT_LIMIT,
        }
    }
}
struct Data {
    db: Connection,
    config: HistoryConfig,
    tick: u64,
    evicted: u64,
    skipped: u64,
    error: Option<&'static str>,
    generation: u64,
}
pub struct HistoryStore {
    data: Mutex<Data>,
    path: PathBuf,
}
struct Block {
    hash: String,
    raw: usize,
    compressed: Vec<u8>,
}
struct Node {
    hash: String,
    parent: String,
    block: String,
    end: bool,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn snapshot_key(scope: &str, id: &str) -> String {
    hash(&serde_json::to_vec(&(scope, id)).expect("strings"))
}
fn node_hash(parent: &str, block: &str, end: bool) -> String {
    hash(&serde_json::to_vec(&(parent, block, end)).expect("strings"))
}

impl HistoryStore {
    pub fn open(path: &Path) -> Result<Self> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            if !metadata.file_type().is_file() {
                bail!("原文缓存路径不是普通文件");
            }
        }
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let db = Connection::open(path).context("无法打开原文缓存")?;
        db.execute_batch("PRAGMA auto_vacuum=FULL; PRAGMA journal_mode=DELETE; PRAGMA secure_delete=ON;
            CREATE TABLE IF NOT EXISTS blocks(hash TEXT PRIMARY KEY, payload BLOB NOT NULL, raw_bytes INTEGER NOT NULL, last_used INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS blocks_lru ON blocks(last_used);
            CREATE TABLE IF NOT EXISTS nodes(hash TEXT PRIMARY KEY, parent TEXT NOT NULL, block TEXT NOT NULL, item_end INTEGER NOT NULL, last_used INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS nodes_lru ON nodes(last_used);
            CREATE TABLE IF NOT EXISTS snapshots(key TEXT PRIMARY KEY, tail TEXT NOT NULL, envelope TEXT NOT NULL, last_used INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS snapshots_lru ON snapshots(last_used);
            CREATE TABLE IF NOT EXISTS preferences(id INTEGER PRIMARY KEY, enabled INTEGER NOT NULL, max_bytes INTEGER NOT NULL);")?;
        let config = db
            .query_row(
                "SELECT enabled,max_bytes FROM preferences WHERE id=1",
                [],
                |r| {
                    Ok(HistoryConfig {
                        enabled: r.get(0)?,
                        max_bytes: r.get(1)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_default();
        let tick = db.query_row("SELECT MAX(COALESCE((SELECT MAX(last_used) FROM blocks),0),COALESCE((SELECT MAX(last_used) FROM nodes),0))", [], |r| r.get(0))?;
        let store = Self {
            data: Mutex::new(Data {
                db,
                config,
                tick,
                evicted: 0,
                skipped: 0,
                error: None,
                generation: 0,
            }),
            path: path.to_owned(),
        };
        {
            let mut data = store
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("原文缓存忙碌"))?;
            apply_limit(&mut data)?;
        }
        Ok(store)
    }
    pub fn enabled(&self) -> bool {
        self.data.lock().map(|d| d.config.enabled).unwrap_or(false)
    }
    pub fn configure(&self, enabled: bool, max_bytes: u64) -> Result<Value> {
        if !(MIN_LIMIT..=MAX_LIMIT).contains(&max_bytes) {
            bail!("缓存容量须为 64 MB–1 TB");
        }
        self.configure_inner(enabled, max_bytes)?;
        self.status()
    }
    fn configure_inner(&self, enabled: bool, max_bytes: u64) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("原文缓存忙碌"))?;
        data.db.execute(
            "INSERT OR REPLACE INTO preferences VALUES(1,?1,?2)",
            params![enabled, max_bytes],
        )?;
        if data.config.enabled != enabled {
            data.generation += 1;
        }
        data.config = HistoryConfig { enabled, max_bytes };
        apply_limit(&mut data)?;
        data.error = None;
        Ok(())
    }
    pub fn status(&self) -> Result<Value> {
        let data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("原文缓存忙碌"))?;
        let (blocks, compressed, raw): (u64,u64,u64) = data.db.query_row("SELECT COUNT(*),COALESCE(SUM(length(payload)),0),COALESCE(SUM(raw_bytes),0) FROM blocks", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        Ok(
            json!({"enabled":data.config.enabled,"max_bytes":data.config.max_bytes,"disk_bytes":std::fs::metadata(&self.path).map(|m|m.len()).unwrap_or(0),"compressed_bytes":compressed,"raw_bytes":raw,"blocks":blocks,"evicted_blocks":data.evicted,"skipped_captures":data.skipped,"error":data.error}),
        )
    }
    pub fn clear(&self) -> Result<Value> {
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("原文缓存忙碌"))?;
            data.db.execute_batch("BEGIN IMMEDIATE; DELETE FROM snapshots; DELETE FROM nodes; DELETE FROM blocks; COMMIT;")?;
            data.error = None;
            data.generation += 1;
            data.evicted = 0;
            data.skipped = 0;
        }
        self.status()
    }
    pub fn capture(&self, scope: &str, request: &Value, response: &Value) -> Result<()> {
        self.capture_at(scope, request, response, self.generation())
    }
    pub fn generation(&self) -> u64 {
        self.data.lock().map(|d| d.generation).unwrap_or(u64::MAX)
    }
    pub fn capture_at(
        &self,
        scope: &str,
        request: &Value,
        response: &Value,
        generation: u64,
    ) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("原文缓存忙碌"))?;
        if !data.config.enabled || generation != data.generation {
            return Ok(());
        }
        let result = capture(&mut data, scope, request, response);
        if result.is_err() {
            data.skipped += 1;
            data.error = Some("部分正文未能缓存，相关请求将无法使用本地历史重建。");
        }
        result.map_err(|_| anyhow::anyhow!("原文缓存写入失败"))
    }
    pub fn restore(&self, scope: &str, response_id: &str) -> Result<Vec<Value>> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("原文缓存忙碌"))?;
        if !data.config.enabled {
            bail!("原文缓存未启用");
        }
        restore(&mut data, scope, response_id).map_err(|_| {
            anyhow::anyhow!(
                "本地历史不完整：所需增量块已淘汰、未采集或无法校验。请让客户端发送完整上下文。"
            )
        })
    }
    pub fn record_skip(&self) {
        if let Ok(mut data) = self.data.lock() {
            if data.config.enabled {
                data.skipped += 1;
            }
        }
    }
}
fn disk_bytes(db: &Connection) -> Result<u64> {
    let pages: u64 = db.pragma_query_value(None, "page_count", |r| r.get(0))?;
    let page_size: u64 = db.pragma_query_value(None, "page_size", |r| r.get(0))?;
    Ok(pages * page_size)
}
fn apply_limit(data: &mut Data) -> Result<()> {
    while disk_bytes(&data.db)? > data.config.max_bytes {
        if !evict(
            data,
            &HashSet::new(),
            &HashSet::new(),
            disk_bytes(&data.db)?
                .saturating_sub(data.config.max_bytes)
                .min(4 * 1024 * 1024),
        )? {
            bail!("缓存容量小于索引所需空间");
        }
    }
    let size: u64 = data
        .db
        .pragma_query_value(None, "page_size", |r| r.get(0))?;
    data.db
        .pragma_update(None, "max_page_count", data.config.max_bytes / size)?;
    Ok(())
}
fn evict(
    data: &mut Data,
    protected_blocks: &HashSet<String>,
    protected_nodes: &HashSet<String>,
    target_bytes: u64,
) -> Result<bool> {
    let candidates = {
        let mut statement = data
            .db
            .prepare("SELECT hash,length(payload) FROM blocks ORDER BY last_used,hash")?;
        let rows =
            statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))?;
        let mut selected = Vec::new();
        let mut bytes = 0;
        for row in rows {
            let (id, size) = row?;
            if !protected_blocks.contains(&id) {
                selected.push(id);
                bytes += size;
            }
            if !selected.is_empty() && (bytes >= target_bytes || selected.len() >= 512) {
                break;
            }
        }
        selected
    };
    if !candidates.is_empty() {
        let tx = data.db.transaction()?;
        for id in &candidates {
            tx.execute("DELETE FROM blocks WHERE hash=?1", [id])?;
        }
        tx.commit()?;
        data.evicted += candidates.len() as u64;
        return Ok(true);
    }
    // Metadata is bounded as well. Removing a chain node leaves an explicit gap;
    // it never deletes a conversation or makes the remaining suffix replayable.
    let candidates = {
        let mut statement = data
            .db
            .prepare("SELECT hash FROM nodes ORDER BY last_used,hash")?;
        let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
        let mut selected = Vec::new();
        for row in rows {
            let id = row?;
            if !protected_nodes.contains(&id) {
                selected.push(id);
            }
            if selected.len() >= (target_bytes / 400).clamp(1, 256) as usize {
                break;
            }
        }
        selected
    };
    if !candidates.is_empty() {
        let tx = data.db.transaction()?;
        for id in candidates {
            tx.execute("DELETE FROM nodes WHERE hash=?1", [id])?;
        }
        tx.execute(
            "DELETE FROM snapshots WHERE tail!='' AND tail NOT IN (SELECT hash FROM nodes)",
            [],
        )?;
        tx.commit()?;
        return Ok(true);
    }
    Ok(data.db.execute(
        "DELETE FROM snapshots WHERE key=(SELECT key FROM snapshots ORDER BY last_used LIMIT 1)",
        [],
    )? > 0)
}
fn input_items(request: &Value) -> Result<Vec<Value>> {
    match request.get("input") {
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(Value::String(text)) => Ok(vec![json!({"role":"user","content":text})]),
        _ => bail!("无法缓存此输入格式"),
    }
}
fn capture(data: &mut Data, scope: &str, request: &Value, response: &Value) -> Result<()> {
    let Some(id) = response["id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 512)
    else {
        return Ok(());
    };
    let Some(output) = response["output"].as_array() else {
        return Ok(());
    };
    if response.get("error").is_some_and(|v| !v.is_null())
        || matches!(
            response["status"].as_str(),
            Some("failed" | "incomplete" | "in_progress")
        )
    {
        return Ok(());
    }
    // Only capture requests whose complete parent chain is available. Never turn
    // an incremental-only request into a supposedly complete new history root.
    let mut items = if let Some(previous) = request["previous_response_id"].as_str() {
        restore(data, scope, previous)?
    } else {
        if request.get("conversation").is_some_and(|v| !v.is_null()) {
            bail!("外部会话历史未采集");
        }
        Vec::new()
    };
    items.extend(input_items(request)?);
    items.extend(output.iter().cloned());
    let envelope: Value = ["instructions", "tools"]
        .into_iter()
        .filter_map(|key| request.get(key).map(|v| (key.to_owned(), v.clone())))
        .collect::<serde_json::Map<_, _>>()
        .into();
    let mut blocks = Vec::new();
    let mut nodes = Vec::new();
    let mut tail = String::new();
    for item in &items {
        let bytes = serde_json::to_vec(item)?;
        for (i, chunk) in bytes.chunks(CHUNK).enumerate() {
            let block = hash(chunk);
            let end = (i + 1) * CHUNK >= bytes.len();
            let node = node_hash(&tail, &block, end);
            blocks.push(Block {
                hash: block.clone(),
                raw: chunk.len(),
                compressed: zstd::stream::encode_all(chunk, 3)?,
            });
            nodes.push(Node {
                hash: node.clone(),
                parent: tail,
                block,
                end,
            });
            tail = node;
        }
    }
    let bytes = serde_json::to_vec(&envelope)?;
    let envelope = hash(&bytes);
    blocks.push(Block {
        hash: envelope.clone(),
        raw: bytes.len(),
        compressed: zstd::stream::encode_all(bytes.as_slice(), 3)?,
    });
    let protected_blocks: HashSet<_> = blocks.iter().map(|b| b.hash.clone()).collect();
    let protected_nodes: HashSet<_> = nodes.iter().map(|n| n.hash.clone()).collect();
    let mut seen = HashSet::new();
    let budget: u64 = blocks
        .iter()
        .filter(|b| seen.insert(&b.hash))
        .map(|b| b.compressed.len() as u64 + 256)
        .sum::<u64>()
        + nodes.len() as u64 * 400
        + 65536;
    if budget > data.config.max_bytes {
        bail!("正文大于缓存容量");
    }
    data.tick += 1;
    let tick = data.tick;
    loop {
        let result = (|| -> rusqlite::Result<()> {
            let tx = data.db.transaction()?;
            for block in &blocks {
                tx.execute("INSERT INTO blocks VALUES(?1,?2,?3,?4) ON CONFLICT(hash) DO UPDATE SET last_used=excluded.last_used",params![block.hash,block.compressed,block.raw,tick])?;
            }
            for node in &nodes {
                tx.execute("INSERT INTO nodes VALUES(?1,?2,?3,?4,?5) ON CONFLICT(hash) DO UPDATE SET last_used=excluded.last_used",params![node.hash,node.parent,node.block,node.end,tick])?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO snapshots VALUES(?1,?2,?3,?4)",
                params![snapshot_key(scope, id), tail, envelope, tick],
            )?;
            tx.commit()
        })();
        match result {
            Ok(()) => return Ok(()),
            Err(rusqlite::Error::SqliteFailure(code, _)) if code.code == ErrorCode::DiskFull => {
                if !evict(data, &protected_blocks, &protected_nodes, 4096)? {
                    bail!("缓存容量不足");
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}
fn read_block(db: &Connection, id: &str) -> Result<Vec<u8>> {
    let (compressed, raw): (Vec<u8>, usize) = db.query_row(
        "SELECT payload,raw_bytes FROM blocks WHERE hash=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let read_limit = raw.checked_add(1).context("缓存块长度无效")?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(raw).context("缓存块内存不足")?;
    zstd::stream::read::Decoder::new(compressed.as_slice())?
        .take(read_limit as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() != raw || hash(&bytes) != id {
        bail!("缓存块校验失败");
    }
    Ok(bytes)
}
fn restore(data: &mut Data, scope: &str, id: &str) -> Result<Vec<Value>> {
    let key = snapshot_key(scope, id);
    let (mut tail, envelope): (String, String) = data.db.query_row(
        "SELECT tail,envelope FROM snapshots WHERE key=?1",
        [&key],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let _ = read_block(&data.db, &envelope)?;
    let mut chunks = Vec::new();
    let mut block_ids = vec![envelope];
    let mut node_ids = Vec::new();
    let mut visited = HashSet::new();
    while !tail.is_empty() {
        if !visited.insert(tail.clone()) {
            bail!("缓存索引存在循环");
        }
        let (parent, block, end): (String, String, bool) = data.db.query_row(
            "SELECT parent,block,item_end FROM nodes WHERE hash=?1",
            [&tail],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if node_hash(&parent, &block, end) != tail {
            bail!("缓存索引校验失败");
        }
        let bytes = read_block(&data.db, &block)?;
        chunks.push((bytes, end));
        block_ids.push(block);
        node_ids.push(tail);
        tail = parent;
    }
    let mut items = Vec::new();
    let mut item = Vec::new();
    for (bytes, end) in chunks.into_iter().rev() {
        item.extend(bytes);
        if end {
            items.push(serde_json::from_slice(&item)?);
            item.clear();
        }
    }
    if !item.is_empty() {
        bail!("缓存历史不完整");
    }
    data.tick += 1;
    let tick = data.tick;
    let tx = data.db.transaction()?;
    for id in block_ids {
        tx.execute(
            "UPDATE blocks SET last_used=?1 WHERE hash=?2",
            params![tick, id],
        )?;
    }
    for id in node_ids {
        tx.execute(
            "UPDATE nodes SET last_used=?1 WHERE hash=?2",
            params![tick, id],
        )?;
    }
    tx.execute(
        "UPDATE snapshots SET last_used=?1 WHERE key=?2",
        params![tick, key],
    )?;
    tx.commit()?;
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn response(id: &str, text: &str) -> Value {
        json!({"id":id,"status":"completed","output":[{"role":"assistant","content":text}]})
    }
    #[test]
    fn deduplicated_blocks_restore_tools_across_restart_and_scope_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        let store = HistoryStore::open(&path).unwrap();
        store.configure(true, MIN_LIMIT).unwrap();
        let input = json!({"input":[{"role":"user","content":"hello"}]});
        store
            .capture("thread-a", &input, &response("r1", "world"))
            .unwrap();
        let before = store.status().unwrap()["blocks"].as_u64().unwrap();
        store
            .capture("thread-a", &input, &response("r1-copy", "world"))
            .unwrap();
        assert_eq!(store.status().unwrap()["blocks"], before);
        let next = json!({"previous_response_id":"r1","input":[{"type":"function_call","call_id":"tool-1","name":"shell","arguments":"{}"},{"type":"function_call_output","call_id":"tool-1","output":"ok"}]});
        store
            .capture("thread-a", &next, &response("r2", "done"))
            .unwrap();
        assert!(store.restore("thread-b", "r2").is_err());
        drop(store);
        let store = HistoryStore::open(&path).unwrap();
        let items = store.restore("thread-a", "r2").unwrap();
        assert_eq!(items.len(), 5);
        assert_eq!(items[2]["call_id"], items[3]["call_id"]);
        store.configure(false, MIN_LIMIT).unwrap();
        assert!(store.restore("thread-a", "r2").is_err());
        store.configure(true, MIN_LIMIT).unwrap();
        store.clear().unwrap();
        assert!(store.restore("thread-a", "r2").is_err());
        assert_eq!(store.status().unwrap()["blocks"], 0);
    }
    #[test]
    fn block_lru_keeps_hot_blocks_and_refuses_holes_without_deleting_a_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(&dir.path().join("cache")).unwrap();
        store.configure(true, MIN_LIMIT).unwrap();
        store
            .capture(
                "thread",
                &json!({"input":"cold"}),
                &response("cold", "first"),
            )
            .unwrap();
        store
            .capture("thread", &json!({"input":"hot"}), &response("hot", "last"))
            .unwrap();
        store.restore("thread", "hot").unwrap();
        {
            let mut data = store.data.lock().unwrap();
            assert!(evict(&mut data, &HashSet::new(), &HashSet::new(), 0).unwrap());
            assert_eq!(
                data.db
                    .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get::<_, u64>(0))
                    .unwrap(),
                2
            );
        }
        assert!(store.restore("thread", "cold").is_err());
        assert!(store.restore("thread", "hot").is_ok());
        store
            .capture(
                "thread",
                &json!({"input":"cold"}),
                &response("cold-again", "first"),
            )
            .unwrap();
        assert!(store.restore("thread", "cold").is_ok());
        assert_eq!(store.status().unwrap()["evicted_blocks"], 1);
    }
    #[test]
    fn shrinking_capacity_bounds_physical_file_and_oversize_capture_does_not_evict_everything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache");
        let store = HistoryStore::open(&path).unwrap();
        store.configure(true, MIN_LIMIT).unwrap();
        let mut seed = 7u64;
        for i in 0..20 {
            let text: String = (0..4096)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    format!("{seed:016x}")
                })
                .collect();
            store
                .capture(
                    "s",
                    &json!({"input":format!("{i}:{text}")}),
                    &response(&format!("r{i}"), "ok"),
                )
                .unwrap();
        }
        store.configure_inner(true, 128 * 1024).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() <= 128 * 1024);
        assert!(store.status().unwrap()["evicted_blocks"].as_u64().unwrap() > 0);
        let fresh: String = (0..2048)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                format!("{seed:016x}")
            })
            .collect();
        store
            .capture(
                "s",
                &json!({"input":fresh}),
                &response("after-limit", "still available"),
            )
            .unwrap();
        assert!(store.restore("s", "after-limit").is_ok());
        assert!(std::fs::metadata(&path).unwrap().len() <= 128 * 1024);
        let blocks = store.status().unwrap()["blocks"].clone();
        assert!(store
            .capture(
                "s",
                &json!({"input":"x".repeat(33 * 1024 * 1024)}),
                &response("huge", "ok")
            )
            .is_err());
        assert_eq!(store.status().unwrap()["blocks"], blocks);
    }
    #[test]
    fn corruption_and_clear_never_replay_partial_or_resurrect_inflight_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(&dir.path().join("cache")).unwrap();
        store.configure(true, MIN_LIMIT).unwrap();
        let generation = store.generation();
        store
            .capture(
                "s",
                &json!({"input":"original"}),
                &response("r", "complete"),
            )
            .unwrap();
        {
            let data = store.data.lock().unwrap();
            data.db
                .execute("UPDATE blocks SET payload=x'1234'", [])
                .unwrap();
        }
        assert!(store.restore("s", "r").is_err());
        store.clear().unwrap();
        store
            .capture_at(
                "s",
                &json!({"input":"old-inflight"}),
                &response("late", "late output"),
                generation,
            )
            .unwrap();
        assert_eq!(store.status().unwrap()["blocks"], 0);
        store
            .capture(
                "s",
                &json!({"input":"new request"}),
                &response("new", "new output"),
            )
            .unwrap();
        assert!(store.restore("s", "new").is_ok());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.path().join("cache"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
