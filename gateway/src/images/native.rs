use super::{error, payload, MODEL};
use crate::model::AppState;
use anyhow::{bail, Context, Result};
use axum::{body::Body, http::{header, request::Parts, StatusCode}, response::{IntoResponse, Response}, Json};
use serde_json::{json, Map, Value};
use std::sync::Arc;

// A tool declaration advertises capability; only forced hosted-image selection
// opts out of ordinary chat routing and its quota accounting.
pub(crate) fn is_request(value: &Value) -> bool {
    value["tool_choice"]["type"] == "image_generation"
        || value["tool_choice"] == "image_generation"
}

fn adapt(mut request: Value) -> Result<(Value, bool, bool)> {
    let stream = match request.get("stream") {
        None => false,
        Some(Value::Bool(stream)) => *stream,
        _ => bail!("stream 参数须为布尔值"),
    };
    request["model"].as_str().filter(|model| !model.is_empty())
        .context("Responses 请求需要聊天载体模型")?;
    let tools = request.get_mut("tools").and_then(Value::as_array_mut)
        .context("请提供 image_generation 工具")?;
    if tools.len() != 1 || tools[0]["type"] != "image_generation" {
        bail!("原生绘图请求仅支持一个 image_generation 工具");
    }
    let mut tool = match tools.pop().unwrap() {
        Value::Object(tool) => tool,
        _ => bail!("图片工具须为对象"),
    };
    tool.remove("type");
    let output_format = tool.remove("output_format");
    if output_format.as_ref().is_some_and(|format| !matches!(format.as_str(), Some("png" | "jpeg" | "webp"))) {
        bail!("图片输出须为 PNG、JPEG 或 WebP");
    }
    let edit = match tool.remove("action").as_ref().and_then(Value::as_str) {
        Some("generate") => false,
        Some("edit") => true,
        _ => bail!("图片 action 须为 generate 或 edit"),
    };
    let mut input = request.get_mut("input").map(Value::take).context("请提供图片提示词")?;
    let mut prompt = String::new();
    let mut images = Vec::new();
    let messages = input.as_array_mut().context("绘图 input 须为用户消息数组")?;
    for message in messages {
        if message["role"] != "user" {
            bail!("原生绘图仅支持用户输入消息");
        }
        let content = message["content"].as_array_mut().context("绘图消息须包含内容数组")?;
        for part in content {
            match part["type"].as_str() {
                Some("input_text") => {
                    let text = part["text"].as_str().context("图片提示词须为文本")?;
                    if !prompt.is_empty() { prompt.push('\n'); }
                    prompt.push_str(text);
                }
                Some("input_image") => {
                    if !part["image_url"].is_string() {
                        bail!("参考图片须为内嵌 data URL");
                    }
                    let mut image = Map::new();
                    image.insert("image_url".into(), part["image_url"].take());
                    images.push(Value::Object(image));
                }
                _ => bail!("绘图输入仅支持 input_text 和 input_image"),
            }
        }
    }
    let mut obj = Map::new();
    obj.insert("model".into(), json!(MODEL));
    obj.insert("prompt".into(), Value::String(prompt));
    if !images.is_empty() { obj.insert("images".into(), Value::Array(images)); }
    obj.extend(tool);
    let mut payload = payload::adapt_object(obj, edit)?;
    if let Some(format) = output_format.filter(|format| format != "png") {
        payload["output_format"] = format;
    }
    Ok((payload, edit, stream))
}

pub(crate) async fn handle(state: Arc<AppState>, request: Value, parts: Parts) -> Response {
    let (payload, edit, stream) = match adapt(request) {
        Ok(payload) => payload,
        Err(problem) => return error(StatusCode::BAD_REQUEST, &problem.to_string()),
    };
    super::execute(state, payload, edit, parts, Some(stream)).await
}

pub(super) fn respond(mut image: Value, request_id: &str, stream: bool) -> Response {
    let data = image["data"].take();
    let output: Vec<_> = match data {
        Value::Array(items) => items,
        _ => unreachable!("validated image data"),
    }.into_iter().enumerate().map(|(index, mut item)| {
            let mut call = json!({"id":format!("ig_{request_id}_{index}"),
                "type":"image_generation_call","status":"completed","model":MODEL,
                "output_format":item["output_format"]});
            call["result"] = item["b64_json"].take();
            for key in ["revised_prompt", "generation_id"] {
                if !item[key].is_null() { call[key] = item[key].take(); }
            }
            for key in ["size", "quality", "background"] {
                if !image[key].is_null() { call[key] = image[key].clone(); }
            }
            call
        }).collect();
    let mut response = json!({"id":format!("resp_{request_id}"),"object":"response",
        "created_at":image["created"],"status":"completed","model":MODEL,
        "tools":[{"type":"image_generation","model":MODEL,"output_format":output.first().map(|item| &item["output_format"])}]});
    response["output"] = Value::Array(output);
    response["usage"] = image["usage"].take();
    if !stream { return Json(response).into_response(); }
    let events = async_stream::stream! {
        for (index, item) in response["output"].as_array().expect("image output").iter().enumerate() {
            yield Ok::<_, std::io::Error>(bytes::Bytes::from(format!(
                "event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"sequence_number\":{index},\"output_index\":{index},\"item\":{item}}}\n\n")));
        }
        let sequence = response["output"].as_array().expect("image output").len();
        yield Ok::<_, std::io::Error>(bytes::Bytes::from(format!(
            "event: response.completed\ndata: {{\"type\":\"response.completed\",\"sequence_number\":{sequence},\"response\":{response}}}\n\n")));
        yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"data: [DONE]\n\n"));
    };
    ([(header::CONTENT_TYPE, "text/event-stream"), (header::CACHE_CONTROL, "no-cache")],
        Body::from_stream(events)).into_response()
}

pub(super) fn output_format(encoded: &str) -> Option<&'static str> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let prefix = encoded.get(..encoded.len().min(16))?;
    let mut decoded = [0; 12];
    let length = STANDARD.decode_slice(prefix, &mut decoded).ok()?;
    let bytes = &decoded[..length];
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") { Some("png") }
    else if bytes.starts_with(b"\xff\xd8\xff") { Some("jpeg") }
    else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") { Some("webp") }
    else { None }
}
