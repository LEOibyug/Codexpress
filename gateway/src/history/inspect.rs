use super::*;
use rusqlite::types::Value as SqlValue;
use std::io::Write;

const ROW_COLUMNS: &str = "key,rowid,metadata";
const INCOMPLETE: &str = "缓存历史不完整：所需块缺失、损坏或索引无法校验。";

fn object<'a>(query: &'a Value, allowed: &[&str]) -> Result<&'a serde_json::Map<String, Value>> {
    let object = query.as_object().context("缓存查询须为对象")?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        bail!("缓存查询包含未知参数");
    }
    Ok(object)
}

fn number(query: &serde_json::Map<String, Value>, key: &str) -> Result<Option<u64>> {
    query.get(key).map(|value| value.as_u64().context("缓存查询数值无效")).transpose()
}

fn row_value(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let id: String = row.get(0)?;
    let sequence: u64 = row.get(1)?;
    let metadata: Option<String> = row.get(2)?;
    // Old snapshots have no provenance. Never derive a timestamp from their LRU tick.
    let metadata = metadata.and_then(|text| serde_json::from_str::<CaptureMetadata>(&text).ok());
    Ok(match metadata {
        Some(m) => json!({"id":id,"sequence":sequence,"captured_at":m.captured_at,"source_id":m.source_id,"source_name":m.source_name,"model":m.model,"ingress":m.ingress,"request_id":m.request_id,"attempt_id":m.attempt_id,"legacy":false}),
        None => json!({"id":id,"sequence":sequence,"captured_at":null,"source_id":null,"source_name":null,"model":null,"ingress":null,"request_id":null,"attempt_id":null,"legacy":true}),
    })
}

impl HistoryStore {
    /// Read-only provenance listing, independent of replay enablement and LRU order.
    pub fn inspect_list(&self, query: &Value) -> Result<Value> {
        let query = object(query, &["from", "to", "source_id", "model", "ingress", "attempt_id", "before", "limit"])?;
        let from = number(query, "from")?;
        let to = number(query, "to")?;
        let before = number(query, "before")?;
        let limit = number(query, "limit")?.unwrap_or(50);
        if !(1..=100).contains(&limit) || from.zip(to).is_some_and(|(from, to)| from > to) {
            bail!("缓存查询范围无效");
        }
        let mut conditions = vec!["1=1".to_owned()];
        let mut parameters = Vec::<SqlValue>::new();
        for (name, value, comparison) in [("captured_at", from, ">="), ("captured_at", to, "<")] {
            if let Some(value) = value {
                // SQLite integers are signed; decimal REAL preserves comparisons beyond i64::MAX.
                conditions.push(format!("json_extract(metadata,'$.{name}') {comparison} ?"));
                parameters.push(if let Ok(value) = i64::try_from(value) { SqlValue::Integer(value) } else { SqlValue::Real(value as f64) });
            }
        }
        for name in ["source_id", "model", "ingress", "attempt_id"] {
            if let Some(value) = query.get(name) {
                let value = value.as_str().context("缓存筛选须为字符串")?;
                conditions.push(format!("json_extract(metadata,'$.{name}') = ?"));
                parameters.push(SqlValue::Text(value.to_owned()));
            }
        }
        let data = self.data.lock().map_err(|_| anyhow::anyhow!("原文缓存忙碌"))?;
        let filter = conditions.join(" AND ");
        let total: u64 = data.db.query_row(
            &format!("SELECT COUNT(*) FROM snapshots WHERE {filter}"),
            rusqlite::params_from_iter(parameters.iter()), |row| row.get(0),
        )?;
        let legacy_total: u64 = data.db.query_row("SELECT COUNT(*) FROM snapshots WHERE metadata IS NULL", [], |row| row.get(0))?;
        if let Some(before) = before {
            conditions.push("rowid < ?".into());
            parameters.push(if let Ok(value) = i64::try_from(before) { SqlValue::Integer(value) } else { SqlValue::Real(before as f64) });
        }
        let sql = format!("SELECT {ROW_COLUMNS} FROM snapshots WHERE {} ORDER BY rowid DESC LIMIT {}", conditions.join(" AND "), limit + 1);
        let mut statement = data.db.prepare(&sql)?;
        let mut records = statement.query_map(rusqlite::params_from_iter(parameters.iter()), row_value)?.collect::<rusqlite::Result<Vec<_>>>()?;
        let more = records.len() > limit as usize;
        records.truncate(limit as usize);
        let next = if more { records.last().and_then(|row| row["sequence"].as_u64()) } else { None };
        Ok(json!({"records":records,"next_cursor":next,"total":total,"legacy_total":legacy_total}))
    }

    /// Inspect retained JSON without modifying bodies, replay state or LRU order.
    pub fn inspect_detail(&self, query: &Value) -> Result<Value> {
        let query = object(query, &["id", "offset", "limit"])?;
        let id = query.get("id").and_then(Value::as_str).context("缓存标识无效")?;
        if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
            bail!("缓存标识无效");
        }
        let offset = number(query, "offset")?.unwrap_or(0);
        let limit = number(query, "limit")?.unwrap_or(65536);
        if !(1..=262144).contains(&limit) {
            bail!("缓存分页大小无效");
        }
        let data = self.data.lock().map_err(|_| anyhow::anyhow!("原文缓存忙碌"))?;
        let metadata = data.db.query_row(&format!("SELECT {ROW_COLUMNS} FROM snapshots WHERE key=?1"), [id], row_value).optional()?.context("缓存记录不存在")?;
        let mut window = Window { offset, capacity: limit as usize + 3, position: 0, bytes: Vec::with_capacity(limit as usize + 3) };
        if materialize(&data.db, id, &mut window).is_err() {
            // Do not offer a suffix or partially verified JSON as a complete conversation.
            return Ok(json!({"id":id,"metadata":metadata,"status":"incomplete","reason":INCOMPLETE,"text":"","next_offset":null,"total_bytes":0}));
        }
        if offset > window.position {
            bail!("缓存分页偏移超出正文");
        }
        let total = window.position;
        let mut end = (limit as usize).min(window.bytes.len());
        // A caller-provided continuation must begin on a Unicode scalar boundary.
        if window.bytes.first().is_some_and(|byte| byte & 0xc0 == 0x80) {
            bail!("缓存分页偏移不在 UTF-8 字符边界");
        }
        while end > 0 && std::str::from_utf8(&window.bytes[..end]).is_err() { end -= 1; }
        // Even limit=1 must make progress when the next scalar takes 2–4 bytes.
        if end == 0 && !window.bytes.is_empty() {
            end = (1..=window.bytes.len().min(4)).find(|&end| std::str::from_utf8(&window.bytes[..end]).is_ok()).context("缓存正文编码无效")?;
        }
        let text = std::str::from_utf8(&window.bytes[..end]).context("缓存正文编码无效")?;
        let next = offset.checked_add(end as u64).context("缓存正文长度无效")?;
        Ok(json!({"id":id,"metadata":metadata,"status":"complete","reason":null,"text":text,"next_offset":if next < total {Some(next)} else {None},"total_bytes":total}))
    }
}

/// Keeps only the requested window while serialization visits the full document.
struct Window {
    offset: u64,
    capacity: usize,
    position: u64,
    bytes: Vec<u8>,
}
impl Write for Window {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let end = self.position.checked_add(bytes.len() as u64).ok_or_else(|| std::io::Error::other("缓存正文长度无效"))?;
        if end > self.offset && self.bytes.len() < self.capacity {
            let start = self.offset.saturating_sub(self.position).min(bytes.len() as u64) as usize;
            let length = (bytes.len() - start).min(self.capacity - self.bytes.len());
            self.bytes.extend_from_slice(&bytes[start..start + length]);
        }
        self.position = end;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

fn materialize(db: &Connection, id: &str, output: &mut Window) -> Result<()> {
    let (mut tail, envelope): (String, String) = db.query_row("SELECT tail,envelope FROM snapshots WHERE key=?1", [id], |row| Ok((row.get(0)?, row.get(1)?)))?;
    let mut nodes = Vec::new();
    let mut visited = HashSet::new();
    while !tail.is_empty() {
        if !visited.insert(tail.clone()) { bail!("缓存索引存在循环"); }
        let (parent, block, end): (String, String, bool) = db.prepare_cached("SELECT parent,block,item_end FROM nodes WHERE hash=?1")?.query_row([&tail], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        if node_hash(&parent, &block, end) != tail { bail!("缓存索引校验失败"); }
        nodes.push((block, end));
        tail = parent;
    }
    let envelope: Value = serde_json::from_slice(&read_block(db, &envelope)?)?;
    let envelope = envelope.as_object().context("缓存指令格式无效")?;
    if envelope.keys().any(|key| key != "instructions" && key != "tools") { bail!("缓存指令格式无效"); }
    output.write_all(b"{")?;
    for (key, value) in envelope {
        serde_json::to_writer(&mut *output, key)?;
        output.write_all(b":")?;
        serde_json::to_writer(&mut *output, value)?;
        output.write_all(b",")?;
    }
    output.write_all(b"\"items\":[")?;
    nodes.reverse();
    let mut start = 0;
    let mut first = true;
    // Validate the entire chain on each page, but decompress only one 64 KiB
    // item block at a time. Stored item JSON is already canonical capture output.
    for index in 0..nodes.len() {
        if nodes[index].1 {
            if !first { output.write_all(b",")?; }
            first = false;
            let mut reader = ItemReader { db, nodes: nodes[start..=index].iter(), current: std::io::Cursor::new(Vec::new()), output: &mut *output };
            let mut decoder = serde_json::Deserializer::from_reader(&mut reader);
            serde::de::IgnoredAny::deserialize(&mut decoder)?;
            decoder.end()?;
            start = index + 1;
        }
    }
    if start != nodes.len() { bail!("缓存历史不完整"); }
    output.write_all(b"]}")?;
    Ok(())
}

struct ItemReader<'a> {
    db: &'a Connection,
    nodes: std::slice::Iter<'a, (String, bool)>,
    current: std::io::Cursor<Vec<u8>>,
    output: &'a mut Window,
}
impl Read for ItemReader<'_> {
    fn read(&mut self, destination: &mut [u8]) -> std::io::Result<usize> {
        if destination.is_empty() { return Ok(0); }
        loop {
            let count = self.current.read(destination)?;
            if count != 0 {
                return Ok(count);
            }
            let Some((block, _)) = self.nodes.next() else { return Ok(0); };
            let load = || -> Result<Vec<u8>> {
                let raw: u64 = self.db.prepare_cached("SELECT raw_bytes FROM blocks WHERE hash=?1")?.query_row([block], |row| row.get(0))?;
                if raw == 0 || raw > CHUNK as u64 { bail!("缓存块长度无效"); }
                read_block(self.db, block)
            };
            let bytes = load().map_err(|_| std::io::Error::other("缓存块无法校验"))?;
            self.output.write_all(&bytes)?;
            self.current = std::io::Cursor::new(bytes);
        }
    }
}
