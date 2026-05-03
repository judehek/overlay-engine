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
use webview2_com::{
    CreateCoreWebView2CompositionControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, Microsoft::Web::WebView2::Win32::{
        CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2CompositionController,
        ICoreWebView2Environment, ICoreWebView2Environment3,
    },
};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::HWND;

/// Block on environment creation; returns the environment once ready.
///
/// Uses an `mpsc::channel` to carry the result out of the completion
/// callback — the callback is stored in a `Box<dyn FnOnce + 'static>`,
/// so any local we capture by `&mut` would have to be `'static` too. A
/// `Sender` is `'static + Send` and has no such issue.
pub(crate) fn create_webview2_environment() -> Result<ICoreWebView2Environment> {
    let (tx, rx) = channel::<Result<ICoreWebView2Environment>>();
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(|handler| unsafe {
            CreateCoreWebView2EnvironmentWithOptions(
                PCWSTR::null(),
                PCWSTR::null(),
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
