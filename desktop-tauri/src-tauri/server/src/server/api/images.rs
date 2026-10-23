//! OpenAI Images API 入口，复用账号选路与请求记账，把图片请求交给 Codex OAuth。

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Multipart, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
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
use super::pipeline::{self, record_early_failure, record_entry, RecordContext};

pub(super) const IMAGE_REQUEST_FIELD: &str = "__codex_image_request";
const MAX_IMAGE_RESPONSE_BYTES: usize = 128 * 1024 * 1024;
const MAX_IMAGE_EDIT_BYTES: usize = 50 * 1024 * 1024;
const GENERATION_TERMINALS: &[&[u8]] = &[
    b"event: image_generation.completed\n",
    b"event: image_generation.completed\r\n",
];
const EDIT_TERMINALS: &[&[u8]] = &[
    b"event: image_edit.completed\n",
    b"event: image_edit.completed\r\n",
];

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

pub(super) fn image_request_from_json(
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
    for field in ["n", "output_compression", "partial_images"] {
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
    let client_stream_requested = image_request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // GPT Image 的非流式 Images JSON 响应不保证带 usage（gpt-image-2 的 token
    // 用量在流式 completed 事件里提供）。内部改用 Images SSE，向下游仍按客户
    // 选择返回 JSON；流式客户端则继续收到上游原始 Images SSE。
    if !client_stream_requested {
        image_request["stream"] = Value::Bool(true);
        image_request
            .as_object_mut()
            .expect("image_request 在入口已保证是对象")
            .entry("partial_images")
            .or_insert(Value::from(0));
    }
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

    let outcome = match outcome {
        Ok(ForwardOutcome::Stream { status, stream })
            if client_stream_requested && (200..300).contains(&status) =>
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
            let usage_telemetry = context.telemetry.clone();
            let mut usage_capture = ImageUsageCapture::default();
            let converted = pipeline::transformed_stream(stream, context, terminals, move |chunk| {
                usage_capture.push(chunk, &usage_telemetry);
                vec![Bytes::copy_from_slice(chunk)]
            });
            guard.handoff();
            return pipeline::sse_response(status, converted);
        }
        other => other,
    };

    let result = match outcome {
        Ok(ForwardOutcome::Stream { status, stream }) if (200..300).contains(&status) => {
            collect_image_response(stream, &image_request, &telemetry).await
        }
        Ok(ForwardOutcome::Stream { status, stream }) => {
            match collect_image_bytes(
                stream,
                1024 * 1024,
                "Codex 图片上游错误响应超过 1 MiB",
                Some(&telemetry),
            )
            .await
            {
                Ok(body) => Err(GatewayError::with_status(
                    i32::from(status),
                    image_upstream_error_message(status, &body),
                )),
                Err(error) => Err(error),
            }
        }
        Ok(ForwardOutcome::Completion { .. }) => Err(GatewayError::with_status(
            500,
            "内部错误：Codex 图片链路没有返回图片结果流",
        )),
        Err(error) => Err(error),
    };

    match result {
        Ok((response_body, response_bytes)) => {
            if let Some(usage) = response_body.get("usage") {
                telemetry.report_usage(usage);
            }
            let raw_response = pipeline::capture_raw_body(&response_bytes);
            let context = RecordContext {
                stats: state.request_stats(),
                telemetry,
                started_at,
                model: requested_model,
                client_model,
                client_reasoning,
                status: 200,
                raw_request,
                raw_response,
                is_test: false,
                client,
            };
            record_entry(&context, None);
            guard.complete();
            image_json_response(response_bytes)
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

pub(super) async fn collect_image_response(
    stream: Box<dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin>,
    request: &Value,
    telemetry: &RequestTelemetry,
) -> Result<(Value, Bytes), GatewayError> {
    let body = collect_image_bytes(
        stream,
        MAX_IMAGE_RESPONSE_BYTES,
        "Codex 图片响应超过 128 MiB",
        Some(telemetry),
    )
    .await?;
    let (value, response_bytes) = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => (value, body),
        Err(json_error) => {
            let value = image_response_from_sse(&body, request).map_err(|error| {
                if error.message.starts_with("Codex 图片 SSE") {
                    error
                } else {
                    GatewayError::with_status(
                        502,
                        format!("Codex 图片响应 JSON 无法解析：{json_error}"),
                    )
                }
            })?;
            let response = serde_json::to_vec(&value).map_err(|error| {
                GatewayError::with_status(502, format!("Codex 图片响应无法序列化：{error}"))
            })?;
            (value, response)
        }
    };
    Ok((value, Bytes::from(response_bytes)))
}

/// 把 Images SSE 收拢为非流式 Images JSON，并保留 completed 事件中的 usage。
fn image_response_from_sse(body: &[u8], request: &Value) -> Result<Value, GatewayError> {
    let text = String::from_utf8_lossy(body);
    let mut event_name = String::new();
    let mut data = Vec::new();
    let mut completed = Vec::new();
    let mut upstream_error = None;

    let mut finish_event = |event_name: &str, data: &[u8]| {
        if data.is_empty() || data == b"[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_slice::<Value>(data) else {
            return;
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty())
            .unwrap_or(event_name);
        if kind == "error" || event_name == "error" {
            upstream_error = value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some("上游图片流返回 error 事件".to_string()));
        } else if matches!(kind, "image_generation.completed" | "image_edit.completed") {
            completed.push(value);
        }
    };

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            finish_event(&event_name, &data);
            event_name.clear();
            data.clear();
        } else if let Some(value) = line.strip_prefix("event:") {
            event_name = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push(b'\n');
            }
            data.extend_from_slice(value.trim_start().as_bytes());
        }
    }
    finish_event(&event_name, &data);
    drop(finish_event);

    if let Some(message) = upstream_error {
        return Err(GatewayError::with_status(
            502,
            format!("Codex 图片 SSE 返回错误：{message}"),
        ));
    }
    if completed.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "Codex 图片 SSE 没有 image_generation.completed 或 image_edit.completed 结果",
        ));
    }

    let mut response = serde_json::Map::new();
    let mut images = Vec::with_capacity(completed.len());
    for event in &completed {
        let Some(encoded) = event.get("b64_json").and_then(Value::as_str) else {
            continue;
        };
        let mut image = serde_json::Map::new();
        image.insert("b64_json".to_string(), Value::String(encoded.to_string()));
        if let Some(revised_prompt) = event.get("revised_prompt") {
            image.insert("revised_prompt".to_string(), revised_prompt.clone());
        }
        images.push(Value::Object(image));
    }
    if images.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "Codex 图片 SSE 完成事件没有 b64_json 图片结果",
        ));
    }

    if let Some(created_at) = completed
        .last()
        .and_then(|event| event.get("created_at").or_else(|| event.get("created")))
    {
        response.insert("created".to_string(), created_at.clone());
    }
    for field in ["background", "output_format", "quality", "size", "usage"] {
        if let Some(value) = completed.last().and_then(|event| event.get(field)) {
            response.insert(field.to_string(), value.clone());
        }
    }
    // 有些上游版本只在 data 事件里带 output_format；请求参数是兼容兜底。
    if !response.contains_key("output_format") {
        if let Some(value) = request.get("output_format") {
            response.insert("output_format".to_string(), value.clone());
        }
    }
    response.insert("data".to_string(), Value::Array(images));
    Ok(Value::Object(response))
}

/// 流式 Images 响应旁路提取 usage；所有原始字节仍原样交给下游。
#[derive(Default)]
struct ImageUsageCapture {
    line: Vec<u8>,
    event_name: String,
    data: Vec<u8>,
    data_overflow: bool,
}

#[derive(serde::Deserialize)]
struct ImageUsageFrame {
    #[serde(default)]
    usage: Option<Value>,
}

impl ImageUsageCapture {
    fn push(&mut self, chunk: &[u8], telemetry: &RequestTelemetry) {
        for byte in chunk {
            if *byte == b'\n' {
                if self.line.last() == Some(&b'\r') {
                    self.line.pop();
                }
                self.process_line(telemetry);
                self.line.clear();
            } else if self.line.len() < MAX_IMAGE_RESPONSE_BYTES {
                self.line.push(*byte);
            } else {
                self.line.clear();
                self.data_overflow = true;
            }
        }
    }

    fn process_line(&mut self, telemetry: &RequestTelemetry) {
        if self.line.is_empty() {
            if !self.data_overflow && !self.data.is_empty() {
                if let Ok(frame) = serde_json::from_slice::<ImageUsageFrame>(&self.data) {
                    if let Some(usage) = frame.usage {
                        telemetry.report_usage(&usage);
                    }
                }
            }
            self.event_name.clear();
            self.data.clear();
            self.data_overflow = false;
            return;
        }
        if let Some(event) = self.line.strip_prefix(b"event:") {
            self.event_name = String::from_utf8_lossy(event).trim().to_string();
        } else if let Some(payload) = self.line.strip_prefix(b"data:") {
            let should_capture =
                self.event_name.ends_with(".completed") || self.event_name.is_empty();
            if should_capture && !self.data_overflow {
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                if self.data.len().saturating_add(payload.len()) > MAX_IMAGE_RESPONSE_BYTES {
                    self.data.clear();
                    self.data_overflow = true;
                } else {
                    self.data.extend_from_slice(payload.strip_prefix(b" ").unwrap_or(payload));
                }
            }
        }
    }
}

fn image_json_response(body: Bytes) -> Response {
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

async fn collect_image_bytes(
    mut stream: Box<dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin>,
    max_bytes: usize,
    too_large_message: &'static str,
    telemetry: Option<&RequestTelemetry>,
) -> Result<Vec<u8>, GatewayError> {
    let mut body = Vec::new();
    let mut first_frame_seen = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            GatewayError::with_status(502, format!("读取 Codex 图片响应失败：{error}"))
        })?;
        if !first_frame_seen && !chunk.is_empty() {
            first_frame_seen = true;
            if let Some(telemetry) = telemetry {
                telemetry.note_first_frame();
            }
        }
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(GatewayError::with_status(502, too_large_message));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn image_upstream_error_message(status: u16, body: &[u8]) -> String {
    let structured_message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|message| !message.is_empty())
                .map(str::to_string)
        });
    let message = structured_message.or_else(|| {
        std::str::from_utf8(body)
            .ok()
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .map(str::to_string)
    });
    message.map_or_else(
        || format!("Codex 图片上游返回 HTTP {status}"),
        |message| format!("Codex 图片上游返回 HTTP {status}：{message}"),
    )
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

#[cfg(test)]
mod tests {
    use super::{image_response_from_sse, ImageUsageCapture};
    use crate::server::core::upstream::usage::RequestTelemetry;
    use serde_json::json;

    #[test]
    fn non_stream_image_sse_keeps_the_result_and_usage_in_images_json() {
        let completed = json!({
            "type": "image_generation.completed",
            "b64_json": "aGVsbG8=",
            "created_at": 123,
            "output_format": "png",
            "usage": {
                "input_tokens": 7,
                "input_tokens_details": { "text_tokens": 4, "image_tokens": 3 },
                "output_tokens": 11,
                "total_tokens": 18,
            },
        });
        let frame = format!(
            "event: image_generation.completed\ndata: {}\n\n",
            serde_json::to_string(&completed).unwrap()
        );
        let response = image_response_from_sse(frame.as_bytes(), &json!({})).unwrap();

        assert_eq!(response.pointer("/data/0/b64_json"), Some(&json!("aGVsbG8=")));
        assert_eq!(response.get("created"), Some(&json!(123)));
        assert_eq!(response.get("output_format"), Some(&json!("png")));
        assert_eq!(response.pointer("/usage/total_tokens"), Some(&json!(18)));
    }

    #[test]
    fn streamed_image_usage_is_captured_when_sse_chunks_split_arbitrarily() {
        let telemetry = RequestTelemetry::new();
        let usage = json!({
            "input_tokens": 7,
            "input_tokens_details": { "text_tokens": 4, "image_tokens": 3 },
            "output_tokens": 11,
            "total_tokens": 18,
        });
        let completed = json!({
            "type": "image_edit.completed",
            "b64_json": "aGVsbG8=",
            "usage": usage,
        });
        let frame = format!(
            "event: image_edit.completed\r\ndata: {}\r\n\r\n",
            serde_json::to_string(&completed).unwrap()
        );
        let mut capture = ImageUsageCapture::default();
        for chunk in frame.as_bytes().chunks(13) {
            capture.push(chunk, &telemetry);
        }

        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.prompt_tokens, 7);
        assert_eq!(snapshot.completion_tokens, 11);
        assert_eq!(snapshot.total_tokens, 18);
    }
}
