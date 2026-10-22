//! Codex OAuth 与 Responses API 端点。

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
pub const CALLBACK_PATH: &str = "/auth/callback";
pub const CALLBACK_PORT: u16 = 1455;
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const SCOPES: &str = "openid email profile offline_access";
pub const ORIGINATOR: &str = "codex_cli_rs";
pub const USER_AGENT: &str = "codex_cli_rs/0.1.0";
pub const REQUEST_TIMEOUT_MS: u64 = 30_000;
