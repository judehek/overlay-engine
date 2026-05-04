//! WebView2-thread-side input dispatch.
//!
//! Translates an asdf-overlay `InputEvent` into the correct
//! `ICoreWebView2CompositionController::SendMouseInput` call (cursor)
//! or a `PostMessageW` to WebView2's internal Chromium widget HWND
//! (keyboard). Runs on the WebView2 STA thread inside the message loop
//! after a `WM_APP_INPUT` wake.
//!
//! WebView2 in visual-hosting / composition mode does **not** expose a
//! `SendKeyboardInput` API. The Chromium widget that owns the rendered
//! page is created as a descendant of the host HWND we hand to
//! `CreateCoreWebView2CompositionController` (parent_hwnd). Posting
//! `WM_KEYDOWN` / `WM_KEYUP` / `WM_CHAR` to that descendant is the
//! community-standard way to inject keystrokes; Chromium routes the
//! messages through its normal input pipeline as if focus were held by
//! WebView2.

use std::cell::Cell;

use anyhow::{Context, Result};
use asdf_overlay_client::event::input::{
    CursorAction, CursorEvent, CursorInput, CursorInputState, InputEvent, KeyInputState, Key,
    KeyboardInput, ScrollAxis,
};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2CompositionController, COREWEBVIEW2_MOUSE_EVENT_KIND,
    COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL, COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE,
    COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN, COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP,
    COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOWN, COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_UP,
    COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE, COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOWN,
    COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_UP, COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL,
    COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_DOWN, COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_UP,
    COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS,
};

// Bit values for COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS, mirroring Win32's
// MK_* flags. Defined locally rather than imported because the
// `webview2-com` re-export path varies between versions and inlining
// these keeps the dispatch module self-contained.
const VK_LEFT_BUTTON: i32 = 0x0001;
const VK_RIGHT_BUTTON: i32 = 0x0002;
const VK_MIDDLE_BUTTON: i32 = 0x0010;
const VK_X_BUTTON1: i32 = 0x0020;
const VK_X_BUTTON2: i32 = 0x0040;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetClassNameW, PostMessageW, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

thread_local! {
    /// Cached Chromium widget HWND under our `parent_hwnd`. Resolved
    /// lazily on the first keyboard event because WebView2 doesn't
    /// create the widget HWND until the first frame is composed; we
    /// can't do this work inline in `web_thread_main` startup.
    ///
    /// `Cell<isize>` rather than `Cell<HWND>` so we can use 0 as the
    /// "not resolved yet" sentinel without leaning on `Option` (HWND
    /// doesn't implement `Copy + Default` ergonomically across windows
    /// crate versions).
    static CHROMIUM_HWND: Cell<isize> = const { Cell::new(0) };

    /// Currently-held mouse buttons as a `COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS`
    /// bitmask. Updated on each Pressed/Released cursor action and
    /// reported on every subsequent SendMouseInput call.
    ///
    /// Required because Chromium treats `virtual_keys` like Win32's
    /// `WM_MOUSEMOVE` `wParam` (`MK_LBUTTON`, etc.): if a MOVE during
    /// an in-progress drag arrives with `virtual_keys=0`, Chromium
    /// synthesises an implicit button-up and the drag is cancelled --
    /// breaking HTML5 `<video>` timeline scrubbing, range slider drags
    /// and HTML5 drag-and-drop.
    static HELD_MOUSE_BUTTONS: Cell<i32> = const { Cell::new(0) };
}

/// Translate a single asdf-overlay input event and push it into WebView2.
///
/// `parent_hwnd` is the host HWND we passed to
/// `CreateCoreWebView2CompositionController`; we walk its descendants to
/// find Chromium's widget HWND for keyboard dispatch.
///
/// `surface_size` is the current `(width, height)` of the WebView2
/// composition surface in pixels; it's used to synthesize the
/// "cursor left the webview" point that replaces a real
/// `COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE` (see [`dispatch_cursor`]).
pub(crate) fn dispatch_input(
    controller: &ICoreWebView2CompositionController,
    parent_hwnd: HWND,
    event: &InputEvent,
    surface_size: (u32, u32),
) -> Result<()> {
    match event {
        InputEvent::Cursor(cursor) => dispatch_cursor(controller, cursor, surface_size),
        InputEvent::Keyboard(kb) => dispatch_keyboard(parent_hwnd, kb),
    }
}

/// Inject a keyboard input into WebView2 by posting the equivalent
/// Win32 message to its internal Chromium widget HWND.
///
/// We **only** post `WM_KEYDOWN` / `WM_KEYUP` (and the SYSKEY variants)
/// here; we deliberately drop the DLL's `KeyboardInput::Char` events.
///
/// Chromium widget HWNDs are descendants of `parent_hwnd` and are
/// owned by the WebView2 STA thread, so our `PostMessageW` lands in
/// that thread's queue and our own message pump in
/// [`crate::composition::thread::run_message_loop`] runs
/// `TranslateMessage` -> `DispatchMessageW`. `TranslateMessage`
/// synthesises a `WM_CHAR` from each `WM_KEYDOWN` based on the
/// **STA-thread-local** keyboard state, which we keep in sync by
/// also forwarding modifier WM_KEYDOWNs (shift/ctrl/alt/...) in
/// order. Posting our own `WM_CHAR` on top of that produced every
/// printable key twice (the user reported "type 'p' once, two 'p's
/// appear in a Google search box").
///
/// `Ime` events are dropped for now -- IME composition wants a richer
/// flow (WM_IME_STARTCOMPOSITION, WM_IME_COMPOSITION, etc.) that we
/// don't have a use case for yet.
fn dispatch_keyboard(parent_hwnd: HWND, kb: &KeyboardInput) -> Result<()> {
    let Some(target) = chromium_widget_hwnd(parent_hwnd) else {
        // No widget HWND under parent_hwnd yet (WebView2 still warming
        // up?) -- silently drop. Keystrokes typed in the first frame or
        // two of overlay creation are normally a non-issue.
        return Ok(());
    };

    match kb {
        KeyboardInput::Key { key, state } => {
            let (msg, lparam) = build_key_message(*key, *state);
            unsafe {
                let _ = PostMessageW(
                    Some(target),
                    msg,
                    WPARAM(key.code.get() as usize),
                    LPARAM(lparam),
                );
            }
        }
        // Intentionally a no-op. See function-level comment.
        KeyboardInput::Char(_) => {}
        KeyboardInput::Ime(_) => {}
    }
    Ok(())
}

/// Build (`message`, `lparam`) for a virtual-key press/release.
///
/// `lparam` encoding (per WM_KEYDOWN / WM_KEYUP docs):
/// * bits 0..15: repeat count (1)
/// * bits 16..23: scan code (left at 0; Chromium re-derives if needed)
/// * bit 24: extended-key flag
/// * bit 29: context code (1 if Alt is held; we approximate using the
///   sys-key message variant for VK_MENU itself)
/// * bit 30: previous key state (0 for the first WM_KEYDOWN of a press,
///   ignored by Chromium for our usage)
/// * bit 31: transition state (1 for WM_KEYUP)
fn build_key_message(key: Key, state: KeyInputState) -> (u32, isize) {
    const VK_MENU: u8 = 0x12;
    const VK_F10: u8 = 0x79;

    let is_sys = matches!(key.code.get(), VK_MENU | VK_F10);
    let mut lparam: u32 = 1; // repeat count
    if key.extended {
        lparam |= 1 << 24;
    }
    if is_sys {
        lparam |= 1 << 29;
    }
    let msg = match (state, is_sys) {
        (KeyInputState::Pressed, false) => WM_KEYDOWN,
        (KeyInputState::Pressed, true) => WM_SYSKEYDOWN,
        (KeyInputState::Released, false) => {
            lparam |= 1 << 31;
            WM_KEYUP
        }
        (KeyInputState::Released, true) => {
            lparam |= 1 << 31;
            WM_SYSKEYUP
        }
    };
    (msg, lparam as i32 as isize)
}

/// Locate WebView2's internal Chromium widget HWND under `parent_hwnd`.
/// Cached after the first successful resolution.
fn chromium_widget_hwnd(parent: HWND) -> Option<HWND> {
    let cached = CHROMIUM_HWND.with(|c| c.get());
    if cached != 0 {
        return Some(HWND(cached as *mut _));
    }

    struct Search {
        result: isize,
    }

    extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // Names Chromium uses for the input/render widget across recent
        // WebView2 versions. The render-widget HWND is the most
        // specific match (it's where the actual WM_KEY*/WM_CHAR routing
        // bottoms out); the WidgetWin variants are fallbacks for older
        // builds.
        const TARGETS: &[&str] = &[
            "Chrome_RenderWidgetHostHWND",
            "Chrome_WidgetWin_1",
            "Chrome_WidgetWin_0",
        ];

        let mut buf = [0u16; 256];
        let len = unsafe { GetClassNameW(hwnd, &mut buf) };
        if len <= 0 {
            return BOOL(1);
        }
        let class = String::from_utf16_lossy(&buf[..len as usize]);
        if TARGETS.iter().any(|t| t == &class.as_str()) {
            // Stop on first hit. EnumChildWindows iterates depth-first
            // top-down; "Chrome_RenderWidgetHostHWND" is deeper than
            // the WidgetWin parents, but we'd be happy with any of them
            // since posting WM_KEY*/WM_CHAR to a Chromium parent gets
            // forwarded to the right child internally. Take whatever
            // we find first.
            let search = unsafe { &mut *(lparam.0 as *mut Search) };
            search.result = hwnd.0 as isize;
            return BOOL(0);
        }
        BOOL(1)
    }

    let mut search = Search { result: 0 };
    unsafe {
        let _ = EnumChildWindows(
            Some(parent),
            Some(enum_proc),
            LPARAM(&mut search as *mut _ as isize),
        );
    }
    if search.result != 0 {
        CHROMIUM_HWND.with(|c| c.set(search.result));
        Some(HWND(search.result as *mut _))
    } else {
        None
    }
}

/// Map an asdf-overlay [`CursorInput`] onto
/// [`ICoreWebView2CompositionController::SendMouseInput`].
///
/// `client` coordinates from asdf-overlay are in overlay-surface-local
/// pixels; the overlay is anchored at (0,0) with the same size as the
/// WebView2 visual, so they're directly usable as webview pixel coords.
///
/// `virtual_keys` encodes modifier/button state. WebView2 mostly uses
/// it for chorded clicks (e.g. ctrl+click for new tab) and -- crucially
/// -- to validate "button is currently held" during MOVE events while a
/// drag is in progress. We track the held-mouse-button mask in
/// [`HELD_MOUSE_BUTTONS`] across calls and surface it here for every
/// event so HTML5 timeline drags and range-slider scrubs work.
/// Modifier keys (Ctrl/Shift/Alt) aren't plumbed yet -- asdf-overlay
/// doesn't include them on the cursor payload; we'd need to track
/// keyboard state locally to add them.
fn dispatch_cursor(
    controller: &ICoreWebView2CompositionController,
    cursor: &CursorInput,
    surface_size: (u32, u32),
) -> Result<()> {
    // WebView2 rejects `COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE` with a bogus
    // (-1, -1) point (it returns E_INVALIDARG). Instead, synthesize the
    // "cursor left the webview" signal as a MOVE to a point just past the
    // right/bottom edge of the webview bounds: the webview sees the cursor
    // leave its hit area and clears hover state, without the API rejecting
    // us. This matches how several WebView2 composition samples fake Leave.
    let point = if matches!(cursor.event, CursorEvent::Leave) {
        POINT {
            x: surface_size.0 as i32 + 1,
            y: surface_size.1 as i32 + 1,
        }
    } else {
        POINT {
            x: cursor.client.x,
            y: cursor.client.y,
        }
    };

    // Update the held-button mask BEFORE composing virtual_keys for
    // this event. Win32 convention is that the mask reflects the state
    // AFTER the event (WM_LBUTTONDOWN -> MK_LBUTTON set, WM_LBUTTONUP
    // -> MK_LBUTTON clear), and Chromium follows the same convention.
    if let CursorEvent::Action { state, action } = &cursor.event {
        let bit = match action {
            CursorAction::Left => VK_LEFT_BUTTON,
            CursorAction::Right => VK_RIGHT_BUTTON,
            CursorAction::Middle => VK_MIDDLE_BUTTON,
            CursorAction::Back => VK_X_BUTTON1,
            CursorAction::Forward => VK_X_BUTTON2,
        };
        HELD_MOUSE_BUTTONS.with(|m| {
            let mut held = m.get();
            if matches!(state, CursorInputState::Pressed { .. }) {
                held |= bit;
            } else {
                held &= !bit;
            }
            m.set(held);
        });
    }
    let virtual_keys =
        COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS(HELD_MOUSE_BUTTONS.with(|m| m.get()));

    let (kind, mouse_data) = match &cursor.event {
        CursorEvent::Move | CursorEvent::Enter | CursorEvent::Leave => {
            (COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE, 0u32)
        }
        CursorEvent::Action { state, action } => {
            let pressed = matches!(state, CursorInputState::Pressed { .. });
            let (kind, data) = match action {
                CursorAction::Left => (
                    if pressed {
                        COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN
                    } else {
                        COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP
                    },
                    0u32,
                ),
                CursorAction::Right => (
                    if pressed {
                        COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOWN
                    } else {
                        COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_UP
                    },
                    0u32,
                ),
                CursorAction::Middle => (
                    if pressed {
                        COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOWN
                    } else {
                        COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_UP
                    },
                    0u32,
                ),
                // XBUTTON1 / XBUTTON2. The high word of `mouseData`
                // carries which xbutton was pressed (1 or 2); Back ==
                // XBUTTON1, Forward == XBUTTON2.
                CursorAction::Back => (
                    if pressed {
                        COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_DOWN
                    } else {
                        COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_UP
                    },
                    0x0001_0000,
                ),
                CursorAction::Forward => (
                    if pressed {
                        COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_DOWN
                    } else {
                        COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_UP
                    },
                    0x0002_0000,
                ),
            };
            (kind, data)
        }
        CursorEvent::Scroll { axis, delta } => {
            // `mouseData` for wheel events is a signed 16-bit scroll
            // delta, conventionally a multiple of `WHEEL_DELTA` (120).
            //
            // Sign semantics differ between asdf-overlay and Windows: the
            // enum doc says "positive = down/right", matching Win32's
            // horizontal wheel but inverted vs. Win32's vertical wheel
            // (where positive = up). Flip the Y axis sign to match
            // WM_MOUSEWHEEL convention that WebView2 expects.
            let signed_delta: i32 = match axis {
                ScrollAxis::Y => -i32::from(*delta),
                ScrollAxis::X => i32::from(*delta),
            };
            let mouse_data = signed_delta as u32;
            let kind = match axis {
                ScrollAxis::Y => COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL,
                ScrollAxis::X => COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL,
            };
            (kind, mouse_data)
        }
    };

    let res = unsafe { controller.SendMouseInput(kind, virtual_keys, mouse_data, point) };
    if let Err(ref e) = res {
        eprintln!(
            "[overlay-engine][dispatch_cursor] kind={} data=0x{:x} point=({}, {}) event={:?} -> 0x{:08x}",
            mouse_kind_name(kind),
            mouse_data,
            point.x,
            point.y,
            cursor.event,
            e.code().0 as u32,
        );
    }
    res.context("SendMouseInput")?;
    Ok(())
}

/// Human-readable name for `COREWEBVIEW2_MOUSE_EVENT_KIND` values so
/// diagnostic prints don't just show opaque integers.
fn mouse_kind_name(kind: COREWEBVIEW2_MOUSE_EVENT_KIND) -> &'static str {
    match kind {
        COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE => "MOVE",
        COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE => "LEAVE",
        COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN => "LDOWN",
        COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP => "LUP",
        COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOWN => "RDOWN",
        COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_UP => "RUP",
        COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOWN => "MDOWN",
        COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_UP => "MUP",
        COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_DOWN => "XDOWN",
        COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_UP => "XUP",
        COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL => "WHEEL",
        COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL => "HWHEEL",
        _ => "?",
    }
}
