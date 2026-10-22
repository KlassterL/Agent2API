//! Codex OAuth 账号的保存、刷新回写与公开形态。

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::priority::next_free_priority;
use super::sql;
use super::state::{mark_name_custom, StoredAccount};
use super::store::{AccountStore, AccountStoreError};
use super::store_util::{token_tail_of, truncate_chars};
use super::{CredentialWrite, CODEX_PROVIDER_ID};
use crate::server::logging;

const MAX_NAME_LENGTH: usize = 100;
const MAX_TOKEN_LENGTH: usize = 8192;

fn pick(payload: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| payload.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or("")
        .to_string()
}

fn account_id_for(uid: &str, email: &str, refresh_token: &str) -> String {
    let identity = if !uid.is_empty() {
        uid
    } else if !email.is_empty() {
        email
    } else {
        refresh_token
    };
    let mut hasher = Sha256::new();
    hasher.update(b"codex:account-id:v1\0");
    hasher.update(identity.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("codex-{}", digest.get(..12).unwrap_or(&digest))
}

impl AccountStore {
    /// 取 Codex 账号记录；空 id 表示优先级最高的启用账号。
    pub fn codex_account_record(&self, account_id: &str) -> Option<Value> {
        let guard = self.guard();
        if !account_id.trim().is_empty() {
            let record = self.record_by_id(&guard, account_id)?;
            return (record.provider() == CODEX_PROVIDER_ID).then(|| record.to_value());
        }
        let mut candidates: Vec<StoredAccount> = self
            .records_for_provider(&guard, CODEX_PROVIDER_ID)
            .into_iter()
            .filter(StoredAccount::enabled)
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        candidates.into_iter().next().map(|record| record.to_value())
    }

    /// 添加或更新 OAuth 账号。网页登录是唯一添加入口。
    pub fn add_codex_account(
        &self,
        payload: &Value,
        name: Option<&str>,
        source: &str,
    ) -> Result<Value, AccountStoreError> {
        let access_token = pick(payload, &["accessToken", "access_token"]);
        let refresh_token = pick(payload, &["refreshToken", "refresh_token"]);
        let uid = pick(payload, &["uid", "chatgptAccountId", "chatgpt_account_id"]);
        if access_token.is_empty() || refresh_token.is_empty() || uid.is_empty() {
            return Err(AccountStoreError::new(
                "Codex OAuth 响应缺少 access token、refresh token 或 ChatGPT account id",
                400,
            ));
        }
        if access_token.len() > MAX_TOKEN_LENGTH || refresh_token.len() > MAX_TOKEN_LENGTH {
            return Err(AccountStoreError::new("Codex OAuth token 过长", 400));
        }
        let email = truncate_chars(&pick(payload, &["email"]), 256);
        let user_id = pick(payload, &["userId", "user_id"]);
        let plan_type = truncate_chars(&pick(payload, &["planType", "plan_type"]), 64);
        let expires_at = payload.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);

        let guard = self.guard();
        let existing = self
            .records_for_provider(&guard, CODEX_PROVIDER_ID)
            .into_iter()
            .find(|record| {
                record.uid() == uid
                    || (!email.is_empty()
                        && record
                            .get("email")
                            .and_then(Value::as_str)
                            .is_some_and(|stored| stored.eq_ignore_ascii_case(&email)))
                    || record.refresh_token() == refresh_token
            });
        let id = existing
            .as_ref()
            .map(|record| record.id().to_string())
            .unwrap_or_else(|| account_id_for(&uid, &email, &refresh_token));
        if existing.is_none() {
            if let Some(occupied) = self.record_by_id(&guard, &id) {
                if occupied.provider() != CODEX_PROVIDER_ID {
                    return Err(AccountStoreError::new(
                        format!("账号 id「{id}」已被{}账号占用，无法添加 Codex 账号", occupied.provider()),
                        400,
                    ));
                }
            }
        }
        let explicit_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, MAX_NAME_LENGTH));
        let record_name = explicit_name
            .or_else(|| existing.as_ref().map(StoredAccount::name).filter(|value| !value.is_empty()))
            .unwrap_or_else(|| if !email.is_empty() { email.clone() } else { format!("Codex {}", token_tail_of(&refresh_token)) });
        let priority = match existing.as_ref() {
            Some(record) => record.priority(),
            None => {
                let used = self.with_conn(&guard, |conn| sql::priorities_all(conn))?;
                next_free_priority(&used)
            }
        };
        let now = logging::now_ms();
        let mut fields = Map::new();
        fields.insert("id".to_string(), Value::String(id.clone()));
        fields.insert("provider".to_string(), Value::String(CODEX_PROVIDER_ID.to_string()));
        fields.insert("name".to_string(), Value::String(record_name));
        mark_name_custom(
            &mut fields,
            name.is_some_and(|value| !value.trim().is_empty()),
            existing.as_ref(),
        );
        fields.insert("uid".to_string(), Value::String(uid));
        fields.insert("accessToken".to_string(), Value::String(access_token));
        fields.insert("refreshToken".to_string(), Value::String(refresh_token.clone()));
        fields.insert("tokenTail".to_string(), Value::String(token_tail_of(&refresh_token)));
        fields.insert("source".to_string(), Value::String(source.to_string()));
        fields.insert("priority".to_string(), Value::from(priority));
        fields.insert("enabled".to_string(), Value::Bool(existing.as_ref().map(StoredAccount::enabled).unwrap_or(true)));
        fields.insert("addedAt".to_string(), Value::from(existing.as_ref().map(StoredAccount::added_at).filter(|value| *value > 0).unwrap_or(now)));
        fields.insert("updatedAt".to_string(), Value::from(now));
        if !email.is_empty() {
            fields.insert("email".to_string(), Value::String(email));
        }
        if !user_id.is_empty() {
            fields.insert("userId".to_string(), Value::String(user_id));
        }
        if !plan_type.is_empty() {
            fields.insert("planType".to_string(), Value::String(plan_type));
        }
        if expires_at > 0 {
            fields.insert("expiresAt".to_string(), Value::from(expires_at));
        }
        if let Some(existing) = existing.as_ref() {
            for (key, value) in existing.fields() {
                fields.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        let saved = StoredAccount::from_map(fields);
        self.with_conn(&guard, |conn| sql::put(conn, &saved))?;
        logging::log("[Accounts]", &format!("✅ Codex 账号已保存（优先级 {priority}）"));
        Ok(self.public_account(&saved))
    }

    /// 只在刷新使用的 refresh token 仍是快照值时回写，避免覆盖用户新登录的凭证。
    pub fn update_codex_credentials_if_current(
        &self,
        id: &str,
        expected_refresh_token: &str,
        access_token: &str,
        refresh_token: Option<&str>,
        expires_at: i64,
    ) -> Result<CredentialWrite, AccountStoreError> {
        let guard = self.guard();
        let Some(mut record) = self.record_by_id(&guard, id) else {
            return Ok(CredentialWrite::Stale);
        };
        if record.provider() != CODEX_PROVIDER_ID {
            return Err(AccountStoreError::bad_request(format!("账号 {id} 不是 Codex 账号")));
        }
        if record.refresh_token() != expected_refresh_token {
            return Ok(CredentialWrite::Stale);
        }
        record.set("accessToken", Value::String(access_token.to_string()));
        if let Some(refresh) = refresh_token.map(str::trim).filter(|value| !value.is_empty()) {
            record.set("refreshToken", Value::String(refresh.to_string()));
            record.set("tokenTail", Value::String(token_tail_of(refresh)));
        }
        if expires_at > 0 {
            record.set("expiresAt", Value::from(expires_at));
        }
        record.set_updated_at(logging::now_ms());
        self.with_conn(&guard, |conn| sql::update_in_place(conn, &record))?;
        Ok(CredentialWrite::Written)
    }

    /// 公开形态只带令牌尾号和存在性标记，不透出 access/refresh token。
    pub fn to_codex_public_account(&self, record: &StoredAccount) -> Value {
        let mut public = record.to_value();
        if let Some(fields) = public.as_object_mut() {
            fields.remove("accessToken");
            fields.remove("refreshToken");
            fields.insert("hasAccessToken".to_string(), Value::Bool(!record.access_token().trim().is_empty()));
            fields.insert("hasRefreshToken".to_string(), Value::Bool(!record.refresh_token().trim().is_empty()));
            fields.insert("desktop".to_string(), Value::Bool(false));
            fields.insert("available".to_string(), Value::Bool(record.enabled() && !record.refresh_token().trim().is_empty()));
        }
        public
    }
}
