//! Cross-platform status pipe between the daemon's WS reader and the
//! Windows tray UI.
//!
//! On non-Windows builds we still construct a noop updater so the
//! daemon code path doesn't have to cfg-gate every call site; on
//! Windows the tray spawns a watcher thread that consumes events
//! from the channel and updates the tray's tooltip + the disabled
//! header menu item to surface auth failures and connect state.

use std::sync::mpsc::{channel, Receiver, Sender};

#[derive(Debug, Clone)]
pub enum TrayStatus {
    /// About to open the WS or backing off between attempts.
    Connecting,
    /// HelloAck received; daemon is registered with the hub.
    Connected,
    /// Hub returned `{t:'error', code:'auth_failed'}` — the cached
    /// token is invalid/expired/revoked. Reconnects keep firing but
    /// will fail identically until the operator runs `login` and
    /// restarts the daemon (the in-memory token doesn't refresh from
    /// disk).
    AuthFailed,
}

impl TrayStatus {
    pub fn label(&self) -> &'static str {
        match self {
            TrayStatus::Connecting => "connecting…",
            TrayStatus::Connected  => "connected",
            TrayStatus::AuthFailed => "AUTH FAILED — run `login`",
        }
    }
    pub fn tooltip(&self) -> String {
        format!("clawborrator-supervisor — {}", self.label())
    }
}

/// Send-side handle. Cloneable; the daemon thread can pass clones
/// through nested call sites without thinking about lifetimes.
#[derive(Clone)]
pub struct TrayStatusUpdater {
    tx: Option<Sender<TrayStatus>>,
}

impl TrayStatusUpdater {
    /// No-op updater for builds without a tray (CLI subcommands,
    /// non-Windows). All `set` calls drop on the floor. Marked
    /// dead-code-allow because the Windows tray builds use
    /// `channel()` exclusively; only non-Windows main wires this in.
    #[allow(dead_code)]
    pub fn noop() -> Self { Self { tx: None } }

    pub fn channel() -> (Self, Receiver<TrayStatus>) {
        let (tx, rx) = channel();
        (Self { tx: Some(tx) }, rx)
    }

    pub fn set(&self, s: TrayStatus) {
        if let Some(tx) = &self.tx {
            // Receiver-dropped means the tray exited. No reason to
            // surface — the daemon thread is about to wind down too.
            let _ = tx.send(s);
        }
    }
}
