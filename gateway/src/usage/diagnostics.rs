//! Private, bounded diagnostics. Only failed upstream feedback may contain text.
use super::{summary, StoreData};
use crate::model::Source;
use anyhow::{bail, Context, Result};
use http::HeaderMap;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::io::Read;

pub const BODY_LIMIT: usize = 8 * 1024 * 1024;
const STORAGE_LIMIT: i64 = 128 * 1024 * 1024;
const HIDDEN: &str = "[已隐藏]";
const COLUMNS: &str = "attempt,request_id,source_id,source_name,model,started_at,status,input_tokens,output_tokens,cached_input_tokens,complete,latency_ms,metadata";

pub fn initialize(connection: &Connection) -> Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS diagnostics(attempt_id TEXT PRIMARY KEY,created_at INTEGER NOT NULL,metadata TEXT NOT NULL,body BLOB);
        CREATE INDEX IF NOT EXISTS diagnostics_time ON diagnostics(created_at);
        CREATE INDEX IF NOT EXISTS diagnostics_ingress ON diagnostics(json_extract(metadata,'$.ingress'),created_at);
        CREATE INDEX IF NOT EXISTS usage_attempt_id ON usage(json_extract(metadata,'$.attempt_id'));")?;
    prune(connection)
}

pub struct Capture {
    pub attempt_id: String,
    pub created_at: u64,
    metadata: Value,
    body: Vec<u8>,
    secrets: Vec<String>,
    received: u64,
    capture_http_body: bool,
    encoding: String,
}

impl Capture {
    pub fn new(attempt_id: &str, created_at: u64, source: &Source, gateway_key: &str) -> Self {
        fn strings(value: &Value, secrets: &mut Vec<String>) {
            match value {
                Value::String(s) if !s.is_empty() => secrets.push(s.clone()),
                Value::Object(o) => o.values().for_each(|v| strings(v, secrets)),
                Value::Array(a) => a.iter().for_each(|v| strings(v, secrets)),
                _ => {}
            }
        }
        let mut secrets = Vec::new();
        strings(&source.secret, &mut secrets);
        if !gateway_key.is_empty() {
            secrets.push(gateway_key.to_owned());
        }
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Self {
            attempt_id: attempt_id.into(),
            created_at,
            metadata: json!({"ingress":"unknown","method":null,"path":null,"upstream_url":null,"http_status":null,"response_headers":{},"error_kind":null,"error_message":null,"error_code":null,"error_param":null,"adapted_parameters":[],"body_complete":false,"body_truncated":false,"body_evicted":false}),
            body: Vec::new(),
            secrets,
            received: 0,
            capture_http_body: false,
            encoding: String::new(),
        }
    }

    pub fn request(
        &mut self,
        ingress: &str,
        method: &str,
        path: &str,
        url: &str,
        adapted: &[String],
    ) {
        self.metadata["ingress"] = json!(self.redact(ingress));
        self.metadata["method"] = json!(self.redact(method));
        self.metadata["path"] = json!(self.redact(path));
        let safe = url::Url::parse(url).ok().map(|mut url| {
            url.set_query(None);
            url.set_fragment(None);
            let _ = url.set_username("");
            let _ = url.set_password(None);
            self.redact(url.as_str())
        });
        self.metadata["upstream_url"] = json!(safe);
        self.metadata["adapted_parameters"] =
            json!(adapted.iter().map(|s| self.redact(s)).collect::<Vec<_>>());
    }

    pub fn response(&mut self, status: u16, headers: &HeaderMap) {
        self.metadata["http_status"] = json!(status);
        let mut safe = serde_json::Map::new();
        for name in [
            "content-type",
            "content-length",
            "content-encoding",
            "retry-after",
            "x-request-id",
            "request-id",
            "cf-ray",
            "date",
            "server",
            "openai-processing-ms",
        ] {
            if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
                safe.insert(name.into(), json!(self.redact(value)));
            }
        }
        self.metadata["response_headers"] = Value::Object(safe);
        self.encoding = headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        self.capture_http_body = status >= 400;
        if self.capture_http_body {
            self.metadata["error_kind"] = json!("http");
        }
    }

    pub fn transport(&mut self, kind: &str, message: &str) {
        self.metadata["error_kind"] = json!(self.redact(kind));
        self.metadata["error_message"] = json!(self.redact(message));
    }

    pub fn chunk(&mut self, bytes: &[u8]) {
        if !self.capture_http_body {
            return;
        }
        self.received = self.received.saturating_add(bytes.len() as u64);
        let take = bytes.len().min(BODY_LIMIT.saturating_sub(self.body.len()));
        self.body.extend_from_slice(&bytes[..take]);
        if take < bytes.len() {
            self.metadata["body_truncated"] = json!(true);
        }
    }

    pub fn full(&self) -> bool {
        self.body.len() >= BODY_LIMIT
    }
    pub fn complete(&mut self) {
        self.metadata["body_complete"] = json!(true);
    }

    pub fn failed_event(&mut self, value: &Value) {
        // HTTP 200 remains non-capturing: later deltas must never enter this body.
        if self.capture_http_body {
            return;
        }
        let response = value.get("response").unwrap_or(value);
        if !matches!(value["type"].as_str(), Some("response.failed" | "error"))
            && response["status"] != "failed"
            && response.get("error").is_none_or(Value::is_null)
        {
            return;
        }
        let mut failure = serde_json::Map::new();
        if let Some(kind) = value.get("type") {
            failure.insert("type".into(), kind.clone());
        }
        for name in [
            "error",
            "code",
            "param",
            "message",
            "detail",
            "details",
            "request_id",
        ] {
            if let Some(v) = response.get(name).or_else(|| value.get(name)) {
                failure.insert(name.into(), v.clone());
            }
        }
        let bytes = serde_json::to_vec(&failure).unwrap_or_default();
        self.body.clear();
        self.received = bytes.len() as u64;
        self.body
            .extend_from_slice(&bytes[..bytes.len().min(BODY_LIMIT)]);
        self.metadata["error_kind"] = json!("upstream_event");
        self.metadata["body_truncated"] = json!(bytes.len() > BODY_LIMIT);
        self.encoding.clear();
        self.complete();
    }

    fn redact(&self, text: &str) -> String {
        let text = redact_json_fragments(text);
        let mut output = String::with_capacity(text.len());
        let mut position = 0;
        let mut matches = self
            .secrets
            .iter()
            .map(|secret| text.find(secret))
            .collect::<Vec<_>>();
        let mut marker = text.find(HIDDEN);
        while position < text.len() {
            let rest = &text[position..];
            let secret = matches
                .iter()
                .zip(&self.secrets)
                .filter_map(|(index, secret)| index.map(|index| (index, secret.len())))
                .min_by_key(|(index, length)| (*index, std::cmp::Reverse(*length)));
            if let Some(found) =
                marker.filter(|marker| secret.is_none_or(|(index, _)| *marker <= index))
            {
                output.push_str(&text[position..found + HIDDEN.len()]);
                position = found + HIDDEN.len();
            } else if let Some((index, length)) = secret {
                output.push_str(&text[position..index]);
                output.push_str(HIDDEN);
                position = index + length;
            } else {
                output.push_str(rest);
                break;
            }
            if output.len() > BODY_LIMIT {
                break;
            }
            if marker.is_some_and(|index| index < position) {
                marker = text[position..].find(HIDDEN).map(|index| position + index);
            }
            for (next, secret) in matches.iter_mut().zip(&self.secrets) {
                if next.is_some_and(|index| index < position) {
                    *next = text[position..].find(secret).map(|index| position + index);
                }
            }
        }
        for secret in &self.secrets {
            if let Some((cut, _)) = secret
                .char_indices()
                .rev()
                .find(|(cut, _)| *cut >= 8 && output.ends_with(&secret[..*cut]))
            {
                output.truncate(output.len() - cut);
                output.push_str(HIDDEN);
                break;
            }
        }
        redact_tokens(&output)
    }

    fn error_text(&self) -> (Option<String>, bool, Option<&'static str>) {
        if self.body.is_empty() {
            return (None, false, None);
        }
        let (bytes, decoded_truncated) = match decode_body(&self.body, &self.encoding) {
            Ok(value) => value,
            Err(_) => return (None, false, Some("错误正文压缩内容无法读取")),
        };
        let text = String::from_utf8_lossy(&bytes);
        let text = if let Ok(mut value) = serde_json::from_str::<Value>(&text) {
            redact_value(&mut value, self);
            self.redact(&serde_json::to_string(&value).unwrap_or_default())
        } else {
            self.redact(&text)
        };
        let mut text = text;
        let truncated = decoded_truncated || text.len() > BODY_LIMIT;
        truncate_utf8(&mut text, BODY_LIMIT);
        (Some(text), truncated, None)
    }

    fn stored_metadata(&self, text: Option<&str>) -> Value {
        let mut metadata = self.metadata.clone();
        metadata["body_total_bytes"] = json!(self.received);
        metadata["body_saved_bytes"] = json!(text.map_or(0, str::len));
        if self.full() && metadata["body_complete"] != true {
            metadata["body_truncated"] = json!(true);
        }
        if let Some(value) = text.and_then(|text| serde_json::from_str::<Value>(text).ok()) {
            let response = value.get("response").unwrap_or(&value);
            let error = response.get("error").unwrap_or(response);
            metadata["error_code"] = error
                .get("code")
                .filter(|v| v.is_string())
                .cloned()
                .unwrap_or(Value::Null);
            metadata["error_param"] = error
                .get("param")
                .filter(|v| v.is_string())
                .cloned()
                .unwrap_or(Value::Null);
            if let Some(message) = error["message"]
                .as_str()
                .or_else(|| error["detail"].as_str())
            {
                metadata["error_message"] = json!(message.chars().take(512).collect::<String>());
            }
        }
        metadata
    }
}

fn truncate_utf8(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

fn bounded_read(reader: impl Read) -> Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    reader
        .take((BODY_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > BODY_LIMIT;
    bytes.truncate(BODY_LIMIT);
    Ok((bytes, truncated))
}

fn decode_body(bytes: &[u8], encoding: &str) -> Result<(Vec<u8>, bool)> {
    let mut decoded = bytes.to_vec();
    let mut truncated = false;
    for encoding in encoding.split(',').map(str::trim).rev() {
        let (next, exceeded) = match encoding {
            "" | "identity" => continue,
            "gzip" | "x-gzip" => {
                bounded_read(flate2::read::MultiGzDecoder::new(decoded.as_slice()))?
            }
            "deflate" => bounded_read(flate2::read::ZlibDecoder::new(decoded.as_slice()))
                .or_else(|_| bounded_read(flate2::read::DeflateDecoder::new(decoded.as_slice())))?,
            "zstd" => bounded_read(zstd::stream::read::Decoder::new(decoded.as_slice())?)?,
            _ => bail!("unsupported diagnostic encoding"),
        };
        decoded = next;
        truncated |= exceeded;
    }
    Ok((decoded, truncated))
}

fn sensitive(name: &str) -> bool {
    let name: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        name.as_str(),
        "authorization"
            | "proxyauthorization"
            | "apikey"
            | "xapikey"
            | "key"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "token"
            | "password"
            | "secret"
            | "clientsecret"
            | "credential"
            | "credentials"
            | "cookie"
            | "setcookie"
            | "authorizationcode"
            | "oauthcode"
            | "state"
            | "codeverifier"
    )
}

fn redact_value(value: &mut Value, capture: &Capture) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if sensitive(key) {
                    *value = json!(HIDDEN);
                } else {
                    redact_value(value, capture);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(item, capture);
            }
        }
        Value::String(text) => *text = capture.redact(text),
        _ => {}
    }
}

// Also sanitize sensitive JSON fields when a bounded capture ends mid-document.
fn redact_json_fragments(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut position = 0;
    while position < text.len() {
        let rest = &text[position..];
        let Some(quote) = rest.find('"') else {
            output.push_str(rest);
            break;
        };
        output.push_str(&rest[..quote]);
        position += quote;
        let rest = &text[position..];
        if rest.starts_with('"') {
            if let Some(end) = json_string_end(text, position) {
                if let Ok(key) = serde_json::from_str::<String>(&text[position..end]) {
                    let colon = end + text[end..].len() - text[end..].trim_start().len();
                    if sensitive(&key) && text[colon..].starts_with(':') {
                        let start = colon + 1 + text[colon + 1..].len()
                            - text[colon + 1..].trim_start().len();
                        output.push_str(&text[position..start]);
                        output.push_str(&format!("\"{HIDDEN}\""));
                        position = json_value_end(text, start);
                        continue;
                    }
                }
                output.push_str(&text[position..end]);
                position = end;
                continue;
            }
        }
        let ch = rest.chars().next().expect("nonempty");
        output.push(ch);
        position += ch.len_utf8();
    }
    output
}

fn json_string_end(text: &str, start: usize) -> Option<usize> {
    let mut escaped = false;
    for (offset, ch) in text[start + 1..].char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(start + 1 + offset + 1);
        }
    }
    None
}

fn json_value_end(text: &str, start: usize) -> usize {
    if text[start..].starts_with('"') {
        return json_string_end(text, start).unwrap_or(text.len());
    }
    let mut depth = 0usize;
    let mut position = start;
    while position < text.len() {
        let ch = text[position..].chars().next().expect("nonempty");
        if ch == '"' {
            position = json_string_end(text, position).unwrap_or(text.len());
            continue;
        }
        match ch {
            '{' | '[' => depth += 1,
            '}' | ']' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    return position + 1;
                }
            }
            ',' | '}' | ']' if depth == 0 => return position,
            c if c.is_whitespace() && depth == 0 => return position,
            _ => {}
        }
        position += ch.len_utf8();
    }
    text.len()
}

fn redact_tokens(text: &str) -> String {
    const PREFIXES: &[&str] = &[
        "bearer ",
        "bearer\t",
        "bearer\r",
        "bearer\n",
        "?code=",
        "&code=",
        "?state=",
        "&state=",
        "?key=",
        "&key=",
        "?api_key=",
        "&api_key=",
        "?access_token=",
        "&access_token=",
        "?refresh_token=",
        "&refresh_token=",
        "?id_token=",
        "&id_token=",
        "?client_secret=",
        "&client_secret=",
    ];
    let lower = text.to_ascii_lowercase();
    let mut output = String::with_capacity(text.len());
    let mut position = 0;
    while position < text.len() {
        if let Some((index, prefix)) = PREFIXES
            .iter()
            .filter_map(|prefix| lower[position..].find(prefix).map(|index| (index, prefix)))
            .min_by_key(|(index, _)| *index)
        {
            let mut begin = position + index + prefix.len();
            if prefix.starts_with("bearer") {
                while text
                    .as_bytes()
                    .get(begin)
                    .is_some_and(u8::is_ascii_whitespace)
                {
                    begin += 1;
                }
            }
            output.push_str(&text[position..begin]);
            if text[begin..].starts_with(HIDDEN) {
                output.push_str(HIDDEN);
                position = begin + HIDDEN.len();
                continue;
            }
            let end = text[begin..]
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(
                            c,
                            '&' | '"'
                                | '\''
                                | '<'
                                | '>'
                                | '\\'
                                | ')'
                                | '}'
                                | ']'
                                | '#'
                                | '，'
                                | '。'
                                | '；'
                        )
                })
                .map_or(text.len(), |length| begin + length);
            if end > begin {
                output.push_str(HIDDEN);
            }
            position = end;
            continue;
        }
        output.push_str(&text[position..]);
        break;
    }
    output
}

pub fn save(connection: &Connection, capture: &Capture, include_body: bool) -> Result<()> {
    let (text, truncated, decode_error) = if include_body {
        capture.error_text()
    } else {
        (None, false, None)
    };
    let mut metadata = capture.stored_metadata(text.as_deref());
    if truncated {
        metadata["body_truncated"] = json!(true);
    }
    if let Some(error) = decode_error {
        metadata["body_decode_error"] = json!(error);
        metadata["body_complete"] = json!(false);
    }
    let body = text
        .map(|text| zstd::stream::encode_all(text.as_bytes(), 1))
        .transpose()?;
    let has_body = body.is_some();
    if body.is_none() {
        if let Some(existing) = connection
            .query_row(
                "SELECT metadata FROM diagnostics WHERE attempt_id=?1",
                [&capture.attempt_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            if let Ok(existing) = serde_json::from_str::<Value>(&existing) {
                for key in [
                    "body_total_bytes",
                    "body_saved_bytes",
                    "body_complete",
                    "body_truncated",
                    "body_evicted",
                    "body_decode_error",
                    "error_code",
                    "error_param",
                    "error_message",
                ] {
                    if let Some(value) = existing.get(key).filter(|v| !v.is_null()) {
                        metadata[key] = value.clone();
                    }
                }
            }
        }
    }
    connection.execute("INSERT INTO diagnostics(attempt_id,created_at,metadata,body) VALUES(?1,?2,?3,?4) ON CONFLICT(attempt_id) DO UPDATE SET metadata=excluded.metadata,body=COALESCE(excluded.body,diagnostics.body)", params![capture.attempt_id,capture.created_at,metadata.to_string(),body])?;
    if has_body {
        prune(connection)
    } else {
        Ok(())
    }
}

fn evict_body(connection: &Connection, id: &str) -> Result<()> {
    connection.execute("UPDATE diagnostics SET body=NULL,metadata=json_set(metadata,'$.body_evicted',json('true'),'$.body_saved_bytes',0) WHERE attempt_id=?1", [id])?;
    Ok(())
}

fn prune(connection: &Connection) -> Result<()> {
    let since = crate::model::now().saturating_sub(30 * 86400);
    connection.execute("UPDATE diagnostics SET body=NULL,metadata=json_set(metadata,'$.body_evicted',json('true'),'$.body_saved_bytes',0) WHERE created_at<?1 AND body IS NOT NULL", [since])?;
    let mut total: i64 = connection.query_row(
        "SELECT COALESCE(SUM(length(body)),0) FROM diagnostics",
        [],
        |row| row.get(0),
    )?;
    if total > STORAGE_LIMIT {
        let mut statement = connection.prepare("SELECT attempt_id,length(body) FROM diagnostics WHERE body IS NOT NULL ORDER BY created_at,attempt_id")?;
        let values = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, size) in values {
            if total <= STORAGE_LIMIT {
                break;
            }
            evict_body(connection, &id)?;
            total -= size;
        }
    }
    Ok(())
}

pub(super) fn detail(data: &StoreData, payload: &Value) -> Result<Value> {
    let id = payload["attempt_id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .context("缺少请求标识")?;
    let offset = payload
        .get("body_offset")
        .map(|v| v.as_u64().context("正文偏移无效"))
        .transpose()?
        .unwrap_or(0);
    let limit = payload
        .get("body_limit")
        .map(|v| {
            v.as_u64()
                .filter(|n| *n > 0 && *n <= 262144)
                .context("正文分块大小无效")
        })
        .transpose()?
        .unwrap_or(65536);
    let saved = data.connection.query_row(&format!("SELECT {COLUMNS} FROM usage WHERE json_extract(metadata,'$.attempt_id')=?1 OR ('legacy:'||attempt)=?1 LIMIT 1"), [id], stored_record).optional()?;
    let active = data.active.get(id).map(|(record, started)| {
        let mut value = serde_json::to_value(record).expect("record");
        value["active"] = json!(true);
        let elapsed = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        value["latency_ms"] = json!(elapsed);
        if let Some(first) = record
            .first_token_latency_ms
            .filter(|_| !record.token_details.stream_complete)
        {
            value["generation_ms"] = json!(elapsed.saturating_sub(first));
        }
        summary::add_rate(&mut value);
        value["cost"] = crate::pricing::estimate(&value, false);
        value
    });
    let record = saved.or(active).context("请求记录不存在")?;
    // Live cost refreshes must not read/decompress error bodies, query related
    // attempts or run retention cleanup on every refresh.
    if payload["record_only"] == true {
        return Ok(json!({"record":record}));
    }
    prune(&data.connection)?;
    let diagnostic: Option<(String, Option<Vec<u8>>)> = data
        .connection
        .query_row(
            "SELECT metadata,body FROM diagnostics WHERE attempt_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let diagnostic = if let Some((metadata, body)) = diagnostic {
        let mut value: Value = serde_json::from_str(&metadata)?;
        let text = if let Some(body) = body {
            let (bytes, truncated) =
                bounded_read(zstd::stream::read::Decoder::new(body.as_slice())?)?;
            if truncated {
                bail!("诊断正文超过读取限制");
            }
            String::from_utf8(bytes).context("诊断正文编码无效")?
        } else {
            String::new()
        };
        let start = usize::try_from(offset).context("正文偏移无效")?;
        if start > text.len() || !text.is_char_boundary(start) {
            bail!("正文偏移无效");
        }
        let mut end = text.len().min(start.saturating_add(limit as usize));
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        // A one-byte page still returns one complete character and always advances.
        if end == start && start < text.len() {
            end += text[start..].chars().next().expect("nonempty").len_utf8();
        }
        value["body_text"] = json!(&text[start..end]);
        value["body_next_offset"] = if end < text.len() {
            json!(end)
        } else {
            Value::Null
        };
        value
    } else {
        Value::Null
    };
    let mut statement = data.connection.prepare(&format!(
        "SELECT {COLUMNS} FROM usage WHERE request_id=?1 ORDER BY attempt LIMIT 100"
    ))?;
    let related = statement
        .query_map([record["request_id"].as_str().unwrap_or("")], stored_record)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"record":record,"diagnostic":diagnostic,"related":related}))
}

fn stored_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let mut record = summary::stored_record(row)?;
    if record["attempt_id"].as_str().is_none_or(str::is_empty) {
        record["attempt_id"] = json!(format!("legacy:{}", row.get::<_, i64>(0)?));
    }
    Ok(record)
}

#[cfg(test)]
#[path = "diagnostics_tests.rs"]
mod tests;
