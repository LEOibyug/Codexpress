use super::{
    error,
    requests::{Format, OutputOptions},
    RESPONSE_CAPTURE_LIMIT,
};
use axum::{
    body::{to_bytes, Body},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn output_headers(original: &HeaderMap, stream: bool) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(if stream {
            "text/event-stream; charset=utf-8"
        } else {
            "application/json"
        }),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    for name in [
        "x-request-id",
        "x-codexpress-adapted-parameters",
        "retry-after",
    ] {
        if let Some(value) = original.get(name) {
            headers.insert(axum::http::HeaderName::from_static(name), value.clone());
        }
    }
    headers
}
fn event(value: &Value) -> Bytes {
    Bytes::from(format!(
        "data: {}\n\n",
        serde_json::to_string(value).unwrap()
    ))
}
fn native_event(value: &Value) -> Bytes {
    Bytes::from(format!(
        "event: {}\ndata: {}\n\n",
        value["type"].as_str().unwrap_or("message"),
        value
    ))
}
fn failure() -> Value {
    json!({"error":{"message":"上游流未正常完成，请检查来源状态后重试","type":"upstream_error","code":"incomplete_upstream_stream"}})
}

#[derive(Default)]
struct Decoder {
    pending: Vec<u8>,
}
impl Decoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, ()> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
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
            if end > RESPONSE_CAPTURE_LIMIT {
                return Err(());
            }
            let block = self.pending.drain(..end + size).collect::<Vec<_>>();
            if let Some(value) = Self::parse(&block[..end])? {
                events.push(value);
            }
        }
        if self.pending.len() > RESPONSE_CAPTURE_LIMIT {
            return Err(());
        }
        Ok(events)
    }
    fn finish(&mut self) -> Result<Vec<Value>, ()> {
        let bytes = std::mem::take(&mut self.pending);
        Ok(Self::parse(&bytes)?.into_iter().collect())
    }
    fn parse(bytes: &[u8]) -> Result<Option<Value>, ()> {
        let text = std::str::from_utf8(bytes).map_err(|_| ())?;
        let mut data = Vec::new();
        let mut name = None;
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("data:") {
                data.push(value.strip_prefix(' ').unwrap_or(value));
            }
            if let Some(value) = line.strip_prefix("event:") {
                name = Some(value.trim());
            }
        }
        if data.is_empty() || data.join("\n").trim() == "[DONE]" {
            return Ok(None);
        }
        let mut value: Value = serde_json::from_str(&data.join("\n")).map_err(|_| ())?;
        if !value.is_object() {
            return Err(());
        }
        if value.get("type").is_none() {
            if let Some(name) = name {
                value["type"] = json!(name);
            }
        }
        Ok(Some(value))
    }
}

fn usage(response: &Value) -> Option<Value> {
    let u = &response["usage"];
    let input = u["input_tokens"].as_u64()?;
    let output = u["output_tokens"].as_u64()?;
    let mut result = json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":u["total_tokens"].as_u64().unwrap_or(input.saturating_add(output))});
    if let Some(cached) = u["input_tokens_details"]["cached_tokens"].as_u64() {
        result["prompt_tokens_details"] = json!({"cached_tokens":cached});
    }
    if let Some(reasoning) = u["output_tokens_details"]["reasoning_tokens"].as_u64() {
        result["completion_tokens_details"] = json!({"reasoning_tokens":reasoning});
    }
    Some(result)
}
fn finish_reason(response: &Value, tools: bool) -> &'static str {
    match response["incomplete_details"]["reason"].as_str() {
        Some("max_output_tokens" | "max_tokens") => "length",
        Some("content_filter") => "content_filter",
        _ if tools => "tool_calls",
        _ => "stop",
    }
}
fn completion(response: &Value, options: &OutputOptions) -> Value {
    let mut text = String::new();
    let mut refusal = String::new();
    let mut calls = Vec::new();
    let mut reasoning = String::new();
    for item in response["output"].as_array().into_iter().flatten() {
        match item["type"].as_str() {
            Some("message")=>for part in item["content"].as_array().into_iter().flatten(){
                if let Some(value)=part["text"].as_str(){text.push_str(value);}
                if let Some(value)=part["refusal"].as_str(){refusal.push_str(value);}
            },
            Some("function_call")=>calls.push(json!({"id":item["call_id"].as_str().or_else(||item["id"].as_str()).unwrap_or(""),"type":"function","function":{"name":item["name"],"arguments":item["arguments"].as_str().unwrap_or("")}})),
            Some("reasoning")=>for part in item["summary"].as_array().into_iter().flatten(){if let Some(value)=part["text"].as_str(){reasoning.push_str(value);}},
            _=>{},
        }
    }
    let mut message = json!({"role":"assistant","content":if text.is_empty() && (!calls.is_empty()||!refusal.is_empty()){Value::Null}else{json!(text)}});
    let tools = !calls.is_empty();
    if tools {
        message["tool_calls"] = json!(calls);
    }
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !refusal.is_empty() {
        message["refusal"] = json!(refusal);
    }
    let mut result = json!({"id":format!("chatcmpl-{}",response["id"].as_str().unwrap_or("local")),"object":"chat.completion","created":response["created_at"].as_u64().unwrap_or_else(crate::model::now),"model":response["model"].as_str().unwrap_or(&options.model),"choices":[{"index":0,"message":message,"finish_reason":finish_reason(response,tools),"logprobs":null}]});
    if let Some(usage) = usage(response) {
        result["usage"] = usage;
    }
    result
}

struct Tool {
    index: usize,
    id: String,
    name: String,
    arguments: String,
}
struct ChatStream {
    id: String,
    model: String,
    created: u64,
    started: bool,
    terminal: bool,
    texts: BTreeMap<(usize, usize), String>,
    tools: BTreeMap<usize, Tool>,
    reasoning: BTreeMap<usize, String>,
    refusals: BTreeMap<(usize, usize), String>,
    response: Option<Value>,
    include_usage: bool,
    observed: usize,
}
impl ChatStream {
    fn new(options: &OutputOptions) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            model: options.model.clone(),
            created: crate::model::now(),
            started: false,
            terminal: false,
            texts: BTreeMap::new(),
            tools: BTreeMap::new(),
            reasoning: BTreeMap::new(),
            refusals: BTreeMap::new(),
            response: None,
            include_usage: options.include_usage,
            observed: 0,
        }
    }
    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        let mut chunk = json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":finish,"logprobs":null}]});
        if self.include_usage {
            chunk["usage"] = Value::Null;
        }
        chunk
    }
    fn ensure_tool(&mut self, index: usize, item: &Value, events: &mut Vec<Value>) {
        if self.tools.contains_key(&index) {
            return;
        }
        let Some(name) = item["name"].as_str() else {
            return;
        };
        let Some(id) = item["call_id"].as_str().or_else(|| item["id"].as_str()) else {
            return;
        };
        let tool = Tool {
            index: self.tools.len(),
            id: id.into(),
            name: name.into(),
            arguments: String::new(),
        };
        events.push(self.chunk(json!({"tool_calls":[{"index":tool.index,"id":tool.id,"type":"function","function":{"name":tool.name,"arguments":""}}]}),None));
        self.tools.insert(index, tool);
    }
    fn full_item(&mut self, index: usize, item: &Value, events: &mut Vec<Value>) -> Result<(), ()> {
        match item["type"].as_str() {
            Some("message") => {
                for (part_index, part) in
                    item["content"].as_array().into_iter().flatten().enumerate()
                {
                    if let Some(text) = part["text"].as_str() {
                        let existing = self.texts.entry((index, part_index)).or_default();
                        let delta = text.strip_prefix(existing.as_str()).ok_or(())?.to_owned();
                        *existing = text.into();
                        if !delta.is_empty() {
                            events.push(self.chunk(json!({"content":delta}), None));
                        }
                    }
                    if let Some(refusal) = part["refusal"].as_str() {
                        let previous = self.refusals.entry((index, part_index)).or_default();
                        let delta = refusal
                            .strip_prefix(previous.as_str())
                            .ok_or(())?
                            .to_owned();
                        *previous = refusal.into();
                        if !delta.is_empty() {
                            events.push(self.chunk(json!({"refusal":delta}), None));
                        }
                    }
                }
            }
            Some("function_call") => {
                if item["name"].as_str().is_none_or(str::is_empty)
                    || item["call_id"]
                        .as_str()
                        .or_else(|| item["id"].as_str())
                        .is_none_or(str::is_empty)
                    || item["arguments"].as_str().is_none()
                {
                    return Err(());
                }
                self.ensure_tool(index, item, events);
                if let Some(tool) = self.tools.get_mut(&index) {
                    let args = item["arguments"].as_str().unwrap_or("");
                    let delta = args.strip_prefix(&tool.arguments).ok_or(())?.to_owned();
                    tool.arguments = args.into();
                    let position = tool.index;
                    if !delta.is_empty() {
                        events.push(self.chunk(json!({"tool_calls":[{"index":position,"function":{"arguments":delta}}]}),None));
                    }
                }
            }
            Some("reasoning") => {
                let summary = item["summary"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<String>();
                let previous = self.reasoning.entry(index).or_default();
                let delta = summary
                    .strip_prefix(previous.as_str())
                    .ok_or(())?
                    .to_owned();
                *previous = summary;
                if !delta.is_empty() {
                    events.push(self.chunk(json!({"reasoning_content":delta}), None));
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn consume(&mut self, event: Value) -> Result<Vec<Value>, ()> {
        if self.terminal {
            return Ok(vec![]);
        }
        self.observed = self
            .observed
            .saturating_add(serde_json::to_vec(&event).map_err(|_| ())?.len());
        if self.observed > RESPONSE_CAPTURE_LIMIT * 4 {
            return Err(());
        }
        let mut events = Vec::new();
        if let Some(response) = event.get("response").filter(|_| !self.started) {
            if let Some(id) = response["id"].as_str() {
                self.id = format!("chatcmpl-{id}");
            }
            if let Some(model) = response["model"].as_str() {
                self.model = model.into();
            }
            if let Some(created) = response["created_at"].as_u64() {
                self.created = created;
            }
        }
        if !self.started {
            events.push(self.chunk(json!({"role":"assistant"}), None));
            self.started = true;
        }
        let index = event["output_index"].as_u64().unwrap_or(0) as usize;
        match event["type"].as_str() {
            Some("response.output_text.delta") => {
                let text = event["delta"].as_str().ok_or(())?;
                self.texts
                    .entry((index, event["content_index"].as_u64().unwrap_or(0) as usize))
                    .or_default()
                    .push_str(text);
                events.push(self.chunk(json!({"content":text}), None));
            }
            Some("response.output_item.added") if event["item"]["type"] == "function_call" => {
                self.ensure_tool(index, &event["item"], &mut events)
            }
            Some("response.function_call_arguments.delta") => {
                let delta = event["delta"].as_str().ok_or(())?;
                if let Some(tool) = self.tools.get_mut(&index) {
                    tool.arguments.push_str(delta);
                    let position = tool.index;
                    events.push(self.chunk(
                        json!({"tool_calls":[{"index":position,"function":{"arguments":delta}}]}),
                        None,
                    ));
                }
            }
            Some("response.output_item.done") => {
                self.full_item(index, &event["item"], &mut events)?
            }
            Some("response.reasoning_summary_text.delta") => {
                let delta = event["delta"].as_str().ok_or(())?;
                self.reasoning.entry(index).or_default().push_str(delta);
                events.push(self.chunk(json!({"reasoning_content":delta}), None));
            }
            Some("response.refusal.delta") => {
                let delta = event["delta"].as_str().ok_or(())?;
                self.refusals
                    .entry((index, event["content_index"].as_u64().unwrap_or(0) as usize))
                    .or_default()
                    .push_str(delta);
                events.push(self.chunk(json!({"refusal":delta}), None));
            }
            Some("response.completed" | "response.incomplete" | "response.done") => {
                let mut response = event["response"].clone();
                if !response.is_object() || response["status"] == "failed" {
                    return Err(());
                }
                if response["output"]
                    .as_array()
                    .is_some_and(|items| !items.iter().all(Value::is_object))
                {
                    return Err(());
                }
                for (i, item) in response["output"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    self.full_item(i, item, &mut events)?;
                }
                if !response["output"].is_array() {
                    let mut output = Vec::new();
                    if !self.texts.is_empty() {
                        output.push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":self.texts.values().cloned().collect::<String>()}]}));
                    }
                    for tool in self.tools.values() {
                        output.push(json!({"type":"function_call","call_id":tool.id,"name":tool.name,"arguments":tool.arguments}));
                    }
                    response["output"] = json!(output);
                }
                events.push(self.chunk(
                    json!({}),
                    Some(finish_reason(&response, !self.tools.is_empty())),
                ));
                if self.include_usage {
                    if let Some(usage) = usage(&response) {
                        let mut chunk = self.chunk(json!({}), None);
                        chunk["choices"] = json!([]);
                        chunk["usage"] = usage;
                        events.push(chunk);
                    }
                }
                self.response = Some(response);
                self.terminal = true;
            }
            Some("error" | "response.failed") => return Err(()),
            _ => {}
        }
        Ok(events)
    }
}

fn json_events(response: &Value) -> Vec<Value> {
    let mut finished = response.clone();
    for item in finished["output"].as_array_mut().unwrap() {
        if item.get("id").is_none() {
            item["id"] = json!(format!("item_{}", uuid::Uuid::new_v4().simple()));
        }
        if item.get("status").is_none() {
            item["status"] = json!("completed");
        }
    }
    let mut created = finished.clone();
    created["status"] = json!("in_progress");
    created["output"] = json!([]);
    created["usage"] = Value::Null;
    let mut events = vec![json!({"type":"response.created","response":created})];
    for (index, item) in finished["output"].as_array().unwrap().iter().enumerate() {
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        if item["type"] == "message" {
            added["content"] = json!([]);
        }
        if item["type"] == "function_call" {
            added["arguments"] = json!("");
        }
        events.push(json!({"type":"response.output_item.added","output_index":index,"item":added}));
        if item["type"] == "message" {
            for (part_index, part) in item["content"].as_array().into_iter().flatten().enumerate() {
                if let Some(text) = part["text"].as_str() {
                    events.push(json!({"type":"response.content_part.added","item_id":item["id"],"output_index":index,"content_index":part_index,"part":{"type":"output_text","text":"","annotations":[]}}));
                    events.push(json!({"type":"response.output_text.delta","item_id":item["id"],"output_index":index,"content_index":part_index,"delta":text,"logprobs":[]}));
                    events.push(json!({"type":"response.output_text.done","item_id":item["id"],"output_index":index,"content_index":part_index,"text":text,"logprobs":[]}));
                    events.push(json!({"type":"response.content_part.done","item_id":item["id"],"output_index":index,"content_index":part_index,"part":part}));
                }
            }
        }
        if item["type"] == "function_call" {
            events.push(json!({"type":"response.function_call_arguments.delta","item_id":item["id"],"output_index":index,"delta":item["arguments"]}));
            events.push(json!({"type":"response.function_call_arguments.done","item_id":item["id"],"output_index":index,"arguments":item["arguments"]}));
        }
        events.push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
    }
    events.push(json!({"type":if finished["status"]=="incomplete"{"response.incomplete"}else{"response.completed"},"response":finished}));
    for (sequence, event) in events.iter_mut().enumerate() {
        event["sequence_number"] = json!(sequence);
    }
    events
}

pub(super) async fn adapt(response: Response, options: OutputOptions) -> Response {
    let status = response.status();
    let headers = output_headers(response.headers(), options.stream);
    if !status.is_success() {
        let _ = to_bytes(response.into_body(), RESPONSE_CAPTURE_LIMIT).await;
        return error(status, "上游拒绝请求，请检查模型、参数或来源状态", None);
    }
    if response
        .headers()
        .get(header::CONTENT_ENCODING)
        .is_some_and(|v| v != "identity")
    {
        return error(
            StatusCode::BAD_GATEWAY,
            "上游返回了无法转换的压缩内容",
            None,
        );
    }
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if is_json {
        let body = match to_bytes(response.into_body(), RESPONSE_CAPTURE_LIMIT).await {
            Ok(b) => b,
            Err(_) => return error(StatusCode::BAD_GATEWAY, "上游响应过大或读取失败", None),
        };
        let value: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "上游未返回有效 Responses JSON",
                    None,
                )
            }
        };
        if !value["output"]
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_object))
            || value["status"] == "failed"
            || value.get("error").is_some_and(|v| !v.is_null())
        {
            return error(StatusCode::BAD_GATEWAY, "上游未返回有效完成结果", None);
        }
        let bytes = if options.stream {
            let mut bytes = Vec::new();
            let mut state = ChatStream::new(&options);
            for event in json_events(&value) {
                if options.format == Format::Responses {
                    bytes.extend_from_slice(&native_event(&event));
                } else {
                    match state.consume(event) {
                        Ok(events) => {
                            for event in events {
                                bytes.extend_from_slice(&self::event(&event));
                            }
                        }
                        Err(_) => return error(StatusCode::BAD_GATEWAY, "上游结果无法转换", None),
                    }
                }
            }
            if options.format == Format::Chat {
                bytes.extend_from_slice(b"data: [DONE]\n\n");
            }
            bytes
        } else {
            serde_json::to_vec(&if options.format == Format::Chat {
                completion(&value, &options)
            } else {
                value
            })
            .unwrap()
        };
        let mut response = Response::new(Body::from(bytes));
        *response.headers_mut() = headers;
        return response;
    }
    if options.stream && options.format == Format::Responses {
        let mut response = response;
        *response.headers_mut() = headers;
        return response;
    }
    let mut upstream = response.into_body().into_data_stream();
    if !options.stream {
        let mut decoder = Decoder::default();
        let mut state = ChatStream::new(&options);
        while let Some(chunk) = upstream.next().await {
            let chunk = match chunk {
                Ok(b) => b,
                Err(_) => return error(StatusCode::BAD_GATEWAY, "上游响应中断", None),
            };
            let events = match decoder.push(&chunk) {
                Ok(e) => e,
                Err(_) => return error(StatusCode::BAD_GATEWAY, "上游流式事件无效或过大", None),
            };
            for event in events {
                if state.consume(event).is_err() {
                    return error(StatusCode::BAD_GATEWAY, "上游未正常完成请求", None);
                }
            }
            if state.terminal {
                break;
            }
        }
        if !state.terminal {
            for event in decoder.finish().unwrap_or_default() {
                if state.consume(event).is_err() {
                    break;
                }
            }
        }
        let Some(value) = state.response else {
            return error(StatusCode::BAD_GATEWAY, "上游未返回完成事件", None);
        };
        let body = if options.format == Format::Chat {
            completion(&value, &options)
        } else {
            value
        };
        let mut response = Response::new(Body::from(serde_json::to_vec(&body).unwrap()));
        *response.headers_mut() = headers;
        return response;
    }
    let stream = async_stream::stream! {
        let mut decoder=Decoder::default();let mut state=ChatStream::new(&options);let mut failed=false;
        while let Some(chunk)=upstream.next().await{
            let events=match chunk{Ok(chunk)=>decoder.push(&chunk),Err(_)=>Err(())};
            let events=match events{Ok(events)=>events,Err(_)=>{failed=true;break;}};
            for incoming in events{
                match state.consume(incoming){Ok(events)=>for value in events{yield Ok::<_,std::io::Error>(event(&value));},Err(_)=>{failed=true;break;}}
            }
            if state.terminal||failed{break;}
        }
        if !state.terminal&&!failed{match decoder.finish(){Ok(events)=>for incoming in events{match state.consume(incoming){Ok(events)=>for value in events{yield Ok(event(&value));},Err(_)=>{failed=true;break;}}},Err(_)=>failed=true}}
        if failed||!state.terminal{yield Ok(event(&failure()));}
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.headers_mut() = headers;
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn buffered_upstream_objects_become_valid_events_for_both_client_protocols() {
        for format in [Format::Chat, Format::Responses] {
            let options = OutputOptions {
                format,
                stream: true,
                include_usage: true,
                model: "m".into(),
            };
            let value = json!({"id":"r","object":"response","model":"m","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]},{"type":"function_call","call_id":"call","name":"f","arguments":"{}"}],"usage":{"input_tokens":1,"output_tokens":2}});
            let response = Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(value.to_string()))
                .unwrap();
            let response = adapt(response, options).await;
            let bytes = to_bytes(response.into_body(), 16384).await.unwrap();
            let events = Decoder::default().push(&bytes).unwrap();
            if format == Format::Chat {
                assert!(events
                    .iter()
                    .any(|v| v["choices"][0]["delta"]["content"] == "hello"));
                assert!(events
                    .iter()
                    .any(|v| v["choices"][0]["finish_reason"] == "tool_calls"));
            } else {
                assert!(events
                    .iter()
                    .any(|v| v["type"] == "response.output_item.added"));
                assert!(events
                    .iter()
                    .any(|v| v["type"] == "response.function_call_arguments.delta"));
                for (i, event) in events.iter().enumerate() {
                    assert_eq!(event["sequence_number"], i);
                }
            }
        }
    }
    #[test]
    fn late_response_metadata_does_not_change_chunk_ids_and_non_objects_are_rejected() {
        let options = OutputOptions {
            format: Format::Chat,
            stream: true,
            include_usage: false,
            model: "m".into(),
        };
        let mut state = ChatStream::new(&options);
        let initial = state
            .consume(json!({"type":"response.output_text.delta","delta":"hello"}))
            .unwrap();
        let final_events=state.consume(json!({"type":"response.completed","response":{"id":"late-id","output":[{"type":"message","content":[{"type":"output_text","text":"hello"}]}]}})).unwrap();
        assert_eq!(initial[0]["id"], final_events[0]["id"]);
        assert!(Decoder::default().push(b"data: []\n\n").is_err());
    }
}
