//! Codex OAuth 授权码 + PKCE、refresh token 续期与账号落库。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::server::core::account_store::{AccountStore, CredentialWrite};
use crate::server::core::egress;
use crate::server::core::proxies::{self, ProxyResolution, ResolvedProxy};
use crate::server::core::providers::refresh_flight::{self, Join, Table};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::CodexCredentials;
use super::endpoints;

const PENDING_TTL_MS: i64 = 10 * 60 * 1000;
const DEFAULT_EXPIRES_IN_SECONDS: i64 = 3600;
const MAX_ERROR_CHARS: usize = 320;

#[derive(Clone)]
struct PendingLogin {
    verifier: String,
    redirect_uri: String,
    created_at: i64,
}

static PENDING: OnceLock<Mutex<HashMap<String, PendingLogin>>> = OnceLock::new();
static FLIGHTS: OnceLock<Table<CodexCredentials>> = OnceLock::new();

fn random_secret() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).ok()?;
    Some(URL_SAFE_NO_PAD.encode(bytes))
}

fn pending() -> &'static Mutex<HashMap<String, PendingLogin>> {
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_pending() -> std::sync::MutexGuard<'static, HashMap<String, PendingLogin>> {
    pending().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn sweep_pending(table: &mut HashMap<String, PendingLogin>) {
    let now = logging::now_ms();
    table.retain(|_, item| now - item.created_at <= PENDING_TTL_MS);
}

/// 创建授权链接并在进程内保留 PKCE verifier；verifier 不出现在 URL 或日志中。
pub fn build_authorize_url() -> Option<(String, String)> {
    let verifier = random_secret()?;
    let state = random_secret()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = url::Url::parse(endpoints::AUTH_URL).ok()?;
    url.query_pairs_mut()
        .append_pair("client_id", endpoints::CLIENT_ID)
        .append_pair("redirect_uri", endpoints::REDIRECT_URI)
        .append_pair("response_type", "code")
        .append_pair("scope", endpoints::SCOPES)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("prompt", "login")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true");
    let mut table = lock_pending();
    sweep_pending(&mut table);
    table.insert(
        state.clone(),
        PendingLogin {
            verifier,
            redirect_uri: endpoints::REDIRECT_URI.to_string(),
            created_at: logging::now_ms(),
        },
    );
    Some((url.to_string(), state))
}

fn client_for(proxy: Option<&ResolvedProxy>) -> std::sync::Arc<reqwest::Client> {
    match proxy {
        Some(proxy) => egress::client_for(Some(proxy)),
        None => egress::client_for_system_proxy(),
    }
}

fn account_proxy(record: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    match proxies::resolve_account_proxy(record.get("proxy")) {
        Some(ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(ProxyResolution::Failed(reason)) => Err(GatewayError::with_status(400, reason)),
        None => Ok(None),
    }
}

fn truncate(text: &str) -> String {
    let text: String = text.chars().filter(|character| !character.is_control()).collect();
    match text.char_indices().nth(MAX_ERROR_CHARS) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text,
    }
}

fn error_detail(payload: &Value, text: &str) -> (String, String) {
    let code = payload.get("error").and_then(|value| {
        value.as_str().or_else(|| value.get("code").and_then(Value::as_str))
    }).unwrap_or("").trim().to_string();
    let detail = payload
        .pointer("/error/message")
        .or_else(|| payload.get("error_description"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| truncate(text));
    (code, detail)
}

async fn post_token_form(
    fields: &[(&str, &str)],
    proxy: Option<&ResolvedProxy>,
) -> Result<(u16, String), GatewayError> {
    let response = client_for(proxy)
        .post(endpoints::TOKEN_URL)
        .header("Accept", "application/json")
        .header("User-Agent", endpoints::USER_AGENT)
        .timeout(Duration::from_millis(endpoints::REQUEST_TIMEOUT_MS))
        .form(fields)
        .send()
        .await
        .map_err(|error| GatewayError::with_status(
            502,
            format!("Codex OAuth 请求失败：{}", egress::describe_error_detail(&error)),
        ))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    Ok((status, text))
}

fn parse_token_payload(status: u16, text: &str, context: &str) -> Result<Value, GatewayError> {
    let payload: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let (code, detail) = error_detail(&payload, text);
        let error_status = if code == "invalid_grant" { 401 } else if status == 429 { 429 } else { 502 };
        let hint = if detail.is_empty() { String::new() } else { format!("：{detail}") };
        return Err(GatewayError::with_status(
            error_status,
            format!("Codex {context}失败（HTTP {status}，{code}）{hint}"),
        ));
    }
    Ok(payload)
}

fn token_string(payload: &Value, key: &str) -> String {
    payload.get(key).and_then(Value::as_str).map(str::trim).unwrap_or("").to_string()
}

fn expires_in(payload: &Value) -> i64 {
    match payload.get("expires_in") {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(DEFAULT_EXPIRES_IN_SECONDS),
        Some(Value::String(text)) => text.trim().parse().unwrap_or(DEFAULT_EXPIRES_IN_SECONDS),
        _ => DEFAULT_EXPIRES_IN_SECONDS,
    }
}

fn jwt_payload(token: &str) -> Option<Value> {
    let encoded = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn auth_claim(payload: &Value, name: &str) -> String {
    let auth = payload.get("https://api.openai.com/auth");
    auth.and_then(|value| value.get(name))
        .and_then(Value::as_str)
        .or_else(|| payload.pointer(&format!("/https:~1~1api.openai.com~1auth/{name}")).and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string()
}

fn claims(tokens: &Value) -> (String, String, String, String) {
    let id_token = token_string(tokens, "id_token");
    let access_token = token_string(tokens, "access_token");
    let id_claims = jwt_payload(&id_token).unwrap_or(Value::Null);
    let access_claims = jwt_payload(&access_token).unwrap_or(Value::Null);
    let account_id = auth_claim(&id_claims, "chatgpt_account_id")
        .or_else_empty(|| auth_claim(&access_claims, "chatgpt_account_id"));
    let user_id = auth_claim(&id_claims, "user_id")
        .or_else_empty(|| auth_claim(&access_claims, "user_id"))
        .or_else_empty(|| id_claims.get("sub").and_then(Value::as_str).unwrap_or("").to_string());
    let email = id_claims.get("email").and_then(Value::as_str)
        .or_else(|| access_claims.get("email").and_then(Value::as_str))
        .unwrap_or("").to_string();
    let plan_type = auth_claim(&id_claims, "chatgpt_plan_type")
        .or_else_empty(|| auth_claim(&access_claims, "chatgpt_plan_type"));
    (account_id, user_id, email, plan_type)
}

trait OrElseEmpty {
    fn or_else_empty(self, next: impl FnOnce() -> String) -> String;
}

impl OrElseEmpty for String {
    fn or_else_empty(self, next: impl FnOnce() -> String) -> String {
        if self.is_empty() { next() } else { self }
    }
}

/// 用本次登录产生的一次性授权码换取 Codex token 并保存账号。
pub async fn exchange_code(
    store: &AccountStore,
    code: &str,
    state: &str,
) -> Result<String, GatewayError> {
    let code = code.trim();
    let state = state.trim();
    if code.is_empty() || state.is_empty() {
        return Err(GatewayError::with_status(400, "Codex 回调缺少授权码或 state"));
    }
    let pending = {
        let mut table = lock_pending();
        sweep_pending(&mut table);
        table.remove(state)
    }
    .ok_or_else(|| GatewayError::with_status(400, "Codex 登录上下文已过期，请重新发起网页登录"))?;
    let (status, text) = post_token_form(
        &[
            ("grant_type", "authorization_code"),
            ("client_id", endpoints::CLIENT_ID),
            ("code", code),
            ("redirect_uri", &pending.redirect_uri),
            ("code_verifier", &pending.verifier),
        ],
        None,
    ).await?;
    let payload = parse_token_payload(status, &text, "授权码兑换")?;
    let access_token = token_string(&payload, "access_token");
    let refresh_token = token_string(&payload, "refresh_token");
    if access_token.is_empty() || refresh_token.is_empty() {
        return Err(GatewayError::with_status(502, "Codex OAuth 响应缺少 access_token 或 refresh_token"));
    }
    let (uid, user_id, email, plan_type) = claims(&payload);
    if uid.is_empty() {
        return Err(GatewayError::with_status(502, "Codex OAuth token 未包含 ChatGPT account id，无法创建可用账号"));
    }
    let expiry = logging::now_ms() + expires_in(&payload).clamp(60, 7 * 24 * 3600) * 1000;
    let account = store.add_codex_account(
        &serde_json::json!({
            "accessToken": access_token,
            "refreshToken": refresh_token,
            "expiresAt": expiry,
            "uid": uid,
            "userId": user_id,
            "email": email,
            "planType": plan_type,
        }),
        None,
        "web",
    ).map_err(|error| GatewayError::with_status(error.status_code, error.message))?;
    Ok(account.get("id").and_then(Value::as_str).unwrap_or("").to_string())
}

async fn refresh_token(
    refresh_token: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<(String, Option<String>, i64), GatewayError> {
    if refresh_token.trim().is_empty() {
        return Err(GatewayError::with_status(401, "Codex 账号缺少 refresh token，请重新网页登录"));
    }
    let (status, text) = post_token_form(
        &[
            ("grant_type", "refresh_token"),
            ("client_id", endpoints::CLIENT_ID),
            ("refresh_token", refresh_token),
        ],
        proxy,
    ).await?;
    let payload = parse_token_payload(status, &text, "token 刷新")?;
    let access_token = token_string(&payload, "access_token");
    if access_token.is_empty() {
        return Err(GatewayError::with_status(502, "Codex 刷新响应缺少 access_token"));
    }
    let rotated = token_string(&payload, "refresh_token");
    let expires_at = logging::now_ms() + expires_in(&payload).clamp(60, 7 * 24 * 3600) * 1000;
    Ok((access_token, (!rotated.is_empty()).then_some(rotated), expires_at))
}

async fn refresh_and_save(
    store: &AccountStore,
    record: &Value,
    credentials: &CodexCredentials,
) -> Result<CodexCredentials, GatewayError> {
    let proxy = account_proxy(record)?;
    let (access_token, rotated_refresh, expires_at) = refresh_token(&credentials.refresh_token, proxy.as_ref()).await?;
    let next_refresh = rotated_refresh.as_deref().unwrap_or(&credentials.refresh_token);
    match store.update_codex_credentials_if_current(
        &credentials.id,
        &credentials.refresh_token,
        &access_token,
        rotated_refresh.as_deref(),
        expires_at,
    ) {
        Ok(CredentialWrite::Written) => Ok(CodexCredentials {
            id: credentials.id.clone(),
            access_token,
            refresh_token: next_refresh.to_string(),
            expires_at,
            account_id: credentials.account_id.clone(),
        }),
        Ok(CredentialWrite::Stale) => {
            logging::log("[Codex]", "⚠️ 刷新结果未写回（期间账号凭证已变更），本次请求使用刚刷新出的令牌");
            Ok(CodexCredentials {
                id: credentials.id.clone(), access_token,
                refresh_token: next_refresh.to_string(), expires_at,
                account_id: credentials.account_id.clone(),
            })
        }
        Err(error) => Err(GatewayError::with_status(error.status_code, error.message)),
    }
}

/// 获取可用 access token；临期或强制刷新时走单飞并比较后回写。
pub async fn ensure_fresh(
    store: &AccountStore,
    account_id: &str,
    force: bool,
) -> Result<CodexCredentials, GatewayError> {
    let record = store.codex_account_record(account_id).ok_or_else(|| {
        GatewayError::with_status(401, if account_id.trim().is_empty() { "没有可用的 Codex 账号，请先登录" } else { "找不到该 Codex 账号" })
    })?;
    let credentials = super::credentials::from_record(&record);
    if !force && !credentials.expiring() {
        return Ok(credentials);
    }
    if credentials.refresh_token.trim().is_empty() {
        return Err(GatewayError::with_status(401, "Codex 账号缺少 refresh token，请重新网页登录"));
    }
    let key = format!("{}:{}:{}", store.file_string(), credentials.id, refresh_flight::fingerprint(&credentials.refresh_token));
    match FLIGHTS.get_or_init(Table::new).join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = refresh_and_save(store, &record, &credentials).await;
            leader.finish(result.clone());
            result
        }
    }
}
