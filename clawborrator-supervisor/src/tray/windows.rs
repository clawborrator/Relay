// Windows tray pump. `tray-icon` requires the icon to be created on
// the same thread that pumps Win32 messages — the main thread.
//
// Three threads, one runtime, one shutdown signal:
//
//   main thread (run_with_tray):
//     builds the tray + menu, then runs the Win32 message loop. The
//     loop also refreshes the dynamic menu (connect status + the live
//     session list) between dispatches — tray-icon's handles are
//     !Send, so all menu mutation has to happen here.
//
//   menu-event drainer (side thread):
//     loops on `MenuEvent::receiver()` (see super::drain_menu_events).
//
//   tokio worker thread:
//     owns the `run_daemon` future, `select!`'d against the shutdown
//     watch. On exit it posts WM_QUIT to the main thread so the tray
//     icon dies with the daemon.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::thread;

use anyhow::{anyhow, Context, Result};
use tracing::info;
use tray_icon::TrayIconBuilder;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, PostThreadMessageW,
    TranslateMessage, MSG, PM_REMOVE, QS_ALLINPUT, WM_QUIT,
};

use crate::status::{TrayStatus, TrayStatusUpdater};
use crate::Cli;

use super::{build_menu, decode_icon, drain_menu_events, MenuState, TOOLTIP};

/// Run the daemon with a system-tray UI. Blocks the main thread until
/// the operator picks Quit (or the daemon crashes / completes on its
/// own). Returns the daemon's last result.
pub fn run_with_tray(cli: Cli, log_path: PathBuf) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (status_updater, status_rx) = TrayStatusUpdater::channel();
    let cfg_for_dash = crate::load_or_init_config().context("loading config for tray dashboard URL")?;
    let hub_url = crate::effective_hub_url(&cli, &cfg_for_dash);

    // Capture the main thread id BEFORE spawning workers so the daemon
    // thread can post WM_QUIT here when it exits.
    let main_thread_id = unsafe { GetCurrentThreadId() };

    // The session manager is shared three ways: the daemon future
    // populates it, the tray pump reads it to render the menu, and the
    // menu-event drainer ends sessions through it.
    let (mgr, restart_rx) = crate::new_session_manager();

    let daemon_status = status_updater.clone();
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
        unsafe { PostThreadMessageW(main_thread_id, WM_QUIT, 0, 0); }
        res
    });

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

    let menu_state = MenuState::new(tray, mgr);
    run_message_loop_with_status(menu_state, status_rx);

    match daemon_handle.join() {
        Ok(res) => res,
        Err(_)  => Err(anyhow!("daemon thread panicked")),
    }
}

/// Win32 message pump that also refreshes the dynamic tray menu.
///
/// `MsgWaitForMultipleObjects` blocks up to 250ms for a message or the
/// timer; on wake-up we refresh the menu, then drain pending messages.
/// Loop exits on WM_QUIT.
fn run_message_loop_with_status(mut menu: MenuState, status_rx: Receiver<TrayStatus>) {
    const STATUS_POLL_MS: u32 = 250;
    unsafe {
        loop {
            let _ = MsgWaitForMultipleObjects(0, std::ptr::null(), 0, STATUS_POLL_MS, QS_ALLINPUT);

            menu.refresh(&status_rx);

            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT { return; }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
