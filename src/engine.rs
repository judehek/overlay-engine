//! [`OverlayEngine`] -- the top-level handle.
//!
//! `attach`, `detach`, and `set_hit_regions` are fully wired here.
//! `navigate` is still a `todo!()` placeholder that will route through
//! the engine loop's request channel as it's implemented.

use std::time::Duration;

use anyhow::{anyhow, Context as _};
use asdf_overlay_client::{
    client::{IpcClientConn, IpcClientEventStream},
    event::{
        input::{CursorEvent, CursorInput, InputEvent, InputPosition},
        OverlayEvent, WindowEvent,
    },
    surface::OverlaySurface,
    HookGuard,
};
use tokio::sync::{mpsc, oneshot};

use crate::composition::{self, FrameUpdate, WebThreadHandle, WebThreadParams};
use crate::config::{DllSource, OverlayConfig};
use crate::error::{Error, Result};
use crate::event::{DetachReason, EngineEvent};
use crate::hit_region::HitRegion;
use crate::input;
use crate::ipc;

/// One attached overlay session.
///
/// An [`OverlayEngine`] owns:
///
/// * The `asdf-overlay` IPC connection and the loaded DLL inside the game.
/// * The WebView2 composition stack and its render thread.
/// * The shared D3D11 texture handed to the in-game DLL on every frame.
///
/// `Clone` is implemented so several handles can dispatch requests to
/// the same underlying engine loop (e.g. a panel registry holding one
/// handle per panel). Cloning is cheap (a `Sender` clone). The loop
/// itself is owned by exactly one task; whichever clone calls
/// [`detach`](Self::detach) first triggers shutdown, after which all
/// other clones return [`Error::AlreadyDetached`] from any operation.
///
/// Drop semantics: dropping the last engine handle leaves the engine
/// loop running until something tells it to detach. For deterministic
/// teardown, prefer [`OverlayEngine::detach`].
#[derive(Clone)]
pub struct OverlayEngine {
    /// Channel into the engine loop. All runtime control flows through
    /// here, so the loop is the single owner of the IPC connection,
    /// WebView2 thread handle, and surface state.
    request_tx: mpsc::Sender<EngineRequest>,

    /// Cached so accessors don't have to round-trip the loop.
    surface_size: (u32, u32),
}

impl OverlayEngine {
    /// Inject the overlay DLL into `target_pid`, bring up the composition
    /// stack, and return once the IPC channel is healthy and we've picked
    /// a game window to render onto.
    ///
    /// On success returns `(handle, events)`:
    /// * `handle` -- the engine. Use it to drive runtime state.
    /// * `events` -- async receiver for [`EngineEvent`]s. The first event
    ///   on this channel is always `EngineEvent::Attached`. The receiver
    ///   stays open until `detach` (or any fatal background error)
    ///   produces the terminal `Detached` event.
    pub async fn attach(
        target_pid: u32,
        config: OverlayConfig,
    ) -> Result<(Self, mpsc::Receiver<EngineEvent>)> {
        let dll = match &config.dll_source {
            DllSource::Dir(dir) => crate::dll::resolve_dir(dir)?,
            DllSource::Files(files) => *files,
        };

        let mut ipc_session =
            ipc::inject(target_pid, dll, config.inject_strategy, config.attach_timeout).await?;

        let web = composition::spawn_web_thread(WebThreadParams {
            url: config
                .initial_url
                .clone()
                .unwrap_or_else(|| "about:blank".to_string()),
            surface_size: config.surface_size,
        })
        .await
        .map_err(Error::Composition)?;

        let (window_id, game_window_size) =
            wait_for_first_window(&mut ipc_session.events, config.attach_timeout).await?;

        ipc::configure_window_hover(&mut ipc_session.conn, window_id).await?;

        // OverlaySurface lives here on the engine side (its own internal
        // D3D11 device); the WebView2 thread hands us KMT handles, and
        // this is what mints the `UpdateSharedHandle`s we forward to the
        // game.
        let surface: OverlaySurface<2> = OverlaySurface::new(None)
            .context("failed to create overlay surface")
            .map_err(Error::Composition)?;

        let (engine_events_tx, engine_events_rx) = mpsc::channel::<EngineEvent>(64);
        let (request_tx, request_rx) = mpsc::channel::<EngineRequest>(16);

        let surface_size = config.surface_size;
        let loop_state = LoopState {
            conn: ipc_session.conn,
            ipc_events: ipc_session.events,
            hook_guard: ipc_session.hook_guard,
            web,
            surface,
            window_id,
            game_window_size,
            surface_size,
            cursor_in_overlay: false,
            hit_regions: Vec::new(),
            engine_events_tx,
            request_rx,
        };

        tokio::spawn(engine_loop(loop_state));

        Ok((
            Self {
                request_tx,
                surface_size,
            },
            engine_events_rx,
        ))
    }

    /// Surface size the engine was attached with. The WebView2 visual
    /// and the staging texture are both this size today.
    pub fn surface_size(&self) -> (u32, u32) {
        self.surface_size
    }

    /// Navigate the WebView2 to a new URL.
    ///
    /// Returns once the engine has dispatched the navigation; completion
    /// arrives later as [`EngineEvent::Navigation`].
    pub async fn navigate(&self, url: impl Into<String>) -> Result<()> {
        let _ = url.into();
        todo!("post a navigation request onto the WebView2 STA thread")
    }

    /// Replace the currently-active set of [`HitRegion`]s.
    ///
    /// Cursor events that fall inside *any* of these regions are
    /// consumed by the overlay and hidden from the game. Cursor events
    /// outside every region pass through to the game unchanged.
    ///
    /// Passing an empty slice disables hover-mode blocking entirely;
    /// the overlay becomes purely visual.
    pub async fn set_hit_regions(&self, regions: &[HitRegion]) -> Result<()> {
        let regions = regions.to_vec();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(EngineRequest::SetHitRegions {
                regions,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::AlreadyDetached)?;
        reply_rx.await.map_err(|_| Error::AlreadyDetached)?
    }

    /// Post `text` into the WebView2 page as
    /// `window.chrome.webview.message`. Pair with the page-side
    /// `window.chrome.webview.addEventListener('message', ...)` to
    /// drive UI state from the host.
    ///
    /// Returns once the message has been queued onto the WebView2 STA
    /// thread; delivery to the page is asynchronous and not reported
    /// back via [`EngineEvent`]. The page itself can ack via
    /// `window.chrome.webview.postMessage(...)` if needed (delivered
    /// to the host as [`EngineEvent::WebMessage`]).
    pub async fn post_web_message(&self, text: impl Into<String>) -> Result<()> {
        let text = text.into();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(EngineRequest::PostWebMessage {
                text,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::AlreadyDetached)?;
        reply_rx.await.map_err(|_| Error::AlreadyDetached)?
    }

    /// Tear down the overlay cleanly. Returns once the WebView2 thread
    /// has joined, the IPC pipe has been closed, and the safe-inject
    /// hook has been released. After this, the [`EngineEvent`] channel
    /// emits a final `Detached { reason: Requested }` and closes.
    pub async fn detach(self) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        // If the loop has already exited, the send will fail; we treat
        // that as already-detached rather than a hard error.
        if self
            .request_tx
            .send(EngineRequest::Detach { reply: reply_tx })
            .await
            .is_err()
        {
            return Err(Error::AlreadyDetached);
        }
        reply_rx.await.map_err(|_| Error::AlreadyDetached)?
    }
}

// ---------------------------------------------------------------------------
// Engine loop -- private
// ---------------------------------------------------------------------------

/// Internal control messages from the public API into the engine loop.
enum EngineRequest {
    SetHitRegions {
        regions: Vec<HitRegion>,
        reply: oneshot::Sender<Result<()>>,
    },
    PostWebMessage {
        text: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Detach {
        reply: oneshot::Sender<Result<()>>,
    },
}

/// Everything the engine loop owns. Lives entirely inside the
/// `engine_loop` task so we don't have to share / lock anything.
struct LoopState {
    conn: IpcClientConn,
    ipc_events: IpcClientEventStream,
    /// Held but never read; dropping it on loop exit unhooks the
    /// safe-inject `WH_GETMESSAGE` (RemoteThread strategy leaves it
    /// `None`).
    hook_guard: Option<HookGuard>,
    web: WebThreadHandle,
    surface: OverlaySurface<2>,
    window_id: u32,
    game_window_size: (u32, u32),
    surface_size: (u32, u32),
    cursor_in_overlay: bool,
    /// Currently active hit regions. Empty = hover blocking disabled
    /// (overlay is purely visual).
    hit_regions: Vec<HitRegion>,
    engine_events_tx: mpsc::Sender<EngineEvent>,
    request_rx: mpsc::Receiver<EngineRequest>,
}

/// Wait for the first `WindowEvent::Added` on the IPC stream, ignoring
/// any preceding non-Added events. Times out per `timeout`.
async fn wait_for_first_window(
    events: &mut IpcClientEventStream,
    timeout: Duration,
) -> Result<(u32, (u32, u32))> {
    let timer = tokio::time::sleep(timeout);
    tokio::pin!(timer);
    loop {
        tokio::select! {
            maybe_event = events.recv() => {
                let Some(event) = maybe_event else {
                    return Err(Error::IpcClosedEarly);
                };
                if let OverlayEvent::Window {
                    id,
                    event: WindowEvent::Added { width, height, .. },
                } = event
                {
                    return Ok((id, (width, height)));
                }
            }
            _ = &mut timer => {
                return Err(Error::Injection(anyhow!(
                    "timed out waiting for the first overlay window"
                )));
            }
        }
    }
}

/// Engine loop body. Routes between IPC events, frames from the
/// WebView2 thread, and runtime control requests until either side
/// closes.
async fn engine_loop(mut state: LoopState) {
    // First event the consumer ever sees on the engine event channel.
    let attached = EngineEvent::Attached {
        target_window_id: state.window_id,
        game_window_size: state.game_window_size,
        surface_size: state.surface_size,
    };
    if state.engine_events_tx.send(attached).await.is_err() {
        // Receiver dropped immediately. Bail.
        let _ = teardown(state, DetachReason::Requested).await;
        return;
    }

    // Default: nothing is interactable. The DLL was configured with
    // `BlockCursorInOverlay { enabled: false }` in
    // `configure_window_hover`, so cursor messages flow straight to
    // the game until the host opens an interactive panel and calls
    // `set_hit_regions(&[..])` (which flips both `state.hit_regions`
    // and the DLL-side flag in lockstep). Defaulting to "full surface
    // interactive" here would make every click anywhere on the game
    // window get routed to a WebView2 that has nothing to render.
    state.hit_regions = Vec::new();

    let mut detach_reply: Option<oneshot::Sender<Result<()>>> = None;
    let mut detach_reason = DetachReason::Requested;

    loop {
        tokio::select! {
            maybe_frame = state.web.frame_rx.recv() => {
                let Some(frame) = maybe_frame else {
                    detach_reason = DetachReason::Error(
                        "WebView2 thread closed its frame channel".into(),
                    );
                    break;
                };
                if let Err(err) = handle_frame(&mut state, frame).await {
                    eprintln!("[overlay-engine] frame forward failed: {err:?}");
                    detach_reason = DetachReason::Error(err.to_string());
                    break;
                }
            }

            maybe_event = state.ipc_events.recv() => {
                let Some(event) = maybe_event else {
                    detach_reason = DetachReason::IpcClosed;
                    break;
                };
                if let HandleResult::Stop(reason) = handle_ipc_event(&mut state, event).await {
                    detach_reason = reason;
                    break;
                }
            }

            maybe_request = state.request_rx.recv() => {
                let Some(request) = maybe_request else {
                    // Engine handle dropped without calling detach.
                    break;
                };
                match request {
                    EngineRequest::SetHitRegions { regions, reply } => {
                        // Mirror the new region set onto the DLL's
                        // `BlockCursorInOverlay` flag *before* updating
                        // local state. When transitioning empty -> non-empty
                        // we need the DLL to start consuming cursor msgs
                        // before the page paints (otherwise the user sees
                        // the panel but their click goes through to the
                        // game beneath it); when going non-empty -> empty
                        // we want input to start flowing back to the game
                        // before the next frame.
                        //
                        // We flip on any non-empty -> empty transition (or
                        // vice versa). Same-state requests are a no-op so
                        // we don't churn the IPC channel when a panel just
                        // resizes its rect.
                        let was_blocking = !state.hit_regions.is_empty();
                        let now_blocking = !regions.is_empty();
                        let toggle_result = if was_blocking != now_blocking {
                            ipc::set_block_cursor_in_overlay(
                                &mut state.conn,
                                state.window_id,
                                now_blocking,
                            )
                            .await
                        } else {
                            Ok(())
                        };
                        match toggle_result {
                            Ok(()) => {
                                state.hit_regions = regions;
                                let _ = reply.send(Ok(()));
                            }
                            Err(err) => {
                                // Surface the IPC failure to the caller
                                // and leave `state.hit_regions` untouched
                                // so the local view stays consistent with
                                // what the DLL is enforcing.
                                let _ = reply.send(Err(err));
                            }
                        }
                    }
                    EngineRequest::PostWebMessage { text, reply } => {
                        match state.web.post_message_tx.send(text) {
                            Ok(()) => {
                                state.web.wake_post_message();
                                let _ = reply.send(Ok(()));
                            }
                            Err(err) => {
                                let _ = reply.send(Err(Error::Other(anyhow!(
                                    "post-message channel closed: {err}"
                                ))));
                            }
                        }
                    }
                    EngineRequest::Detach { reply } => {
                        detach_reply = Some(reply);
                        break;
                    }
                }
            }

            maybe_text = state.web.web_message_rx.recv() => {
                match maybe_text {
                    Some(text) => {
                        let _ = state
                            .engine_events_tx
                            .send(EngineEvent::WebMessage(text))
                            .await;
                    }
                    None => {
                        // STA thread closed its outgoing channel —
                        // treat as a fatal error so the engine
                        // detaches cleanly.
                        detach_reason = DetachReason::Error(
                            "WebView2 thread closed its web-message channel".into(),
                        );
                        break;
                    }
                }
            }
        }
    }

    let result = teardown(state, detach_reason).await;
    if let Some(reply) = detach_reply {
        let _ = reply.send(result);
    }
}

enum HandleResult {
    Continue,
    Stop(DetachReason),
}

/// Push a freshly-arrived frame into asdf-overlay. Mints a new
/// `UpdateSharedHandle` (and forwards it) when the OverlaySurface
/// rotates internally.
async fn handle_frame(state: &mut LoopState, frame: FrameUpdate) -> Result<()> {
    let FrameUpdate { handle, width, height } = frame;
    match state
        .surface
        .update_from_shared(width, height, handle, None)
    {
        Ok(Some(update)) => {
            ipc::push_shared_handle(&mut state.conn, state.window_id, update).await?;
        }
        Ok(None) => {
            // OverlaySurface reused its existing internal texture; the
            // game already has the right handle.
        }
        Err(err) => {
            eprintln!("[overlay-engine] OverlaySurface::update_from_shared failed: {err:?}");
        }
    }
    Ok(())
}

/// Process an IPC `OverlayEvent` arriving from the in-game DLL. Returns
/// `Stop(reason)` if the loop should exit (e.g. window destroyed).
async fn handle_ipc_event(state: &mut LoopState, event: OverlayEvent) -> HandleResult {
    match event {
        OverlayEvent::Window {
            id,
            event: WindowEvent::Added { .. },
        } => {
            // We only track the first window today. Additional windows
            // are ignored until multi-window support lands.
            let _ = id;
        }
        OverlayEvent::Window {
            id,
            event: WindowEvent::Resized { width, height },
        } if id == state.window_id => {
            let _ = state
                .engine_events_tx
                .send(EngineEvent::GameWindowResized { width, height })
                .await;
        }
        OverlayEvent::Window {
            id,
            event: WindowEvent::Destroyed,
        } if id == state.window_id => {
            return HandleResult::Stop(DetachReason::IpcClosed);
        }
        OverlayEvent::Window {
            event: WindowEvent::Input(input_event),
            ..
        } => {
            forward_input(state, input_event);
        }
        OverlayEvent::Window {
            event: WindowEvent::InputBlockingEnded,
            ..
        } => {
            // No-op while we don't surface a `BlockInput` API on the
            // engine. The DLL only emits this when its own
            // `BlockInput` flag flips off, which we never set.
        }
        // Other window IDs / event combinations: ignored.
        _ => {}
    }
    HandleResult::Continue
}

/// Hit-test a cursor event against the engine's current
/// `hit_regions`, synthesize a Leave on the inside->outside transition,
/// and post a wake to the WebView2 thread if anything was forwarded.
fn forward_input(state: &mut LoopState, event: InputEvent) {
    let forwarded = match event {
        InputEvent::Cursor(cursor) => {
            let inside = input::hit_test::cursor_in_any_region(&state.hit_regions, &cursor);
            let was_inside = state.cursor_in_overlay;
            state.cursor_in_overlay = inside;
            if inside {
                state.web.input_tx.send(InputEvent::Cursor(cursor)).is_ok()
            } else if was_inside {
                let leave = InputEvent::Cursor(CursorInput {
                    event: CursorEvent::Leave,
                    client: InputPosition { x: -1, y: -1 },
                    window: cursor.window,
                });
                state.web.input_tx.send(leave).is_ok()
            } else {
                false
            }
        }
        // Keyboard events are forwarded only while the cursor is over
        // the overlay. This matches the standard browser model: the
        // cursor's position decides which surface "owns" keyboard
        // focus. Without this gate, every keystroke the user types
        // while gaming would be injected into the WebView2 page.
        InputEvent::Keyboard(kb) => {
            if state.cursor_in_overlay {
                state.web.input_tx.send(InputEvent::Keyboard(kb)).is_ok()
            } else {
                false
            }
        }
    };
    if forwarded {
        state.web.wake_input();
    }
}

/// Cleanly shut down the engine loop's resources and emit the terminal
/// `EngineEvent::Detached` event.
async fn teardown(mut state: LoopState, reason: DetachReason) -> Result<()> {
    // Drop the texture before closing IPC.
    let _ = ipc::clear_shared_handle(&mut state.conn, state.window_id).await;

    // Ask the WebView2 thread to exit its message loop, then wake it
    // out of GetMessageW so the shutdown signal is observed
    // immediately. Join the thread so D3D / WV2 / DComp objects fully
    // release before we return.
    let _ = state.web.shutdown_tx.send(());
    state.web.wake_shutdown();
    let join = state.web.join_handle;
    let _ = tokio::task::spawn_blocking(move || {
        let _ = join.join();
    })
    .await;

    let _ = state
        .engine_events_tx
        .send(EngineEvent::Detached { reason })
        .await;

    // Drop the hook guard last, on this task's executor: this releases
    // the WindowsHook in the OS, which causes the in-game DLL to
    // unload.
    drop(state.hook_guard);

    Ok(())
}
