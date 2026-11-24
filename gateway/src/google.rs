//! Google AI Studio (Gemini GenerateContent) protocol adapter.
//! Converts OpenAI Responses requests/responses to and from Gemini REST/SSE
//! without altering existing `api` or `official` passthrough paths.
use crate::model::Source;
use axum::http::StatusCode;
use base64::Engine as _;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

const MAX_THOUGHT_SIGNATURES: usize = 4096;
const SSE_FRAME_LIMIT: usize = 32 * 1024 * 1024;

#[derive(Default)]
struct ThoughtSignatureCache {
    values: HashMap<String, String>,
    order: VecDeque<String>,
}

/// Bounded in-memory cache mapping `functionCall` IDs to Gemini `thoughtSignature`
/// tokens for multi-turn tool continuation. Never persisted or logged.
#[derive(Clone, Default)]
pub(crate) struct ThoughtSignatures(Arc<Mutex<ThoughtSignatureCache>>);

impl ThoughtSignatures {
    pub(crate) fn remember(&self, call_id: &str, signature: &str) {
        if call_id.is_empty()
            || signature.is_empty()
            || call_id.len() > 512
            || signature.len() > 65_536
        {
            return;
        }
        let mut cache = self.0.lock().expect("thought signatures");
        if cache.values.contains_key(call_id) {
            cache.values.insert(call_id.to_owned(), signature.to_owned());
            cache.order.retain(|id| id != call_id);
            cache.order.push_back(call_id.to_owned());
            return;
        }
        while cache.values.len() >= MAX_THOUGHT_SIGNATURES {
            if let Some(oldest) = cache.order.pop_front() {
                cache.values.remove(&oldest);
            } else {
                break;
            }
        }
        cache.values.insert(call_id.to_owned(), signature.to_owned());
        cache.order.push_back(call_id.to_owned());
    }

    pub(crate) fn get(&self, call_id: &str) -> Option<String> {
        if call_id.is_empty() {
            return None;
        }
        self.0
            .lock()
            .expect("thought signatures")
            .values
            .get(call_id)
            .cloned()
    }
}

pub(crate) struct PreparedGoogleRequest {
    pub(crate) url: url::Url,
    pub(crate) body: Bytes,
    pub(crate) stream: bool,
}

pub(crate) fn normalize_model(model: &str) -> Result<&str, (StatusCode, &'static str)> {
    let trimmed = model.trim().strip_prefix("models/").unwrap_or(model.trim());
    if trimmed.is_empty()
        || trimmed.len() > 256
        || trimmed.contains("..")
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        return Err((StatusCode::BAD_REQUEST, "Google AI Studio 模型名称无效"));
    }
    Ok(trimmed)
}

pub(crate) fn endpoint(
    source: &Source,
    model: &str,
    stream: bool,
) -> Result<url::Url, (StatusCode, &'static str)> {
    let model = normalize_model(model)?;
    let base = source.base_url.trim_end_matches('/');
    let base = base.strip_suffix("/models").unwrap_or(base);
    let action = if stream {
        "streamGenerateContent"
    } else {
        "generateContent"
    };
    let mut url = url::Url::parse(&format!("{base}/models/{model}:{action}"))
        .map_err(|_| (StatusCode::BAD_GATEWAY, "Google AI Studio 接口地址无效"))?;
    if stream {
        url.query_pairs_mut().append_pair("alt", "sse");
    }
    Ok(url)
}

fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (header, data) = rest.split_once(',')?;
    let mut segments = header.split(';');
    let mime = segments.next()?.trim();
    if mime.is_empty() || !segments.any(|part| part.trim().eq_ignore_ascii_case("base64")) {
        return None;
    }
    let cleaned: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if cleaned.is_empty() || base64::engine::general_purpose::STANDARD.decode(&cleaned).is_err() {
        return None;
    }
    Some((mime.to_owned(), cleaned))
}

fn push_turn(contents: &mut Vec<Value>, role: &str, mut parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut() {
        if last["role"].as_str() == Some(role) {
            if let Some(existing) = last["parts"].as_array_mut() {
                existing.append(&mut parts);
                return;
            }
        }
    }
    contents.push(json!({"role": role, "parts": parts}));
}

fn extract_system_parts(content: &Value, out: &mut Vec<Value>) -> Result<(), (StatusCode, &'static str)> {
    if let Some(text) = content.as_str() {
        if !text.is_empty() {
            out.push(json!({"text": text}));
        }
        return Ok(());
    }
    let Some(items) = content.as_array() else {
        return Ok(());
    };
    for part in items {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                out.push(json!({"text": text}));
            }
        } else if part["type"].as_str().is_some_and(|t| t != "input_text" && t != "text" && t != "output_text") {
            return Err((StatusCode::BAD_REQUEST, "系统提示仅支持文本内容"));
        }
    }
    Ok(())
}

fn convert_message_parts(
    content: &Value,
    assistant: bool,
) -> Result<Vec<Value>, (StatusCode, &'static str)> {
    if let Some(text) = content.as_str() {
        return Ok(if text.is_empty() {
            Vec::new()
        } else {
            vec![json!({"text": text})]
        });
    }
    if content.is_null() {
        return Ok(Vec::new());
    }
    let Some(items) = content.as_array() else {
        return Err((StatusCode::BAD_REQUEST, "消息内容须为文本或数组"));
    };
    let mut parts = Vec::new();
    for item in items {
        match item["type"].as_str() {
            Some("input_text" | "output_text" | "text") | None if item.get("text").is_some() => {
                if let Some(text) = item["text"].as_str().filter(|t| !t.is_empty()) {
                    parts.push(json!({"text": text}));
                }
            }
            Some("refusal") => {
                if let Some(text) = item["refusal"].as_str().filter(|t| !t.is_empty()) {
                    parts.push(json!({"text": text}));
                }
            }
            Some("input_image") => {
                let raw = item["image_url"]
                    .as_str()
                    .or_else(|| item.pointer("/image_url/url").and_then(Value::as_str))
                    .ok_or((StatusCode::BAD_REQUEST, "图片输入缺少有效 URL"))?;
                let (mime, data) = parse_data_url(raw).ok_or((
                    StatusCode::BAD_REQUEST,
                    "Google AI Studio 暂仅支持 Base64 内联图片（data:...;base64,...）",
                ))?;
                parts.push(json!({"inlineData": {"mimeType": mime, "data": data}}));
            }
            Some("input_file") => {
                if let Some(raw) = item
                    .get("file_data")
                    .or_else(|| item.get("file_url"))
                    .and_then(Value::as_str)
                {
                    if let Some((mime, data)) = parse_data_url(raw) {
                        parts.push(json!({"inlineData": {"mimeType": mime, "data": data}}));
                    } else if let Some(mime) = item["mime_type"].as_str().filter(|m| !m.is_empty()) {
                        let cleaned: String = raw.chars().filter(|c| !c.is_ascii_whitespace()).collect();
                        if base64::engine::general_purpose::STANDARD.decode(&cleaned).is_err() {
                            return Err((StatusCode::BAD_REQUEST, "文件内容不是有效的 Base64 数据"));
                        }
                        parts.push(json!({"inlineData": {"mimeType": mime, "data": cleaned}}));
                    } else {
                        return Err((
                            StatusCode::BAD_REQUEST,
                            "Google AI Studio 文件输入须提供 data URL 或 mime_type",
                        ));
                    }
                } else {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        "Google AI Studio 暂不支持通过远程 file_id 引用文件",
                    ));
                }
            }
            _ if assistant => {}
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Google AI Studio 暂不支持该输入内容类型",
                ))
            }
        }
    }
    Ok(parts)
}

fn sanitize_schema(value: &Value) -> Option<Value> {
    let obj = value.as_object()?;
    let mut out = Map::new();

    if let Some(any_of) = obj.get("anyOf").and_then(Value::as_array) {
        let non_null: Vec<Value> = any_of
            .iter()
            .filter(|entry| entry.get("type").and_then(Value::as_str) != Some("null"))
            .filter_map(sanitize_schema)
            .collect();
        let has_null = any_of.len() > non_null.len();
        if non_null.len() == 1 {
            if let Some(mut inner) = non_null.into_iter().next().and_then(|v| v.as_object().cloned()) {
                if has_null {
                    inner.insert("nullable".into(), json!(true));
                }
                if let Some(desc) = obj.get("description") {
                    inner.entry("description").or_insert_with(|| desc.clone());
                }
                return Some(Value::Object(inner));
            }
        } else if !non_null.is_empty() {
            out.insert("anyOf".into(), Value::Array(non_null));
            if has_null {
                out.insert("nullable".into(), json!(true));
            }
        }
    }

    if let Some(raw_type) = obj.get("type") {
        match raw_type {
            Value::String(s) => {
                if let Some(mapped) = map_schema_type(s) {
                    out.insert("type".into(), json!(mapped));
                }
            }
            Value::Array(types) => {
                let mut non_null = Vec::new();
                let mut has_null = false;
                for item in types {
                    if item.as_str() == Some("null") {
                        has_null = true;
                    } else if let Some(s) = item.as_str().and_then(map_schema_type) {
                        non_null.push(s);
                    }
                }
                if let Some(first) = non_null.first() {
                    out.insert("type".into(), json!(first));
                }
                if has_null {
                    out.insert("nullable".into(), json!(true));
                }
            }
            _ => {}
        }
    }

    for key in [
        "description",
        "format",
        "minimum",
        "maximum",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "nullable",
    ] {
        if let Some(v) = obj.get(key) {
            out.insert(key.into(), v.clone());
        }
    }

    if let Some(values) = obj.get("enum").and_then(Value::as_array) {
        let strings: Vec<Value> = values
            .iter()
            .filter_map(|v| v.as_str().map(|s| json!(s)))
            .collect();
        if !strings.is_empty() {
            out.insert("enum".into(), Value::Array(strings));
        }
    } else if let Some(const_val) = obj.get("const").and_then(Value::as_str) {
        out.insert("enum".into(), json!([const_val]));
        out.entry("type").or_insert_with(|| json!("STRING"));
    }

    if let Some(items) = obj.get("items").and_then(sanitize_schema) {
        out.insert("items".into(), items);
    }

    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        let mut cleaned_props = Map::new();
        for (name, schema) in props {
            if let Some(cleaned) = sanitize_schema(schema) {
                cleaned_props.insert(name.clone(), cleaned);
            }
        }
        if !cleaned_props.is_empty() {
            if let Some(required) = obj.get("required").and_then(Value::as_array) {
                let valid_req: Vec<Value> = required
                    .iter()
                    .filter(|r| r.as_str().is_some_and(|name| cleaned_props.contains_key(name)))
                    .cloned()
                    .collect();
                if !valid_req.is_empty() {
                    out.insert("required".into(), Value::Array(valid_req));
                }
            }
            out.insert("properties".into(), Value::Object(cleaned_props));
        }
    }

    if out.get("type").and_then(Value::as_str) == Some("OBJECT") && !out.contains_key("properties") {
        // Gemini rejects OBJECT schemas with missing/empty properties.
        if obj.get("properties").is_none_or(|p| p.as_object().is_some_and(Map::is_empty)) {
            return Some(Value::Object(out));
        }
    }
    Some(Value::Object(out))
}

fn map_schema_type(raw: &str) -> Option<&'static str> {
    match raw.to_ascii_lowercase().as_str() {
        "object" => Some("OBJECT"),
        "string" => Some("STRING"),
        "number" => Some("NUMBER"),
        "integer" => Some("INTEGER"),
        "boolean" => Some("BOOLEAN"),
        "array" => Some("ARRAY"),
        _ => None,
    }
}

fn thinking_config(model: &str, effort: &str) -> Option<Value> {
    let lower = model.to_ascii_lowercase();
    let effort = effort.trim().to_ascii_lowercase();
    let is_gemini_3_or_newer = lower.contains("gemini-3") || lower.contains("gemini-4");
    let is_gemini_25 = lower.contains("gemini-2.5");
    if is_gemini_3_or_newer {
        let (level, include_thoughts) = match effort.as_str() {
            "none" => ("minimal", false),
            "minimal" => ("minimal", true),
            "low" => ("low", true),
            "medium" => ("medium", true),
            "high" | "xhigh" => ("high", true),
            _ => return None,
        };
        return Some(json!({
            "includeThoughts": include_thoughts,
            "thinkingLevel": level,
        }));
    }
    if is_gemini_25 {
        let budget = match effort.as_str() {
            "none" => 0,
            "minimal" => 512,
            "low" => 1024,
            "medium" => 8192,
            "high" | "xhigh" => 24576,
            _ => return None,
        };
        return Some(json!({
            "includeThoughts": budget > 0,
            "thinkingBudget": budget,
        }));
    }
    None
}

fn parse_function_arguments(raw: Option<&Value>) -> Result<Value, (StatusCode, &'static str)> {
    match raw {
        None | Some(Value::Null) => Ok(json!({})),
        Some(Value::Object(_)) => Ok(raw.unwrap().clone()),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Ok(json!({}));
            }
            let parsed: Value = serde_json::from_str(trimmed)
                .map_err(|_| (StatusCode::BAD_REQUEST, "历史工具调用参数必须是有效的 JSON"))?;
            if parsed.is_object() {
                Ok(parsed)
            } else {
                Ok(json!({"value": parsed}))
            }
        }
        Some(other) => Ok(json!({"value": other})),
    }
}

fn parse_function_response(raw: &Value) -> Value {
    if raw.is_object() {
        return raw.clone();
    }
    if let Some(text) = raw.as_str() {
        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(text) {
            return Value::Object(obj);
        }
        return json!({"output": text});
    }
    if let Some(items) = raw.as_array() {
        let texts: Vec<&str> = items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect();
        if !texts.is_empty() && texts.len() == items.len() {
            let joined = texts.join("\n");
            if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&joined) {
                return Value::Object(obj);
            }
            return json!({"output": joined});
        }
    }
    json!({"output": raw})
}

pub(crate) fn build_request(
    source: &Source,
    parsed: &Value,
    signatures: &ThoughtSignatures,
) -> Result<PreparedGoogleRequest, (StatusCode, &'static str)> {
    let model = parsed["model"]
        .as_str()
        .ok_or((StatusCode::BAD_REQUEST, "请求缺少 model"))?;
    let normalized_model = normalize_model(model)?;
    let stream = parsed["stream"].as_bool().unwrap_or(false);
    let url = endpoint(source, normalized_model, stream)?;

    let mut system_parts = Vec::new();
    if let Some(instructions) = parsed["instructions"].as_str().filter(|s| !s.is_empty()) {
        system_parts.push(json!({"text": instructions}));
    }

    let mut contents = Vec::new();
    let mut call_names: HashMap<String, String> = HashMap::new();

    match parsed.get("input") {
        Some(Value::String(text)) => {
            if !text.is_empty() {
                push_turn(&mut contents, "user", vec![json!({"text": text})]);
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                let kind = item["type"].as_str();
                let role = item["role"].as_str();
                match (kind, role) {
                    (_, Some("system" | "developer")) => {
                        extract_system_parts(&item["content"], &mut system_parts)?;
                    }
                    (_, Some("user")) => {
                        let parts = convert_message_parts(&item["content"], false)?;
                        push_turn(&mut contents, "user", parts);
                    }
                    (_, Some("assistant")) => {
                        let parts = convert_message_parts(&item["content"], true)?;
                        push_turn(&mut contents, "model", parts);
                    }
                    (Some("reasoning"), _) => {}
                    (Some("function_call"), _) => {
                        let name = item["name"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .ok_or((StatusCode::BAD_REQUEST, "函数调用缺少名称"))?;
                        let call_id = item["call_id"]
                            .as_str()
                            .or_else(|| item["id"].as_str())
                            .unwrap_or("");
                        if !call_id.is_empty() {
                            call_names.insert(call_id.to_owned(), name.to_owned());
                        }
                        let args = parse_function_arguments(item.get("arguments"))?;
                        let mut part = json!({"functionCall": {"name": name, "args": args}});
                        if let Some(sig) = signatures
                            .get(call_id)
                            .or_else(|| item["thought_signature"].as_str().map(str::to_owned))
                        {
                            part["thoughtSignature"] = json!(sig);
                        }
                        push_turn(&mut contents, "model", vec![part]);
                    }
                    (Some("function_call_output"), _) => {
                        let call_id = item["call_id"].as_str().unwrap_or("");
                        let name = call_names
                            .get(call_id)
                            .map(String::as_str)
                            .or_else(|| item["name"].as_str())
                            .filter(|s| !s.is_empty())
                            .ok_or((StatusCode::BAD_REQUEST, "工具结果缺少对应的函数调用"))?;
                        let response = parse_function_response(&item["output"]);
                        push_turn(
                            &mut contents,
                            "user",
                            vec![json!({"functionResponse": {"name": name, "response": response}})],
                        );
                    }
                    (Some("item_reference"), _) => {
                        return Err((
                            StatusCode::BAD_REQUEST,
                            "Google AI Studio 不支持引用上游历史条目，请发送完整上下文",
                        ));
                    }
                    _ => {
                        return Err((
                            StatusCode::BAD_REQUEST,
                            "Google AI Studio 暂不支持该输入条目格式",
                        ));
                    }
                }
            }
        }
        _ => {
            return Err((StatusCode::BAD_REQUEST, "请求缺少有效的 input 内容"));
        }
    }

    if contents.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "请求缺少有效对话消息"));
    }

    let mut request = Map::new();
    request.insert("contents".into(), Value::Array(contents));
    if !system_parts.is_empty() {
        request.insert("systemInstruction".into(), json!({"parts": system_parts}));
    }

    let mut declarations = Vec::new();
    if let Some(tools) = parsed.get("tools").and_then(Value::as_array) {
        for tool in tools {
            if tool["type"] != "function" {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Google AI Studio 来源当前仅支持函数工具",
                ));
            }
            let function = tool.get("function").filter(|v| v.is_object()).unwrap_or(tool);
            let name = function["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or((StatusCode::BAD_REQUEST, "工具定义缺少函数名称"))?;
            let mut decl = Map::new();
            decl.insert("name".into(), json!(name));
            if let Some(desc) = function.get("description").and_then(Value::as_str) {
                decl.insert("description".into(), json!(desc));
            }
            if let Some(raw_params) = function.get("parameters") {
                if let Some(mut cleaned) = sanitize_schema(raw_params) {
                    let empty_obj = cleaned.get("type").and_then(Value::as_str) == Some("OBJECT")
                        && cleaned
                            .get("properties")
                            .is_none_or(|p| p.as_object().is_some_and(Map::is_empty));
                    if !empty_obj {
                        if let Some(obj) = cleaned.as_object_mut() {
                            obj.entry("type").or_insert_with(|| json!("OBJECT"));
                        }
                        decl.insert("parameters".into(), cleaned);
                    }
                }
            }
            declarations.push(Value::Object(decl));
        }
    }
    if !declarations.is_empty() {
        request.insert("tools".into(), json!([{"functionDeclarations": declarations}]));
        if let Some(choice) = parsed.get("tool_choice").filter(|v| !v.is_null()) {
            let config = match choice {
                Value::String(mode) => match mode.as_str() {
                    "none" => Some(json!({"functionCallingConfig": {"mode": "NONE"}})),
                    "auto" => Some(json!({"functionCallingConfig": {"mode": "AUTO"}})),
                    "required" => Some(json!({"functionCallingConfig": {"mode": "ANY"}})),
                    _ => return Err((StatusCode::BAD_REQUEST, "不支持的 tool_choice 模式")),
                },
                Value::Object(obj) => {
                    if obj.get("type").and_then(Value::as_str) != Some("function") {
                        return Err((StatusCode::BAD_REQUEST, "Google AI Studio 仅支持指定函数工具"));
                    }
                    let name = obj
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| obj.get("function").and_then(|f| f.get("name")).and_then(Value::as_str))
                        .filter(|s| !s.is_empty())
                        .ok_or((StatusCode::BAD_REQUEST, "tool_choice 缺少函数名称"))?;
                    Some(json!({
                        "functionCallingConfig": {
                            "mode": "ANY",
                            "allowedFunctionNames": [name]
                        }
                    }))
                }
                _ => return Err((StatusCode::BAD_REQUEST, "tool_choice 格式无效")),
            };
            if let Some(config) = config {
                request.insert("toolConfig".into(), config);
            }
        }
    }

    let mut generation = Map::new();
    if let Some(temp) = parsed.get("temperature").filter(|v| v.is_number()) {
        generation.insert("temperature".into(), temp.clone());
    }
    if let Some(top_p) = parsed.get("top_p").filter(|v| v.is_number()) {
        generation.insert("topP".into(), top_p.clone());
    }
    if let Some(max_tokens) = parsed.get("max_output_tokens").and_then(Value::as_u64) {
        generation.insert("maxOutputTokens".into(), json!(max_tokens));
    }
    if let Some(format) = parsed.pointer("/text/format").filter(|v| v.is_object()) {
        match format["type"].as_str() {
            Some("json_object") => {
                generation.insert("responseMimeType".into(), json!("application/json"));
            }
            Some("json_schema") => {
                generation.insert("responseMimeType".into(), json!("application/json"));
                if let Some(schema) = format.get("schema").and_then(sanitize_schema) {
                    generation.insert("responseSchema".into(), schema);
                }
            }
            _ => {}
        }
    }
    if let Some(effort) = parsed.pointer("/reasoning/effort").and_then(Value::as_str) {
        if let Some(thinking) = thinking_config(normalized_model, effort) {
            generation.insert("thinkingConfig".into(), thinking);
        }
    }
    if !generation.is_empty() {
        request.insert("generationConfig".into(), Value::Object(generation));
    }

    let body = serde_json::to_vec(&Value::Object(request))
        .map_err(|_| (StatusCode::BAD_REQUEST, "无法序列化请求"))?;
    Ok(PreparedGoogleRequest {
        url,
        body: Bytes::from(body),
        stream,
    })
}

fn error_mapping(status: StatusCode, grpc_status: Option<&str>) -> (&'static str, &'static str) {
    match (status.as_u16(), grpc_status.unwrap_or("")) {
        (_, "RESOURCE_EXHAUSTED") | (429, _) => ("rate_limit_exceeded", "rate_limit_exceeded"),
        (_, "UNAUTHENTICATED") | (401, _) => ("authentication_error", "invalid_api_key"),
        (_, "PERMISSION_DENIED") | (403, _) => ("permission_error", "permission_denied"),
        (_, "NOT_FOUND") | (404, _) => ("invalid_request_error", "model_not_found"),
        (_, "INVALID_ARGUMENT") | (400, _) => ("invalid_request_error", "invalid_request_error"),
        (500..=599, _) => ("server_error", "server_error"),
        _ => ("invalid_request_error", "upstream_error"),
    }
}

pub(crate) fn normalize_error_value(status: StatusCode, value: &Value) -> Value {
    let root = value
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(value);
    let err = root.get("error").unwrap_or(root);
    let grpc_status = err["status"].as_str();
    let (err_type, err_code) = error_mapping(status, grpc_status);
    let message = err["message"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Google AI Studio 请求被拒绝");
    json!({
        "error": {
            "message": message,
            "type": err_type,
            "code": err_code
        }
    })
}

pub(crate) fn translate_error(status: StatusCode, body: &[u8]) -> Bytes {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        let normalized = normalize_error_value(status, &value);
        return Bytes::from(serde_json::to_vec(&normalized).unwrap());
    }
    let (err_type, err_code) = error_mapping(status, None);
    Bytes::from(
        json!({
            "error": {
                "message": "Google AI Studio 请求失败，请检查密钥或模型设置",
                "type": err_type,
                "code": err_code
            }
        })
        .to_string(),
    )
}

fn translate_usage(usage: &Value) -> Option<Value> {
    if !usage.is_object() {
        return None;
    }
    let prompt = usage["promptTokenCount"].as_u64();
    let cached = usage["cachedContentTokenCount"].as_u64();
    let candidates = usage["candidatesTokenCount"].as_u64();
    let thoughts = usage["thoughtsTokenCount"].as_u64();
    if prompt.is_none() && candidates.is_none() && thoughts.is_none() {
        return None;
    }
    let input_tokens = prompt.unwrap_or(0);
    let output_tokens = candidates.unwrap_or(0).saturating_add(thoughts.unwrap_or(0));
    let total_tokens = usage["totalTokenCount"]
        .as_u64()
        .unwrap_or_else(|| input_tokens.saturating_add(output_tokens))
        .max(input_tokens.saturating_add(output_tokens));
    let mut result = json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens,
    });
    if let Some(cached) = cached {
        result["input_tokens_details"] = json!({"cached_tokens": cached.min(input_tokens)});
    }
    if let Some(thoughts) = thoughts {
        result["output_tokens_details"] = json!({"reasoning_tokens": thoughts});
    }
    Some(result)
}

fn status_from_finish_reason(reason: Option<&str>) -> (&'static str, Value) {
    match reason {
        Some("MAX_TOKENS") => ("incomplete", json!({"reason": "max_output_tokens"})),
        Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY") => {
            ("incomplete", json!({"reason": "content_filter"}))
        }
        _ => ("completed", Value::Null),
    }
}

pub(crate) fn translate_non_streaming(
    body: &[u8],
    model: &str,
    signatures: &ThoughtSignatures,
) -> Result<Bytes, &'static str> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| "Google AI Studio 返回了无效 JSON")?;
    if value.get("error").is_some_and(|e| !e.is_null()) {
        return Err("Google AI Studio 返回了异常结果");
    }
    let candidate = value["candidates"]
        .as_array()
        .and_then(|c| c.first())
        .ok_or("Google AI Studio 响应缺少候选输出")?;
    let mut output = Vec::new();
    let mut reasoning_text = String::new();
    let mut message_text = String::new();
    let mut pending_sig: Option<String> = None;

    for part in candidate["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(sig) = part["thoughtSignature"].as_str().filter(|s| !s.is_empty()) {
            pending_sig = Some(sig.to_owned());
        }
        if let Some(fc) = part.get("functionCall") {
            if !reasoning_text.is_empty() {
                output.push(json!({
                    "id": format!("rs_{}", uuid::Uuid::new_v4().simple()),
                    "type": "reasoning",
                    "status": "completed",
                    "summary": [{"type": "summary_text", "text": std::mem::take(&mut reasoning_text)}],
                }));
            }
            if !message_text.is_empty() {
                output.push(json!({
                    "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": std::mem::take(&mut message_text), "annotations": []}],
                }));
            }
            let name = fc["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("Google AI Studio 工具调用缺少函数名")?;
            let call_id = fc["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("call_g_{}", uuid::Uuid::new_v4().simple()));
            if let Some(sig) = part["thoughtSignature"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or(pending_sig.as_deref())
            {
                signatures.remember(&call_id, sig);
            }
            pending_sig = None;
            let args = serde_json::to_string(fc.get("args").unwrap_or(&json!({}))).unwrap_or_else(|_| "{}".into());
            output.push(json!({
                "id": format!("fc_{}", uuid::Uuid::new_v4().simple()),
                "type": "function_call",
                "status": "completed",
                "call_id": call_id,
                "name": name,
                "arguments": args,
            }));
        } else if part["thought"] == true {
            if let Some(text) = part["text"].as_str() {
                reasoning_text.push_str(text);
            }
        } else if let Some(text) = part["text"].as_str() {
            if !reasoning_text.is_empty() {
                output.push(json!({
                    "id": format!("rs_{}", uuid::Uuid::new_v4().simple()),
                    "type": "reasoning",
                    "status": "completed",
                    "summary": [{"type": "summary_text", "text": std::mem::take(&mut reasoning_text)}],
                }));
            }
            message_text.push_str(text);
        }
    }

    if !reasoning_text.is_empty() {
        output.push(json!({
            "id": format!("rs_{}", uuid::Uuid::new_v4().simple()),
            "type": "reasoning",
            "status": "completed",
            "summary": [{"type": "summary_text", "text": reasoning_text}],
        }));
    }
    if !message_text.is_empty() || output.is_empty() {
        output.push(json!({
            "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": message_text, "annotations": []}],
        }));
    }

    let (status, incomplete_details) =
        status_from_finish_reason(candidate["finishReason"].as_str());
    let mut response = json!({
        "id": format!("resp_g_{}", uuid::Uuid::new_v4().simple()),
        "object": "response",
        "created_at": crate::model::now(),
        "status": status,
        "incomplete_details": incomplete_details,
        "model": value["modelVersion"].as_str().unwrap_or(model),
        "output": output,
    });
    if let Some(usage) = translate_usage(&value["usageMetadata"]) {
        response["usage"] = usage;
    }
    Ok(Bytes::from(serde_json::to_vec(&response).unwrap()))
}

struct StreamTranslator {
    response_id: String,
    created_at: u64,
    model: String,
    signatures: ThoughtSignatures,
    started: bool,
    failed: bool,
    saw_candidate: bool,
    sequence: usize,
    next_output_index: usize,
    active_reasoning: Option<(usize, String, String)>,
    active_message: Option<(usize, String, String)>,
    completed_items: Vec<Value>,
    finish_reason: Option<String>,
    usage: Option<Value>,
    pending_signature: Option<String>,
}

impl StreamTranslator {
    fn new(model: String, signatures: ThoughtSignatures) -> Self {
        Self {
            response_id: format!("resp_g_{}", uuid::Uuid::new_v4().simple()),
            created_at: crate::model::now(),
            model,
            signatures,
            started: false,
            failed: false,
            saw_candidate: false,
            sequence: 0,
            next_output_index: 0,
            active_reasoning: None,
            active_message: None,
            completed_items: Vec::new(),
            finish_reason: None,
            usage: None,
            pending_signature: None,
        }
    }

    fn emit(&mut self, mut event: Value, out: &mut Vec<u8>) {
        let name = event["type"].as_str().unwrap_or("message").to_owned();
        event["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        out.extend_from_slice(b"event: ");
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b"\ndata: ");
        serde_json::to_writer(&mut *out, &event).expect("JSON event");
        out.extend_from_slice(b"\n\n");
    }

    fn ensure_started(&mut self, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        let base = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": "in_progress",
            "model": self.model,
            "output": [],
            "usage": Value::Null
        });
        self.emit(json!({"type": "response.created", "response": base}), out);
        self.emit(json!({"type": "response.in_progress", "response": base}), out);
    }

    fn close_reasoning(&mut self, out: &mut Vec<u8>) {
        let Some((index, item_id, text)) = self.active_reasoning.take() else {
            return;
        };
        let part = json!({"type": "summary_text", "text": text});
        self.emit(
            json!({
                "type": "response.reasoning_summary_text.done",
                "item_id": item_id,
                "output_index": index,
                "summary_index": 0,
                "text": part["text"]
            }),
            out,
        );
        self.emit(
            json!({
                "type": "response.reasoning_summary_part.done",
                "item_id": item_id,
                "output_index": index,
                "summary_index": 0,
                "part": part
            }),
            out,
        );
        let item = json!({
            "id": item_id,
            "type": "reasoning",
            "status": "completed",
            "summary": [part]
        });
        self.emit(
            json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": item
            }),
            out,
        );
        self.completed_items.push(item);
    }

    fn close_message(&mut self, out: &mut Vec<u8>) {
        let Some((index, item_id, text)) = self.active_message.take() else {
            return;
        };
        let part = json!({"type": "output_text", "text": text, "annotations": []});
        self.emit(
            json!({
                "type": "response.output_text.done",
                "item_id": item_id,
                "output_index": index,
                "content_index": 0,
                "text": part["text"],
                "logprobs": []
            }),
            out,
        );
        self.emit(
            json!({
                "type": "response.content_part.done",
                "item_id": item_id,
                "output_index": index,
                "content_index": 0,
                "part": part
            }),
            out,
        );
        let item = json!({
            "id": item_id,
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [part]
        });
        self.emit(
            json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": item
            }),
            out,
        );
        self.completed_items.push(item);
    }

    fn process_chunk(&mut self, chunk_json: &Value, out: &mut Vec<u8>) {
        if let Some(model_ver) = chunk_json["modelVersion"].as_str().filter(|s| !s.is_empty()) {
            if !self.started {
                self.model = model_ver.to_owned();
            }
        }
        if let Some(usage) = translate_usage(&chunk_json["usageMetadata"]) {
            self.usage = Some(usage);
        }
        if let Some(err) = chunk_json.get("error").filter(|e| !e.is_null()) {
            self.ensure_started(out);
            self.close_reasoning(out);
            self.close_message(out);
            let normalized = normalize_error_value(StatusCode::BAD_GATEWAY, chunk_json);
            let mut failed = json!({
                "id": self.response_id,
                "object": "response",
                "created_at": self.created_at,
                "status": "failed",
                "model": self.model,
                "output": self.completed_items,
                "error": normalized["error"]
            });
            if err["status"].as_str() == Some("RESOURCE_EXHAUSTED") {
                failed["error"]["code"] = json!("rate_limit_exceeded");
            }
            self.emit(json!({"type": "response.failed", "response": failed}), out);
            self.failed = true;
            return;
        }
        let Some(candidate) = chunk_json["candidates"].as_array().and_then(|c| c.first()) else {
            return;
        };
        self.saw_candidate = true;
        if let Some(reason) = candidate["finishReason"].as_str().filter(|s| !s.is_empty()) {
            self.finish_reason = Some(reason.to_owned());
        }
        self.ensure_started(out);
        for part in candidate["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(sig) = part["thoughtSignature"].as_str().filter(|s| !s.is_empty()) {
                self.pending_signature = Some(sig.to_owned());
            }
            if let Some(fc) = part.get("functionCall") {
                self.close_reasoning(out);
                self.close_message(out);
                let Some(name) = fc["name"].as_str().filter(|s| !s.is_empty()) else {
                    continue;
                };
                let call_id = fc["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("call_g_{}", uuid::Uuid::new_v4().simple()));
                if let Some(sig) = part["thoughtSignature"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .or(self.pending_signature.as_deref())
                {
                    self.signatures.remember(&call_id, sig);
                }
                self.pending_signature = None;
                let args = serde_json::to_string(fc.get("args").unwrap_or(&json!({})))
                    .unwrap_or_else(|_| "{}".into());
                let index = self.next_output_index;
                self.next_output_index += 1;
                let item_id = format!("fc_{}", uuid::Uuid::new_v4().simple());
                self.emit(
                    json!({
                        "type": "response.output_item.added",
                        "output_index": index,
                        "item": {
                            "id": item_id,
                            "type": "function_call",
                            "status": "in_progress",
                            "call_id": call_id,
                            "name": name,
                            "arguments": ""
                        }
                    }),
                    out,
                );
                self.emit(
                    json!({
                        "type": "response.function_call_arguments.delta",
                        "item_id": item_id,
                        "output_index": index,
                        "delta": args
                    }),
                    out,
                );
                self.emit(
                    json!({
                        "type": "response.function_call_arguments.done",
                        "item_id": item_id,
                        "output_index": index,
                        "arguments": args
                    }),
                    out,
                );
                let completed = json!({
                    "id": item_id,
                    "type": "function_call",
                    "status": "completed",
                    "call_id": call_id,
                    "name": name,
                    "arguments": args
                });
                self.emit(
                    json!({
                        "type": "response.output_item.done",
                        "output_index": index,
                        "item": completed
                    }),
                    out,
                );
                self.completed_items.push(completed);
            } else if part["thought"] == true {
                let text = part["text"].as_str().unwrap_or("");
                if text.is_empty() {
                    continue;
                }
                self.close_message(out);
                if self.active_reasoning.is_none() {
                    let index = self.next_output_index;
                    self.next_output_index += 1;
                    let item_id = format!("rs_{}", uuid::Uuid::new_v4().simple());
                    self.emit(
                        json!({
                            "type": "response.output_item.added",
                            "output_index": index,
                            "item": {"id": item_id, "type": "reasoning", "status": "in_progress", "summary": []}
                        }),
                        out,
                    );
                    self.emit(
                        json!({
                            "type": "response.reasoning_summary_part.added",
                            "item_id": item_id,
                            "output_index": index,
                            "summary_index": 0,
                            "part": {"type": "summary_text", "text": ""}
                        }),
                        out,
                    );
                    self.active_reasoning = Some((index, item_id, String::new()));
                }
                if let Some((index, item_id, acc)) = &mut self.active_reasoning {
                    let index = *index;
                    let item_id = item_id.clone();
                    acc.push_str(text);
                    self.emit(
                        json!({
                            "type": "response.reasoning_summary_text.delta",
                            "item_id": item_id,
                            "output_index": index,
                            "summary_index": 0,
                            "delta": text
                        }),
                        out,
                    );
                }
            } else if let Some(text) = part["text"].as_str() {
                if text.is_empty() && self.active_message.is_some() {
                    continue;
                }
                self.close_reasoning(out);
                if self.active_message.is_none() {
                    let index = self.next_output_index;
                    self.next_output_index += 1;
                    let item_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
                    self.emit(
                        json!({
                            "type": "response.output_item.added",
                            "output_index": index,
                            "item": {"id": item_id, "type": "message", "role": "assistant", "status": "in_progress", "content": []}
                        }),
                        out,
                    );
                    self.emit(
                        json!({
                            "type": "response.content_part.added",
                            "item_id": item_id,
                            "output_index": index,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []}
                        }),
                        out,
                    );
                    self.active_message = Some((index, item_id, String::new()));
                }
                if !text.is_empty() {
                    if let Some((index, item_id, acc)) = &mut self.active_message {
                        let index = *index;
                        let item_id = item_id.clone();
                        acc.push_str(text);
                        self.emit(
                            json!({
                                "type": "response.output_text.delta",
                                "item_id": item_id,
                                "output_index": index,
                                "content_index": 0,
                                "delta": text,
                                "logprobs": []
                            }),
                            out,
                        );
                    }
                }
            }
        }
    }

    fn finish(mut self) -> Result<Bytes, ()> {
        if self.failed {
            return Ok(Bytes::new());
        }
        if !self.saw_candidate {
            return Err(());
        }
        let mut out = Vec::new();
        self.close_reasoning(&mut out);
        self.close_message(&mut out);
        let (status, incomplete_details) =
            status_from_finish_reason(self.finish_reason.as_deref());
        let event_type = if status == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
        };
        let mut response = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "incomplete_details": incomplete_details,
            "model": self.model,
            "output": self.completed_items,
        });
        if let Some(usage) = self.usage.take() {
            response["usage"] = usage;
        }
        self.emit(json!({"type": event_type, "response": response}), &mut out);
        Ok(Bytes::from(out))
    }
}

#[derive(Default)]
struct SseFrameDecoder {
    pending: Vec<u8>,
}

impl SseFrameDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, ()> {
        self.pending.extend_from_slice(bytes);
        let mut values = Vec::new();
        loop {
            let boundary = self
                .pending
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|n| (n, 2))
                .into_iter()
                .chain(
                    self.pending
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|n| (n, 4)),
                )
                .min_by_key(|v| v.0);
            let Some((end, size)) = boundary else { break };
            if end > SSE_FRAME_LIMIT {
                return Err(());
            }
            let block: Vec<u8> = self.pending.drain(..end + size).collect();
            if let Some(value) = Self::parse(&block[..end])? {
                values.push(value);
            }
        }
        if self.pending.len() > SSE_FRAME_LIMIT {
            return Err(());
        }
        Ok(values)
    }

    fn finish(&mut self) -> Result<Vec<Value>, ()> {
        let tail = std::mem::take(&mut self.pending);
        Ok(Self::parse(&tail)?.into_iter().collect())
    }

    fn parse(bytes: &[u8]) -> Result<Option<Value>, ()> {
        let text = std::str::from_utf8(bytes).map_err(|_| ())?;
        let mut data = Vec::new();
        for line in text.lines() {
            if let Some(raw) = line.strip_prefix("data:") {
                data.push(raw.strip_prefix(' ').unwrap_or(raw));
            }
        }
        if data.is_empty() {
            return Ok(None);
        }
        let joined = data.join("\n");
        if joined.trim() == "[DONE]" {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(&joined).map_err(|_| ())?;
        Ok(Some(value))
    }
}

pub(crate) fn translate_sse_stream<S, E>(
    mut upstream: S,
    model: String,
    signatures: ThoughtSignatures,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, E>> + Unpin + Send + 'static,
    E: Send + 'static,
{
    async_stream::stream! {
        let mut decoder = SseFrameDecoder::default();
        let mut translator = StreamTranslator::new(model, signatures);
        let mut stream_failed = false;

        while let Some(chunk) = upstream.next().await {
            let Ok(chunk) = chunk else {
                stream_failed = true;
                yield Err(std::io::Error::other("Google AI Studio 流已中断"));
                break;
            };
            let Ok(frames) = decoder.push(&chunk) else {
                stream_failed = true;
                yield Err(std::io::Error::other("Google AI Studio 流式帧无效"));
                break;
            };
            let mut out = Vec::new();
            for frame in frames {
                translator.process_chunk(&frame, &mut out);
            }
            if !out.is_empty() {
                yield Ok(Bytes::from(out));
            }
            if translator.failed {
                break;
            }
        }

        if !stream_failed && !translator.failed {
            match decoder.finish() {
                Ok(frames) => {
                    let mut out = Vec::new();
                    for frame in frames {
                        translator.process_chunk(&frame, &mut out);
                    }
                    if !out.is_empty() {
                        yield Ok(Bytes::from(out));
                    }
                }
                Err(()) => {
                    yield Err(std::io::Error::other("Google AI Studio 流式尾部无效"));
                    return;
                }
            }
            match translator.finish() {
                Ok(final_bytes) => {
                    if !final_bytes.is_empty() {
                        yield Ok(final_bytes);
                    }
                }
                Err(()) => {
                    yield Err(std::io::Error::other("Google AI Studio 未返回有效输出"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    fn google_source() -> Source {
        serde_json::from_value(json!({
            "id": "g-1",
            "name": "Google AI Studio",
            "kind": "google",
            "base_url": "https://generativelanguage.googleapis.com/v1beta",
            "enabled": true,
            "priority": 0,
            "proxy": {"mode": "direct"},
            "models": ["gemini-2.5-flash", "gemini-3-pro-preview"],
            "secret": {"api_key": "AIza-test-fixture"}
        }))
        .unwrap()
    }

    #[test]
    fn builds_gemini_request_with_schema_sanitization_thinking_exclusivity_and_thought_signature() {
        let source = google_source();
        let signatures = ThoughtSignatures::default();
        signatures.remember("call_prev", "sig_xyz_123");

        let parsed = json!({
            "model": "gemini-3-pro-preview",
            "stream": true,
            "instructions": "Be helpful",
            "input": [
                {"role": "developer", "content": "Follow schema"},
                {"role": "user", "content": [
                    {"type": "input_text", "text": "Look"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
                ]},
                {"type": "function_call", "call_id": "call_prev", "name": "lookup", "arguments": "{\"q\":\"weather\"}"},
                {"type": "function_call_output", "call_id": "call_prev", "output": "{\"temp\":22}"}
            ],
            "tools": [{
                "type": "function",
                "name": "lookup",
                "description": "Search",
                "parameters": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "q": {"type": ["string", "null"], "default": "a"}
                    },
                    "required": ["q", "missing"]
                }
            }],
            "tool_choice": {"type": "function", "name": "lookup"},
            "reasoning": {"effort": "medium"},
            "text": {"format": {"type": "json_object"}},
            "max_output_tokens": 500
        });

        let prepared = build_request(&source, &parsed, &signatures).unwrap();
        assert_eq!(
            prepared.url.as_str(),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-pro-preview:streamGenerateContent?alt=sse"
        );
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be helpful");
        assert_eq!(body["systemInstruction"]["parts"][1]["text"], "Follow schema");
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][0]["parts"][1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(body["contents"][1]["role"], "model");
        assert_eq!(body["contents"][1]["parts"][0]["thoughtSignature"], "sig_xyz_123");
        assert_eq!(body["contents"][2]["role"], "user");
        assert_eq!(body["contents"][2]["parts"][0]["functionResponse"]["name"], "lookup");

        let params = &body["tools"][0]["functionDeclarations"][0]["parameters"];
        assert!(params.get("$schema").is_none());
        assert!(params.get("additionalProperties").is_none());
        assert_eq!(params["type"], "OBJECT");
        assert_eq!(params["properties"]["q"]["type"], "STRING");
        assert_eq!(params["properties"]["q"]["nullable"], true);
        assert_eq!(params["required"], json!(["q"]));

        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"], json!(["lookup"]));
        assert_eq!(body["generationConfig"]["responseMimeType"], "application/json");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 500);
        assert_eq!(body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "medium");
        assert!(body["generationConfig"]["thinkingConfig"].get("thinkingBudget").is_none());

        // Gemini 2.5 uses thinkingBudget only.
        let parsed_25 = json!({
            "model": "gemini-2.5-flash",
            "input": "hi",
            "reasoning": {"effort": "low"}
        });
        let body_25: Value =
            serde_json::from_slice(&build_request(&source, &parsed_25, &signatures).unwrap().body)
                .unwrap();
        assert_eq!(body_25["generationConfig"]["thinkingConfig"]["thinkingBudget"], 1024);
        assert!(body_25["generationConfig"]["thinkingConfig"].get("thinkingLevel").is_none());

        // Non-thinking model omits thinkingConfig.
        let parsed_20 = json!({
            "model": "gemini-2.0-flash",
            "input": "hi",
            "reasoning": {"effort": "high"}
        });
        let body_20: Value =
            serde_json::from_slice(&build_request(&source, &parsed_20, &signatures).unwrap().body)
                .unwrap();
        assert!(body_20["generationConfig"].get("thinkingConfig").is_none());
    }

    #[test]
    fn translates_non_streaming_response_with_reasoning_tools_and_subset_usage() {
        let signatures = ThoughtSignatures::default();
        let raw = json!({
            "modelVersion": "gemini-2.5-pro",
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"thought": true, "text": "thinking..."},
                        {"text": "calling tool"},
                        {"functionCall": {"name": "calc", "args": {"x": 2}}, "thoughtSignature": "sig-1"}
                    ]
                },
                "finishReason": "MAX_TOKENS"
            }],
            "usageMetadata": {
                "promptTokenCount": 100,
                "cachedContentTokenCount": 40,
                "candidatesTokenCount": 30,
                "thoughtsTokenCount": 12,
                "totalTokenCount": 142
            }
        });
        let bytes = translate_non_streaming(&serde_json::to_vec(&raw).unwrap(), "gemini-2.5-pro", &signatures).unwrap();
        let resp: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(resp["status"], "incomplete");
        assert_eq!(resp["incomplete_details"]["reason"], "max_output_tokens");
        assert_eq!(resp["usage"]["input_tokens"], 100);
        assert_eq!(resp["usage"]["input_tokens_details"]["cached_tokens"], 40);
        assert_eq!(resp["usage"]["output_tokens"], 42);
        assert_eq!(resp["usage"]["output_tokens_details"]["reasoning_tokens"], 12);
        assert_eq!(resp["output"][0]["type"], "reasoning");
        assert_eq!(resp["output"][1]["type"], "message");
        assert_eq!(resp["output"][2]["type"], "function_call");
        let call_id = resp["output"][2]["call_id"].as_str().unwrap();
        assert_eq!(signatures.get(call_id).as_deref(), Some("sig-1"));
    }

    #[tokio::test]
    async fn translates_sse_stream_including_crlf_thoughts_tools_and_eof_completion() {
        let signatures = ThoughtSignatures::default();
        let chunks = vec![
            Ok::<_, ()>(Bytes::from("data: {\"candidates\":[{\"content\":{\"parts\":[{\"thought\":true,\"text\":\"plan\"}]}}]}\r\n\r\n")),
            Ok(Bytes::from("data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hello \"},{\"text\":\"world\"}]}}]}\n\n")),
            Ok(Bytes::from("data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"go\",\"args\":{\"a\":1}},\"thoughtSignature\":\"sig-stream\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":20,\"candidatesTokenCount\":10,\"thoughtsTokenCount\":5}}\n\n")),
        ];
        let mut translated = Vec::new();
        let mut out_stream = Box::pin(translate_sse_stream(
            stream::iter(chunks),
            "gemini-3-flash".into(),
            signatures.clone(),
        ));
        while let Some(item) = out_stream.next().await {
            translated.extend_from_slice(&item.unwrap());
        }
        let text = String::from_utf8(translated).unwrap();
        assert!(text.contains("event: response.created\n"));
        assert!(text.contains("event: response.reasoning_summary_text.delta\n"));
        assert!(text.contains("event: response.output_text.delta\n"));
        assert!(text.contains("event: response.function_call_arguments.delta\n"));
        assert!(text.contains("event: response.completed\n"));

        // Verify call signature was captured.
        let mut found_call = None;
        for frame in text.split("\n\n") {
            if let Some(data) = frame.lines().find_map(|l| l.strip_prefix("data: ")) {
                let value: Value = serde_json::from_str(data).unwrap();
                if value["type"] == "response.completed" {
                    assert_eq!(value["response"]["usage"]["output_tokens"], 15);
                    found_call = value["response"]["output"][2]["call_id"].as_str().map(str::to_owned);
                }
            }
        }
        assert_eq!(signatures.get(&found_call.unwrap()).as_deref(), Some("sig-stream"));
    }

    #[test]
    fn normalizes_google_errors_into_openai_shape() {
        let err = translate_error(
            StatusCode::TOO_MANY_REQUESTS,
            br#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED"}}"#,
        );
        let parsed: Value = serde_json::from_slice(&err).unwrap();
        assert_eq!(parsed["error"]["code"], "rate_limit_exceeded");
        assert_eq!(parsed["error"]["type"], "rate_limit_exceeded");
        assert_eq!(parsed["error"]["message"], "Quota exceeded");
    }
}
