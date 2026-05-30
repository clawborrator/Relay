// System-tray (menu-bar) integration. Gives the operator a passive
// "the supervisor is running" signal, a connect-state readout, a live
// list of the sessions this machine is managing, and per-session
// actions — for when the daemon was launched by the OS autostart
// facility with no console.
//
// The menu is DYNAMIC: a pump (one per GUI platform) rebuilds it
// whenever the connect status or the session set changes, so sessions
// spawned via the hub / PairWave appear without the operator doing
// anything. Each session is a submenu with:
//   - Attach in Terminal — opens a terminal running `attach <id>`
//     (the raw-mode PTY proxy can't run inside a dropdown).
//   - End session       — kills it.
//
// This module root holds everything identical across Windows and
// macOS: menu construction, the id → action mapping, the embedded
// icon, and the refresh bookkeeping. What differs is the event pump —
// a Win32 GetMessage loop vs. a Cocoa NSApplication pump — so each
// platform owns a submodule exposing its own `run_with_tray`.
//
// Linux has no submodule: its systemd-user service is headless, so
// main.rs runs the daemon directly there with a no-op status updater.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use tracing::{info, warn};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    Icon, TrayIcon,
};

use crate::sessions::{SessionManager, SessionSummary};
use crate::status::TrayStatus;

#[cfg(target_os = "windows")] mod windows;
#[cfg(target_os = "macos")]   mod macos;

#[cfg(target_os = "windows")] pub use windows::run_with_tray;
#[cfg(target_os = "macos")]   pub use macos::run_with_tray;

const TRAY_PNG: &[u8] = include_bytes!("../../assets/tray.png");
const TOOLTIP:  &str  = "clawborrator-supervisor";

// Static menu-item ids. Per-session items use the prefixes below with
// the session id appended, so the click handler can decode the target.
const ID_DASHBOARD:  &str = "cw:dashboard";
const ID_LOG:        &str = "cw:log";
const ID_QUIT:       &str = "cw:quit";
const ATTACH_PREFIX: &str = "cw:attach:";
const END_PREFIX:    &str = "cw:end:";

fn menu_err(e: tray_icon::menu::Error) -> anyhow::Error {
    anyhow!("menu error: {e}")
}

fn decode_icon() -> Result<Icon> {
    let img = image::load_from_memory_with_format(TRAY_PNG, image::ImageFormat::Png)
        .map_err(|e| anyhow!("decoding tray.png: {e}"))?
        .into_rgba8();
    let (w, h) = img.dimensions();
    Icon::from_rgba(img.into_raw(), w, h).map_err(|e| anyhow!("Icon::from_rgba: {e}"))
}

/// Human label for a session row: "<routingName> — <folder>", or just
/// the folder when there's no routing name.
fn session_label(s: &SessionSummary) -> String {
    let folder = s.folder.file_name().and_then(|n| n.to_str()).unwrap_or("?");
    let base = match &s.routing_name {
        Some(rn) if !rn.is_empty() => format!("{rn} — {folder}"),
        _ => folder.to_string(),
    };
    if s.alive { base } else { format!("{base} (exited)") }
}

/// Cheap change-detection signature of the session set. The menu is
/// rebuilt only when this differs from the last refresh, so a stable
/// set of sessions causes zero menu churn.
fn session_sig(sessions: &[SessionSummary]) -> String {
    let mut parts: Vec<String> =
        sessions.iter().map(|s| format!("{}:{}", s.id, s.alive)).collect();
    parts.sort();
    parts.join(",")
}

/// Build the full tray menu for the given status + session list.
fn build_menu(status_label: &str, sessions: &[SessionSummary]) -> Result<Menu> {
    let menu = Menu::new();

    // Disabled status header.
    menu.append(&MenuItem::new(
        format!("clawborrator-supervisor — {status_label}"),
        false,
        None,
    ))
    .map_err(menu_err)?;
    menu.append(&PredefinedMenuItem::separator()).map_err(menu_err)?;

    // One submenu per managed session.
    if sessions.is_empty() {
        menu.append(&MenuItem::new("No active sessions", false, None))
            .map_err(menu_err)?;
    } else {
        for s in sessions {
            let sub = Submenu::new(session_label(s), true);
            sub.append(&MenuItem::with_id(
                format!("{ATTACH_PREFIX}{}", s.id),
                "Attach in Terminal",
                s.alive,
                None,
            ))
            .map_err(menu_err)?;
            sub.append(&MenuItem::with_id(
                format!("{END_PREFIX}{}", s.id),
                "End session",
                true,
                None,
            ))
            .map_err(menu_err)?;
            menu.append(&sub).map_err(menu_err)?;
        }
    }
    menu.append(&PredefinedMenuItem::separator()).map_err(menu_err)?;

    menu.append(&MenuItem::with_id(ID_DASHBOARD, "Open dashboard", true, None))
        .map_err(menu_err)?;
    menu.append(&MenuItem::with_id(ID_LOG, "Open log folder", true, None))
        .map_err(menu_err)?;
    menu.append(&PredefinedMenuItem::separator()).map_err(menu_err)?;
    menu.append(&MenuItem::with_id(ID_QUIT, "Quit", true, None))
        .map_err(menu_err)?;

    Ok(menu)
}

/// Owns the tray icon plus the inputs the menu is rendered from. The
/// platform pump calls `refresh` every tick; the menu is rebuilt (via
/// `set_menu`) only when the status or the session set actually
/// changed. tray-icon's `TrayIcon` is `!Send` — `MenuState` therefore
/// lives entirely on the pump thread that built the tray.
struct MenuState {
    tray:   TrayIcon,
    mgr:    Arc<SessionManager>,
    status: TrayStatus,
    sig:    String,
}

impl MenuState {
    fn new(tray: TrayIcon, mgr: Arc<SessionManager>) -> Self {
        // `sig` starts as a sentinel no real session set produces, so
        // the first refresh always paints the dynamic menu.
        Self { tray, mgr, status: TrayStatus::Connecting, sig: "\u{0}".into() }
    }

    fn refresh(&mut self, status_rx: &Receiver<TrayStatus>) {
        let mut changed = false;
        while let Ok(s) = status_rx.try_recv() {
            self.status = s;
            changed = true;
        }
        let sessions = self.mgr.list_sessions();
        let sig = session_sig(&sessions);
        if sig != self.sig {
            self.sig = sig;
            changed = true;
        }
        if !changed {
            return;
        }
        match build_menu(self.status.label(), &sessions) {
            Ok(menu) => self.tray.set_menu(Some(Box::new(menu))),
            Err(e)   => warn!(?e, "rebuilding tray menu failed"),
        }
        if let Err(e) = self.tray.set_tooltip(Some(&self.status.tooltip())) {
            warn!(?e, "tray set_tooltip failed");
        }
    }
}

/// What a clicked menu item maps to. `Attach`/`End` carry the session id
/// decoded from the item's prefixed id.
enum MenuAction {
    OpenDashboard,
    OpenLog,
    Quit,
    Attach(String),
    End(String),
    Unknown,
}

fn classify(ev: &MenuEvent) -> MenuAction {
    let id: &str = ev.id.as_ref();
    if id == ID_DASHBOARD {
        MenuAction::OpenDashboard
    } else if id == ID_LOG {
        MenuAction::OpenLog
    } else if id == ID_QUIT {
        MenuAction::Quit
    } else if let Some(sid) = id.strip_prefix(ATTACH_PREFIX) {
        MenuAction::Attach(sid.to_string())
    } else if let Some(sid) = id.strip_prefix(END_PREFIX) {
        MenuAction::End(sid.to_string())
    } else {
        MenuAction::Unknown
    }
}

/// Side-thread loop over `MenuEvent::receiver()`. Quit flips the
/// shutdown watch (the daemon thread then tears the pump down). `End`
/// kills the session directly via the shared manager; `Attach` opens a
/// terminal running the `attach` subcommand.
fn drain_menu_events(
    hub_url:     String,
    log_path:    PathBuf,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    mgr:         Arc<SessionManager>,
) {
    for ev in MenuEvent::receiver() {
        match classify(&ev) {
            MenuAction::OpenDashboard => {
                if let Err(e) = webbrowser::open(&hub_url) {
                    warn!(?e, hub_url = %hub_url, "failed to open dashboard");
                }
            }
            // Daily-rolled log — open the folder so the operator can
            // pick the current day's file.
            MenuAction::OpenLog => open_path(log_path.parent().unwrap_or(&log_path)),
            MenuAction::Attach(sid) => open_attach_terminal(&sid),
            MenuAction::End(sid) => match crate::spawn::kill_session(&mgr, &sid) {
                Ok(())  => info!(session_id = %sid, "ended session from tray"),
                Err(e)  => warn!(error = %e, session_id = %sid, "tray End failed"),
            },
            MenuAction::Quit => {
                info!("tray Quit clicked");
                let _ = shutdown_tx.send(true);
                return;
            }
            MenuAction::Unknown => {}
        }
    }
}

/// Open a path in the user's default handler. Best-effort — failures
/// only warn.
#[cfg(target_os = "windows")]
fn open_path(path: &Path) {
    let path_str = match path.to_str() {
        Some(s) => s,
        None => { warn!(path = %path.display(), "log path is not valid UTF-8"); return; }
    };
    let r = std::process::Command::new("cmd")
        .args(["/c", "start", "", path_str])
        .spawn();
    if let Err(e) = r {
        warn!(?e, path = %path.display(), "failed to open path");
    }
}

#[cfg(target_os = "macos")]
fn open_path(path: &Path) {
    if let Err(e) = std::process::Command::new("open").arg(path).spawn() {
        warn!(?e, path = %path.display(), "failed to open path");
    }
}

/// Open a terminal window attached to `session_id`. The live attach is
/// a raw-mode PTY proxy — it can't run inside the menu — so this
/// launches a terminal running `clawborrator-supervisor attach <id>`.
#[cfg(target_os = "macos")]
fn open_attach_terminal(session_id: &str) {
    let exe = match std::env::current_exe() {
        Ok(e)  => e.display().to_string(),
        Err(e) => { warn!(?e, "could not resolve current exe for attach"); return; }
    };
    // AppleScript: open Terminal and run the attach command. The exe
    // path is single-quoted so a path with spaces still parses.
    let script = format!(
        "tell application \"Terminal\"\nactivate\ndo script \"'{exe}' attach {session_id}\"\nend tell"
    );
    if let Err(e) = std::process::Command::new("osascript").arg("-e").arg(&script).spawn() {
        warn!(?e, "failed to launch Terminal for attach");
    }
}

#[cfg(target_os = "windows")]
fn open_attach_terminal(session_id: &str) {
    use std::os::windows::process::CommandExt;

    // The daemon is a GUI-subsystem process with no console, so the
    // child needs its own console window — CREATE_NEW_CONSOLE. `cmd /k`
    // runs the attach there and keeps the window open afterwards (so a
    // detach / error message stays readable).
    //
    // Every part is passed as a SEPARATE `Command` arg, so std quotes
    // the (possibly space-containing) exe path exactly once. The old
    // code pre-formatted `"<exe>" attach <id>` into a single string and
    // routed it through `start`, which re-parsed the quotes and tried
    // to execute the quoted path verbatim ("... is not recognized").
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    let exe = match std::env::current_exe() {
        Ok(e)  => e,
        Err(e) => { warn!(?e, "could not resolve current exe for attach"); return; }
    };
    if let Err(e) = std::process::Command::new("cmd")
        .arg("/k")
        .arg(&exe)
        .arg("attach")
        .arg(session_id)
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
    {
        warn!(?e, "failed to launch console for attach");
    }
}
