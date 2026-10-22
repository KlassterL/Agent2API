//! OpenAI Images API 入口，复用账号选路与请求记账，把图片请求交给 Codex OAuth。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{Extension, Multipart, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use base64::Engine as _;
use futures::StreamExt;
use serde_json::{json, Map, Value};

use crate::server::core::key_scope::KeyScope;
use crate::server::core::upstream::cancellation;
use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::core::upstream::{ForwardOutcome, ForwardRequest};
use crate::server::errors::GatewayError;
use crate::server::logging;
use crate::server::ServerState;

use super::disconnect_guard::DisconnectGuard;
use super::pipeline::{self, json_response, record_early_failure, record_entry, RecordContext};

const IMAGE_REQUEST_FIELD: &str = "__codex_image_request";
const MAX_IMAGE_RESPONSE_BYTES: usize = 128 * 1024 * 1024;
const MAX_IMAGE_EDIT_BYTES: usize = 50 * 1024 * 1024;
const GENERATION_TERMINALS: &[&[u8]] = &[b"event: image_generation.completed\n"];
const EDIT_TERMINALS: &[&[u8]] = &[b"event: image_edit.completed\n"];

/// POST /v1/images/generations
pub async fn images_generations(
    State(state): State<ServerState>,
    headers: HeaderMap,
    key_scope: Option<Extension<KeyScope>>,
    body: Bytes,
) -> Response {
    let started_at = logging::now_ms();
    let client = crate::server::core::client::classify(&pipeline::user_agent_of(&headers));
    let Some(payload) = serde_json::from_slice::<Value>(&body)
        .ok()
        .filter(Value::is_object)
    else {
        let error = GatewayError::bad_request("图片请求体必须是 JSON 对象");
        record_early_failure(&state, started_at, "", "", &client, &error);
        return error.payload_response();
    };
    let image_request = match image_request_from_json(&payload, "generate", &[], None) {
        Ok(image_request) => image_request,
        Err(error) => {
            let model = pipeline::model_field_text(&payload);
            record_early_failure(&state, started_at, &model, "", &client, &error);
            return error.payload_response();
        }
    };
    forward_image_request(
        state,
        headers,
        key_scope.map(|Extension(scope)| scope),
        image_request,
        body.to_vec(),
        started_at,
        client,
        "/v1/images/generations",
    )
    .await
}

/// POST /v1/images/edits (multipart/form-data)
pub async fn images_edits(
    State(state): State<ServerState>,
    headers: HeaderMap,
    key_scope: Option<Extension<KeyScope>>,
    mut multipart: Multipart,
) -> Response {
    let started_at = logging::now_ms();
    let client = crate::server::core::client::classify(&pipeline::user_agent_of(&headers));
    let mut fields = Map::new();
    let mut images = Vec::new();
    let mut mask = None;
    let mut total_file_bytes = 0usize;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                let gateway = GatewayError::bad_request(format!("无法解析图片编辑表单：{error}"));
                record_early_failure(&state, started_at, "", "", &client, &gateway);
                return gateway.payload_response();
            }
        };
        let name = field.name().unwrap_or_default().to_string();
        let file_name = field.file_name().map(str::to_string);
        let mime_type = field.content_type().map(|value| value.to_string());
        if file_name.is_some() || name == "image" || name.starts_with("image[") || name == "mask" {
            let bytes = match field.bytes().await {
                Ok(bytes) => bytes,
                Err(error) => {
                    let gateway = GatewayError::bad_request(format!("无法读取上传图片：{error}"));
                    record_early_failure(&state, started_at, "", "", &client, &gateway);
                    return gateway.payload_response();
                }
            };
            total_file_bytes = total_file_bytes.saturating_add(bytes.len());
            if total_file_bytes > MAX_IMAGE_EDIT_BYTES {
                let gateway = GatewayError::with_status(413, "图片编辑上传总大小不能超过 50 MiB");
                record_early_failure(&state, started_at, "", "", &client, &gateway);
                return gateway.payload_response();
            }
            if bytes.is_empty() {
                let gateway = GatewayError::bad_request("图片编辑文件不能为空");
                record_early_failure(&state, started_at, "", "", &client, &gateway);
                return gateway.payload_response();
            }
            let content_type = image_content_type(mime_type.as_deref(), file_name.as_deref());
            let data_url = format!(
                "data:{content_type};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            );
            if name == "mask" {
                mask = Some(data_url);
            } else if name == "image" || name.starts_with("image[") {
                images.push(data_url);
            }
            continue;
        }

        let value = match field.text().await {
            Ok(value) => value,
            Err(error) => {
                let gateway = GatewayError::bad_request(format!("无法读取图片编辑字段：{error}"));
                record_early_failure(&state, started_at, "", "", &client, &gateway);
                return gateway.payload_response();
            }
        };
        if !name.trim().is_empty() {
            fields.insert(name, Value::String(value));
        }
    }

    if images.is_empty() {
        let error = GatewayError::bad_request("图片编辑请求至少需要一个 image 文件");
        let model = pipeline::model_field_text(&Value::Object(fields.clone()));
        record_early_failure(&state, started_at, &model, "", &client, &error);
        return error.payload_response();
    }
    let payload = Value::Object(fields);
    let image_request = match image_request_from_json(&payload, "edit", &images, mask.as_deref()) {
        Ok(image_request) => image_request,
        Err(error) => {
            let model = pipeline::model_field_text(&payload);
            record_early_failure(&state, started_at, &model, "", &client, &error);
            return error.payload_response();
        }
    };
    let raw = serde_json::to_vec(&payload).unwrap_or_default();
    forward_image_request(
        state,
        headers,
        key_scope.map(|Extension(scope)| scope),
        image_request,
        raw,
        started_at,
        client,
        "/v1/images/edits",
    )
    .await
}

fn image_request_from_json(
    payload: &Value,
    action: &str,
    images: &[String],
    mask: Option<&str>,
) -> Result<Value, GatewayError> {
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| GatewayError::bad_request("图片请求必须指定 GPT Image model"))?;
    if !crate::server::core::providers::codex::models::is_image_model(model) {
        return Err(GatewayError::bad_request(format!(
            "不支持的 GPT Image 模型：{model}。可用列表见 GET /v1/models"
        )));
    }
    let prompt = payload
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| GatewayError::bad_request("图片请求必须指定 prompt"))?;

    let mut image_request = json!({
        "model": model,
        "prompt": prompt,
        "action": action,
        "images": images,
    });
    for field in [
        "size",
        "quality",
        "background",
        "output_format",
        "input_fidelity",
        "moderation",
        "response_format",
    ] {
        if let Some(value) = payload.get(field).filter(|value| !value.is_null()) {
            image_request[field] = value.clone();
        }
    }
    for field in ["output_compression", "partial_images"] {
        if let Some(value) = payload.get(field).filter(|value| !value.is_null()) {
            let number = value.as_i64().or_else(|| {
                value
                    .as_str()
                    .and_then(|text| text.trim().parse::<i64>().ok())
            });
            if let Some(number) = number {
                image_request[field] = Value::from(number);
            }
        }
    }
    if let Some(value) = payload.get("stream").filter(|value| !value.is_null()) {
        let stream = value.as_bool().or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<bool>().ok())
        });
        if let Some(stream) = stream {
            image_request["stream"] = Value::Bool(stream);
        }
    }
    if let Some(mask) = mask.filter(|value| !value.trim().is_empty()) {
        image_request["mask"] = Value::String(mask.to_string());
    } else if let Some(mask) = payload
        .get("mask")
        .and_then(|value| value.get("image_url").or(Some(value)))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        image_request["mask"] = Value::String(mask.to_string());
    }
    if action == "edit" && images.is_empty() {
        if let Some(values) = payload.get("images").and_then(Value::as_array) {
            let urls: Vec<String> = values
                .iter()
                .filter_map(|value| {
                    value
                        .as_str()
                        .or_else(|| value.pointer("/image_url").and_then(Value::as_str))
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                })
                .collect();
            image_request["images"] = json!(urls);
        }
    }
    Ok(image_request)
}

async fn forward_image_request(
    state: ServerState,
    headers: HeaderMap,
    scope: Option<KeyScope>,
    mut image_request: Value,
    raw_request_bytes: Vec<u8>,
    started_at: i64,
    client: String,
    path: &'static str,
) -> Response {
    let client_model = pipeline::model_field_text(&json!({
        "model": image_request.get("model").cloned().unwrap_or(Value::Null)
    }));
    let client_reasoning = String::new();
    let mut forward_body = json!({
        "model": client_model,
        "messages": [{
            "role": "user",
            "content": image_request.get("prompt").and_then(Value::as_str).unwrap_or_default(),
        }],
        "stream": true,
    });
    forward_body[IMAGE_REQUEST_FIELD] = image_request.clone();
    let requested_model = match pipeline::resolve_model(&state, &mut forward_body, scope.as_ref()) {
        Ok(model) => model,
        Err(error) => {
            record_early_failure(
                &state,
                started_at,
                &client_model,
                &client_reasoning,
                &client,
                &error,
            );
            return error.payload_response();
        }
    };
    image_request["model"] = Value::String(requested_model.clone());
    forward_body[IMAGE_REQUEST_FIELD] = image_request.clone();

    pipeline::write_debug_files(
        &raw_request_bytes,
        "POST",
        path,
        &pipeline::user_agent_of(&headers),
    );
    let telemetry = Arc::new(RequestTelemetry::with_id());
    let telemetry_id = telemetry.snapshot().id;
    state.request_stats().record_started(
        &telemetry_id,
        started_at,
        &requested_model,
        &client_model,
        &client_reasoning,
        &client,
        false,
    );
    if let Some(token) = cancellation::register(&telemetry_id) {
        telemetry.set_cancel_token(token);
    }
    let mut guard = DisconnectGuard::new(state.request_stats(), telemetry_id.clone());
    telemetry.set_live_sink(pipeline::live_row_sink(
        state.request_stats(),
        telemetry_id,
        started_at,
    ));
    let raw_request = pipeline::capture_raw_body(&raw_request_bytes);
    let outcome = state
        .upstream()
        .forward(ForwardRequest {
            body: forward_body,
            stream: true,
            dedupe_key: pipeline::sha256_hex(&raw_request_bytes),
            client_headers: headers,
            telemetry: telemetry.clone(),
            allowed_providers: scope,
            pinned_account: None,
            ignore_model_gate: false,
            proxy_override: None,
        })
        .await;

    let stream_requested = image_request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let outcome = match outcome {
        Ok(ForwardOutcome::Stream { status, stream })
            if stream_requested && (200..300).contains(&status) =>
        {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            let context = RecordContext {
                stats: state.request_stats(),
                telemetry,
                started_at,
                model: requested_model,
                client_model,
                client_reasoning,
                status: i64::from(status.as_u16()),
                raw_request,
                raw_response: None,
                is_test: false,
                client,
            };
            let stream_prefix =
                if image_request.get("action").and_then(Value::as_str) == Some("edit") {
                    "image_edit"
                } else {
                    "image_generation"
                };
            let terminals = if stream_prefix == "image_edit" {
                EDIT_TERMINALS
            } else {
                GENERATION_TERMINALS
            };
            let mut translator = ImageStreamTranslator::new(image_request);
            let converted =
                pipeline::transformed_stream(stream, context, terminals, move |chunk| {
                    translator.push(chunk)
                });
            guard.handoff();
            return pipeline::sse_response(status, converted);
        }
        other => other,
    };

    let result = match outcome {
        Ok(ForwardOutcome::Stream { status, stream }) if (200..300).contains(&status) => {
            collect_image_response(stream, &image_request).await
        }
        Ok(ForwardOutcome::Stream { status, .. }) => Err(GatewayError::with_status(
            i32::from(status),
            format!("Codex 图片上游返回 HTTP {status}"),
        )),
        Ok(ForwardOutcome::Completion { .. }) => Err(GatewayError::with_status(
            500,
            "内部错误：Codex 图片链路没有返回 Responses 流",
        )),
        Err(error) => Err(error),
    };

    match result {
        Ok(response_body) => {
            let context = RecordContext {
                stats: state.request_stats(),
                telemetry,
                started_at,
                model: requested_model,
                client_model,
                client_reasoning,
                status: 200,
                raw_request,
                raw_response: pipeline::capture_raw_json(&response_body),
                is_test: false,
                client,
            };
            record_entry(&context, None);
            guard.complete();
            json_response(response_body)
        }
        Err(error) => {
            let status = i64::from(error.http_status().as_u16());
            let message = error.message.clone();
            let context = RecordContext {
                stats: state.request_stats(),
                telemetry,
                started_at,
                model: requested_model,
                client_model,
                client_reasoning,
                status,
                raw_request,
                raw_response: None,
                is_test: false,
                client,
            };
            record_entry(&context, Some(message));
            guard.complete();
            error.payload_response()
        }
    }
}

async fn collect_image_response(
    mut stream: Box<dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin>,
    image_request: &Value,
) -> Result<Value, GatewayError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            GatewayError::with_status(502, format!("读取 Codex 图片响应失败：{error}"))
        })?;
        if bytes.len().saturating_add(chunk.len()) > MAX_IMAGE_RESPONSE_BYTES {
            return Err(GatewayError::with_status(502, "Codex 图片响应超过 128 MiB"));
        }
        bytes.extend_from_slice(&chunk);
    }

    let text = String::from_utf8_lossy(&bytes);
    let mut completed: Option<Value> = None;
    let mut items_by_index = BTreeMap::<i64, Value>::new();
    let mut fallback_items = Vec::new();
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "response.output_item.done" => {
                if let Some(item) = event.get("item").filter(|item| is_image_call(item)) {
                    if let Some(index) = event.get("output_index").and_then(Value::as_i64) {
                        items_by_index.insert(index, item.clone());
                    } else {
                        fallback_items.push(item.clone());
                    }
                }
            }
            "response.completed" => completed = Some(event),
            "response.failed" | "error" => {
                let message = event
                    .pointer("/response/error/message")
                    .or_else(|| event.pointer("/error/message"))
                    .or_else(|| event.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Codex 图片生成失败");
                return Err(GatewayError::with_status(502, message.to_string()));
            }
            _ => {}
        }
    }
    let completed =
        completed.ok_or_else(|| GatewayError::with_status(502, "Codex 图片响应在完成前断开"))?;
    let response = completed.get("response").unwrap_or(&completed);
    if let Some(error) = response.get("error").filter(|value| !value.is_null()) {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Codex 图片生成失败");
        return Err(GatewayError::with_status(502, message.to_string()));
    }

    let output_items = response
        .get("output")
        .and_then(Value::as_array)
        .filter(|items| items.iter().any(is_image_call))
        .cloned()
        .unwrap_or_else(|| {
            items_by_index
                .into_values()
                .chain(fallback_items)
                .collect::<Vec<_>>()
        });
    let image_calls: Vec<&Value> = output_items
        .iter()
        .filter(|item| is_image_call(item))
        .collect();
    if image_calls.is_empty() {
        return Err(GatewayError::with_status(502, "Codex 没有返回图片结果"));
    }

    let first = image_calls[0];
    let fallback_format = image_request
        .get("output_format")
        .and_then(Value::as_str)
        .unwrap_or("png");
    let output_format = first
        .get("output_format")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback_format);
    let response_format = image_request
        .get("response_format")
        .and_then(Value::as_str)
        .unwrap_or("b64_json");
    let mut data = Vec::with_capacity(image_calls.len());
    for call in image_calls {
        let result = call
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if result.is_empty() {
            continue;
        }
        let mut item = Map::new();
        if let Some(prompt) = call.get("revised_prompt").and_then(Value::as_str) {
            item.insert(
                "revised_prompt".to_string(),
                Value::String(prompt.to_string()),
            );
        }
        let field = if response_format.eq_ignore_ascii_case("url") {
            "url"
        } else {
            "b64_json"
        };
        let value = if field == "url" {
            format!(
                "data:{};base64,{result}",
                mime_type_from_format(
                    call.get("output_format")
                        .and_then(Value::as_str)
                        .unwrap_or(output_format),
                )
            )
        } else {
            result.to_string()
        };
        item.insert(field.to_string(), Value::String(value));
        data.push(Value::Object(item));
    }
    if data.is_empty() {
        return Err(GatewayError::with_status(502, "Codex 图片结果为空"));
    }

    let created = response
        .get("created_at")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .unwrap_or_else(unix_timestamp);
    let mut result = json!({ "created": created, "data": data });
    for field in ["background", "quality", "size"] {
        let value = first
            .get(field)
            .filter(|value| !value.is_null())
            .or_else(|| image_request.get(field).filter(|value| !value.is_null()));
        if let Some(value) = value {
            result[field] = value.clone();
        }
    }
    result["output_format"] = Value::String(output_format.to_string());
    if let Some(usage) = response
        .pointer("/tool_usage/image_gen")
        .filter(|value| value.is_object())
    {
        result["usage"] = usage.clone();
    }
    Ok(result)
}

/// Codex Responses SSE 到 OpenAI Images 流式事件的轻量转换器。
///
/// 上游图像结果只在 `response.completed` 中保证完整；在此之前将部分图像事件
/// 立即转换并下发，完成事件则沿用最终 `image_generation_call` 结果。
struct ImageStreamTranslator {
    request: Value,
    pending_line: Vec<u8>,
    event_data: Vec<u8>,
    output_items: BTreeMap<i64, Value>,
    fallback_items: Vec<Value>,
    response_bytes: usize,
    failed: bool,
}

impl ImageStreamTranslator {
    fn new(request: Value) -> Self {
        Self {
            request,
            pending_line: Vec::new(),
            event_data: Vec::new(),
            output_items: BTreeMap::new(),
            fallback_items: Vec::new(),
            response_bytes: 0,
            failed: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Vec<Bytes> {
        if self.failed {
            return Vec::new();
        }
        self.response_bytes = self.response_bytes.saturating_add(chunk.len());
        if self.response_bytes > MAX_IMAGE_RESPONSE_BYTES {
            self.failed = true;
            let body = json!({
                "error": { "message": "Codex 图片响应超过 128 MiB" }
            });
            return vec![Bytes::from(format!(
                "event: error\ndata: {}\n\n",
                serde_json::to_string(&body).unwrap_or_default()
            ))];
        }

        let mut frames = Vec::new();
        for byte in chunk {
            if *byte == b'\n' {
                let line = std::mem::take(&mut self.pending_line);
                if line.is_empty() || line == b"\r" {
                    frames.extend(self.finish_event());
                } else if let Some(data) = line.strip_prefix(b"data:") {
                    if !self.event_data.is_empty() {
                        self.event_data.push(b'\n');
                    }
                    self.event_data
                        .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
                }
            } else {
                self.pending_line.push(*byte);
            }
        }
        frames
    }

    fn finish_event(&mut self) -> Vec<Bytes> {
        if self.event_data.is_empty() {
            return Vec::new();
        }
        let data = std::mem::take(&mut self.event_data);
        let Ok(event) = serde_json::from_slice::<Value>(&data) else {
            return Vec::new();
        };
        match event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "response.output_item.done" => {
                if let Some(item) = event.get("item").filter(|item| is_image_call(item)) {
                    if let Some(index) = event.get("output_index").and_then(Value::as_i64) {
                        self.output_items.insert(index, item.clone());
                    } else {
                        self.fallback_items.push(item.clone());
                    }
                }
                Vec::new()
            }
            "response.image_generation_call.partial_image" => {
                self.partial_image_event(&event).into_iter().collect()
            }
            "response.completed" => self.completed_events(&event),
            "response.failed" | "error" => {
                self.failed = true;
                let message = event
                    .pointer("/response/error/message")
                    .or_else(|| event.pointer("/error/message"))
                    .or_else(|| event.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Codex 图片生成失败");
                let body = json!({ "error": { "message": message } });
                vec![Bytes::from(format!(
                    "event: error\ndata: {}\n\n",
                    serde_json::to_string(&body).unwrap_or_default()
                ))]
            }
            _ => Vec::new(),
        }
    }

    fn partial_image_event(&self, event: &Value) -> Option<Bytes> {
        let image = event
            .get("partial_image_b64")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())?;
        let event_name = format!("{}.partial_image", self.stream_prefix());
        let mut payload = json!({
            "type": event_name,
            "partial_image_index": event.get("partial_image_index").cloned().unwrap_or(Value::from(0)),
        });
        for field in [
            "created_at",
            "size",
            "quality",
            "background",
            "output_format",
        ] {
            if let Some(value) = event.get(field).filter(|value| !value.is_null()) {
                payload[field] = value.clone();
            }
        }
        self.insert_image_data(
            &mut payload,
            image,
            event.get("output_format").and_then(Value::as_str),
        );
        Some(self.sse_frame(&event_name, payload))
    }

    fn completed_events(&mut self, event: &Value) -> Vec<Bytes> {
        let response = event.get("response").unwrap_or(event);
        if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
            self.failed = true;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Codex 图片生成失败");
            let body = json!({ "error": { "message": message } });
            return vec![Bytes::from(format!(
                "event: error\ndata: {}\n\n",
                serde_json::to_string(&body).unwrap_or_default()
            ))];
        }

        let response_items = response
            .get("output")
            .and_then(Value::as_array)
            .filter(|items| items.iter().any(is_image_call));
        let fallback;
        let items = if let Some(items) = response_items {
            items
                .iter()
                .filter(|item| is_image_call(item))
                .collect::<Vec<_>>()
        } else {
            fallback = self
                .output_items
                .values()
                .chain(self.fallback_items.iter())
                .collect::<Vec<_>>();
            fallback
        };
        let event_name = format!("{}.completed", self.stream_prefix());
        let created = response
            .get("created_at")
            .and_then(Value::as_i64)
            .filter(|value| *value > 0)
            .unwrap_or_else(unix_timestamp);
        let usage = response.pointer("/tool_usage/image_gen");
        let frames: Vec<Bytes> = items
            .into_iter()
            .filter_map(|call| {
                let result = call
                    .get("result")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())?;
                let mut payload = json!({ "type": event_name, "created_at": created });
                for field in ["size", "quality", "background", "output_format"] {
                    if let Some(value) = call.get(field).filter(|value| !value.is_null()) {
                        payload[field] = value.clone();
                    }
                }
                if let Some(usage) = usage.filter(|value| value.is_object()) {
                    payload["usage"] = usage.clone();
                }
                self.insert_image_data(
                    &mut payload,
                    result,
                    call.get("output_format").and_then(Value::as_str),
                );
                Some(self.sse_frame(&event_name, payload))
            })
            .collect();
        if frames.is_empty() {
            self.failed = true;
            let body = json!({ "error": { "message": "Codex 没有返回图片结果" } });
            return vec![Bytes::from(format!(
                "event: error\ndata: {}\n\n",
                serde_json::to_string(&body).unwrap_or_default()
            ))];
        }
        frames
    }

    fn stream_prefix(&self) -> &'static str {
        if self.request.get("action").and_then(Value::as_str) == Some("edit") {
            "image_edit"
        } else {
            "image_generation"
        }
    }

    fn insert_image_data(&self, payload: &mut Value, result: &str, output_format: Option<&str>) {
        if self
            .request
            .get("response_format")
            .and_then(Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case("url"))
        {
            payload["url"] = Value::String(format!(
                "data:{};base64,{result}",
                mime_type_from_format(output_format.unwrap_or("png"))
            ));
        } else {
            payload["b64_json"] = Value::String(result.to_string());
        }
    }

    fn sse_frame(&self, event_name: &str, payload: Value) -> Bytes {
        let body = serde_json::to_string(&payload).unwrap_or_default();
        Bytes::from(format!("event: {event_name}\ndata: {body}\n\n"))
    }
}

fn is_image_call(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("image_generation_call")
}

fn mime_type_from_format(format: &str) -> &'static str {
    match format.trim().to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn image_content_type(content_type: Option<&str>, file_name: Option<&str>) -> String {
    if let Some(content_type) = content_type
        .map(str::trim)
        .filter(|value| value.starts_with("image/"))
    {
        return content_type.to_string();
    }
    match file_name
        .and_then(|name| name.rsplit('.').next())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "webp" => "image/webp".to_string(),
        "gif" => "image/gif".to_string(),
        _ => "image/png".to_string(),
    }
}
