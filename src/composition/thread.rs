//! Orchestrator for the WebView2 STA thread.
//!
//! [`spawn_web_thread`] is the only public entry point. It:
//!
//! 1. Sets up Tokio + std mpsc channels (frame producer, input consumer,
//!    shutdown).
//! 2. Spawns a real OS thread named `overlay-engine-webview` running
//!    [`web_thread_main`].
//! 3. Awaits the thread reporting its Win32 thread id (so the caller
//!    can `PostThreadMessageW` it to wake the message loop on input).
//! 4. Returns a [`WebThreadHandle`] that owns the channel ends and the
//!    `JoinHandle`; dropping the handle does not stop the thread, the
//!    caller must explicitly send on `shutdown_tx`.
//!
//! The thread itself owns the entire COM/WinRT pipeline:
//!
//! * RoInitialize STA + DispatcherQueueController
//! * D3D11 device + staging shared-KMT texture
//! * Hidden host HWND
//! * WebView2 environment + composition controller
//! * UI.Composition `Compositor` + visual tree (root + web_visual)
//! * Graphics.Capture framepool + session
//! * Win32 message loop dispatching FrameArrived + WebView2 callbacks +
//!   our custom `WM_APP_INPUT` input drain wakes.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use asdf_overlay_client::event::input::InputEvent;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_COLOR, ICoreWebView2, ICoreWebView2CompositionController, ICoreWebView2Controller,
    ICoreWebView2Controller2,
};
use windows::core::{Interface, HSTRING};
use windows::System::DispatcherQueueController;
use windows::UI::Composition::{Compositor, ContainerVisual};
use windows::Win32::Foundation::{LPARAM, RECT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DispatcherQueueOptions, DQTAT_COM_STA, DQTYPE_THREAD_CURRENT,
    RoInitialize, RO_INIT_SINGLETHREADED,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostQuitMessage, PostThreadMessageW, TranslateMessage, MSG,
    WM_APP, WM_QUIT,
};
use windows_numerics::Vector2;

use super::{capture, d3d, frame::FrameUpdate, host, webview};
use crate::input::dispatch_input;

/// Custom thread message used by the engine's tokio side to wake the
/// WebView2 STA thread after queueing an `InputEvent`.
///
/// Must be `>= WM_APP` and `< WM_APP + 0x4000` per Win32 rules.
pub(crate) const WM_APP_INPUT: u32 = WM_APP + 1;

/// Custom thread message used to wake the STA thread when the engine
/// has queued one or more outgoing `PostWebMessageAsString` calls.
pub(crate) const WM_APP_POST_MSG: u32 = WM_APP + 2;

/// Configuration the engine hands the WebView2 STA thread on spawn.
pub(crate) struct WebThreadParams {
    /// URL the WebView2 should navigate to immediately after creation.
    pub url: String,
    /// `(width, height)` of the WebView2 composition surface. The
    /// staging texture and capture framepool are created at this size.
    pub surface_size: (u32, u32),
    /// Scripts registered via `AddScriptToExecuteOnDocumentCreated`
    /// before the initial `Navigate` call. Each script runs at the
    /// start of every document the WebView2 loads, before the page's
    /// own scripts. Empty by default.
    pub document_created_scripts: Vec<String>,
    /// Path WebView2 should use as its user-data folder (cookies,
    /// local storage, cache). `None` lets WebView2 pick its
    /// `<exe>.WebView2/EBWebView/` default.
    pub user_data_folder: Option<String>,
}

/// Caller-side handle to a running WebView2 STA thread.
pub(crate) struct WebThreadHandle {
    /// Win32 thread id; pass to `PostThreadMessageW` to wake the loop.
    pub thread_id: u32,
    /// Receiver for `FrameUpdate`s the thread emits each time a new
    /// shared handle is ready.
    pub frame_rx: tokio_mpsc::UnboundedReceiver<FrameUpdate>,
    /// Push `InputEvent`s here; pair with `wake_input` to make the
    /// thread drain the queue on its next message-loop iteration.
    pub input_tx: Sender<InputEvent>,
    /// Push outgoing `PostWebMessageAsString` payloads here; pair with
    /// `wake_post_message` to make the thread drain the queue.
    pub post_message_tx: Sender<String>,
    /// Receiver for incoming `window.chrome.webview.postMessage(text)`
    /// calls from the page running inside the WebView2.
    pub web_message_rx: tokio_mpsc::UnboundedReceiver<String>,
    /// Send `()` to ask the thread to exit its message loop. Drop is
    /// not enough — the thread also has to be woken with an empty
    /// `PostThreadMessageW` call.
    pub shutdown_tx: Sender<()>,
    /// Joined on detach so the thread shuts down cleanly.
    pub join_handle: JoinHandle<()>,
}

impl WebThreadHandle {
    /// PostThreadMessage(WM_APP_INPUT) so the thread wakes from
    /// `GetMessageW` and drains the input queue on its next loop pass.
    pub fn wake_input(&self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_APP_INPUT, WPARAM(0), LPARAM(0));
        }
    }

    /// PostThreadMessage(WM_APP_POST_MSG) so the thread wakes and
    /// drains the outgoing-`PostWebMessage` queue.
    pub fn wake_post_message(&self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_APP_POST_MSG, WPARAM(0), LPARAM(0));
        }
    }

    /// Wake the thread with a no-op message so it observes the
    /// shutdown_tx signal. The shutdown channel is checked at the top of
    /// each loop iteration but the loop blocks in `GetMessageW` waiting
    /// for the next message to arrive otherwise.
    pub fn wake_shutdown(&self) {
        unsafe {
            // WM_NULL (0x0000) is fine here: it gets dispatched to nobody
            // and just unblocks GetMessageW so the shutdown_tx check at
            // the top of the loop runs.
            let _ = PostThreadMessageW(self.thread_id, 0, WPARAM(0), LPARAM(0));
        }
    }
}

/// Spawn the WebView2 composition thread and return a handle once the
/// thread has reported its Win32 thread id.
///
/// Errors if the thread fails to start or panics before publishing its
/// id; thread errors after that point surface as the next `frame_rx`
/// `recv()` returning `None`.
pub(crate) async fn spawn_web_thread(params: WebThreadParams) -> Result<WebThreadHandle> {
    let (frame_tx, frame_rx) = tokio_mpsc::unbounded_channel::<FrameUpdate>();
    let (input_tx, input_rx) = channel::<InputEvent>();
    let (post_message_tx, post_message_rx) = channel::<String>();
    let (web_message_tx, web_message_rx) = tokio_mpsc::unbounded_channel::<String>();
    let (tid_tx, tid_rx) = oneshot::channel::<u32>();
    let (shutdown_tx, shutdown_rx) = channel::<()>();

    let url = params.url;
    let surface_size = params.surface_size;
    let document_created_scripts = params.document_created_scripts;
    let user_data_folder = params.user_data_folder;

    let join_handle = thread::Builder::new()
        .name("overlay-engine-webview".into())
        .spawn(move || {
            if let Err(err) = web_thread_main(
                url,
                surface_size,
                document_created_scripts,
                user_data_folder,
                frame_tx,
                input_rx,
                post_message_rx,
                web_message_tx,
                tid_tx,
                shutdown_rx,
            ) {
                eprintln!("[overlay-engine] webview thread error: {err:?}");
            }
        })
        .context("spawn webview thread")?;

    let thread_id = tid_rx
        .await
        .context("webview thread exited before reporting its tid")?;

    Ok(WebThreadHandle {
        thread_id,
        frame_rx,
        input_tx,
        post_message_tx,
        web_message_rx,
        shutdown_tx,
        join_handle,
    })
}

/// Body of the WebView2 STA thread.
#[allow(clippy::too_many_arguments)]
fn web_thread_main(
    url: String,
    surface_size: (u32, u32),
    document_created_scripts: Vec<String>,
    user_data_folder: Option<String>,
    frame_tx: tokio_mpsc::UnboundedSender<FrameUpdate>,
    input_rx: Receiver<InputEvent>,
    post_message_rx: Receiver<String>,
    web_message_tx: tokio_mpsc::UnboundedSender<String>,
    tid_tx: oneshot::Sender<u32>,
    shutdown_rx: Receiver<()>,
) -> Result<()> {
    // 0. Publish our Win32 thread id so the caller can PostThreadMessage
    // us to wake the GetMessageW loop when input events are queued. Do
    // this before anything else so if later setup fails, the caller
    // isn't stuck awaiting the oneshot.
    let tid = unsafe { GetCurrentThreadId() };
    let _ = tid_tx.send(tid);

    // 1. WinRT / COM apartment. CoreWebView2 + UI.Composition +
    // Graphics.Capture all want to run on an STA with a DispatcherQueue
    // present.
    unsafe { RoInitialize(RO_INIT_SINGLETHREADED) }.context("RoInitialize failed")?;

    let dq_options = DispatcherQueueOptions {
        dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
        threadType: DQTYPE_THREAD_CURRENT,
        apartmentType: DQTAT_COM_STA,
    };
    let _dq_controller: DispatcherQueueController =
        unsafe { CreateDispatcherQueueController(dq_options) }
            .context("CreateDispatcherQueueController failed")?;

    // 2. Hidden parent window. CoreWebView2 composition mode still wants
    // an HWND to hook certain Windows messages to, even if nobody ever
    // sees it.
    let parent_hwnd = host::create_host_window()?;

    // 3. Our D3D11 device. Shared between the graphics-capture framepool
    // and the output staging texture so we can CopyResource between them.
    let (d3d_device, d3d_ctx) = d3d::create_d3d11_device()?;

    // 4. Staging shared-KMT texture. This is what we'll expose to the
    // asdf-overlay client side. Recreated on size change (not yet
    // implemented).
    let (width, height) = surface_size;
    let staging = d3d::StagingTexture::new(&d3d_device, width, height)?;

    // 5. WebView2 environment (blocking call; internally pumps messages).
    let environment = webview::create_webview2_environment(user_data_folder.as_deref())?;

    // 6. WebView2 composition controller.
    let composition_controller = webview::create_composition_controller(&environment, parent_hwnd)?;

    // 7. Compositor visual tree. `root` is what we hand to
    // Graphics.Capture; `web_visual` is where WebView2 renders. Both
    // children share the same Compositor so they can be composited
    // together by DComp.
    let compositor = Compositor::new().context("Compositor::new")?;
    let root: ContainerVisual = compositor
        .CreateContainerVisual()
        .context("CreateContainerVisual root")?;
    root.SetSize(Vector2 {
        X: width as f32,
        Y: height as f32,
    })?;
    root.SetIsVisible(true)?;

    let web_visual: ContainerVisual = compositor
        .CreateContainerVisual()
        .context("CreateContainerVisual web")?;
    web_visual.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
    root.Children()?.InsertAtTop(&web_visual)?;

    // Hand the visual to WebView2 as its render target. The webview2-com
    // bindings follow the windows-rs convention where COM putters are
    // exposed as `SetXxx` (not `put_Xxx`).
    let web_visual_as_iunknown: windows::core::IUnknown = web_visual.cast()?;
    unsafe { composition_controller.SetRootVisualTarget(&web_visual_as_iunknown) }
        .context("SetRootVisualTarget")?;

    // Size the webview controller's logical bounds so it knows to paint
    // at staging resolution.
    let controller: ICoreWebView2Controller = composition_controller.cast()?;
    unsafe {
        controller.SetBounds(RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        })
    }
    .context("SetBounds")?;
    unsafe { controller.SetIsVisible(true) }.context("SetIsVisible")?;

    // Make the WebView2 surface transparent. Without this the controller
    // paints a solid white background behind everything the page draws,
    // which would land on the game as an opaque rectangle the size of the
    // staging texture. `COREWEBVIEW2_COLOR { A: 0, .. }` opts the controller
    // into per-pixel alpha so only what the page draws is composited.
    let controller2: ICoreWebView2Controller2 = controller
        .cast()
        .context("cast ICoreWebView2Controller -> Controller2")?;
    unsafe {
        controller2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR {
            A: 0,
            R: 0,
            G: 0,
            B: 0,
        })
    }
    .context("SetDefaultBackgroundColor(transparent)")?;

    // 8. Graphics.Capture pipeline.
    let winrt_device = d3d::wrap_d3d11_as_winrt(&d3d_device)?;
    let capture_item = capture::capture_item_from_visual(&root)?;
    let live_capture = capture::start_capture(capture::CaptureSetup {
        winrt_device: &winrt_device,
        capture_item: &capture_item,
        d3d_ctx: d3d_ctx.clone(),
        staging_texture: staging.texture.clone(),
        staging_mutex: staging.keyed_mutex.clone(),
        staging_handle: staging.shared_handle,
        width: staging.width,
        height: staging.height,
        frame_tx: frame_tx.clone(),
    })?;

    // 9. Navigate. CoreWebView2::Navigate pulls the page and, when it's
    // ready to paint, the composition tree starts receiving visuals,
    // which drives FrameArrived events.
    let webview = unsafe { controller.CoreWebView2() }.context("CoreWebView2()")?;

    // Tighten WebView2 settings for the in-game overlay context.
    //
    // The default context menu opens as a real top-level Win32 popup,
    // which yanks foreground focus off the game (looks like an alt-tab
    // to the user) and can't be rendered correctly inside an exclusive-
    // fullscreen game in any case. Disable it; pages that want a
    // context menu can implement their own DOM-based one. Status bar
    // and "swipe nav gestures" are likewise irrelevant for a panel
    // overlay and would just waste pixels / generate spurious nav.
    let settings = unsafe { webview.Settings() }.context("Settings()")?;
    unsafe { settings.SetAreDefaultContextMenusEnabled(false) }
        .context("SetAreDefaultContextMenusEnabled(false)")?;
    unsafe { settings.SetIsStatusBarEnabled(false) }
        .context("SetIsStatusBarEnabled(false)")?;
    if let Ok(s4) = settings.cast::<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings4>()
    {
        // ICoreWebView2Settings4 -> SetIsGeneralAutofillEnabled / SetIsPasswordAutosaveEnabled.
        // Overlay panels don't have form fields where autofill helps,
        // and the autofill UI itself is another popup we don't want.
        let _ = unsafe { s4.SetIsGeneralAutofillEnabled(false) };
        let _ = unsafe { s4.SetIsPasswordAutosaveEnabled(false) };
    }
    if let Ok(s6) = settings.cast::<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings6>()
    {
        // Disable horizontal swipe-nav gestures (back/forward) -- they
        // can fire on touchpad input over the overlay and navigate the
        // shell page off-target.
        let _ = unsafe { s6.SetIsSwipeNavigationEnabled(false) };
    }

    webview::register_web_message_handler(&webview, web_message_tx)
        .context("register_web_message_handler")?;

    // Register any document-created scripts BEFORE Navigate so the
    // very first document already has them attached. Scripts are
    // processed in registration order — useful when a later script
    // depends on host helpers a former one set up.
    for script in &document_created_scripts {
        webview::add_document_created_script(&webview, script)
            .context("add_document_created_script")?;
    }

    let url_wide = HSTRING::from(url.as_str());
    unsafe { webview.Navigate(&url_wide) }.context("Navigate failed")?;

    // 10. Message loop. Pumps Win32 + WinRT callbacks (FrameArrived,
    // WebView2 navigation events, etc.) until the caller tells us to
    // shut down. Also drains queued input events on WM_APP_INPUT wakes.
    run_message_loop(
        shutdown_rx,
        input_rx,
        post_message_rx,
        &composition_controller,
        &webview,
        parent_hwnd,
        surface_size,
    )?;

    // Cleanup. Drop the live capture session, then the controllers; the
    // DispatcherQueueController drops when we return.
    live_capture.close();
    drop(staging);
    drop(controller);
    drop(composition_controller);
    drop(environment);

    Ok(())
}

/// Classic Win32 message loop. Terminates when `shutdown_rx` reports a
/// pending value (or its sender is dropped).
///
/// `WM_APP_INPUT` thread-level messages posted by the caller cause the
/// loop to drain `input_rx` and dispatch every queued event into the
/// WebView2 composition controller. `PostThreadMessageW` delivers
/// messages with a null HWND so we handle them inline before
/// `TranslateMessage` / `DispatchMessageW` (which would otherwise be a
/// no-op for them anyway).
#[allow(clippy::too_many_arguments)]
fn run_message_loop(
    shutdown_rx: Receiver<()>,
    input_rx: Receiver<InputEvent>,
    post_message_rx: Receiver<String>,
    controller: &ICoreWebView2CompositionController,
    webview: &ICoreWebView2,
    parent_hwnd: windows::Win32::Foundation::HWND,
    surface_size: (u32, u32),
) -> Result<()> {
    let mut msg = MSG::default();
    loop {
        if shutdown_rx.try_recv().is_ok() {
            unsafe { PostQuitMessage(0) };
        }
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
        if got <= 0 || msg.message == WM_QUIT {
            break;
        }
        if msg.message == WM_APP_INPUT && msg.hwnd.0.is_null() {
            while let Ok(input) = input_rx.try_recv() {
                if let Err(err) = dispatch_input(controller, parent_hwnd, &input, surface_size) {
                    eprintln!("[overlay-engine] dispatch_input failed: {err:?}");
                }
            }
            continue;
        }
        if msg.message == WM_APP_POST_MSG && msg.hwnd.0.is_null() {
            while let Ok(text) = post_message_rx.try_recv() {
                let payload = HSTRING::from(text.as_str());
                if let Err(err) = unsafe { webview.PostWebMessageAsString(&payload) } {
                    eprintln!("[overlay-engine] PostWebMessageAsString failed: {err:?}");
                }
            }
            continue;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
