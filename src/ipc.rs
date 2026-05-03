//! Thin wrapper over `asdf-overlay-client`'s injection + IPC API.
//!
//! Centralizes:
//!
//! * Performing the injection ([`asdf_overlay_client::inject_only_with`])
//!   and then opening the IPC named pipe with our own retry loop —
//!   `inject_with` upstream opens the pipe just once and races the DLL's
//!   IPC server startup, which leads to `ERROR_FILE_NOT_FOUND` failures.
//! * Bringing a freshly-announced game window into the engine's default
//!   "hover-mode" configuration (anchor/position at the origin, cursor
//!   listening on, `BlockCursorInOverlay` enabled).
//! * Pushing / clearing the shared D3D11 handle the WebView2 thread
//!   produces.
//!
//! Deliberately not exposed publicly from the crate — the engine's
//! public surface stays at the [`OverlayEngine`] level.
//!
//! [`OverlayEngine`]: crate::OverlayEngine

use std::time::Duration;

use anyhow::{anyhow, Context as _};
use asdf_overlay_client::{
    client::{IpcClientConn, IpcClientEventStream},
    common::{
        ipc::create_ipc_addr,
        request::{
            BlockCursorInOverlay, ListenInput, SetAnchor, SetPosition, UpdateSharedHandle,
        },
        size::PercentLength,
    },
    inject_only_with, HookGuard, InjectStrategy, InjectionResult, OverlayDll,
};
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::time::{sleep, timeout};

use crate::error::{Error, Result};

/// How often to retry opening the named pipe while waiting for the
/// in-game DLL to start its IPC server.
const PIPE_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Live IPC session: connection, event stream, and (for the
/// `WindowsHook` safe-inject path) the hook guard the engine has to
/// keep alive for as long as the overlay should stay loaded.
pub(crate) struct IpcSession {
    pub conn: IpcClientConn,
    pub events: IpcClientEventStream,
    /// Owned guard for `SetWindowsHookEx`-based injection. Dropping it
    /// asks the OS to release the hook, which in turn unloads the
    /// overlay DLL from the target. The engine keeps it alive for the
    /// session's lifetime; `RemoteThread` injection leaves this `None`.
    pub hook_guard: Option<HookGuard>,
}

/// Inject the overlay DLL into `pid` and bring up the IPC channel.
///
/// `attach_timeout` bounds both the actual injection and the subsequent
/// named-pipe handshake; on timeout the function returns
/// [`Error::Injection`].
///
/// Implemented as `inject_only_with` followed by a polling-open of the
/// named pipe (rather than the upstream client's `inject_with`), because
/// the safe-inject path returns the moment `SetWindowsHookEx` succeeds —
/// which is *before* the in-game DLL has actually loaded and started
/// serving on the IPC pipe. Opening the pipe just once at that point
/// races and frequently fails with `ERROR_FILE_NOT_FOUND`. We retry
/// every [`PIPE_RETRY_INTERVAL`] until the pipe shows up or
/// `attach_timeout` elapses.
pub(crate) async fn inject(
    pid: u32,
    dll: OverlayDll<'_>,
    strategy: InjectStrategy,
    attach_timeout: Duration,
) -> Result<IpcSession> {
    let InjectionResult {
        module_handle: _,
        hook: hook_guard,
    } = inject_only_with(pid, dll, strategy, Some(attach_timeout))
        .map_err(Error::Injection)?;

    let addr = create_ipc_addr(pid);
    let pipe = timeout(attach_timeout, wait_for_pipe(addr))
        .await
        .map_err(|_| {
            Error::Injection(anyhow!(
                "timed out waiting for in-game IPC pipe to open"
            ))
        })?
        .map_err(Error::Injection)?;

    let (conn, events) = IpcClientConn::new(pipe)
        .await
        .context("IpcClientConn handshake")
        .map_err(Error::Injection)?;

    Ok(IpcSession {
        conn,
        events,
        hook_guard,
    })
}

/// Open the asdf-overlay named pipe, retrying on
/// `ERROR_FILE_NOT_FOUND` (2) — the error tokio surfaces when the
/// server side isn't listening yet. Caller is responsible for the
/// outer timeout.
async fn wait_for_pipe(
    addr: String,
) -> anyhow::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    /// `ERROR_FILE_NOT_FOUND` from `winerror.h`. Surfaced by
    /// `ClientOptions::open` when the server-side pipe doesn't exist.
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    /// `ERROR_PIPE_BUSY`: server exists but all instances are in use.
    /// Retry; the server hands out a fresh instance per client.
    const ERROR_PIPE_BUSY: i32 = 231;

    loop {
        match ClientOptions::new().open(&addr) {
            Ok(client) => return Ok(client),
            Err(err) => {
                let code = err.raw_os_error();
                if matches!(code, Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PIPE_BUSY)) {
                    sleep(PIPE_RETRY_INTERVAL).await;
                    continue;
                }
                return Err(anyhow::Error::from(err).context("ClientOptions::open"));
            }
        }
    }
}

/// Configure a game window for hover-mode interaction:
///
/// * Anchor + position at the top-left so the WebView2 surface coords
///   match the game-window client coords directly.
/// * Cursor input listening on.
/// * `BlockCursorInOverlay` enabled — the DLL only consumes cursor
///   events while the cursor is over the overlay rect or during a drag
///   started inside it (goverlay-style hit-testing).
///
/// Keyboard input is intentionally left off; the engine forwards
/// keyboard separately once full keyboard support lands.
pub(crate) async fn configure_window_hover(conn: &mut IpcClientConn, id: u32) -> Result<()> {
    let mut window = conn.window(id);
    window
        .request(SetAnchor {
            x: PercentLength::Length(0.0),
            y: PercentLength::Length(0.0),
        })
        .await
        .map_err(Error::Ipc)?;
    window
        .request(SetPosition {
            x: PercentLength::Length(0.0),
            y: PercentLength::Length(0.0),
        })
        .await
        .map_err(Error::Ipc)?;
    window
        .request(ListenInput {
            cursor: true,
            // Subscribe to keyboard events too. The engine forwards
            // them to WebView2 only while the cursor is over the
            // overlay (gated in `forward_input`), so keys typed while
            // the user is gaming continue to reach the game; keys
            // typed while hovering an overlay input get delivered to
            // the page.
            keyboard: true,
        })
        .await
        .map_err(Error::Ipc)?;
    window
        .request(BlockCursorInOverlay { enabled: true })
        .await
        .map_err(Error::Ipc)?;
    Ok(())
}

/// Push a freshly-rotated `UpdateSharedHandle` to the game window. Each
/// time the WebView2 thread mints a new staging texture
/// (first frame, or surface size changed) the engine must re-issue
/// this so the in-game DLL knows where to read frames from.
pub(crate) async fn push_shared_handle(
    conn: &mut IpcClientConn,
    id: u32,
    update: UpdateSharedHandle,
) -> Result<()> {
    conn.window(id)
        .request(update)
        .await
        .map_err(Error::Ipc)?;
    Ok(())
}

/// Tell asdf-overlay to drop its current shared texture before we close
/// the IPC channel. Used during detach.
pub(crate) async fn clear_shared_handle(conn: &mut IpcClientConn, id: u32) -> Result<()> {
    conn.window(id)
        .request(UpdateSharedHandle { handle: None })
        .await
        .map_err(Error::Ipc)?;
    Ok(())
}
