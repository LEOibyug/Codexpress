use serde_json::{json, Map, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Format {
    Chat,
    Responses,
}
pub(super) struct OutputOptions {
    pub format: Format,
    pub stream: bool,
    pub include_usage: bool,
    pub model: String,
}
pub(crate) struct Invalid {
    pub message: &'static str,
    pub param: &'static str,
}
fn invalid(param: &'static str, message: &'static str) -> Invalid {
    Invalid { message, param }
}
type Result<T> = std::result::Result<T, Invalid>;

fn content(value: &Value, assistant: bool) -> Result<Value> {
    if let Some(text) = value.as_str() {
        return Ok(json!([{"type":if assistant {"output_text"} else {"input_text"},"text":text}]));
    }
    if value.is_null() {
        return Ok(json!([]));
    }
    let array = value
        .as_array()
        .ok_or_else(|| invalid("content", "消息内容须为文本或内容数组"))?;
    let mut result = Vec::new();
    for part in array {
        match part["type"].as_str() {
            Some("text" | "input_text" | "output_text") => {
                let text = part["text"]
                    .as_str()
                    .ok_or_else(|| invalid("content", "文本内容缺少 text"))?;
                let mut item = part.clone();
                item["type"] = json!(if assistant {
                    "output_text"
                } else {
                    "input_text"
                });
                item["text"] = json!(text);
                result.push(item);
            }
            Some("image_url") => {
                let image = &part["image_url"];
                let url = image
                    .as_str()
                    .or_else(|| image["url"].as_str())
                    .ok_or_else(|| invalid("image_url", "图片内容缺少 URL"))?;
                let mut item = json!({"type":"input_image","image_url":url});
                if let Some(detail) = image.get("detail") {
                    item["detail"] = detail.clone();
                }
                result.push(item);
            }
            Some("file") => {
                let file = part["file"]
                    .as_object()
                    .ok_or_else(|| invalid("file", "文件内容格式无效"))?;
                let mut item = file.clone();
                item.insert("type".into(), json!("input_file"));
                result.push(Value::Object(item));
            }
            Some("input_image" | "input_file" | "refusal") => result.push(part.clone()),
            _ => {
                return Err(invalid(
                    "content",
                    "此内容类型无法转换为 Responses；当前支持文本、图片和文件输入",
                ))
            }
        }
    }
    Ok(Value::Array(result))
}

fn messages(messages: &Value) -> Result<Value> {
    let messages = messages
        .as_array()
        .ok_or_else(|| invalid("messages", "messages 须为消息数组"))?;
    let mut input = Vec::new();
    let mut legacy_calls = std::collections::HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        let role = message["role"]
            .as_str()
            .ok_or_else(|| invalid("messages", "消息缺少 role"))?;
        if matches!(role, "tool" | "function") {
            let id = message["tool_call_id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| legacy_calls.remove(message["name"].as_str()?))
                .ok_or_else(|| invalid("tool_call_id", "工具结果缺少对应调用 ID"))?;
            let output = if let Some(text) = message["content"].as_str() {
                json!(text)
            } else {
                content(&message["content"], false)?
            };
            input.push(json!({"type":"function_call_output","call_id":id,"output":output}));
            continue;
        }
        if !matches!(role, "user" | "assistant" | "system" | "developer") {
            return Err(invalid("messages", "不支持此消息角色"));
        }
        let parts = content(&message["content"], role == "assistant")?;
        if !parts.as_array().unwrap().is_empty() {
            input.push(json!({"type":"message","role":role,"content":parts}));
        }
        if let Some(calls) = message.get("tool_calls") {
            for call in calls
                .as_array()
                .ok_or_else(|| invalid("tool_calls", "tool_calls 须为数组"))?
            {
                if call["type"] != "function" {
                    return Err(invalid("tool_calls", "当前仅支持函数工具调用"));
                }
                let id = call["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid("tool_calls", "工具调用缺少 ID"))?;
                let function = &call["function"];
                let name = function["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid("tool_calls", "工具调用缺少名称"))?;
                let arguments = function["arguments"]
                    .as_str()
                    .ok_or_else(|| invalid("tool_calls", "工具参数须为 JSON 字符串"))?;
                input.push(
                    json!({"type":"function_call","call_id":id,"name":name,"arguments":arguments}),
                );
            }
        }
        if let Some(call) = message.get("function_call") {
            let name = call["name"]
                .as_str()
                .ok_or_else(|| invalid("function_call", "函数调用缺少名称"))?;
            let args = call["arguments"]
                .as_str()
                .ok_or_else(|| invalid("function_call", "函数参数须为字符串"))?;
            let id = format!("call_legacy_{index}");
            legacy_calls.insert(name.to_owned(), id.clone());
            input.push(json!({"type":"function_call","call_id":id,"name":name,"arguments":args}));
        }
    }
    Ok(Value::Array(input))
}

fn tools(value: &Value) -> Result<Value> {
    let tools = value
        .as_array()
        .ok_or_else(|| invalid("tools", "tools 须为工具数组"))?;
    let mut result = Vec::new();
    for tool in tools {
        if tool["type"] == "function" && tool.get("function").is_some() {
            let mut function = tool["function"]
                .as_object()
                .ok_or_else(|| invalid("tools", "函数定义格式无效"))?
                .clone();
            function.insert("type".into(), json!("function"));
            result.push(Value::Object(function));
        } else {
            result.push(tool.clone());
        }
    }
    Ok(Value::Array(result))
}
fn aliases(body: &mut Map<String, Value>) -> Result<()> {
    if let Some(verbosity) = body.remove("verbosity") {
        body.entry("text")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| invalid("text", "text 须为对象"))?
            .entry("verbosity")
            .or_insert(verbosity);
    }
    for alias in ["max_completion_tokens", "max_tokens"] {
        if let Some(value) = body.remove(alias) {
            if body
                .get("max_output_tokens")
                .is_some_and(|existing| *existing != value)
            {
                return Err(invalid("max_output_tokens", "输出长度参数相互冲突"));
            }
            body.insert("max_output_tokens".into(), value);
        }
    }
    if let Some(value) = body.remove("reasoning_effort") {
        let reasoning = body
            .entry("reasoning")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| invalid("reasoning", "reasoning 须为对象"))?;
        reasoning.entry("effort").or_insert(value);
    }
    if let Some(value) = body.remove("response_format") {
        let format = if value["type"] == "json_schema" {
            let mut schema = value["json_schema"]
                .as_object()
                .ok_or_else(|| invalid("response_format", "JSON Schema 格式无效"))?
                .clone();
            schema.insert("type".into(), json!("json_schema"));
            Value::Object(schema)
        } else {
            value
        };
        let text = body
            .entry("text")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| invalid("text", "text 须为对象"))?;
        if text.get("format").is_some_and(|old| *old != format) {
            return Err(invalid("response_format", "输出格式设置相互冲突"));
        }
        text.insert("format".into(), format);
    }
    if let Some(value) = body.remove("functions") {
        if body.contains_key("tools") {
            return Err(invalid("tools", "不能同时提供 tools 与旧版 functions"));
        }
        let values = value
            .as_array()
            .ok_or_else(|| invalid("functions", "functions 须为数组"))?;
        body.insert(
            "tools".into(),
            Value::Array(
                values
                    .iter()
                    .map(|f| json!({"type":"function","function":f}))
                    .collect(),
            ),
        );
    }
    if let Some(value) = body.remove("function_call") {
        body.entry("tool_choice").or_insert(if value.is_object() {
            json!({"type":"function","name":value["name"]})
        } else {
            value
        });
    }
    if let Some(value) = body.get("tools").cloned() {
        body.insert("tools".into(), tools(&value)?);
    }
    if let Some(choice) = body.get_mut("tool_choice") {
        if choice["type"] == "function" && choice.get("function").is_some() {
            *choice = json!({"type":"function","name":choice["function"]["name"]});
        }
    }
    Ok(())
}

pub(super) fn normalize(value: Value, format: Format) -> Result<(Value, OutputOptions)> {
    let mut original = value
        .as_object()
        .ok_or_else(|| invalid("body", "请求须为 JSON 对象"))?
        .clone();
    let model = original
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_whitespace))
        .ok_or_else(|| invalid("model", "请提供有效模型 ID"))?
        .to_owned();
    let stream = match original.get("stream") {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| invalid("stream", "stream 须为布尔值"))?,
        None => false,
    };
    let include_usage = original
        .get("stream_options")
        .is_some_and(|v| v["include_usage"] == true);
    let chat_body = format == Format::Chat
        || (!original.contains_key("input") && original.contains_key("messages"));
    let mut body = if chat_body {
        if original
            .get("n")
            .is_some_and(|v| !v.is_null() && v.as_u64() != Some(1))
        {
            return Err(invalid("n", "Responses 每次只支持一个候选结果，请使用 n=1"));
        }
        for key in [
            "stop",
            "logit_bias",
            "logprobs",
            "top_logprobs",
            "presence_penalty",
            "frequency_penalty",
            "seed",
            "modalities",
            "audio",
        ] {
            if original.get(key).is_some_and(|v| {
                if key == "modalities" && *v == json!(["text"]) {
                    return false;
                }
                !v.is_null()
                    && v != false
                    && v.as_i64() != Some(0)
                    && !v.as_array().is_some_and(Vec::is_empty)
                    && !v.as_object().is_some_and(Map::is_empty)
            }) {
                return Err(invalid(
                    key,
                    "此 Chat Completions 参数无法等价转换为 Responses，请移除后重试",
                ));
            }
        }
        let input = messages(
            original
                .get("messages")
                .ok_or_else(|| invalid("messages", "请提供 messages 消息数组"))?,
        )?;
        let mut body = Map::new();
        for key in [
            "model",
            "temperature",
            "top_p",
            "max_tokens",
            "max_completion_tokens",
            "tools",
            "functions",
            "function_call",
            "tool_choice",
            "parallel_tool_calls",
            "reasoning",
            "reasoning_effort",
            "verbosity",
            "response_format",
            "text",
            "service_tier",
            "metadata",
            "store",
            "prompt_cache_key",
            "prompt_cache_retention",
            "safety_identifier",
        ] {
            if let Some(value) = original.remove(key) {
                if !value.is_null() {
                    body.insert(key.into(), value);
                }
            }
        }
        if let Some(user) = original.get("user") {
            body.entry("safety_identifier").or_insert(user.clone());
        }
        body.insert("input".into(), input);
        body
    } else {
        let input = original
            .get("input")
            .cloned()
            .ok_or_else(|| invalid("input", "请提供 input 内容"))?;
        let input = if let Some(text) = input.as_str() {
            json!([{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}])
        } else if input.is_object() {
            json!([input])
        } else if input.is_array() {
            input
        } else {
            return Err(invalid("input", "input 须为文本或输入项数组"));
        };
        original.insert("input".into(), input);
        original
    };
    aliases(&mut body)?;
    if format == Format::Chat
        && body.get("tools").is_some_and(|tools| {
            tools
                .as_array()
                .is_some_and(|items| items.iter().any(|tool| tool["type"] != "function"))
        })
    {
        return Err(invalid(
            "tools",
            "Chat Completions 转换当前仅支持函数工具，其他工具请使用 Responses 接口",
        ));
    }
    if body
        .get("instructions")
        .is_some_and(|v| !v.is_null() && !v.is_string())
    {
        return Err(invalid("instructions", "instructions 须为文本"));
    }
    if body.get("instructions").is_none_or(Value::is_null) {
        body.insert("instructions".into(), json!(""));
    }
    if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
        for item in items {
            if !item.is_object() {
                return Err(invalid("input", "输入项须为对象"));
            }
            if let Some(role) = item["role"].as_str() {
                let parts = content(&item["content"], role == "assistant")?;
                item["content"] = parts;
            }
        }
    }
    // Upstreams may require SSE even for a downstream non-streaming request.
    // Presentation is restored by the egress adapter, never by dropping output.
    body.insert("stream".into(), json!(true));
    body.entry("store").or_insert(json!(false));
    if let Some(options) = body
        .get_mut("stream_options")
        .and_then(Value::as_object_mut)
    {
        options.remove("include_usage");
        if options.is_empty() {
            body.remove("stream_options");
        }
    }
    Ok((
        Value::Object(body),
        OutputOptions {
            format,
            stream,
            include_usage,
            model,
        },
    ))
}

pub(crate) fn upstream_payload(
    body: &Value,
    official: bool,
    history_input: Option<&Value>,
) -> Result<(Vec<u8>, Vec<String>)> {
    let mut body = body.clone();
    let mut ignored = Vec::new();
    if official {
        if body.get("conversation").is_some_and(|v| !v.is_null()) {
            return Err(invalid(
                "conversation",
                "官方来源不支持此会话存储方式，请提供完整 input 历史",
            ));
        }
        if body
            .get("previous_response_id")
            .is_some_and(|v| !v.is_null())
        {
            body["input"] = history_input.cloned().ok_or_else(|| {
                invalid(
                    "previous_response_id",
                    "本地未保存完整历史，请重新发送完整 input",
                )
            })?;
            body.as_object_mut().unwrap().remove("previous_response_id");
        }
        if let Some(items) = body["input"].as_array_mut() {
            for item in items {
                if item["role"] == "system" {
                    item["role"] = json!("developer");
                }
            }
        }
        // The Codex OAuth wire contract accepts a string tool_choice. A forced
        // function keeps its semantics by restricting tools before "required".
        if let Some(choice) = body.get("tool_choice").filter(|v| v.is_object()).cloned() {
            if choice["type"] != "function" {
                return Err(invalid("tool_choice", "官方来源暂不支持此工具选择格式"));
            }
            let name = choice["name"]
                .as_str()
                .ok_or_else(|| invalid("tool_choice", "缺少所选函数名称"))?;
            let selected = body["tools"]
                .as_array()
                .and_then(|tools| {
                    tools
                        .iter()
                        .find(|t| t["type"] == "function" && t["name"] == name)
                })
                .cloned()
                .ok_or_else(|| invalid("tool_choice", "所选函数未在 tools 中定义"))?;
            body["tools"] = json!([selected]);
            body["tool_choice"] = json!("required");
        }
        let allowed = [
            "model",
            "input",
            "instructions",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "reasoning",
            "store",
            "stream",
            "include",
            "prompt_cache_key",
            "text",
            "service_tier",
            "stream_options",
            "client_metadata",
            "access_programs",
        ];
        let map = body.as_object_mut().unwrap();
        map.retain(|key, _| {
            if allowed.contains(&key.as_str()) {
                true
            } else {
                ignored.push(key.clone());
                false
            }
        });
        body["stream"] = json!(true);
        body["store"] = json!(false);
        if body.get("instructions").is_none() {
            body["instructions"] = json!("");
        }
        if body.get("tool_choice").is_none() {
            body["tool_choice"] = json!("auto");
        }
        if body.get("parallel_tool_calls").is_none() {
            body["parallel_tool_calls"] = json!(true);
        }
        let include = body
            .as_object_mut()
            .unwrap()
            .entry("include")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| invalid("include", "include 须为数组"))?;
        if !include.iter().any(|v| v == "reasoning.encrypted_content") {
            include.push(json!("reasoning.encrypted_content"));
        }
    } else if body["max_output_tokens"]
        .as_u64()
        .is_some_and(|v| v > 0 && v < 16)
    {
        body["max_output_tokens"] = json!(16);
        ignored.push("max_output_tokens".into());
    }
    Ok((serde_json::to_vec(&body).unwrap(), ignored))
}

/// Apply only explicit, pre-generation validation rejections. Never interpret
/// network errors, arbitrary 400s or model output as permission to replay.
pub(crate) fn repair(body: &mut Value, rejection: &Value) -> Option<String> {
    let error = rejection.get("error").unwrap_or(rejection);
    let code = error["code"].as_str().unwrap_or("");
    let param = error["param"].as_str().unwrap_or("");
    let message = error["message"]
        .as_str()
        .or_else(|| rejection["detail"].as_str())
        .unwrap_or("");
    for field in [
        "max_output_tokens",
        "temperature",
        "top_p",
        "metadata",
        "safety_identifier",
        "prompt_cache_retention",
    ] {
        let explicit = (code == "unsupported_parameter" && param == field)
            || message == format!("Unsupported parameter: {field}")
            || message == format!("Unsupported parameter: '{field}'")
            || message == format!("Unsupported parameter: \"{field}\"");
        if explicit && body.as_object_mut()?.remove(field).is_some() {
            return Some(field.into());
        }
    }
    if ((param == "instructions" && code == "missing_required_parameter")
        || message == "Instructions are required")
        && body.get("instructions").is_none()
    {
        body["instructions"] = json!("");
        return Some("instructions".into());
    }
    if ((param == "store" && matches!(code, "unsupported_value" | "invalid_value"))
        || message == "Store must be set to false")
        && body["store"] == true
    {
        body["store"] = json!(false);
        return Some("store".into());
    }
    if param == "tool_choice" && code == "invalid_type" && body["tool_choice"]["type"] == "function"
    {
        let name = body["tool_choice"]["name"].as_str()?;
        let selected = body["tools"]
            .as_array()?
            .iter()
            .find(|t| t["type"] == "function" && t["name"] == name)?
            .clone();
        body["tools"] = json!([selected]);
        body["tool_choice"] = json!("required");
        return Some("tool_choice".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chat_maps_roles_images_tools_results_and_structured_output() {
        let (body,_) = normalize(json!({"model":"m","messages":[
            {"role":"system","content":"policy"},
            {"role":"user","content":[{"type":"text","text":"look"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA==","detail":"low"}}]},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_1","content":"result"}],
            "tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],
            "response_format":{"type":"json_schema","json_schema":{"name":"answer","schema":{"type":"object"},"strict":true}},
            "max_tokens":123,"reasoning_effort":"low","temperature":0.4}),Format::Chat).ok().unwrap();
        assert_eq!(body["input"][0]["role"], "system");
        assert_eq!(
            body["input"][1]["content"][1]["image_url"],
            "data:image/png;base64,AA=="
        );
        assert_eq!(body["input"][2]["type"], "function_call");
        assert_eq!(body["input"][3]["call_id"], "call_1");
        assert_eq!(body["tools"][0]["name"], "f");
        assert_eq!(body["text"]["format"]["name"], "answer");
        assert_eq!(body["max_output_tokens"], 123);
        assert_eq!(body["reasoning"]["effort"], "low");
        let (api, ignored) = upstream_payload(&body, false, None).ok().unwrap();
        assert!(ignored.is_empty());
        let api: Value = serde_json::from_slice(&api).unwrap();
        assert_eq!(api["temperature"], 0.4);
    }
    #[test]
    fn official_contract_is_isolated_and_preserves_forced_tool_semantics_and_history() {
        let (body,_) = normalize(json!({"model":"m","input":[{"role":"system","content":"policy"}],"previous_response_id":"old","tools":[{"type":"function","function":{"name":"wanted"}},{"type":"function","function":{"name":"other"}}],"tool_choice":{"type":"function","function":{"name":"wanted"}},"max_output_tokens":100,"temperature":0.2,"metadata":{"PRIVATE":"not in output"},"vendor_flag":true}),Format::Responses).ok().unwrap();
        assert!(upstream_payload(&body, true, None).is_err());
        let history =
            json!([{"role":"user","content":[{"type":"input_text","text":"complete history"}]}]);
        let (bytes, ignored) = upstream_payload(&body, true, Some(&history)).ok().unwrap();
        let official: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(official["input"], history);
        assert!(official.get("previous_response_id").is_none());
        assert_eq!(official["tools"].as_array().unwrap().len(), 1);
        assert_eq!(official["tool_choice"], "required");
        assert_eq!(official["tools"][0]["name"], "wanted");
        assert_eq!(official["store"], false);
        assert_eq!(official["stream"], true);
        assert!(ignored.contains(&"max_output_tokens".to_owned()));
        assert!(!ignored.join(",").contains("PRIVATE"));
        let (api, _) = upstream_payload(&body, false, None).ok().unwrap();
        let api: Value = serde_json::from_slice(&api).unwrap();
        assert_eq!(api["vendor_flag"], true);
        assert_eq!(api["input"][0]["role"], "system");
    }
    #[test]
    fn meaningful_unrepresentable_parameters_and_conflicting_limits_are_rejected() {
        for extra in [
            json!({"n":2}),
            json!({"stop":["stop"]}),
            json!({"max_tokens":20,"max_completion_tokens":30}),
            json!({"messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{}}]}]}),
        ] {
            let mut body = json!({"model":"m","messages":[{"role":"user","content":"hello"}]});
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(normalize(body, Format::Chat).is_err());
        }
    }
}
