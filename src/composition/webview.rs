//! Async creation of `ICoreWebView2Environment` and
//! `ICoreWebView2CompositionController` via the
//! `webview2-com::CreateCoreWebView2*CompletedHandler` helpers.
//!
//! Both calls are kicked off as Win32 async COM operations whose results
//! are delivered to a completion callback on the same thread; we bridge
//! that into a blocking call by parking on a `mpsc::channel` until the
//! callback fires. The thread is the WebView2 STA thread, which is
//! already pumping messages, so the COM dispatch can run.

use std::sync::mpsc::channel;

use anyhow::{anyhow, Context, Result};
use tokio::sync::mpsc as tokio_mpsc;
use webview2_com::{
    take_pwstr, AddScriptToExecuteOnDocumentCreatedCompletedHandler,
    CreateCoreWebView2CompositionControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler,
    Microsoft::Web::WebView2::Win32::{
        CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2CompositionController,
        ICoreWebView2Environment, ICoreWebView2Environment3,
    },
    WebMessageReceivedEventHandler,
};
use windows::core::{Interface, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::HWND;

/// Block on environment creation; returns the environment once ready.
///
/// `user_data_folder` is the path WebView2 stores cookies / local
/// storage / cache under. Pass `None` to let WebView2 pick its
/// `<exe>.WebView2/EBWebView/` default; pass `Some(path)` to share a
/// cookie jar with another WebView2 instance (e.g. a Tauri main
/// window that's signing the user into Instagram so the in-game
/// overlay sees the same session).
///
/// Uses an `mpsc::channel` to carry the result out of the completion
/// callback — the callback is stored in a `Box<dyn FnOnce + 'static>`,
/// so any local we capture by `&mut` would have to be `'static` too. A
/// `Sender` is `'static + Send` and has no such issue.
pub(crate) fn create_webview2_environment(
    user_data_folder: Option<&str>,
) -> Result<ICoreWebView2Environment> {
    // Hold the wide-string buffer alive across the async call. PCWSTR
    // is just a borrowed pointer, so the HSTRING owning the bytes has
    // to outlive `wait_for_async_operation` — capture it by move into
    // the kickoff closure rather than computing the pointer outside.
    let user_data_wide = user_data_folder.map(HSTRING::from);
    let (tx, rx) = channel::<Result<ICoreWebView2Environment>>();
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            let user_data_ptr = user_data_wide
                .as_ref()
                .map(|s| PCWSTR::from_raw(s.as_ptr()))
                .unwrap_or_else(PCWSTR::null);
            CreateCoreWebView2EnvironmentWithOptions(
                PCWSTR::null(),
                user_data_ptr,
                None,
                &handler,
            )
            .map_err(Into::into)
        }),
        Box::new(move |hr, env| {
            let r = match env {
                Some(e) if hr.is_ok() => Ok(e),
                _ => Err(anyhow!("CoreWebView2Environment creation failed: {hr:?}")),
            };
            let _ = tx.send(r);
            Ok(())
        }),
    )
    .map_err(|e| anyhow!("wait_for_async_operation (env): {e:?}"))?;
    rx.recv()
        .map_err(|_| anyhow!("environment handler never fired"))?
}

/// Block on composition-controller creation; returns it once ready.
///
/// Caller is responsible for setting bounds, root visual target, and
/// visibility on the returned controller.
pub(crate) fn create_composition_controller(
    env: &ICoreWebView2Environment,
    parent_hwnd: HWND,
) -> Result<ICoreWebView2CompositionController> {
    let env3: ICoreWebView2Environment3 = env.cast().context("cast to Environment3")?;
    let (tx, rx) = channel::<Result<ICoreWebView2CompositionController>>();
    CreateCoreWebView2CompositionControllerCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            env3.CreateCoreWebView2CompositionController(parent_hwnd, &handler)
                .map_err(Into::into)
        }),
        Box::new(move |hr, controller| {
            let r = match controller {
                Some(c) if hr.is_ok() => Ok(c),
                _ => Err(anyhow!(
                    "CoreWebView2CompositionController creation failed: {hr:?}"
                )),
            };
            let _ = tx.send(r);
            Ok(())
        }),
    )
    .map_err(|e| anyhow!("wait_for_async_operation (composition): {e:?}"))?;
    rx.recv()
        .map_err(|_| anyhow!("composition controller handler never fired"))?
}

/// Subscribe to `window.chrome.webview.postMessage(text)` calls from the
/// page running inside `webview`. Each message is decoded to UTF-8 and
/// forwarded to `tx`.
///
/// Registered once on attach. We never `remove_WebMessageReceived`: the
/// webview is torn down with the engine, which drops its handlers along
/// with it.
pub(crate) fn register_web_message_handler(
    webview: &ICoreWebView2,
    tx: tokio_mpsc::UnboundedSender<String>,
) -> Result<()> {
    let handler = WebMessageReceivedEventHandler::create(Box::new(move |_sender, args| {
        let Some(args) = args else {
            return Ok(());
        };
        let mut raw: PWSTR = PWSTR::null();
        // `TryGetWebMessageAsString` returns a non-success HRESULT for
        // structured-clone payloads. We skip those.
        if unsafe { args.TryGetWebMessageAsString(&mut raw) }.is_err() || raw.is_null() {
            return Ok(());
        }
        // `take_pwstr` copies into a String and CoTaskMemFrees the
        // original buffer for us.
        let text = take_pwstr(raw);
        let _ = tx.send(text);
        Ok(())
    }));
    let mut token: i64 = 0;
    unsafe { webview.add_WebMessageReceived(&handler, &mut token) }
        .context("add_WebMessageReceived")?;
    Ok(())
}

/// Register a script that the WebView2 runs at "document created" time
/// for **every** navigation, before any of the page's own scripts run.
///
/// We use this to inject a host-controlled UI layer (e.g. a draggable
/// frame around the panel content) without relying on the embedded
/// shell page — top-frame navigation would replace the shell, but a
/// document-created script survives because it's hung off the WebView2
/// runtime, not the document.
///
/// Blocks the current STA thread until the underlying COM async
/// operation completes, same pattern as the other helpers in this
/// module. Safe to call multiple times to register multiple scripts.
pub(crate) fn add_document_created_script(
    webview: &ICoreWebView2,
    script: &str,
) -> Result<()> {
    // COM interface is refcounted — clone so the `wait_for_async_operation`
    // closure is `'static` (it cannot capture a `&ICoreWebView2`).
    let webview = webview.clone();
    let script_wide = HSTRING::from(script);
    let (tx, rx) = channel::<Result<()>>();
    AddScriptToExecuteOnDocumentCreatedCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            webview
                .AddScriptToExecuteOnDocumentCreated(PCWSTR::from_raw(script_wide.as_ptr()), &handler)
                .map_err(Into::into)
        }),
        Box::new(move |hr, _id| {
            let r = if hr.is_ok() {
                Ok(())
            } else {
                Err(anyhow!(
                    "AddScriptToExecuteOnDocumentCreated failed: {hr:?}"
                ))
            };
            let _ = tx.send(r);
            Ok(())
        }),
    )
    .map_err(|e| anyhow!("wait_for_async_operation (add script): {e:?}"))?;
    rx.recv()
        .map_err(|_| anyhow!("add-script handler never fired"))?
}
