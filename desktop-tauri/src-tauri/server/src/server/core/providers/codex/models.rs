//! Codex 当前可用模型的内置目录。

use serde_json::{json, Value};

pub const DEFAULT_MAIN_MODEL: &str = "gpt-6.1-sol";

const MODELS: &[(&str, &str)] = &[
    (DEFAULT_MAIN_MODEL, "GPT-6.1 Sol"),
    ("gpt-6-astra", "GPT-6 Astra"),
    ("gpt-6-sol", "GPT-6 Sol"),
    ("gpt-6-luna", "GPT-6 Luna"),
    ("gpt-5.6-sol", "GPT-5.6 Sol"),
    ("gpt-5.6-terra", "GPT-5.6 Terra"),
    ("gpt-5.6-luna", "GPT-5.6 Luna"),
    ("codex-auto-review", "Codex Auto Review"),
];

const IMAGE_MODELS: &[(&str, &str)] = &[
    ("gpt-image-1.5", "GPT Image 1.5"),
    ("gpt-image-2", "GPT Image 2"),
    ("gpt-image-2.5", "GPT Image 2.5"),
    ("gpt-image-2.5-sunburst", "GPT Image 2.5 Sunburst"),
    ("gpt-image-2.5-flare", "GPT Image 2.5 Flare"),
];

pub fn list() -> Vec<Value> {
    MODELS
        .iter()
        .map(|(id, name)| model_entry(id, name, false))
        .chain(
            IMAGE_MODELS
                .iter()
                .map(|(id, name)| model_entry(id, name, true)),
        )
        .collect()
}

pub fn is_image_model(model: &str) -> bool {
    IMAGE_MODELS
        .iter()
        .any(|(id, _)| id.eq_ignore_ascii_case(model.trim()))
}

fn model_entry(id: &str, name: &str, image: bool) -> Value {
    if image {
        json!({
            "id": id,
            "name": name,
            "type": "image",
            "supportsToolCall": false,
            "supportsReasoning": false,
            "supportsImages": false,
            "supportsImageGeneration": true,
        })
    } else {
        json!({
            "id": id,
            "name": name,
            "supportsToolCall": true,
            "supportsReasoning": true,
            "supportsImages": true,
        })
    }
}
