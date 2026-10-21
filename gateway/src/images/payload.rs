use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::{FromRequest, Multipart},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::Bytes;
use serde_json::{json, Map, Value};

const IMAGE_LIMIT: usize = 10 * 1024 * 1024;
fn inline(bytes: &[u8]) -> Result<Value> {
    if bytes.len() > IMAGE_LIMIT {
        bail!("单张参考图片不能超过 10 MB");
    }
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        bail!("参考图片须为 PNG、JPEG 或 WebP");
    };
    Ok(json!({"image_url":format!("data:{mime};base64,{}",STANDARD.encode(bytes))}))
}
pub async fn adapt(
    mime: &str,
    bytes: Bytes,
    edit: bool,
    extensions: axum::http::Extensions,
) -> Result<Value> {
    let mut obj: Map<String, Value> =
        if mime.to_ascii_lowercase().starts_with("multipart/form-data") {
            if !edit {
                bail!("文生图请使用 JSON 请求");
            }
            let mut req = axum::http::Request::builder()
                .header("content-type", mime)
                .body(Body::from(bytes))
                .unwrap();
            *req.extensions_mut() = extensions;
            let mut multipart = Multipart::from_request(req, &())
                .await
                .map_err(|_| anyhow::anyhow!("无法读取图片表单"))?;
            let mut obj = Map::new();
            let mut images = Vec::new();
            let mut total = 0;
            while let Some(field) = multipart
                .next_field()
                .await
                .map_err(|_| anyhow::anyhow!("图片表单不完整"))?
            {
                let name = field.name().unwrap_or("").to_owned();
                let data = field
                    .bytes()
                    .await
                    .map_err(|_| anyhow::anyhow!("无法读取图片字段"))?;
                if matches!(name.as_str(), "image" | "image[]" | "images" | "images[]") {
                    total += data.len();
                    if images.len() >= 5 || total > 32 * 1024 * 1024 {
                        bail!("最多支持 5 张参考图片，总大小不超过 32 MB");
                    }
                    images.push(inline(&data)?);
                } else {
                    if obj.contains_key(&name) {
                        bail!("图片参数不能重复");
                    }
                    let text = std::str::from_utf8(&data).context("图片参数须为 UTF-8 文本")?;
                    if text.len() > 64 * 1024 {
                        bail!("图片参数过长");
                    }
                    let value = match name.as_str() {
                        "n" => json!(text.parse::<u64>().context("图片数量无效")?),
                        "stream" => json!(text.parse::<bool>().context("stream 参数无效")?),
                        _ => json!(text),
                    };
                    obj.insert(name, value);
                }
            }
            obj.insert("images".into(), json!(images));
            obj
        } else if mime
            .split(';')
            .next()
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
        {
            serde_json::from_slice::<Value>(&bytes)
                .context("图片请求不是有效 JSON")?
                .as_object()
                .cloned()
                .context("图片请求须为对象")?
        } else {
            bail!("请使用 JSON 或图片编辑 multipart 表单");
        };
    for key in obj.keys() {
        if ![
            "model",
            "prompt",
            "n",
            "quality",
            "size",
            "background",
            "response_format",
            "output_format",
            "stream",
            "images",
            "image",
            "input_fidelity",
            "user",
            "moderation",
        ]
        .contains(&key.as_str())
        {
            bail!("官方图片入口不支持参数：{key}");
        }
    }
    if obj.get("stream").is_some_and(|v| v != &Value::Bool(false)) {
        bail!("官方图片入口暂不支持流式输出");
    }
    if obj.get("response_format").is_some_and(|v| v != "b64_json") {
        bail!("图片响应仅支持 b64_json");
    }
    if obj.get("output_format").is_some_and(|v| v != "png") {
        bail!("官方图片入口当前仅支持 PNG 输出");
    }
    if obj.contains_key("moderation") || obj.contains_key("user") {
        bail!("官方套餐生图不支持 moderation 或 user 参数");
    }
    // GPT Image 2 always uses high fidelity; the public API's field is not on
    // the Codex wire. Only the equivalent value can be safely removed.
    if obj.get("input_fidelity").is_some_and(|v| v != "high") {
        bail!("官方图片入口仅支持 high 输入保真度");
    }
    let prompt = obj
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && s.len() <= 64000)
        .context("请填写不超过 64000 字节的图片提示词")?
        .to_owned();
    let model = match obj.get("model") {
        None => super::MODEL,
        Some(Value::String(value)) => value.as_str(),
        _ => bail!("图片模型须为字符串"),
    };
    if model != super::MODEL {
        bail!("官方图片入口当前仅支持 gpt-image-2");
    }
    let n = obj
        .get("n")
        .map(|v| {
            v.as_u64()
                .filter(|v| (1..=10).contains(v))
                .context("图片数量须为 1–10")
        })
        .transpose()?
        .unwrap_or(1);
    let mut out = json!({"model":model,"prompt":prompt,"n":n});
    for (key, allowed) in [
        ("quality", vec!["auto", "low", "medium", "high"]),
        ("background", vec!["auto", "opaque", "transparent"]),
    ] {
        if let Some(v) = obj.get(key) {
            if !v.as_str().is_some_and(|s| allowed.contains(&s)) {
                bail!("无效的图片参数：{key}");
            }
            out[key] = v.clone();
        }
    }
    if let Some(size) = obj.get("size") {
        let s = size.as_str().context("图片尺寸须为 auto 或 宽x高")?;
        if s != "auto" {
            let (w, h) = s.split_once('x').context("图片尺寸须为 auto 或 宽x高")?;
            let w = w.parse::<u64>().context("图片宽度无效")?;
            let h = h.parse::<u64>().context("图片高度无效")?;
            if w == 0
                || h == 0
                || w > 3840
                || h > 3840
                || w % 16 != 0
                || h % 16 != 0
                || w.max(h) > 3 * w.min(h)
                || !(655360..=8294400).contains(&(w * h))
            {
                bail!("图片尺寸不符合 gpt-image-2 要求");
            }
        }
        out["size"] = size.clone();
    }
    if edit {
        if obj.contains_key("images") && obj.contains_key("image") {
            bail!("image 与 images 不能同时提供");
        }
        let images = obj
            .remove("images")
            .or_else(|| obj.remove("image"))
            .context("编辑图片需要参考图片")?;
        let images = match images {
            Value::Array(v) => v,
            Value::String(v) => vec![json!({"image_url":v})],
            _ => bail!("参考图片格式无效"),
        };
        if images.is_empty() || images.len() > 5 {
            bail!("编辑需要 1–5 张参考图片");
        }
        let mut clean = Vec::new();
        let mut total = 0;
        for image in images {
            if let Some(fields) = image.as_object() {
                if fields.len() != 1 || !fields.contains_key("image_url") {
                    bail!("参考图片对象仅支持 image_url，不支持 mask 或 file_id");
                }
            }
            let url = image
                .as_str()
                .or_else(|| image["image_url"].as_str())
                .context("仅支持内嵌参考图片，不支持 file_id")?;
            let (prefix, data) = url
                .split_once(',')
                .context("参考图片须为 base64 data URL")?;
            if ![
                "data:image/png;base64",
                "data:image/jpeg;base64",
                "data:image/webp;base64",
            ]
            .contains(&prefix)
            {
                bail!("参考图片须为内嵌 PNG、JPEG 或 WebP，不读取远程地址");
            }
            if data.len() > IMAGE_LIMIT.div_ceil(3) * 4 {
                bail!("参考图片过大");
            }
            let decoded = STANDARD.decode(data).context("参考图片 base64 无效")?;
            total += decoded.len();
            if total > 32 * 1024 * 1024 {
                bail!("参考图片总大小不能超过 32 MB");
            }
            clean.push(inline(&decoded)?);
        }
        out["images"] = json!(clean);
    } else if obj.contains_key("images") || obj.contains_key("image") {
        bail!("包含参考图片时请使用 images/edits");
    }
    Ok(out)
}
