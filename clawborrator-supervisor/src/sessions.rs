// Session manager — owns the per-managed-session state on the
// daemon side. Each running CC has:
//
//   - a PTY master (write end for typing into CC, read end for the
//     vt100 reader to consume)
//   - a child process handle (so we can kill it on `session.kill`)
//   - a vt100 parser fed by every byte that comes off the PTY,
//     queryable for `session.screenshot`
//
// The map is keyed by the hub's session UUID (resolved from the
// sidecar file CC's MCP writes). Until the sidecar lands, sessions
// live under a temporary "pending key" so kill/screenshot can still
// reach them by routingName if needed. Phase-1 simplification: we
// only key by sessionId post-sidecar; pre-sidecar kills happen by
// returning the pending PTY child from session.create itself.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use portable_pty::{Child, MasterPty, PtySize};
use tokio::sync::mpsc;
use vt100::Parser;

use crate::parser_plugins::{PluginRegistry, watcher::{RestartRequest, WatcherHandle}};
use crate::token_usage::TokenUsageSamplerHandle;

pub const PTY_ROWS: u16 = 40;
pub const PTY_COLS: u16 = 120;

/// Shared write-half of a PTY master. Wrapped in Arc<Mutex<…>> so the
/// session (owns the lifetime) and any auxiliary task (auto-enter,
/// future input forwarders) can safely poke bytes into CC's stdin.
/// See ManagedSession's lifecycle commentary below for the why.
pub type SharedWriter = Arc<Mutex<Box<dyn std::io::Write + Send>>>;

// Session-scoped state.
//
// Lifecycle gotcha: `take_writer()` returns an OWNED writer that
// wraps an OS handle. When that writer drops, the handle closes,
// which on Windows ConPTY closes CC's stdin → CC reads EOF → CC
// exits. We can't let the auto-enter task own the writer because
// the task ends after 10s and would take the writer with it. So
// we wrap it in Arc<Mutex<...>> here; the auto-enter task gets a
// clone, the session keeps the original alive, and CC's stdin
// stays open for the full session lifetime.
//
// `_master` is retained for resize() in a future slice. `folder`
// + `routing_name` are retained for `session.restart`. `scratch_dir`
// + `channel_token_id` are retained for `session.destroy`.
pub struct ManagedSession {
    pub _session_id:      String,
    pub folder:           PathBuf,
    pub routing_name:     Option<String>,
    pub _master:          Box<dyn MasterPty + Send>,
    pub _writer:          SharedWriter,
    pub child:            Box<dyn Child + Send + Sync>,
    pub parser:           Arc<Mutex<Parser>>,
    /// Broadcast of raw PTY output bytes. The pty-reader thread
    /// publishes every chunk here so `ipc` attach clients can stream
    /// the live session into a terminal. No receivers just means
    /// nobody is currently attached.
    pub output_tx:        tokio::sync::broadcast::Sender<Vec<u8>>,
    pub scratch_dir:      PathBuf,
    pub channel_token_id: i64,
    /// Operator-supplied CLI flags forwarded to CC. Kept on the
    /// session so the parser-plugin restart path (--resume /
    /// --continue with no convo) can strip the offender and respawn.
    pub extra_flags:      Vec<String>,
    /// Whether the spawn opted into the auto-dismiss path. Forwarded
    /// to soft_restart_session on plugin-driven respawn so the new
    /// spawn keeps the same prompt-handling mode.
    pub auto_enter:       bool,
    /// Cancel handle for the per-session parser watcher. Dropped on
    /// destroy / replaced on soft-restart.
    pub watcher:          Option<WatcherHandle>,
    /// The `--session-id` UUID handed to this CC incarnation. CC names
    /// its transcript jsonl after it; the token-usage sampler uses it
    /// to locate the transcript. Fresh per spawn (create / soft-restart
    /// / respawn) — see token_usage.rs.
    pub cc_session_id:    String,
    /// Cancel handle for the per-session token-usage sampler. Cancel
    /// posts a final snapshot; dropped on destroy / kill, replaced on
    /// soft-restart.
    pub token_usage_sampler: Option<TokenUsageSamplerHandle>,
}

/// Plain metadata snapshot of one managed session, returned by
/// `SessionManager::list_sessions`.
pub struct SessionSummary {
    pub id:           String,
    pub folder:       PathBuf,
    pub routing_name: Option<String>,
    pub alive:        bool,
}

/// Trust-hardening guardrails for session.create. Loaded from the
/// daemon config file at startup and held on the SessionManager so
/// both the hub-WS `session.create` op and the local IPC `new`
/// command go through the same enforcement.
///
/// This is the daemon side of the desktop-sharing trust model
/// (hub_v1/docs/DESKTOP-SHARING.md). Without `allowed_roots` set, an
/// `operator` grantee can spawn CC in any path on the owner's
/// machine; with it set, the daemon refuses anything outside.
///
/// Default (no roots, no cap) preserves legacy single-owner behavior.
#[derive(Clone, Debug, Default)]
pub struct SharingPolicy {
    /// Absolute folder paths CC sessions may be spawned under. Empty
    /// means "no restriction". A session.create whose folder does
    /// not canonicalize under one of these roots is rejected.
    pub allowed_roots:           Vec<std::path::PathBuf>,
    /// Maximum number of concurrently-running CC sessions on this
    /// daemon. None means no cap.
    pub max_concurrent_sessions: Option<u32>,
}

pub struct SessionManager {
    inner:          Mutex<HashMap<String, Arc<Mutex<ManagedSession>>>>,
    /// Shared plugin registry — same set of plugins fires against
    /// every managed session's screen.
    pub registry:   Arc<PluginRegistry>,
    /// Channel handed to every watcher so a matched
    /// RestartWithoutFlag plugin can request a respawn. The receiver
    /// side lives in main.rs (handle_restart_requests).
    pub restart_tx: mpsc::UnboundedSender<RestartRequest>,
    /// Trust-hardening guardrails enforced in precheck_create.
    pub policy:     SharingPolicy,
}

impl SessionManager {
    pub fn new(
        registry:   Arc<PluginRegistry>,
        restart_tx: mpsc::UnboundedSender<RestartRequest>,
        policy:     SharingPolicy,
    ) -> Self {
        Self { inner: Default::default(), registry, restart_tx, policy }
    }

    /// Count of currently-alive (child not yet exited) managed
    /// sessions. Backs the concurrent-session cap check in
    /// precheck_create.
    pub fn count_alive(&self) -> usize {
        let map = self.inner.lock().unwrap();
        let mut n = 0;
        for entry in map.values() {
            if let Ok(mut s) = entry.lock() {
                if matches!(s.child.try_wait(), Ok(None)) { n += 1; }
            }
        }
        n
    }

    pub fn insert(&self, sid: String, sess: ManagedSession) {
        self.inner.lock().unwrap().insert(sid, Arc::new(Mutex::new(sess)));
    }

    pub fn get(&self, sid: &str) -> Result<Arc<Mutex<ManagedSession>>> {
        self.inner.lock().unwrap().get(sid)
            .cloned()
            .ok_or_else(|| anyhow!("no managed session with id {sid}"))
    }

    pub fn remove(&self, sid: &str) -> Option<Arc<Mutex<ManagedSession>>> {
        self.inner.lock().unwrap().remove(sid)
    }

    /// Return the session id of any currently-managed session
    /// whose folder matches `folder`. Used to refuse a second
    /// concurrent create in the same folder — two CC instances
    /// sharing the .claude/clawborrator/ sidecars (identity.json /
    /// runtime.json) race-write each other and produce undefined
    /// behavior.
    pub fn find_by_folder(&self, folder: &std::path::Path) -> Option<String> {
        let map = self.inner.lock().unwrap();
        for (sid, entry) in map.iter() {
            if let Ok(s) = entry.lock() {
                if s.folder == folder { return Some(sid.clone()); }
            }
        }
        None
    }

    /// Snapshot of all currently-managed session ids. Sent in the
    /// supervisor `hello` frame so the hub can reconcile its
    /// managed_by_machine_id state against the daemon's actual
    /// in-memory truth (after a daemon restart, this is `[]` and
    /// the hub clears managed_by for everything that was
    /// previously managed by this machine).
    pub fn list_session_ids(&self) -> Vec<String> {
        self.inner.lock().unwrap().keys().cloned().collect()
    }

    /// Metadata for every managed session — backs the local `sessions`
    /// CLI command. `alive` is a best-effort `try_wait` probe: false
    /// means the CC child has exited but the row hasn't been reaped.
    pub fn list_sessions(&self) -> Vec<SessionSummary> {
        let map = self.inner.lock().unwrap();
        let mut out = Vec::with_capacity(map.len());
        for (id, entry) in map.iter() {
            if let Ok(mut s) = entry.lock() {
                let alive = matches!(s.child.try_wait(), Ok(None));
                out.push(SessionSummary {
                    id:           id.clone(),
                    folder:       s.folder.clone(),
                    routing_name: s.routing_name.clone(),
                    alive,
                });
            }
        }
        out
    }

    /// Snapshot of every live ManagedSession's scratch_dir. Used by
    /// the orphan-scratch sweep at startup: anything under
    /// `~/.clawborrator/sessions/` that's NOT in this set is leftover
    /// from a prior daemon run and gets removed.
    pub fn list_scratch_dirs(&self) -> Vec<std::path::PathBuf> {
        let map = self.inner.lock().unwrap();
        let mut out = Vec::with_capacity(map.len());
        for entry in map.values() {
            if let Ok(s) = entry.lock() {
                out.push(s.scratch_dir.clone());
            }
        }
        out
    }
}

pub fn fresh_pty_size() -> PtySize {
    PtySize { rows: PTY_ROWS, cols: PTY_COLS, pixel_width: 0, pixel_height: 0 }
}
