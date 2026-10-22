//! Codex 账号会话凭证读取与过期判定。

use serde_json::Value;

pub const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;

#[derive(Clone, Debug)]
pub struct CodexCredentials {
    pub id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub account_id: String,
}

impl CodexCredentials {
    pub fn expiring(&self) -> bool {
        self.access_token.trim().is_empty()
            || self.expires_at <= crate::server::logging::now_ms() + REFRESH_SKEW_MS
    }
}

pub fn from_record(record: &Value) -> CodexCredentials {
    CodexCredentials {
        id: record.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
        access_token: record.get("accessToken").and_then(Value::as_str).unwrap_or("").to_string(),
        refresh_token: record.get("refreshToken").and_then(Value::as_str).unwrap_or("").to_string(),
        expires_at: record
            .get("expiresAt")
            .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
            .unwrap_or(0),
        account_id: record.get("uid").and_then(Value::as_str).unwrap_or("").to_string(),
    }
}
