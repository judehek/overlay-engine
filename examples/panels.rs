//! End-to-end demo of the high-level [`Overlay`] API.
//!
//! Walks through:
//!
//! * Configuring the asset server with a `static_dir` of panel HTML.
//! * Attaching to a target process by name.
//! * Creating two panels (a non-interactive notification at the top
//!   right, an interactive 60% panel in the middle).
//! * Round-tripping a message both ways.
//! * Reacting to `PanelRequestClose` from the panel iframe.
//!
//! Run with:
//!
//! ```text
//! cargo run --example panels --release -- "League of Legends.exe" ./panels-dist ./dlls
//! ```
//!
//! `./panels-dist` should contain `notifications.html` and
//! `replay.html`, each importing `@overlay-engine/client` and using
//! `host.postMessage` / `host.onMessage` / `host.requestClose`.

use std::env;
use std::time::Duration;

use overlay_engine::{Overlay, OverlayEvent, PanelOptions, Rect};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "usage: {} <process-name> <panels-dist-dir> <dll-dir>",
            args[0]
        );
        std::process::exit(2);
    }
    let process_name = &args[1];
    let panels_dir = &args[2];
    let dll_dir = &args[3];

    let pid = overlay_engine::process::find_by_name(process_name)
        .ok_or_else(|| anyhow::anyhow!("no running process named {process_name:?}"))?;
    println!("[example/panels] target pid={pid} ({process_name:?})");

    let overlay = Overlay::builder()
        .dll_dir(dll_dir)
        .static_dir(panels_dir)
        .build()
        .await?;
    println!("[example/panels] overlay built; attaching...");

    let mut events = overlay.attach(pid).await?;
    println!("[example/panels] attached; awaiting shell:ready...");

    let notif = overlay
        .create_panel(PanelOptions::new(
            "notifications",
            "/notifications.html",
            Rect {
                x: 0,
                y: 110,
                w: 300,
                h: 100,
            },
        ))
        .await?;
    let replay = overlay
        .create_panel(
            PanelOptions::new(
                "replay",
                "/replay.html",
                Rect {
                    x: 200,
                    y: 200,
                    w: 1280,
                    h: 720,
                },
            )
            .interactive()
            .z_index(10),
        )
        .await?;

    // Drive sample traffic on a side task so the main loop can react
    // to events without blocking.
    let demo_notif = notif.clone();
    let demo_replay = replay.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = demo_notif
            .post_message(&serde_json::json!({ "type": "show", "text": "hello from host" }))
            .await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let _ = demo_replay
            .post_message(&serde_json::json!({ "type": "play", "url": "/sample.mp4" }))
            .await;
    });

    while let Some(event) = events.recv().await {
        match event {
            OverlayEvent::ShellReady => println!("[example/panels] shell ready"),
            OverlayEvent::PanelLoaded { panel_id } => {
                println!("[example/panels] panel loaded: {panel_id}")
            }
            OverlayEvent::PanelMessage { panel_id, payload } => {
                println!("[example/panels] panel({panel_id}) -> host: {payload}")
            }
            OverlayEvent::PanelRequestClose { panel_id } => {
                println!("[example/panels] panel({panel_id}) requested close");
                if panel_id == "replay" {
                    let _ = replay.close().await;
                }
            }
            OverlayEvent::PanelError { panel_id, error } => {
                eprintln!("[example/panels] panel({panel_id}) error: {error}")
            }
            OverlayEvent::Engine(engine_event) => {
                println!("[example/panels] engine event: {engine_event:?}");
                if matches!(engine_event, overlay_engine::EngineEvent::Detached { .. }) {
                    break;
                }
            }
        }
    }

    println!("[example/panels] event stream closed; exiting");
    Ok(())
}
