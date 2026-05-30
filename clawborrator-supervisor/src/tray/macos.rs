// macOS tray pump. tray-icon's status item is an `NSStatusItem` whose
// menu is driven by AppKit's responder chain — that needs a live
// `NSApplication` on the main thread with its run loop pumped.
//
// This module creates the app as an *accessory* (menu-bar presence,
// no Dock icon) and drives it with a manual `nextEventMatchingMask:`
// pump rather than the blocking `NSApplication.run()`, so the same
// loop can also refresh the dynamic menu (connect status + the live
// session list) — the analogue of the Windows message loop.
//
// Threading:
//   main thread  — builds the tray + menu (tray-icon handles are
//                  !Send, main-thread-only) and runs the Cocoa pump,
//                  which rebuilds the menu as sessions change.
//   menu drainer — side thread on `MenuEvent::receiver()`.
//   tokio worker — owns the `run_daemon` future; on exit it flips the
//                  shared `stop` flag so the pump falls through.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread;

use anyhow::{anyhow, Context, Result};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode};
use tracing::info;
use tray_icon::TrayIconBuilder;

use crate::status::{TrayStatus, TrayStatusUpdater};
use crate::Cli;

use super::{build_menu, decode_icon, drain_menu_events, MenuState, TOOLTIP};

/// Run the daemon with a menu-bar tray UI. Blocks the main thread
/// until the operator picks Quit (or the daemon crashes / completes
/// on its own). Returns the daemon's last result.
pub fn run_with_tray(cli: Cli, log_path: PathBuf) -> Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow!("run_with_tray must be called on the main thread"))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (status_updater, status_rx) = TrayStatusUpdater::channel();
    let cfg_for_dash = crate::load_or_init_config().context("loading config for tray dashboard URL")?;
    let hub_url = crate::effective_hub_url(&cli, &cfg_for_dash);

    // The session manager is shared three ways: the daemon future
    // populates it, the tray pump reads it to render the menu, and the
    // menu-event drainer ends sessions through it.
    let (mgr, restart_rx) = crate::new_session_manager();

    // Shared stop flag — the daemon thread sets it on exit so a daemon
    // crash / clean finish tears the tray down too.
    let stop = Arc::new(AtomicBool::new(false));

    let daemon_status = status_updater.clone();
    let daemon_stop = stop.clone();
    let daemon_mgr = mgr.clone();
    let daemon_handle = thread::spawn(move || -> Result<()> {
        let res = runtime.block_on(async move {
            let mut shutdown_rx = shutdown_rx;
            tokio::select! {
                res = crate::run_daemon(cli, daemon_status, daemon_mgr, restart_rx) => res,
                _   = shutdown_rx.changed() => {
                    info!("tray Quit received; shutting down daemon");
                    Ok(())
                }
            }
        });
        daemon_stop.store(true, Ordering::SeqCst);
        res
    });

    // Accessory app: menu-bar presence only, no Dock icon.
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();

    // Initial menu — empty session list; the pump fills it in as the
    // daemon registers sessions.
    let menu = build_menu("starting…", &[])?;
    let icon = decode_icon().context("decoding embedded tray icon")?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(TOOLTIP)
        .with_icon(icon)
        .build()
        .map_err(|e| anyhow!("creating tray icon: {e}"))?;

    thread::spawn({
        let log_path = log_path.clone();
        let mgr = mgr.clone();
        move || drain_menu_events(hub_url, log_path, shutdown_tx, mgr)
    });

    let mut menu_state = MenuState::new(tray, mgr);
    run_event_pump(&app, &mut menu_state, &status_rx, &stop);

    match daemon_handle.join() {
        Ok(res) => res,
        Err(_)  => Err(anyhow!("daemon thread panicked")),
    }
}

/// Cocoa event pump. Each iteration blocks up to 250ms for events,
/// drains them, then refreshes the dynamic menu — bounding both menu
/// freshness and shutdown latency to ~250ms.
fn run_event_pump(
    app:       &NSApplication,
    menu:      &mut MenuState,
    status_rx: &Receiver<TrayStatus>,
    stop:      &AtomicBool,
) {
    while !stop.load(Ordering::SeqCst) {
        let deadline = NSDate::dateWithTimeIntervalSinceNow(0.25);
        loop {
            let event = unsafe {
                app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&deadline),
                    NSDefaultRunLoopMode,
                    true,
                )
            };
            match event {
                Some(ev) => app.sendEvent(&ev),
                None => break,
            }
        }
        menu.refresh(status_rx);
    }
}
