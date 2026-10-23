//! Codex 的 ProviderAdapter：Bearer OAuth + Codex Responses API。

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::protocol;
use crate::server::core::providers::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass,
};
use crate::server::core::providers::{content_block, ProviderKind};
use crate::server::errors::GatewayError;

use super::{credentials, endpoints, models, oauth};

pub struct CodexAdapter;

pub static CODEX_ADAPTER: CodexAdapter = CodexAdapter;

impl ProviderAdapter for CodexAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let access_token = account
            .pointer("/auth/accessToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if access_token.is_empty() {
            return Err(GatewayError::with_status(
                401,
                "Codex 账号缺少 access token，请重新登录",
            ));
        }
        let account_id = account
            .pointer("/account/uid")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if account_id.is_empty() {
            return Err(GatewayError::with_status(
                401,
                "Codex 账号缺少 ChatGPT account id，请重新登录",
            ));
        }
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if model.is_empty() {
            return Err(GatewayError::with_status(400, "Codex 请求缺少 model 字段"));
        }
        let image_request = body.get("__codex_image_request");
        let is_image_request = image_request.is_some();
        let image_stream = image_request
            .and_then(|request| request.get("stream"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let (url, mut request) = if let Some(image_request) = image_request {
            if !models::is_image_model(model) {
                return Err(GatewayError::bad_request(
                    "Codex 图片请求必须使用 GPT Image 模型",
                ));
            }
            let (url, request) = super::images::direct_request(image_request).map_err(|message| {
                GatewayError::with_status(400, format!("Codex 图片请求转换失败：{message}"))
            })?;
            (url, request)
        } else {
            if models::is_image_model(model) {
                return Err(GatewayError::bad_request(
                    "GPT Image 模型请通过 /v1/images/generations 或 /v1/images/edits 使用",
                ));
            }
            let request = protocol::responses_outbound::responses_request_from_chat(body, model)
                .map_err(|message| {
                    GatewayError::with_status(400, format!("Codex 请求转换失败：{message}"))
                })?;
            (endpoints::RESPONSES_URL.to_string(), request)
        };
        if !is_image_request {
            if let Some(object) = request.as_object_mut() {
                object.insert("stream".to_string(), Value::Bool(true));
                object.insert("store".to_string(), Value::Bool(false));
            }
        }
        let mut headers = vec![
            (
                "Authorization".to_string(),
                format!("Bearer {access_token}"),
            ),
            ("ChatGPT-Account-Id".to_string(), account_id.to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
            (
                "Accept".to_string(),
                if is_image_request {
                    if image_stream {
                        "text/event-stream"
                    } else {
                        "application/json"
                    }
                } else {
                    "text/event-stream"
                }
                .to_string(),
            ),
            ("Originator".to_string(), endpoints::ORIGINATOR.to_string()),
            (
                "Session-Id".to_string(),
                crate::server::core::upstream::request::new_request_id(),
            ),
            ("User-Agent".to_string(), endpoints::USER_AGENT.to_string()),
        ];
        if is_image_request {
            headers.push((
                "x-codex-image-turn-id".to_string(),
                crate::server::core::upstream::request::new_request_id(),
            ));
            Ok(ChatRequestPlan::codex_images(url, headers, request))
        } else {
            Ok(ChatRequestPlan::codex_responses(
                url,
                headers,
                request,
            ))
        }
    }

    fn supports_chat(&self) -> bool {
        true
    }

    fn supports_refresh(&self) -> bool {
        true
    }

    fn system_proxy_when_unset(&self) -> bool {
        true
    }

    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            oauth::ensure_fresh(store, account_id, false)
                .await
                .map(|credentials| credentials.access_token)
        })
    }

    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            oauth::ensure_fresh(store, account_id, true)
                .await
                .map(|credentials| credentials.access_token)
        })
    }

    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        store
            .codex_account_record(account_id)
            .map(|record| credentials::from_record(&record).expiring())
            .unwrap_or(false)
    }

    fn supports_web_login(&self) -> bool {
        true
    }

    fn build_login_url(&self) -> Option<(String, String)> {
        oauth::build_authorize_url()
    }

    fn exchange_login_code<'a>(
        &'a self,
        store: &'a AccountStore,
        code: &'a str,
        state: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move { oauth::exchange_code(store, code, state).await })
    }

    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = error_body
            .pointer("/error/message")
            .or_else(|| error_body.get("message"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("上游错误");
        let message = format!("上游返回 {status}: {raw}");
        let code = error_body
            .pointer("/error/code")
            .or_else(|| error_body.get("code"))
            .and_then(Value::as_i64);
        let lowered = raw.to_ascii_lowercase();
        if status == 401 || status == 403 {
            return UpstreamErrorClass::TokenExpired { message };
        }
        if status == 429
            || ["rate_limit", "quota", "capacity"]
                .iter()
                .any(|part| lowered.contains(part))
        {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status: 429,
            };
        }
        if status == 400 {
            return content_block::classify_or_fatal(status, error_body, message, code);
        }
        UpstreamErrorClass::Fatal {
            status,
            message,
            upstream_code: code,
        }
    }

    fn refresh_models<'a>(
        &'a self,
        _store: &'a AccountStore,
        _account_id: &'a str,
        _force: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>> {
        Box::pin(async { ModelRefreshOutcome::unchanged() })
    }
}
