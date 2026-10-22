//! OpenAI Images API 请求到 Codex Responses `image_generation` 工具的映射。

use serde_json::{json, Value};

pub const MAIN_MODEL: &str = "gpt-5.4-mini";

pub fn responses_request(image: &Value) -> Result<Value, String> {
    let action = image
        .get("action")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| matches!(*value, "generate" | "edit"))
        .ok_or_else(|| "图片请求缺少有效的 action".to_string())?;
    let image_model = image
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| crate::server::core::providers::codex::models::is_image_model(value))
        .ok_or_else(|| "图片请求缺少有效的 GPT Image 模型".to_string())?;
    let prompt = image
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "图片请求缺少 prompt".to_string())?;

    let mut tool = json!({
        "type": "image_generation",
        "model": image_model,
        "action": action,
    });
    for field in [
        "size",
        "quality",
        "background",
        "output_format",
        "input_fidelity",
        "moderation",
    ] {
        if let Some(value) = image.get(field).filter(|value| value.is_string()) {
            if !value.as_str().unwrap_or_default().trim().is_empty() {
                tool[field] = value.clone();
            }
        }
    }
    let stream_requested = image
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    for field in ["output_compression", "partial_images"] {
        if field == "partial_images" && !stream_requested {
            continue;
        }
        if let Some(value) = image.get(field).and_then(Value::as_i64) {
            tool[field] = Value::from(value);
        }
    }
    if let Some(mask) = image
        .get("mask")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        tool["input_image_mask"] = json!({ "image_url": mask });
    }

    let mut content = vec![json!({ "type": "input_text", "text": prompt })];
    if let Some(images) = image.get("images").and_then(Value::as_array) {
        for image_url in images
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            content.push(json!({ "type": "input_image", "image_url": image_url }));
        }
    }

    Ok(json!({
        "model": MAIN_MODEL,
        "input": [{ "type": "message", "role": "user", "content": content }],
        "stream": true,
        "store": false,
        "reasoning": { "effort": "medium", "summary": "auto" },
        "parallel_tool_calls": true,
        "include": ["reasoning.encrypted_content"],
        "tool_choice": { "type": "image_generation" },
        "tools": [tool],
    }))
}
