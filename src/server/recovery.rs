//! Read-only startup diagnostics. Never construct a controller for invalid config.

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use super::{routes, ApiError, ServeOptions};
use crate::config::{self, Config};

struct Recovery {
    message: RwLock<String>,
    token: Option<String>,
    loopback: bool,
}

fn diagnostic(err: &anyhow::Error, opts: &ServeOptions) -> String {
    // TOML's Display includes source lines. Do not expose config values.
    let detail = match err.downcast_ref::<toml::de::Error>() {
        Some(_) => {
            "The configuration contains invalid TOML. Check the engine logs for details.".into()
        }
        None => format!("{err:#}"),
    };
    let location = config::find_config(opts.config.as_deref())
        .map(|path| format!(" ({})", path.display()))
        .unwrap_or_default();
    format!("NeutronSync cannot start: {detail} Correct the configuration file{location}. The engine retries automatically; sync is disabled until the configuration is valid.")
}

async fn gate(State(ctx): State<Arc<Recovery>>, req: Request, next: Next) -> Response {
    if req.uri().path() != "/api/v1/health" {
        if routes::from_a_browser(&req) || !routes::host_allowed(ctx.loopback, &req) {
            return ApiError::forbidden(
                "Requests from web pages or unexpected hosts are not accepted.",
            )
            .into_response();
        }
        if let Some(token) = &ctx.token {
            let given = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or("");
            if !routes::same_secret(given.as_bytes(), token.as_bytes()) {
                return ApiError::unauthorized().into_response();
            }
        }
    }
    next.run(req).await
}

async fn unavailable(State(ctx): State<Arc<Recovery>>) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_config",
        ctx.message.read().unwrap().clone(),
    )
}

/// Bind TCP only: an invalid config cannot identify the state owner safely, so
/// this mode must not replace a running engine's unix socket or state lock.
pub(super) fn wait_for_config(
    opts: &ServeOptions,
    listen: SocketAddr,
    token: Option<String>,
    err: anyhow::Error,
) -> Result<Option<Config>> {
    eprintln!("neutronsync serve: {err:#}");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .with_context(|| format!("cannot listen on {listen}"))?;
        let ctx = Arc::new(Recovery {
            message: RwLock::new(diagnostic(&err, opts)),
            token,
            loopback: listen.ip().is_loopback(),
        });
        let app = Router::new()
            .route(
                "/api/v1/health",
                get(|| async { Json(json!({"ok": true, "ready": false})) }),
            )
            .fallback(unavailable)
            .layer(middleware::from_fn_with_state(ctx.clone(), gate))
            .with_state(ctx.clone());
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stop_rx.await;
                })
                .await
        });
        let mut retry = tokio::time::interval(Duration::from_secs(1));
        let signal = super::wait_for_signal();
        tokio::pin!(signal);
        let cfg = loop {
            tokio::select! {
                _ = &mut signal => break None,
                _ = retry.tick() => match config::load(opts.config.as_deref()) {
                    Ok(cfg) => break Some(cfg),
                    Err(err) => *ctx.message.write().unwrap() = diagnostic(&err, opts),
                }
            }
        };
        let _ = stop_tx.send(());
        // Do not let a stalled client hold up recovery or shutdown.
        let mut task = task;
        if tokio::time::timeout(Duration::from_secs(2), &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
        }
        Ok(cfg)
    })
}
