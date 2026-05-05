//! Minimal local HTTP server that hosts the embedded shell page and
//! optionally a user-supplied `static_dir` of panel assets.
//!
//! The injected WebView2 can only fetch `http(s)://` and `file://`
//! URLs — Tauri's `tauri://localhost` scheme is unavailable inside
//! a vanilla composition-mode WebView2. This server gives us a
//! stable `http://127.0.0.1:NNNNN` origin we can point the WebView2
//! at.
//!
//! Routes:
//! * `GET /__overlay/shell.html` — embedded shell page.
//! * `GET /__overlay/shell.js`   — embedded shell bundle.
//! * `GET /<path>`               — files under `static_dir` (panel assets).
//! * Anything mounted by `OverlayBuilder::extra_router(...)`
//!   (e.g. an Ascent-specific `/video?token=...` endpoint).
//!
//! The server binds to a kernel-assigned port on `127.0.0.1` and
//! reports it via [`OverlayAssetServer::shell_url`]. It runs as a
//! tokio task for the lifetime of the [`OverlayAssetServer`] handle;
//! drop terminates it.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower_http::services::ServeDir;

use crate::error::{Error, Result};

const SHELL_HTML: &str = include_str!("../../shell/dist/shell.html");
const SHELL_JS: &str = include_str!("../../shell/dist/shell.js");

/// Running asset-server handle. Drop to shut down the server.
pub(crate) struct OverlayAssetServer {
    shell_url: String,
    /// Shutdown signal. `Option` so `Drop` can move it out and send.
    shutdown: Option<oneshot::Sender<()>>,
}

impl OverlayAssetServer {
    /// URL of the shell page the engine should navigate to.
    pub(crate) fn shell_url(&self) -> &str {
        &self.shell_url
    }
}

impl Drop for OverlayAssetServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Configuration for the asset server. Built by `OverlayBuilder` and
/// passed straight through; intentionally crate-private.
pub(crate) struct AssetServerConfig {
    /// Optional directory whose contents are served at `/<path>`.
    /// `None` means no panel-asset hosting (only the embedded shell
    /// + extra router are reachable).
    pub static_dir: Option<PathBuf>,
    /// Optional custom routes merged into the server's router.
    pub extra_router: Option<Router>,
}

pub(crate) async fn start(config: AssetServerConfig) -> Result<OverlayAssetServer> {
    let mut router = Router::new()
        .route(
            "/__overlay/shell.html",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    SHELL_HTML,
                )
                    .into_response()
            }),
        )
        .route(
            "/__overlay/shell.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
                    SHELL_JS,
                )
                    .into_response()
            }),
        );

    if let Some(dir) = config.static_dir {
        // `ServeDir` is registered as the fallback service so explicit
        // routes above (and any `extra_router` routes) take precedence
        // over an accidentally-overlapping file path.
        router = router.fallback_service(ServeDir::new(dir));
    }

    if let Some(extra) = config.extra_router {
        router = router.merge(extra);
    }

    let addr: SocketAddr = "127.0.0.1:0".parse().expect("static addr parse");
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("asset server bind failed: {e}")))?;
    let local = listener
        .local_addr()
        .map_err(|e| Error::Other(anyhow::anyhow!("asset server local_addr failed: {e}")))?;
    let shell_url = format!("http://{}/__overlay/shell.html", local);

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        let server = axum::serve(listener, router).with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        });
        if let Err(err) = server.await {
            eprintln!("[overlay/asset_server] server task exited with error: {err}");
        }
    });

    eprintln!("[overlay/asset_server] serving shell at {shell_url}");
    Ok(OverlayAssetServer {
        shell_url,
        shutdown: Some(shutdown_tx),
    })
}
