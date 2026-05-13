//! Hidden Win32 host window for WebView2.
//!
//! `ICoreWebView2CompositionController` still requires a real `HWND` to
//! parent against, even though we never display anything in it. We create
//! the smallest possible OVERLAPPED window with a no-op wndproc; the
//! window class is registered on demand and reused if present.

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, PostQuitMessage, RegisterClassW, CW_USEDEFAULT,
    WM_CLOSE, WM_DESTROY, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

const HOST_CLASS: &str = "OverlayEngineWebViewHost\0";

/// Create a minimal hidden HWND for WebView2 to parent against.
///
/// Safe to call multiple times; `RegisterClassW` will simply fail with
/// ERROR_CLASS_ALREADY_EXISTS, which we ignore. The returned handle is
/// owned by the caller and should be destroyed on shutdown by exiting
/// the message loop (which posts `WM_DESTROY`).
pub(crate) fn create_host_window() -> Result<HWND> {
    let class_name: Vec<u16> = HOST_CLASS.encode_utf16().collect();
    let hinstance = unsafe {
        windows::Win32::System::LibraryLoader::GetModuleHandleW(PCWSTR::null())
    }
    .context("GetModuleHandleW")?;

    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance.into(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    // RegisterClassW returns 0 if the class already exists; that's fine
    // for our reuse case so we don't error here.
    let _atom = unsafe { RegisterClassW(&wc) };

    let hwnd = unsafe {
        CreateWindowExW(
            // WS_EX_TOOLWINDOW excludes the window from the taskbar and
            // the Alt+Tab switcher. Without it, the host HWND — which is
            // a real top-level window with a title — gets a taskbar
            // entry and a live-preview thumbnail on hover, even though
            // we never call ShowWindow on it. Some users hover/Alt-Tab
            // through it and see a ghost "overlay-engine shell" window
            // hanging over their desktop with whatever the panel surface
            // currently renders. A tool window is invisible to that
            // tasklist code path entirely.
            WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            windows::core::w!("overlay-engine-webview-host"),
            WS_OVERLAPPED,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
    }
    .context("CreateWindowExW")?;
    Ok(hwnd)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_CLOSE | WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}
