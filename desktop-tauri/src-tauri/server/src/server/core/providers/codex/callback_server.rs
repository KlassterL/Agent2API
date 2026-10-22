//! Codex 官方桌面授权使用固定的 localhost:1455 回调地址。
//!
//! 与 EasyCLIProxyAPI 的 WebUI 登录链一致：本地监听固定回调端口，再把浏览器
//! 带回的原始查询串转发到 Agent2API 的统一 OAuth 收尾路由。

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::OnceLock;

use axum::extract::{OriginalUri, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use super::endpoints;
use crate::server::logging;

static FORWARDER: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// 确保本地 Codex 回调端口已转发到网关。服务进程内只启动一次。
pub async fn ensure_running(gateway_port: u16) -> Result<(), String> {
    let forward_base = format!("http://127.0.0.1:{gateway_port}");
    let forwarder = FORWARDER.get_or_init(|| Mutex::new(None));
    let mut active = forwarder.lock().await;
    if let Some(existing) = active.as_deref() {
        return if existing == forward_base {
            Ok(())
        } else {
            Err(format!(
                "Codex 回调转发器已指向 {existing}，不能改为 {forward_base}"
            ))
        };
    }

    let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, endpoints::CALLBACK_PORT))
        .await
        .map_err(|error| {
            format!(
                "Codex 登录需要监听 {}，但该回调端口不可用：{error}",
                endpoints::REDIRECT_URI
            )
        })?;
    let v6 = match TcpListener::bind((Ipv6Addr::LOCALHOST, endpoints::CALLBACK_PORT)).await {
        Ok(listener) => Some(listener),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
            ) =>
        {
            None
        }
        Err(error) => {
            return Err(format!(
                "Codex 登录需要监听 {}，但 IPv6 回调端口不可用：{error}",
                endpoints::REDIRECT_URI
            ));
        }
    };

    let app = router(forward_base.clone());
    spawn_forwarder(v4, app.clone(), "IPv4");
    if let Some(v6) = v6 {
        spawn_forwarder(v6, app, "IPv6");
    }
    *active = Some(forward_base.clone());
    logging::log(
        "[Login]",
        &format!(
            "Codex OAuth 回调已监听 localhost:{} 并转发到网关 {forward_base}",
            endpoints::CALLBACK_PORT
        ),
    );
    Ok(())
}

fn router(forward_base: String) -> Router {
    Router::new()
        .route(endpoints::CALLBACK_PATH, get(forward))
        .fallback(not_found)
        .with_state(forward_base)
}

/// 保留 OpenAI 回传的完整查询串，由网关统一校验 state 并完成 PKCE 换码。
async fn forward(State(forward_base): State<String>, OriginalUri(uri): OriginalUri) -> Response {
    let path_and_query = uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or(endpoints::CALLBACK_PATH);
    let mut response = Redirect::to(&format!("{forward_base}{path_and_query}")).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

fn spawn_forwarder(listener: TcpListener, app: Router, family: &'static str) {
    crate::spawn_task(async move {
        if let Err(error) = axum::serve(listener, app).await {
            logging::log(
                "[Login]",
                &format!("⚠️ Codex {family} OAuth 回调转发器异常退出：{error}"),
            );
        }
    });
}
