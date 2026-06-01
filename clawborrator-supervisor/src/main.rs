// clawborrator-supervisor — Step 1 (handshake-only) + OAuth login.
//
// Connects to the hub's /supervisor WebSocket, sends a `hello` frame
// identifying this machine, then keeps the connection alive with
// 30s pings. No command handling yet — that's Step 2+. Reconnects
// with exponential backoff on disconnect so the daemon survives
// transient network blips and hub restarts.
//
// Auth: first run walks the SPA OAuth + PKCE flow (browser-based)
// to mint a `cw_app_…` Bearer token, then persists it to the local
// config. Subsequent runs reuse the cached token until you nuke
// `~/.clawborrator/desktop_v1.json`. CLAWBORRATOR_PAT env var
// overrides the cache for ad-hoc testing.
//
// Identity: the daemon assigns itself a stable machine_id stored
// in the same config file. Hostname alone isn't unique (people
// rename machines, dual-boot, VMs); the install nonce makes it
// durable across renames while staying stable across daemon
// restarts.

// Release Windows builds: link as the GUI subsystem so Task Scheduler
// (and double-clicks) don't pop a console window. Debug builds keep
// the console subsystem so `cargo run` still works ergonomically.
// Subcommands re-attach to the parent shell's console at runtime via
// AttachConsole — see `attach_parent_console_if_any`.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod auth;
mod autostart;
mod ipc;
mod logging;
mod oauth;
mod parser_plugins;
mod sessions;
mod spawn;
mod status;
mod token_usage;
#[cfg(any(target_os = "windows", target_os = "macos"))] mod tray;
#[cfg(target_os = "windows")] mod gui;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::time::{sleep, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};
use url::Url;

use crate::sessions::{SessionManager, SharingPolicy};
use crate::spawn::{create_session, destroy_session, input_session, kill_session, restart_session, respawn_preserving_id_session, screenshot_session, soft_restart_session, sweep_orphan_scratch_dirs, CreateArgs};
use crate::status::{TrayStatus, TrayStatusUpdater};

const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
// Note: DAEMON_VERSION is sourced from Cargo.toml, so a version bump
// in Cargo.toml flows automatically to the hello frame + --version.
const DEFAULT_HUB_URL: &str = "https://next.clawborrator.com";
// Where `login` pairs by default. The hub URL itself is learned FROM
// shadows during pairing, not configured here.
const DEFAULT_SHADOWS_URL: &str = "https://shadows-app.fly.dev";
const PING_INTERVAL: Duration = Duration::from_secs(30);
const RECONNECT_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);
// If no frame arrives from the hub for this long, treat the WS as a
// dead connection and bail so run_with_reconnect takes over. The hub
// pings on its own ~30s cadence, so a healthy link always refreshes
// well inside this window; it only trips on a silently-dropped
// connection (blackhole outage, laptop sleep, NAT eviction), which a
// TCP socket does not surface as an error for many minutes on its own
// (Linux tcp_retries2 holds an established socket ~13-30 min).
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(90);
// Cap on the WS connect + handshake so a reconnect attempt against a
// still-degraded network cannot hang the reconnect loop indefinitely.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Parser, Debug)]
#[command(author, version, about = "clawborrator desktop supervisor daemon")]
pub(crate) struct Cli {
    /// Hub base URL. Resolution order (first match wins):
    ///   1. --hub-url <url>            (this flag)
    ///   2. CLAWBORRATOR_HUB_URL env var
    ///   3. cfg.hub_url cached at last successful `login`
    ///   4. https://next.clawborrator.com (built-in default)
    ///
    /// Once `login` succeeds against a given URL, that URL is cached
    /// in ~/.clawborrator/desktop_v1.json. The Task-Scheduler entry
    /// installed by `install-task` runs the binary with no arguments
    /// at logon, so multi-hub operators rely on this cache rather
    /// than baking the URL into the autostart command line.
    #[arg(long, env = "CLAWBORRATOR_HUB_URL")]
    hub_url: Option<String>,

    /// shadows app base URL to pair against for `login`. Resolution:
    ///   1. --shadows-url <url>
    ///   2. CLAWBORRATOR_SHADOWS_URL env var
    ///   3. cfg.shadows_url cached at last successful `login`
    ///   4. https://shadows-app.fly.dev (built-in default)
    /// The hub URL is learned FROM shadows during pairing.
    #[arg(long, env = "CLAWBORRATOR_SHADOWS_URL")]
    shadows_url: Option<String>,

    /// Bearer token (`cw_pat_*` or `cw_app_*`). Read from
    /// CLAWBORRATOR_PAT env var if not provided. OAuth-driven mint
    /// flow is a follow-on.
    #[arg(long, env = "CLAWBORRATOR_PAT")]
    pat: Option<String>,

    /// Override the machine_id (otherwise read/generated from the
    /// config file at `~/.clawborrator/desktop_v1.json`).
    #[arg(long, env = "CLAWBORRATOR_MACHINE_ID")]
    machine_id: Option<String>,

    /// Internal: set on the installed Task-Scheduler entry so the daemon
    /// runs headless (tray only) and never opens the first-run setup
    /// window. A bare interactive launch (no flag) shows the wizard when
    /// this machine isn't paired yet.
    #[arg(long, hide = true)]
    background: bool,

    /// No subcommand = run the daemon (default). Subcommands manage
    /// the platform's autostart entry so the daemon launches at
    /// user logon.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Pair this machine against the shadows app (`--shadows-url`),
    /// mint a hub token for your shadow principal, and cache it. Print
    /// a code; approve it in the shadows web UI. Required before the
    /// daemon can start. No-op if a valid token is already cached
    /// unless `--force` is passed.
    Login {
        /// Re-pair even if a valid token is already cached.
        #[arg(long)]
        force: bool,
    },
    /// Revoke the cached app token server-side (best-effort) and
    /// clear it from the local config. The machine_id is preserved
    /// so a subsequent `login` re-uses this machine's identity.
    Logout,
    /// Register an autostart entry that launches this binary at
    /// user logon. On Windows that's a per-user Task Scheduler
    /// entry — no admin elevation needed. Requires a cached token
    /// (run `login` first).
    InstallTask,
    /// Remove the autostart entry. Idempotent.
    UninstallTask,
    /// Show whether the autostart entry is currently registered.
    TaskStatus,
    /// Check the host for the supervisor's runtime prerequisites:
    /// the `claude` CLI on PATH (Claude Code), and `npm`/`npx` on
    /// PATH (used by Claude Code to launch the clawborrator-mcp
    /// server). Reports each as found / missing + the install command
    /// to fix. Exits 0 if everything is present, 1 otherwise.
    /// Useful to run BEFORE creating your first session so you find
    /// out about missing tools without having to interpret the
    /// `502: spawning claude` error from orchard.
    PrereqCheck,
    /// List the Claude Code sessions this machine's daemon is
    /// currently managing. Talks to the running daemon over its local
    /// IPC socket.
    Sessions,
    /// Attach your terminal to a managed session for a live, two-way
    /// view — you see its output and can type into it. Detach with
    /// Ctrl-]; the session keeps running daemon-managed.
    Attach {
        /// Session id (from `sessions`).
        session_id: String,
    },
    /// End (kill) a managed session by id.
    End {
        /// Session id (from `sessions`).
        session_id: String,
    },
    /// Start a new managed Claude Code session in a folder.
    New {
        /// Project folder to run the session in.
        folder: String,
        /// Optional routing name for the session.
        #[arg(long)]
        routing_name: Option<String>,
        /// Extra CLI flag passed through to `claude` (repeatable).
        #[arg(long = "flag")]
        flags: Vec<String>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct Config {
    /// Stable per-install identifier. Generated once on first run;
    /// persists across daemon restarts. NOT keyed off hostname so
    /// that a hostname change doesn't orphan the registration.
    pub(crate) machine_id: String,
    /// `cw_app_…` Bearer token minted via the OAuth flow on first
    /// run. Optional only because the file is created BEFORE the
    /// flow runs (so we have a stable machine_id during the
    /// browser round-trip). Once the OAuth flow completes the
    /// token is written back via `save_config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) token: Option<String>,
    /// Hub URL the token connects to. LEARNED from shadows during
    /// pairing (the /device/token response carries it), not configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) hub_url: Option<String>,
    /// shadows app URL this install last paired against. Lets `login`
    /// short-circuit when re-pairing against the same shadows app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) shadows_url: Option<String>,
    // ─── Desktop-sharing guardrails ─────────────────────────────────
    // See hub_v1/docs/DESKTOP-SHARING.md. Both default to off so a
    // single-owner daemon behaves exactly as before.

    /// Absolute folder paths the daemon will let CC sessions spawn
    /// under. Empty (default) means no restriction. When set, a
    /// session.create whose folder is not under one of these roots is
    /// refused — the trust-hardening control for desktop-share
    /// `operator` grantees, who could otherwise spawn CC in any path
    /// on the owner's machine. Edit this list to scope what shared
    /// users can reach.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) allowed_roots: Vec<String>,
    /// Cap on concurrently-running CC sessions on this daemon. None
    /// or absent = no cap. Counts alive children (kill / end
    /// frees a slot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_concurrent_sessions: Option<u32>,
}

fn config_path() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .ok_or_else(|| anyhow!("could not resolve home dir"))?
        .join(".clawborrator");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {dir:?}"))?;
    Ok(dir.join("shadows-desktop.json"))
}

/// Load or generate the per-install config. First run mints a fresh
/// uuid for `machine_id` and writes the file with no token (the
/// OAuth flow fills that in later). Subsequent runs reuse both.
pub(crate) fn load_or_init_config() -> Result<Config> {
    let path = config_path()?;
    if path.exists() {
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {path:?}"))?;
        let cfg: Config = serde_json::from_str(&text).with_context(|| "parsing config")?;
        return Ok(cfg);
    }
    let cfg = Config {
        machine_id:              uuid::Uuid::new_v4().to_string(),
        token:                   None,
        hub_url:                 None,
        shadows_url:             None,
        allowed_roots:           Vec::new(),
        max_concurrent_sessions: None,
    };
    save_config(&cfg)?;
    info!(path = %path.display(), "wrote fresh config with new machine_id");
    Ok(cfg)
}

pub(crate) fn save_config(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    let json = serde_json::to_string_pretty(cfg)?;
    std::fs::write(&path, json).with_context(|| format!("writing {path:?}"))?;
    // Best-effort tighten perms on Unix; no-op on Windows.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Translate `https://host/...` → `wss://host/supervisor`. The hub
/// is HTTPS in production; localhost dev uses ws://. Either way we
/// just swap the scheme rather than asking the operator to type
/// two URLs.
fn ws_url_for_supervisor(hub_url: &str) -> Result<Url> {
    let mut u = Url::parse(hub_url).with_context(|| format!("invalid hub url: {hub_url}"))?;
    let scheme = match u.scheme() {
        "https" => "wss",
        "http"  => "ws",
        other => return Err(anyhow!("unsupported hub scheme: {other}")),
    };
    u.set_scheme(scheme).map_err(|_| anyhow!("set_scheme failed"))?;
    u.set_path("/supervisor");
    Ok(u)
}

#[derive(Serialize, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
enum OutFrame<'a> {
    /// First frame after WS handshake. Hub uses it to register +
    /// validate the daemon. `current_sessions` lets the hub
    /// reconcile its managed_by_machine_id state — on a fresh
    /// daemon start this is empty, so the hub clears managed_by
    /// for all of this machine's previously-managed sessions
    /// (their CCs are dead since the daemon was their parent).
    /// `version` lets the hub gracefully downgrade for older
    /// daemons.
    Hello {
        machine_id:       &'a str,
        daemon_version:   &'a str,
        hostname:         &'a str,
        capabilities:     &'a [&'a str],
        current_sessions: &'a [String],
    },
    Ping,
    Pong,
    /// RPC success — replies to a hub `cmd` frame by id.
    Ok {
        id:   &'a str,
        data: serde_json::Value,
    },
    /// RPC failure — replies to a hub `cmd` frame by id.
    Err {
        id:      &'a str,
        code:    &'a str,
        message: &'a str,
    },
}

#[derive(Deserialize, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
enum InFrame {
    /// Hub acknowledges the registration. Carries server-assigned
    /// fields the daemon may want to log (e.g. the user this PAT
    /// resolved to). Optional fields are flexible while the
    /// protocol is in flux.
    HelloAck {
        #[serde(default)] user_login: Option<String>,
        #[serde(default)] message:    Option<String>,
    },
    /// Hub-initiated ping; daemon responds with Pong.
    Ping,
    /// Reply to a daemon-initiated Ping.
    Pong,
    /// Hub-initiated RPC. Daemon executes `op` against `args` and
    /// responds with an `ok` (carrying `data`) or `err` frame
    /// keyed by the same `id`.
    Cmd {
        id:   String,
        op:   String,
        #[serde(default)] args: serde_json::Value,
    },
    /// Hub-side rejection. Today's known codes:
    ///   - "auth_failed" — bearer token rejected (post-login revoke,
    ///     desktop-delete, or stale post-rotation token). Reconnects
    ///     will keep failing until the operator re-authenticates;
    ///     surface to the tray so it's visible without log-diving.
    Error {
        code: String,
        #[serde(default)] message: Option<String>,
    },
    /// Catch-all so unknown frames don't kill the connection.
    #[serde(other)]
    Unknown,
}

/// Per-connection context threaded through every command handler.
/// Owns the session-manager Arc + the auth bits the daemon needs
/// to mint channel tokens / stamp managedBy on session.create.
struct DaemonCtx {
    pub hub_url:    String,
    pub pat:        String,
    pub machine_id: String,
    pub mgr:        Arc<SessionManager>,
    /// Send-side of the tray status channel. The daemon thread
    /// publishes connect-state transitions here; on Windows the
    /// tray's watcher thread consumes them. On non-Windows / CLI
    /// subcommand paths this is a noop, so call sites don't have
    /// to cfg-gate.
    pub tray:       TrayStatusUpdater,
}

async fn run_session(ctx: &DaemonCtx, ws_url: &Url, cfg: &Config) -> Result<()> {
    let host = hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());

    info!(url = %ws_url, "connecting to /supervisor");
    ctx.tray.set(TrayStatus::Connecting);

    let mut request: Request = ws_url.as_str().into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", ctx.pat).parse().context("invalid PAT for header")?,
    );

    let (mut ws, response) =
        match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request)).await {
            Ok(res) => res.with_context(|| "ws handshake failed")?,
            Err(_)  => anyhow::bail!("ws handshake timed out after {CONNECT_TIMEOUT:?}"),
        };
    info!(status = %response.status(), "ws connected");

    // Send the hello frame immediately; hub registers us against
    // (userId-from-token, machine_id) and responds with HelloAck.
    let current_sessions = ctx.mgr.list_session_ids();
    let hello = OutFrame::Hello {
        machine_id:       &cfg.machine_id,
        daemon_version:   DAEMON_VERSION,
        hostname:         &host,
        capabilities:     &["session.create", "session.kill", "session.destroy", "session.restart", "session.softrestart", "session.respawn_preserving_id", "session.screenshot", "session.input"],
        current_sessions: &current_sessions,
    };
    ws.send(Message::Text(serde_json::to_string(&hello)?)).await?;

    // One-shot orphan-scratch sweep after the hub's autoStart-respawn
    // batch should have completed. We spawn it 30s post-hello so:
    //   - all preserve_session_id=true rows have their fresh scratch
    //     dirs inserted into mgr (autoStart respawn fires from hub
    //     immediately on hello_ack)
    //   - any preserve_session_id=false rows are also in mgr (same
    //     batch — they go through session.create which inserts
    //     immediately)
    // Anything still under ~/.clawborrator/sessions/ that's NOT in
    // mgr's scratch_dir set is orphan from a prior daemon run.
    {
        let mgr = ctx.mgr.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            sweep_orphan_scratch_dirs(&mgr);
        });
    }

    let mut next_ping = Instant::now() + PING_INTERVAL;
    // Last time any frame arrived from the hub. The liveness watchdog
    // in the loop bails when this goes stale — see LIVENESS_TIMEOUT.
    let mut last_recv = Instant::now();

    loop {
        let now = Instant::now();
        // Wake for whichever comes first: the next keepalive ping, or
        // the liveness deadline.
        let wake_at = next_ping.min(last_recv + LIVENESS_TIMEOUT);
        let until_wake = if wake_at > now { wake_at - now } else { Duration::ZERO };
        tokio::select! {
            biased;
            _ = sleep(until_wake) => {
                let now = Instant::now();
                // Liveness check first. A healthy link refreshes
                // last_recv every ~30s via the hub's own pings, so a
                // stale last_recv means the connection is silently
                // dead — no socket error surfaces for many minutes on
                // a blackhole outage. Bail so run_with_reconnect takes
                // over instead of spinning in a dead session forever.
                if now.duration_since(last_recv) >= LIVENESS_TIMEOUT {
                    return Err(anyhow!(
                        "hub silent for {:?}; treating the connection as dead",
                        now.duration_since(last_recv),
                    ));
                }
                if now >= next_ping {
                    ws.send(Message::Text(serde_json::to_string(&OutFrame::Ping)?)).await?;
                    next_ping = now + PING_INTERVAL;
                }
            }
            msg = ws.next() => {
                let Some(msg) = msg else { return Err(anyhow!("ws stream ended")); };
                last_recv = Instant::now();
                if !handle_ws_message(ctx, &mut ws, msg?).await? {
                    return Ok(());
                }
            }
        }
    }
}

/// WS-frame demux. Pulled out of `run_session`'s `tokio::select!` arm
/// so the loop body stays flat and the per-variant handling lives in
/// one place. Returns `Ok(true)` to keep looping; `Ok(false)` for a
/// clean exit (hub-initiated Close); `Err` for protocol failures the
/// caller should propagate.
async fn handle_ws_message<S>(
    ctx: &DaemonCtx,
    ws:  &mut tokio_tungstenite::WebSocketStream<S>,
    msg: Message,
) -> Result<bool>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match msg {
        Message::Text(text)   => { handle_text(ctx, ws, &text).await?; Ok(true) }
        Message::Binary(_)    => { warn!("ignoring unexpected binary frame"); Ok(true) }
        Message::Ping(p)      => { ws.send(Message::Pong(p)).await?; Ok(true) }
        Message::Pong(_)      => Ok(true),
        Message::Close(frame) => { info!(?frame, "hub closed the connection"); Ok(false) }
        Message::Frame(_)     => Ok(true),
    }
}

async fn handle_text<S>(ctx: &DaemonCtx, ws: &mut tokio_tungstenite::WebSocketStream<S>, text: &str) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let parsed: InFrame = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            warn!(?e, raw = text, "failed to parse frame");
            return Ok(());
        }
    };
    match parsed {
        InFrame::HelloAck { user_login, message } => {
            info!(?user_login, ?message, "registered with hub");
            ctx.tray.set(TrayStatus::Connected);
        }
        InFrame::Ping => {
            ws.send(Message::Text(serde_json::to_string(&OutFrame::Pong)?)).await?;
        }
        InFrame::Pong => { /* application-level pong; nothing to do */ }
        InFrame::Cmd { id, op, args } => {
            handle_cmd(ctx, ws, &id, &op, args).await?;
        }
        InFrame::Error { code, message } => {
            if code == "auth_failed" {
                ctx.tray.set(TrayStatus::AuthFailed);
                error!(
                    code = %code, msg = ?message,
                    "hub rejected our token; re-run `clawborrator-supervisor login` and restart the daemon (in-memory token doesn't refresh from disk)"
                );
            } else {
                warn!(code = %code, msg = ?message, "hub returned error frame");
            }
        }
        InFrame::Unknown => {
            warn!(raw = text, "received unknown frame");
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct SessionCreateArgs {
    folder:        String,
    #[serde(default, rename = "routingName")]
    routing_name:  Option<String>,
    #[serde(default, rename = "extraFlags")]
    extra_flags:   Vec<String>,
    /// Hub forwards the operator's AUTO START / MANUAL START
    /// choice as a boolean. Default true (auto) — matches the
    /// SPA modal's default and lets older callers continue
    /// working without specifying.
    #[serde(default = "default_true", rename = "autoEnter")]
    auto_enter:    bool,
}

fn default_true() -> bool { true }

#[derive(Deserialize)]
struct SessionRefArgs {
    #[serde(rename = "sessionId")]
    session_id: String,
}

/// Restart-only args: session id + the operator's persisted
/// AUTO/MANUAL choice + the operator's persisted CLI flags. The hub
/// reads `auto_enter` and `extra_flags` off the sessions row and
/// forwards them here so a Restart preserves both the prompt-handling
/// mode AND the operator's --model / --add-dir / etc. choices, instead
/// of silently dropping them. Defaults (auto_enter=true, extra_flags
/// empty) keep older hubs / direct-CLI testing on the prior implicit
/// behavior.
#[derive(Deserialize)]
struct SessionRestartArgs {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(default = "default_true", rename = "autoEnter")]
    auto_enter: bool,
    #[serde(default, rename = "extraFlags")]
    extra_flags: Vec<String>,
}

/// Dispatch helpers detect "no managed session" errors (the
/// SessionManager's miss path) and map them to a specific
/// `session_not_found` code. Hub-side orphan-restart relies on
/// this code to decide whether to fall back to recreate.
fn classify_dispatch_err(default_code: &str, e: anyhow::Error) -> (String, String) {
    let msg = e.to_string();
    if msg.contains("no managed session") {
        ("session_not_found".into(), msg)
    } else {
        (default_code.into(), msg)
    }
}

async fn dispatch_session_create(ctx: &DaemonCtx, args: serde_json::Value) -> std::result::Result<serde_json::Value, (String, String)> {
    let parsed: SessionCreateArgs = serde_json::from_value(args).map_err(|e| ("bad_args".into(), e.to_string()))?;
    let folder = PathBuf::from(parsed.folder);
    let routing_name_owned = parsed.routing_name;
    let extra_flags = parsed.extra_flags;
    let auto_enter = parsed.auto_enter;
    let create_args = CreateArgs {
        hub_url:      &ctx.hub_url,
        pat:          &ctx.pat,
        machine_id:   &ctx.machine_id,
        folder,
        routing_name: routing_name_owned.as_deref(),
        extra_flags:  &extra_flags,
        auto_enter,
    };
    match create_session(&ctx.mgr, create_args).await {
        Ok(session_id) => Ok(serde_json::json!({ "sessionId": session_id })),
        Err(e)         => Err(("create_failed".into(), e.to_string())),
    }
}

fn dispatch_session_kill(ctx: &DaemonCtx, args: serde_json::Value) -> std::result::Result<serde_json::Value, (String, String)> {
    let parsed: SessionRefArgs = serde_json::from_value(args).map_err(|e| ("bad_args".into(), e.to_string()))?;
    match kill_session(&ctx.mgr, &parsed.session_id) {
        Ok(())  => Ok(serde_json::json!({ "ok": true })),
        Err(e)  => Err(classify_dispatch_err("kill_failed", e)),
    }
}

async fn dispatch_session_destroy(ctx: &DaemonCtx, args: serde_json::Value) -> std::result::Result<serde_json::Value, (String, String)> {
    let parsed: SessionRefArgs = serde_json::from_value(args).map_err(|e| ("bad_args".into(), e.to_string()))?;
    match destroy_session(&ctx.mgr, &ctx.hub_url, &ctx.pat, &parsed.session_id).await {
        Ok(())  => Ok(serde_json::json!({ "ok": true })),
        Err(e)  => Err(classify_dispatch_err("destroy_failed", e)),
    }
}

async fn dispatch_session_restart(ctx: &DaemonCtx, args: serde_json::Value) -> std::result::Result<serde_json::Value, (String, String)> {
    let parsed: SessionRestartArgs = serde_json::from_value(args).map_err(|e| ("bad_args".into(), e.to_string()))?;
    match restart_session(&ctx.mgr, &ctx.hub_url, &ctx.pat, &ctx.machine_id, &parsed.session_id, parsed.auto_enter, &parsed.extra_flags).await {
        Ok(new_id) => Ok(serde_json::json!({ "sessionId": new_id })),
        Err(e)     => Err(classify_dispatch_err("restart_failed", e)),
    }
}

/// Soft-restart: SIGKILL the child + respawn CC against the existing
/// scratch dir / identity.json / channel token, so the new MCP
/// registers with the SAME sessionId. Hub gates eligibility (the
/// session row must have preserve_session_id=true); daemon trusts
/// that gate and does the spawn unconditionally. Reuses
/// SessionRestartArgs (sessionId + autoEnter + extraFlags) since the
/// arg shape is identical.
async fn dispatch_session_softrestart(ctx: &DaemonCtx, args: serde_json::Value) -> std::result::Result<serde_json::Value, (String, String)> {
    let parsed: SessionRestartArgs = serde_json::from_value(args).map_err(|e| ("bad_args".into(), e.to_string()))?;
    match soft_restart_session(&ctx.mgr, &ctx.hub_url, &ctx.pat, &parsed.session_id, parsed.auto_enter, &parsed.extra_flags).await {
        Ok(()) => Ok(serde_json::json!({ "sessionId": parsed.session_id, "softRestarted": true })),
        Err(e) => Err(classify_dispatch_err("soft_restart_failed", e)),
    }
}

/// Args for session.respawn_preserving_id. Different from
/// SessionRestartArgs because the daemon's mgr is empty post-reboot
/// — we can't look up folder/routingName from in-memory state. Hub
/// passes them in.
#[derive(Deserialize)]
struct SessionRespawnPreservingArgs {
    #[serde(rename = "sessionId")]    session_id:   String,
    folder:       String,
    #[serde(default, rename = "routingName")] routing_name: Option<String>,
    #[serde(default = "default_true", rename = "autoEnter")]
    auto_enter:   bool,
    #[serde(default, rename = "extraFlags")]
    extra_flags:  Vec<String>,
}

/// Respawn-preserving-id: the autoStart-respawn entry point for
/// sessions opted into preserveSessionId. Reads identity.json from
/// the cwd, calls the hub's rotate-channel-token endpoint, mints a
/// fresh scratch dir, spawns CC. SessionId stays the same; channel
/// token rotates; old scratch dir orphans (cleaned by the startup
/// sweep that runs after this finishes for all autoStart rows).
async fn dispatch_session_respawn_preserving_id(ctx: &DaemonCtx, args: serde_json::Value) -> std::result::Result<serde_json::Value, (String, String)> {
    let parsed: SessionRespawnPreservingArgs = serde_json::from_value(args).map_err(|e| ("bad_args".into(), e.to_string()))?;
    let folder = std::path::PathBuf::from(parsed.folder);
    match respawn_preserving_id_session(
        &ctx.mgr, &ctx.hub_url, &ctx.pat, &parsed.session_id,
        folder, parsed.routing_name.as_deref(),
        parsed.auto_enter, &parsed.extra_flags,
    ).await {
        Ok(sid) => Ok(serde_json::json!({ "sessionId": sid, "preserved": true })),
        Err(e)  => Err(classify_dispatch_err("respawn_preserving_id_failed", e)),
    }
}

fn dispatch_session_screenshot(ctx: &DaemonCtx, args: serde_json::Value) -> std::result::Result<serde_json::Value, (String, String)> {
    let parsed: SessionRefArgs = serde_json::from_value(args).map_err(|e| ("bad_args".into(), e.to_string()))?;
    match screenshot_session(&ctx.mgr, &parsed.session_id) {
        Ok(v)  => Ok(v),
        Err(e) => Err(classify_dispatch_err("screenshot_failed", e)),
    }
}

#[derive(Deserialize)]
struct SessionInputArgs {
    #[serde(rename = "sessionId")]
    session_id: String,
    bytes:      String,
}

fn dispatch_session_input(ctx: &DaemonCtx, args: serde_json::Value) -> std::result::Result<serde_json::Value, (String, String)> {
    let parsed: SessionInputArgs = serde_json::from_value(args).map_err(|e| ("bad_args".into(), e.to_string()))?;
    match input_session(&ctx.mgr, &parsed.session_id, parsed.bytes.as_bytes()) {
        Ok(())  => Ok(serde_json::json!({ "ok": true, "wrote": parsed.bytes.len() })),
        Err(e)  => Err(classify_dispatch_err("input_failed", e)),
    }
}

// Dispatch a hub-initiated RPC. Each verb returns either Ok(data) →
// daemon emits `ok` frame, or Err((code, message)) → daemon emits
// `err`. session.restart is intentionally still unimplemented —
// requires retaining the create-args for the original session, which
// is its own slice. session.create / kill / screenshot are real.
async fn handle_cmd<S>(
    ctx: &DaemonCtx,
    ws:  &mut tokio_tungstenite::WebSocketStream<S>,
    id:  &str,
    op:  &str,
    args: serde_json::Value,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let result: std::result::Result<serde_json::Value, (String, String)> = match op {
        "session.create"     => dispatch_session_create(ctx, args).await,
        "session.kill"       => dispatch_session_kill(ctx, args),
        "session.destroy"    => dispatch_session_destroy(ctx, args).await,
        "session.restart"               => dispatch_session_restart(ctx, args).await,
        "session.softrestart"           => dispatch_session_softrestart(ctx, args).await,
        "session.respawn_preserving_id" => dispatch_session_respawn_preserving_id(ctx, args).await,
        "session.screenshot" => dispatch_session_screenshot(ctx, args),
        "session.input"      => dispatch_session_input(ctx, args),
        other                => Err(("unknown_op".into(), format!("unknown op: {other}"))),
    };
    match result {
        Ok(data) => ws.send(Message::Text(serde_json::to_string(&OutFrame::Ok { id, data })?)).await?,
        Err((code, msg)) => ws.send(Message::Text(serde_json::to_string(&OutFrame::Err {
            id, code: &code, message: &msg,
        })?)).await?,
    }
    Ok(())
}

async fn run_with_reconnect(ctx: DaemonCtx, ws_url: Url, cfg: Config) -> Result<()> {
    let mut backoff = RECONNECT_BACKOFF_INITIAL;
    loop {
        match run_session(&ctx, &ws_url, &cfg).await {
            Ok(()) => {
                info!("session ended cleanly; reconnecting in {:?}", RECONNECT_BACKOFF_INITIAL);
                backoff = RECONNECT_BACKOFF_INITIAL;
            }
            Err(e) => {
                error!(?e, "session error; reconnecting in {:?}", backoff);
            }
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }
}

/// Resolve the Bearer token to use. Priority:
///   1. `--pat` flag / CLAWBORRATOR_PAT env (always wins; ad-hoc)
///   2. cached token in `~/.clawborrator/desktop_v1.json` (verified
///      to match `--hub-url`)
///
/// No implicit OAuth fallback — the daemon can be launched from a
/// non-interactive context (Task Scheduler at logon, with no console
/// or browser available), where opening a browser would silently fail.
/// Resolve the effective hub URL via the documented precedence:
/// CLI flag → CLAWBORRATOR_HUB_URL env → cfg.hub_url cache → built-in default.
/// Clap collapses the first two into `cli.hub_url`; this helper handles the
/// remaining two so the daemon honors a cached hub when no flag/env is set.
pub(crate) fn effective_hub_url(cli: &Cli, cfg: &Config) -> String {
    if let Some(s) = cli.hub_url.as_deref() { return s.to_string(); }
    if let Some(s) = cfg.hub_url.as_deref() { return s.to_string(); }
    DEFAULT_HUB_URL.to_string()
}

/// Resolve the shadows app URL to pair against:
/// CLI flag → CLAWBORRATOR_SHADOWS_URL env → cfg.shadows_url cache → default.
pub(crate) fn effective_shadows_url(cli: &Cli, cfg: &Config) -> String {
    if let Some(s) = cli.shadows_url.as_deref() { return s.to_string(); }
    if let Some(s) = cfg.shadows_url.as_deref() { return s.to_string(); }
    DEFAULT_SHADOWS_URL.to_string()
}

/// Use the `login` subcommand to mint a token interactively.
fn resolve_token(cli: &Cli, cfg: &Config) -> Result<String> {
    if let Some(t) = &cli.pat {
        return Ok(t.clone());
    }
    let cached = cfg.token.as_deref().ok_or_else(|| anyhow!(
        "no cached app token — run `clawborrator-supervisor login` first to authenticate"
    ))?;
    let hub = effective_hub_url(cli, cfg);
    if cfg.hub_url.as_deref() != Some(hub.as_str()) {
        return Err(anyhow!(
            "cached token was minted against {:?}, not {hub} — run `clawborrator-supervisor login` against the new hub",
            cfg.hub_url,
        ));
    }
    Ok(cached.to_string())
}

/// Re-attach to the parent shell's console on Windows release builds
/// so `eprintln!` / subcommand output reach the user. The release
/// binary is `windows_subsystem = "windows"` (no console allocated by
/// default) which is great for Task Scheduler launches but breaks
/// `clawborrator-supervisor.exe install-task` invoked from PowerShell.
/// AttachConsole(ATTACH_PARENT_PROCESS) silently no-ops if the parent
/// has no console (e.g. Task Scheduler), so it's safe to always call.
#[cfg(all(target_os = "windows", not(debug_assertions)))]
fn attach_parent_console_if_any() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS); }
}

#[cfg(not(all(target_os = "windows", not(debug_assertions))))]
fn attach_parent_console_if_any() {}

async fn run_subcommand(cli: &Cli, cmd: Command) -> Result<()> {
    let provider = autostart::current();
    match cmd {
        Command::Login { force } => cmd_login(cli, force).await,
        Command::Logout          => cmd_logout(cli).await,
        Command::InstallTask     => install_task(provider),
        Command::UninstallTask   => uninstall_task(provider),
        Command::TaskStatus      => task_status(provider),
        Command::PrereqCheck     => prereq_check(),
        Command::Sessions        => cmd_sessions().await,
        Command::Attach { session_id }               => ipc::client_attach(session_id).await,
        Command::End { session_id }                  => cmd_end(session_id).await,
        Command::New { folder, routing_name, flags } => cmd_new(folder, routing_name, flags).await,
    }
}

/// `sessions` — print the daemon's live managed sessions.
async fn cmd_sessions() -> Result<()> {
    let sessions = ipc::client_list().await?;
    if sessions.is_empty() {
        eprintln!("No managed sessions on this machine.");
        return Ok(());
    }
    eprintln!("{:<38}  {:<5}  {:<20}  {}", "SESSION ID", "ALIVE", "ROUTING", "FOLDER");
    for s in sessions {
        eprintln!(
            "{:<38}  {:<5}  {:<20}  {}",
            s.id,
            if s.alive { "yes" } else { "no" },
            s.routing_name.as_deref().unwrap_or("-"),
            s.folder,
        );
    }
    Ok(())
}

/// `end <id>` — kill a managed session.
async fn cmd_end(session_id: String) -> Result<()> {
    ipc::client_end(session_id.clone()).await?;
    eprintln!("Ended session {session_id}.");
    Ok(())
}

/// `new <folder>` — spawn a new managed session.
async fn cmd_new(folder: String, routing_name: Option<String>, flags: Vec<String>) -> Result<()> {
    let id = ipc::client_new(folder, routing_name, flags).await?;
    eprintln!("Started session {id}.");
    eprintln!("Attach to it with:  clawborrator-supervisor attach {id}");
    Ok(())
}

fn prereq_check() -> Result<()> {
    // Search PATH for each tool. On Linux we ALSO check $HOME/.local/bin
    // explicitly since that's where the official Claude installer drops
    // the binary, and where ad-hoc npm installs land for users that
    // configure `prefix=~/.local`. spawn_cc + the install-task unit
    // template both prepend ~/.local/bin to PATH, so a prereq-check
    // that fails to find tools there would falsely report missing.
    let mut missing = Vec::new();
    let mut found = Vec::new();
    for (name, install_hint) in [
        ("claude", "curl -fsSL https://claude.ai/install.sh | bash"),
        ("npx",    "sudo apt install -y nodejs npm   # or dnf on fedora/rhel"),
        ("npm",    "sudo apt install -y nodejs npm"),
        ("node",   "sudo apt install -y nodejs"),
    ] {
        if let Some(path) = find_on_path(name) {
            found.push((name, path));
        } else {
            missing.push((name, install_hint));
        }
    }
    eprintln!("Runtime prerequisite check:");
    eprintln!();
    for (name, path) in &found {
        eprintln!("  [ok]      {name:<8}  {}", path.display());
    }
    for (name, hint) in &missing {
        eprintln!("  [missing] {name:<8}  install: {hint}");
    }
    eprintln!();
    if missing.is_empty() {
        eprintln!("All prereqs present. Session creation through orchard should work.");
        Ok(())
    } else {
        eprintln!("{} missing prereq(s). Install them, then re-run this check.", missing.len());
        eprintln!("(If you install npm-global tools to ~/.npm-global/bin/, also add that path to the supervisor's systemd unit Environment= line, OR install via sudo so they land in /usr/local/bin/.)");
        std::process::exit(1);
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    // Build the search list: $PATH entries, plus ~/.local/bin on
    // Linux/macOS as a fallback (some installers drop binaries there
    // even when ~/.local/bin isn't in PATH).
    let mut paths: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        let local_bin = PathBuf::from(home).join(".local").join("bin");
        if !paths.iter().any(|p| p == &local_bin) {
            paths.push(local_bin);
        }
    }
    // Try the bare name, plus .exe on Windows.
    let candidates: Vec<String> = if cfg!(target_os = "windows") {
        vec![format!("{name}.exe"), format!("{name}.cmd"), name.to_string()]
    } else {
        vec![name.to_string()]
    };
    for dir in &paths {
        for cand in &candidates {
            let full = dir.join(cand);
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

async fn cmd_login(cli: &Cli, force: bool) -> Result<()> {
    let mut cfg = load_or_init_config()?;
    let shadows = effective_shadows_url(cli, &cfg);
    match auth::login(&mut cfg, &shadows, force).await? {
        auth::LoginOutcome::AlreadyLoggedIn { login, hub_url } => {
            eprintln!("Already paired as {login} (hub {hub_url}).");
            eprintln!("Pass --force to re-pair.");
        }
        auth::LoginOutcome::Authenticated { login, hub_url } => {
            save_config(&cfg)?;
            match login {
                Some(l) => eprintln!("Paired as {l} (hub {hub_url})."),
                None    => eprintln!("Paired (couldn't confirm identity via /api/v1/me; hub {hub_url})."),
            }
            eprintln!("Run `install-task` next if you want the daemon to launch at logon.");
        }
    }
    Ok(())
}

async fn cmd_logout(_cli: &Cli) -> Result<()> {
    let mut cfg = load_or_init_config()?;
    match auth::logout(&mut cfg).await {
        auth::LogoutOutcome::Revoked         => eprintln!("Hub revoked the token."),
        auth::LogoutOutcome::NoCachedToken   => eprintln!("No cached token; nothing to revoke server-side."),
        auth::LogoutOutcome::RevokeFailed(e) => warn!(?e, "hub /auth/logout failed; clearing local cache anyway"),
    }
    save_config(&cfg)?;
    eprintln!("Local config cleared. machine_id preserved.");

    let provider = autostart::current();
    if let Ok(autostart::AutostartStatus::Installed { .. }) = provider.status() {
        eprintln!("Note: autostart is still installed — without a token the daemon will fail at next logon.");
        eprintln!("Run `uninstall-task` to remove it, or `login` to mint a fresh token.");
    }
    Ok(())
}

fn install_task(provider: &dyn autostart::AutostartProvider) -> Result<()> {
    let cfg = load_or_init_config()?;
    if cfg.token.is_none() {
        anyhow::bail!("no cached token — run `clawborrator-supervisor login` first; otherwise the autostart entry will fire with no credentials");
    }
    let exe = std::env::current_exe().context("resolving current exe path")?;
    eprintln!("Installing autostart via {} for: {}", provider.facility_name(), exe.display());
    eprintln!("Token cached for hub: {}", cfg.hub_url.as_deref().unwrap_or("<none>"));
    provider.install(&exe)?;
    eprintln!("Done. The supervisor will launch at your next logon.");
    Ok(())
}

fn uninstall_task(provider: &dyn autostart::AutostartProvider) -> Result<()> {
    eprintln!("Removing autostart from {}…", provider.facility_name());
    provider.uninstall()?;
    eprintln!("Done.");
    Ok(())
}

fn task_status(provider: &dyn autostart::AutostartProvider) -> Result<()> {
    match provider.status()? {
        autostart::AutostartStatus::Installed { details } => {
            eprintln!("Installed ({}). {}", provider.facility_name(), details);
        }
        autostart::AutostartStatus::NotInstalled => {
            eprintln!("Not installed ({} — run `install-task` to register).", provider.facility_name());
        }
    }
    Ok(())
}

/// Build the SessionManager + the parser-plugin restart channel.
/// Split out of `run_daemon` so the tray can hold its own clone of the
/// manager — it reads the live session list for the menu — while the
/// daemon thread gets the same instance.
/// Derive the runtime SharingPolicy from a loaded Config. Empty
/// allowed_roots + no cap is the legacy default (no restriction); set
/// either in `~/.clawborrator/desktop_v1.json` to harden the daemon
/// for desktop-share grantees.
fn policy_from_config(cfg: &Config) -> SharingPolicy {
    SharingPolicy {
        allowed_roots:           cfg.allowed_roots.iter().map(PathBuf::from).collect(),
        max_concurrent_sessions: cfg.max_concurrent_sessions,
    }
}

pub(crate) fn new_session_manager() -> (
    Arc<SessionManager>,
    tokio::sync::mpsc::UnboundedReceiver<crate::parser_plugins::watcher::RestartRequest>,
) {
    let registry = Arc::new(crate::parser_plugins::PluginRegistry::with_defaults());
    let (restart_tx, restart_rx) = tokio::sync::mpsc::unbounded_channel();
    // Best-effort policy load — config absence (first run before
    // OAuth) falls back to the empty default, matching legacy
    // single-owner behavior.
    let policy = load_or_init_config()
        .as_ref()
        .map(policy_from_config)
        .unwrap_or_default();
    if !policy.allowed_roots.is_empty() || policy.max_concurrent_sessions.is_some() {
        info!(
            allowed_roots = ?policy.allowed_roots,
            max_concurrent_sessions = ?policy.max_concurrent_sessions,
            "sharing policy loaded",
        );
    }
    let mgr = Arc::new(SessionManager::new(registry, restart_tx, policy));
    (mgr, restart_rx)
}

pub(crate) async fn run_daemon(
    cli:        Cli,
    tray:       TrayStatusUpdater,
    mgr:        Arc<SessionManager>,
    restart_rx: tokio::sync::mpsc::UnboundedReceiver<crate::parser_plugins::watcher::RestartRequest>,
) -> Result<()> {
    let mut cfg = load_or_init_config()?;
    if let Some(forced) = cli.machine_id.clone() { cfg.machine_id = forced; }
    let hub = effective_hub_url(&cli, &cfg);
    info!(machine_id = %cfg.machine_id, hub = %hub, daemon = DAEMON_VERSION, "starting");

    let token = resolve_token(&cli, &cfg)?;
    let ws_url = ws_url_for_supervisor(&hub)?;

    // The parser-plugin restart channel feeds RestartWithoutFlag matches
    // (--resume / --continue dead-end detection) into a respawn handler.
    tokio::spawn(handle_restart_requests(mgr.clone(), restart_rx, hub.clone(), token.clone()));

    // Local IPC server — backs the `sessions` / `attach` / `end` /
    // `new` subcommands. Runs for the daemon's lifetime.
    {
        let ipc_mgr = mgr.clone();
        let ipc_cfg = Arc::new(ipc::IpcConfig {
            hub_url:    hub.clone(),
            pat:        token.clone(),
            machine_id: cfg.machine_id.clone(),
        });
        tokio::spawn(async move {
            if let Err(e) = ipc::serve(ipc_mgr, ipc_cfg).await {
                warn!(error = %e, "IPC server exited");
            }
        });
    }

    let ctx = DaemonCtx {
        hub_url:    hub.clone(),
        pat:        token,
        machine_id: cfg.machine_id.clone(),
        mgr,
        tray,
    };

    tokio::select! {
        res = run_with_reconnect(ctx, ws_url, cfg) => res,
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl-c received; shutting down");
            Ok(())
        }
    }
}

/// Consume RestartWithoutFlag events emitted by parser plugins
/// (--resume / --continue dead-ends). For each, snapshot the current
/// session's flags, strip the offending flag, and soft-restart so
/// the new CC spawn picks up the corrected argv. The session row +
/// channel token + scratch dir all survive — sessionId is preserved.
async fn handle_restart_requests(
    mgr: Arc<SessionManager>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::parser_plugins::watcher::RestartRequest>,
    hub_url: String,
    pat: String,
) {
    while let Some(req) = rx.recv().await {
        if let Err(e) = restart_without_flag(&mgr, &req, &hub_url, &pat).await {
            warn!(session_id = %req.session_id, flag = %req.flag_to_strip,
                  err = %e, "restart-without-flag failed");
        }
    }
}

async fn restart_without_flag(
    mgr: &SessionManager,
    req: &crate::parser_plugins::watcher::RestartRequest,
    hub_url: &str,
    pat: &str,
) -> Result<()> {
    let entry = mgr.get(&req.session_id)?;
    let (auto_enter, new_flags) = {
        let s = entry.lock().unwrap();
        let new: Vec<String> = s.extra_flags.iter()
            .filter(|f| !matches_flag(f, &req.flag_to_strip))
            .cloned()
            .collect();
        (s.auto_enter, new)
    };
    info!(session_id = %req.session_id, stripped = %req.flag_to_strip,
          remaining_flags = ?new_flags, "restart-without-flag: respawning");
    soft_restart_session(mgr, hub_url, pat, &req.session_id, auto_enter, &new_flags).await
}

/// True when `flag` is exactly `target` OR a `--target=…` form. CC
/// accepts both `--resume X` and `--resume=X`; the watcher just
/// knows the bare flag name.
fn matches_flag(flag: &str, target: &str) -> bool {
    flag == target || flag.starts_with(&format!("{target}="))
}

/// True if this machine is already paired (a token is available from the
/// --pat flag / CLAWBORRATOR_PAT env, both folded into cli.pat by clap,
/// or cached in the config). Drives whether the first-run wizard shows.
#[cfg(target_os = "windows")]
fn is_paired(cli: &Cli) -> bool {
    cli.pat.is_some() || load_or_init_config().ok().and_then(|c| c.token).is_some()
}

/// Shadows app URL to pre-fill in the wizard: --shadows-url / env (both
/// in cli.shadows_url) -> cached config -> built-in default.
#[cfg(target_os = "windows")]
fn default_shadows_url(cli: &Cli) -> String {
    cli.shadows_url
        .clone()
        .or_else(|| load_or_init_config().ok().and_then(|c| c.shadows_url))
        .unwrap_or_else(|| DEFAULT_SHADOWS_URL.to_string())
}

fn main() -> Result<()> {
    attach_parent_console_if_any();

    let mut cli = Cli::parse();

    // Subcommand path — short-lived, console-driven. tokio on the
    // main thread is fine because there's no tray to run there.
    if let Some(cmd) = cli.command.take() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("building tokio runtime")?;
        return runtime.block_on(run_subcommand(&cli, cmd));
    }

    // Daemon path — long-running, file-based logging always on.
    let log = logging::init().context("initializing logging")?;
    info!(log_path = %log.log_path.display(), "logs will be written here");

    // Windows: an interactive launch (NOT the --background Task entry)
    // with no cached token shows the first-run setup wizard, then exits.
    // The wizard installs + starts the background Task, which re-launches
    // the daemon with --background and goes straight to the tray.
    #[cfg(target_os = "windows")]
    {
        if !cli.background && !is_paired(&cli) {
            return gui::run_first_run_wizard(default_shadows_url(&cli));
        }
        // Tray owns the main thread (the Win32 message loop is
        // thread-affine); the daemon future runs on a tokio worker
        // started inside `tray::run_with_tray`.
        tray::run_with_tray(cli, log.log_path.clone())
    }

    // macOS: the Cocoa NSApplication pump is thread-affine, so the tray
    // owns the main thread here too. (No first-run GUI yet on macOS.)
    #[cfg(target_os = "macos")]
    {
        tray::run_with_tray(cli, log.log_path.clone())
    }

    // Linux (and any other OS): headless — no GUI session guarantee,
    // so tokio runs on main and the daemon runs directly. Pass a noop
    // status updater so the daemon's call sites don't have to cfg-gate.
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("building tokio runtime")?;
        let (mgr, restart_rx) = new_session_manager();
        runtime.block_on(run_daemon(cli, TrayStatusUpdater::noop(), mgr, restart_rx))
    }
}
