//! OpenAI Codex agent provider：网页登录 OAuth、凭证续期、模型目录与 Responses 转发。

pub mod adapter;
pub mod callback_server;
pub mod credentials;
pub mod endpoints;
pub mod images;
pub mod models;
pub mod oauth;

pub use adapter::{CodexAdapter, CODEX_ADAPTER};
