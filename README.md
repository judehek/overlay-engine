# overlay-engine

Composited WebView2 overlay engine for Windows games.

`overlay-engine` wraps [`asdf-overlay`](https://github.com/judehek/asdf-overlay)
(DLL injection + render hook + IPC) and drives a WebView2 in composition mode
to render an HTML/JS UI onto the game's swap chain. It exposes a single Rust
API for embedding overlay UIs into running games, with hover-mode hit-testing
so the game stays interactive outside the overlay's clickable regions.

Designed for, and battle-tested against, Riot's anti-cheat (Vanguard, on
League of Legends) — uses `SetWindowsHookEx(WH_GETMESSAGE)` injection with a
signed DLL rather than `CreateRemoteThread`.

## What the engine owns

- **Injection lifecycle.** Pick a target process by PID, ship the signed
  `asdf-overlay` DLL into it via the safe `SetWindowsHookEx` path, hold the
  hook open until detach.
- **IPC.** A named-pipe channel to the in-game DLL.
- **Composition stack.** A WebView2 visual-hosting / composition controller
  whose output is captured into a shared D3D11 texture handed to the DLL. No
  separate transparent OS window — works in exclusive fullscreen.
- **Input routing.** Pointer events from the game flow to the WebView when
  the cursor is over a clickable region; otherwise the game keeps full
  control. Keyboard events follow the same gating, with `TranslateMessage`
  on the engine's STA pump synthesising `WM_CHAR` for the embedded
  Chromium.

## What it deliberately does not own

- The overlay UI itself. The consumer points the engine at a URL (or a
  local `file://` of a Tauri / Vite build); WebView2 handles the rest.
- Multi-window compositing. There is exactly one WebView2 surface per
  attached game window. Multiple "panels" (notifications, modals, ...) are
  expected to be DOM elements inside that single WebView2.

## Quick start

```rust,no_run
use overlay_engine::{OverlayEngine, OverlayConfig};

#[tokio::main]
async fn main() -> overlay_engine::Result<()> {
    let pid = overlay_engine::process::find_by_name("League of Legends.exe")
        .ok_or_else(|| {
            overlay_engine::Error::TargetNotFound("League of Legends.exe".into())
        })?;

    let config = OverlayConfig::builder()
        .dll_dir("./dlls")
        .url("https://my-overlay.app/")
        .build()?;

    let (engine, mut events) = OverlayEngine::attach(pid, config).await?;

    while let Some(event) = events.recv().await {
        println!("engine event: {event:?}");
    }

    engine.detach().await?;
    Ok(())
}
```

## Building

`overlay-engine` consumes `asdf-overlay-client` via a path dependency at
`../asdf-overlay/crates/client`. There is one supported layout:

2. **Standalone clone.** Check out
   [`judehek/asdf-overlay`](https://github.com/judehek/asdf-overlay) as a
   sibling directory:

   ```bash
   git clone https://github.com/judehek/overlay-engine.git
   git clone https://github.com/judehek/asdf-overlay.git
   cd overlay-engine
   cargo check
   ```

   Both repos must be at compatible commits — pin them in lockstep. We may
   publish `asdf-overlay-client` to crates.io later to avoid this; for now
   the path dep is intentional so we can iterate on the asdf API without
   release cuts.

## Windows Only

Windows 10+ x86_64 only. There is no plan to support other platforms — the
whole stack (DLL injection, DComp, Graphics.Capture, WebView2 composition)
is Windows-specific by construction.

## Architecture

For a deep dive into how the pieces fit together (host process,
asdf-overlay DLL, WebView2 STA thread, capture pump, hit-region routing),
see the rustdoc: `cargo doc --open`.

## License

MIT. See [LICENSE](LICENSE).
