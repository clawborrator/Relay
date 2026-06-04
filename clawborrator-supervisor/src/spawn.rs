// Spawn flow for `session.create`. Steps, in order:
//
//   1. Validate the folder exists + is a directory.
//   2. Mint a channel token via the hub REST API. The token + hub
//      URL go into a temporary .mcp.json that we write under
//      ~/.clawborrator/sessions/<scratch>/ — NOT into the user's
//      project folder, so we don't pollute their repo with a
//      file containing a live secret.
//   3. Spawn `claude --dangerously-load-development-channels=
//      server:clawborrator --mcp-config <our-config>` under a PTY,
//      with cwd set to the target folder.
//   4. Kick a background reader task that drains the PTY master
//      into the vt100 parser (so screenshots work) and a dumb
//      auto-answer task that hammers Enter for 10 seconds (to
//      acknowledge CC's startup prompts). Categorized
//      startupAnswers will replace the dumb hammer in a follow-on.
//   5. Poll <folder>/.claude/clawborrator/runtime.json for up to
//      30 seconds for the resolved hub session id (CC's MCP writes
//      it on register). Return that id once present.
//
// On failure at any step we revoke the channel token and clean up
// the spawn-config dir so we don't accumulate orphans.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use portable_pty::{native_pty_system, CommandBuilder};
use serde::{Deserialize, Serialize};
use tokio::time::{sleep, Instant};
use tracing::{info, warn};
use vt100::Parser;

use crate::parser_plugins::watcher::{spawn_watcher, WatcherHandle};
use crate::sessions::{fresh_pty_size, ManagedSession, SessionManager, SharedWriter, PTY_COLS, PTY_ROWS};
use crate::token_usage::spawn_token_usage_sampler;

const SIDECAR_POLL_TIMEOUT_MS: u64 = 30_000;

#[derive(Serialize)]
struct MintTokenBody<'a> { name: &'a str }

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct MintTokenResponse {
    id:    i64,
    token: String,
    name:  String,
}

#[derive(Serialize)]
struct PreflightBody<'a> {
    folder:       &'a str,
    #[serde(rename = "machineId")]
    machine_id:   &'a str,
    #[serde(rename = "routingName", skip_serializing_if = "Option::is_none")]
    routing_name: Option<&'a str>,
}

#[derive(Deserialize, Debug)]
struct PreflightResponse {
    #[serde(rename = "sessionId")]
    session_id:       String,
    #[serde(rename = "channelToken")]
    channel_token:    String,
    #[serde(rename = "channelTokenId")]
    channel_token_id: i64,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct SidecarPayload {
    #[serde(rename = "sessionId")]
    session_id: String,
}

/// Preflight a managed session — hub allocates the sessionId AND
/// mints the channel token in one atomic call. Used in place of the
/// older mint+spawn+sidecar-poll dance: the daemon writes the
/// returned sessionId directly into the folder's sidecar BEFORE
/// spawning CC, so CC's clawborrator-mcp registers with that exact
/// id and the hub's UPSERT path rebinds to the preflight row.
async fn preflight(
    hub_url:      &str,
    pat:          &str,
    folder:       &str,
    machine_id:   &str,
    routing_name: Option<&str>,
) -> Result<PreflightResponse> {
    let url = format!("{}/api/v1/sessions/preflight", hub_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .user_agent(format!("clawborrator-supervisor/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.post(url)
        .bearer_auth(pat)
        .json(&PreflightBody { folder, machine_id, routing_name })
        .send()
        .await
        .context("POST /sessions/preflight")?;
    if !resp.status().is_success() {
        let s = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("preflight failed: {s} {body}");
    }
    Ok(resp.json::<PreflightResponse>().await?)
}

/// Mint a channel token via REST. Returned token goes into the
/// .mcp.json so CC's MCP can register against the hub.
#[allow(dead_code)]
async fn mint_channel_token(hub_url: &str, pat: &str, name: &str) -> Result<MintTokenResponse> {
    let url = format!("{}/api/v1/tokens/channel", hub_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .user_agent(format!("clawborrator-supervisor/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.post(url)
        .bearer_auth(pat)
        .json(&MintTokenBody { name })
        .send()
        .await
        .context("POST /tokens/channel")?;
    if !resp.status().is_success() {
        let s = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("token mint failed: {s} {body}");
    }
    Ok(resp.json::<MintTokenResponse>().await?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RotateChannelTokenResponse {
    channel_token:    String,
    channel_token_id: i64,
}

/// Rotate the channel token for an existing managed session,
/// preserving the sessionId. Used by `respawn_preserving_id_session`
/// after a daemon restart / PC reboot — the old plaintext is gone
/// from the daemon's memory, so we mint a fresh one for the new
/// scratch dir while keeping the same sessionId on the hub side.
/// Hub-side: revokes the old token row, mints a new one, UPDATEs
/// sessions.channel_token_id atomically.
async fn rotate_channel_token(hub_url: &str, pat: &str, session_id: &str) -> Result<RotateChannelTokenResponse> {
    let url = format!(
        "{}/api/v1/sessions/{}/rotate-channel-token",
        hub_url.trim_end_matches('/'), session_id,
    );
    let client = reqwest::Client::builder()
        .user_agent(format!("clawborrator-supervisor/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.post(url).bearer_auth(pat).send().await
        .context("POST /sessions/:id/rotate-channel-token")?;
    if !resp.status().is_success() {
        let s = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("rotate-channel-token failed: {s} {body}");
    }
    Ok(resp.json::<RotateChannelTokenResponse>().await?)
}

async fn revoke_channel_token(hub_url: &str, pat: &str, id: i64) {
    let url = format!("{}/api/v1/tokens/{}", hub_url.trim_end_matches('/'), id);
    let client = match reqwest::Client::builder().build() {
        Ok(c) => c, Err(_) => return,
    };
    let _ = client.delete(url).bearer_auth(pat).send().await;
}

#[allow(dead_code)]
async fn patch_session_managed_by(hub_url: &str, pat: &str, session_id: &str, machine_id: &str) -> Result<()> {
    let url = format!("{}/api/v1/sessions/{}", hub_url.trim_end_matches('/'), session_id);
    let client = reqwest::Client::builder().build()?;
    let resp = client.patch(url)
        .bearer_auth(pat)
        .json(&serde_json::json!({ "managedByMachineId": machine_id }))
        .send()
        .await
        .context("PATCH /sessions/:id")?;
    if !resp.status().is_success() {
        let s = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("session managedBy patch failed: {s} {body}");
    }
    Ok(())
}

/// Resolve the npm spec for the `clawborrator-mcp` channel package.
///
/// `.mcp.json` runs the channel via `npx`, and `npx` with a bare
/// package name reuses whatever version it has cached — it never
/// re-checks the registry. That makes a session reset (or a full
/// delete-and-recreate) keep serving the stale cached version. So we
/// pin: each freshly-created session queries the registry for the
/// current `latest` and writes `clawborrator-mcp@<version>`, which is
/// a cache key `npx` does not yet have after a new publish, forcing it
/// to fetch the new version.
///
/// On any failure (offline, registry hiccup, unexpected body) this
/// falls back to the bare unpinned name so session creation never
/// breaks over a version lookup.
async fn resolve_mcp_spec() -> String {
    const PKG: &str = "clawborrator-mcp";
    let resolved = async {
        let client = reqwest::Client::builder()
            .user_agent(format!("clawborrator-supervisor/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(5))
            .build()?;
        let body: serde_json::Value = client
            .get(format!("https://registry.npmjs.org/{PKG}/latest"))
            .send().await?
            .error_for_status()?
            .json().await?;
        let version = body.get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("registry response missing version field"))?;
        anyhow::Ok(format!("{PKG}@{version}"))
    }.await;
    match resolved {
        Ok(spec) => { info!(spec = %spec, "pinned clawborrator-mcp to current latest"); spec }
        Err(e)   => { warn!(error = %e, "could not resolve clawborrator-mcp latest; using unpinned spec"); PKG.to_string() }
    }
}

fn write_mcp_json(scratch_dir: &PathBuf, hub_ws_url: &str, token: &str, mcp_spec: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(scratch_dir).with_context(|| format!("creating {scratch_dir:?}"))?;
    let mcp_path = scratch_dir.join(".mcp.json");
    let json = serde_json::json!({
        "mcpServers": {
            "clawborrator": {
                "command": "npx",
                "args":    ["-y", mcp_spec],
                "env": {
                    "CLAWBORRATOR_HUB_URL": hub_ws_url,
                    "CLAWBORRATOR_TOKEN":   token,
                }
            }
        }
    });
    std::fs::write(&mcp_path, serde_json::to_string_pretty(&json)? + "\n")
        .with_context(|| format!("writing {mcp_path:?}"))?;
    Ok(mcp_path)
}

fn hub_ws_url(hub_url: &str) -> String {
    let trimmed = hub_url.trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("https://") {
        return format!("wss://{rest}");
    }
    if let Some(rest) = trimmed.strip_prefix("http://") {
        return format!("ws://{rest}");
    }
    trimmed.to_string()
}

/// Spawn CC under a PTY in `folder`, configured to load the MCP
/// at `mcp_path`. Returns the PTY master + child handle so the
/// caller can wire reader/writer tasks.
fn spawn_cc(folder: &PathBuf, mcp_path: &PathBuf, cc_session_id: &str, extra_flags: &[String]) -> Result<(Box<dyn portable_pty::MasterPty + Send>, Box<dyn portable_pty::Child + Send + Sync>)> {
    let pty = native_pty_system().openpty(fresh_pty_size())
        .map_err(|e| anyhow!("openpty: {e}"))?;
    let mut cmd = CommandBuilder::new("claude");
    cmd.cwd(folder);
    cmd.arg("--dangerously-load-development-channels=server:clawborrator");
    cmd.arg("--mcp-config");
    cmd.arg(mcp_path.as_os_str());
    // Pin CC's session id so the token-usage sampler can find this
    // incarnation's transcript jsonl by name (<cc_session_id>.jsonl).
    // Fresh uuid per spawn — CC refuses a --session-id whose
    // transcript already exists, so this is never reused across
    // create / soft-restart / respawn.
    cmd.arg("--session-id");
    cmd.arg(cc_session_id);
    // Operator-supplied extra flags. Appended last so they can
    // override our defaults if needed (CC's CLI takes the last
    // value when a flag repeats). One argv slot per entry —
    // operators pass `--model opus` as two entries `["--model","opus"]`
    // or as a single `["--model=opus"]`.
    for flag in extra_flags {
        if !flag.is_empty() { cmd.arg(flag); }
    }
    // CC rejects `--session-id` together with `--resume`/`--continue`
    // unless `--fork-session` is also present. We always pin
    // `--session-id` (above), so when the operator asks to resume or
    // continue we must fork — which is what we want anyway: the prior
    // conversation is replayed but written into the fresh pinned id,
    // keeping the token-usage sampler's `<cc_session_id>.jsonl` lookup
    // intact and chaining continuity across restarts. Auto-append it
    // so operators never have to remember the combination.
    let wants_resume = extra_flags.iter().any(|f| {
        matches!(f.as_str(), "--resume" | "-r" | "--continue" | "-c")
            || f.starts_with("--resume=")
            || f.starts_with("--continue=")
    });
    let already_forks = extra_flags.iter().any(|f| f == "--fork-session");
    if wants_resume && !already_forks {
        cmd.arg("--fork-session");
    }
    // Explicit terminal type. portable-pty doesn't set this on
    // Windows ConPTY by default, and CC's TUI banner falls back
    // to a degraded layout (boxes collapse onto single rows) when
    // it doesn't see a known terminfo. xterm-256color is the
    // standard truecolor terminal everywhere — vt100 emulator on
    // our side speaks the same dialect. COLORTERM=truecolor lets
    // CC's startup banner emit 24-bit color escape sequences.
    cmd.env("TERM",      "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    // Ensure the dirs the Claude Code / Node installers drop binaries
    // into are on the spawned CC process's PATH. The official Claude
    // installer (curl … claude.ai/install.sh) puts `claude` in
    // $HOME/.local/bin; on macOS, Homebrew puts node/npm/npx in
    // /opt/homebrew/bin (Apple Silicon) or /usr/local/bin (Intel).
    // The OS autostart entry (systemd unit / launchd plist) sets PATH
    // explicitly to cover this — this runtime prepend is belt-and-
    // suspenders for daemons run outside that entry (cargo run, manual
    // invocation) or installed before the entry's PATH template existed.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            let home = std::path::PathBuf::from(home);
            // The dirs Relay forces onto the session's PATH (Claude install
            // dir, Homebrew, node version managers). Shared with the
            // prereq-check so what it reports matches what sessions get.
            let prepend = session_path_prepend_dirs(&home);
            let existing = std::env::var_os("PATH").unwrap_or_default();
            let mut new_path = std::ffi::OsString::new();
            for dir in prepend {
                if !new_path.is_empty() { new_path.push(":"); }
                new_path.push(dir);
            }
            if !existing.is_empty() {
                if !new_path.is_empty() { new_path.push(":"); }
                new_path.push(&existing);
            }
            cmd.env("PATH", new_path);
        }
    }
    let child = pty.slave.spawn_command(cmd)
        .map_err(|e| anyhow!("spawn claude: {e} (is the `claude` CLI installed and on PATH? install with `curl -fsSL https://claude.ai/install.sh | bash` then re-run `clawborrator-supervisor install-task` so the autostart entry's PATH picks it up)"))?;
    // We can drop the slave handle now — the spawned child holds
    // the only reference that matters.
    drop(pty.slave);
    Ok((pty.master, child))
}

/// The directories Relay prepends to every spawned session's PATH:
/// `~/.local/bin` (the official Claude installer's target), Homebrew on
/// macOS, and any node version-manager bins. Sharing this with the
/// prereq-check keeps "what we report" in sync with "what sessions get".
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn session_path_prepend_dirs(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut dirs = vec![home.join(".local").join("bin")];
    #[cfg(target_os = "macos")]
    {
        dirs.push(std::path::PathBuf::from("/opt/homebrew/bin"));
        dirs.push(std::path::PathBuf::from("/usr/local/bin"));
    }
    dirs.extend(node_manager_bin_dirs(home));
    // Relay-managed Node (installed by the wizard / `install-prereqs`).
    // Last, so a user's own node — system or version-manager — still wins.
    dirs.push(crate::prereq_install::managed_node_bin_dir(home));
    dirs
}

/// `bin` directories of node version managers (nvm/fnm/volta/asdf),
/// highest version first. These are the dirs that only the user's
/// interactive shell normally adds to PATH, so a daemon-spawned process
/// can't see node without this. Returns only dirs that exist.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn node_manager_bin_dirs(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;

    // Version dir names like "v22.15.1" / "20.10.0" — sort by the numeric
    // (major, minor, patch) so v9 doesn't beat v22 (lexical would).
    fn semver_key(name: &str) -> (u64, u64, u64) {
        let s = name.trim_start_matches('v');
        let mut it = s.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
        (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
    }
    // Collect <ver-dir>/<suffix> for each version under `root`, newest first.
    fn versioned(root: PathBuf, suffix: &str) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = match std::fs::read_dir(&root) {
            Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect(),
            Err(_) => return Vec::new(),
        };
        v.sort_by(|a, b| {
            let ka = a.file_name().and_then(|s| s.to_str()).map(semver_key).unwrap_or_default();
            let kb = b.file_name().and_then(|s| s.to_str()).map(semver_key).unwrap_or_default();
            kb.cmp(&ka) // descending — newest version first
        });
        v.into_iter().map(|p| p.join(suffix)).filter(|p| p.is_dir()).collect()
    }

    let mut dirs = Vec::new();
    // nvm: ~/.nvm/versions/node/<ver>/bin
    dirs.extend(versioned(home.join(".nvm/versions/node"), "bin"));
    // fnm: ~/.local/share/fnm or ~/.fnm, node-versions/<ver>/installation/bin
    for base in [home.join(".local/share/fnm"), home.join(".fnm")] {
        dirs.extend(versioned(base.join("node-versions"), "installation/bin"));
    }
    // volta + asdf use fixed shim dirs.
    for fixed in [home.join(".volta/bin"), home.join(".asdf/shims")] {
        if fixed.is_dir() { dirs.push(fixed); }
    }
    dirs
}

/// Drive a reader thread that pumps PTY bytes into a vt100 parser
/// shared with the session manager. Runs until the reader returns
/// 0 bytes (PTY closed), at which point the task exits.
fn spawn_pty_reader(
    mut reader: Box<dyn std::io::Read + Send>,
    parser: Arc<Mutex<Parser>>,
    output_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0)  => break,
                Ok(n)  => {
                    if let Ok(mut p) = parser.lock() {
                        p.process(&buf[..n]);
                    }
                    // Fan the same bytes out to any `attach` clients.
                    // `send` errs only when there are no subscribers —
                    // expected whenever nobody is attached; ignore it.
                    let _ = output_tx.send(buf[..n].to_vec());
                }
                Err(e) => {
                    warn!(?e, "pty reader: read failed");
                    break;
                }
            }
        }
    });
}

#[allow(dead_code)]
async fn poll_sidecar(folder: &PathBuf) -> Result<String> {
    let sidecar = folder.join(".claude").join("clawborrator").join("runtime.json");
    let deadline = Instant::now() + Duration::from_millis(SIDECAR_POLL_TIMEOUT_MS);
    while Instant::now() < deadline {
        if sidecar.exists() {
            if let Ok(text) = std::fs::read_to_string(&sidecar) {
                if let Ok(parsed) = serde_json::from_str::<SidecarPayload>(&text) {
                    return Ok(parsed.session_id);
                }
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
    bail!("sidecar at {} did not appear within {SIDECAR_POLL_TIMEOUT_MS}ms", sidecar.display())
}

pub struct CreateArgs<'a> {
    pub hub_url:      &'a str,
    pub pat:          &'a str,
    pub machine_id:   &'a str,
    pub folder:       PathBuf,
    pub routing_name: Option<&'a str>,
    /// Extra CLI flags to append after the daemon-required ones
    /// (`--dangerously-load-development-channels=server:clawborrator`
    /// + `--mcp-config <path>`). Each entry is a single argv slot;
    /// e.g. `["--model", "opus"]` or `["--add-dir=/repos/foo"]`.
    /// Operator-supplied via the SPA's create-session modal; the
    /// daemon does NOT validate them — CC owns the contract.
    pub extra_flags:  &'a [String],
    /// When true, the daemon types Enter once per second for
    /// AUTO_ENTER_DURATION_MS after spawn — dismisses CC's
    /// default-yes startup prompts (Trust folder, Load MCP,
    /// etc.). Operator can disable from the SPA when they need
    /// to answer prompts themselves.
    pub auto_enter:   bool,
}

// Stage helpers for create_session — split out so each stage is its
// own ~5-CC function and the orchestrator stays at ~6. Order: precheck
// → clean stale → preflight → persisted-session → mcp.json → spawn CC
// → wire PTY → register. The preflight call itself stays inline in
// create_session because the response feeds three subsequent stages.

/// Refuse the create if the folder is bogus or another managed
/// session is already running there. Two CC instances in the same
/// folder race on `.claude/clawborrator/identity.json` and produce
/// undefined behavior — at best the second create's RPC times out;
/// at worst it binds to the first session.
fn precheck_create(mgr: &SessionManager, args: &CreateArgs<'_>) -> Result<()> {
    if !args.folder.is_dir() {
        bail!("folder does not exist or is not a directory: {}", args.folder.display());
    }
    if let Some(existing) = mgr.find_by_folder(&args.folder) {
        bail!(
            "folder {} already has an active managed session ({}). Kill or destroy it first, or pick a different folder.",
            args.folder.display(), existing,
        );
    }
    // ─── Desktop-sharing guardrails ─────────────────────────────────
    // Both apply to every code path that reaches create_session: the
    // hub-WS `session.create` op (used by desktop-share grantees) AND
    // the local IPC `new` command. See hub_v1/docs/DESKTOP-SHARING.md.

    // 1) Concurrent-session cap. Counts CC children whose try_wait
    //    returns Ok(None); kill / end frees a slot.
    if let Some(cap) = mgr.policy.max_concurrent_sessions {
        let alive = mgr.count_alive() as u32;
        if alive >= cap {
            bail!(
                "concurrent-session cap reached: {alive}/{cap} on this daemon. Kill or end a session before starting another."
            );
        }
    }

    // 2) Allowed-folder root. Empty list = no restriction. Otherwise
    //    the requested folder must canonicalize under one of the
    //    configured roots. Canonicalization on both sides handles
    //    relative paths, trailing separators, and symlinks
    //    consistently; a misconfigured root (canonicalize fails) is
    //    ignored rather than crashing the daemon.
    if !mgr.policy.allowed_roots.is_empty() {
        let abs = std::fs::canonicalize(&args.folder)
            .with_context(|| format!("canonicalizing folder {}", args.folder.display()))?;
        let allowed = mgr.policy.allowed_roots.iter().any(|root| {
            match std::fs::canonicalize(root) {
                Ok(r) => abs.starts_with(&r),
                Err(_) => false,
            }
        });
        if !allowed {
            let roots: Vec<String> = mgr.policy.allowed_roots.iter()
                .map(|p| p.display().to_string())
                .collect();
            bail!(
                "folder {} is not under any allowed root configured on this daemon ({})",
                args.folder.display(), roots.join(", "),
            );
        }
    }
    Ok(())
}

/// Best-effort wipe of stale persisted-session files left behind by a
/// previous session in the same folder. The MCP reads
/// `<cwd>/.claude/clawborrator/identity.json` on boot to rebind across
/// `claude --resume`; an unwiped one would cause it to register with
/// the dead session's id instead of the preflight one. Failures are
/// silently ignored — they're advisory cleanup, not a hard prereq.
///
/// Also nukes two legacy paths from earlier daemon/MCP builds:
///   - `<cwd>/.claude/clawborrator/session.json` (pre-0.2.4 identity)
///   - `<cwd>/.claude/clawborrator.session.json`  (pre-0.2.4 hook sidecar)
/// — so existing sessions don't leak cruft when their first restart
/// runs under the new daemon. Forward-only: the new daemon never
/// READS these legacy paths, only deletes them.
fn clean_stale_persisted_files(folder: &Path) {
    let clawborrator_dir = folder.join(".claude").join("clawborrator");
    let identity_path = clawborrator_dir.join("identity.json");
    if identity_path.exists() { let _ = std::fs::remove_file(&identity_path); }
    let runtime_path = clawborrator_dir.join("runtime.json");
    if runtime_path.exists() { let _ = std::fs::remove_file(&runtime_path); }
    // Legacy paths from <0.2.4 daemons / <0.0.33 MCP — best-effort
    // sweep on first restart after upgrade. Removable in a future
    // version once nothing in the wild writes these names.
    let legacy_identity = clawborrator_dir.join("session.json");
    if legacy_identity.exists() { let _ = std::fs::remove_file(&legacy_identity); }
    let legacy_runtime  = folder.join(".claude").join("clawborrator.session.json");
    if legacy_runtime.exists()  { let _ = std::fs::remove_file(&legacy_runtime); }
}

/// Drop the persisted-session JSON the MCP reads on boot, plus a
/// `.gitignore` so the directory never ends up in source control.
/// Has to run BEFORE spawning CC — the MCP reads this file in its
/// register-frame path.
fn write_persisted_session(
    folder:       &Path,
    session_id:   &str,
    routing_name: Option<&str>,
    hub_url:      &str,
) -> Result<()> {
    let persisted_dir = folder.join(".claude").join("clawborrator");
    std::fs::create_dir_all(&persisted_dir)
        .with_context(|| format!("creating {} failed", persisted_dir.display()))?;
    let gitignore = persisted_dir.join(".gitignore");
    if !gitignore.exists() {
        let _ = std::fs::write(&gitignore, "*\n");
    }
    let sidecar_path = persisted_dir.join("identity.json");
    let sidecar_json = serde_json::json!({
        "sessionId":   session_id,
        "routingName": routing_name,
        "hubUrl":      hub_ws_url(hub_url),
        "writtenAt":   chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(&sidecar_path, serde_json::to_string_pretty(&sidecar_json)? + "\n")
        .with_context(|| format!("writing persisted-session {} failed", sidecar_path.display()))?;
    info!(path = %sidecar_path.display(), "wrote persisted-session with preflight sessionId");
    Ok(())
}

/// Mint a per-session scratch dir under `~/.clawborrator/sessions/<uuid>/`
/// and drop a `.mcp.json` there pointing at the hub WS + carrying the
/// preflight channel token. Returns `(scratch_dir, mcp_path)` —
/// scratch_dir is held by the manager for cleanup on destroy; mcp_path
/// is passed to `spawn_cc` as `--mcp-config <path>`.
async fn prepare_mcp_config(hub_url: &str, channel_token: &str) -> Result<(PathBuf, PathBuf)> {
    // `shadows-sessions` (not `sessions`) so the scratch root is disjoint
    // from a co-running desktop_v1 daemon's; otherwise each daemon's
    // startup orphan-sweep would delete the OTHER's session scratch dirs.
    let scratch_dir = match dirs::home_dir() {
        Some(h) => h.join(".clawborrator").join("shadows-sessions").join(uuid::Uuid::new_v4().to_string()),
        None    => bail!("could not resolve home dir"),
    };
    let ws_url = hub_ws_url(hub_url);
    let mcp_spec = resolve_mcp_spec().await;
    let mcp_path = write_mcp_json(&scratch_dir, &ws_url, channel_token, &mcp_spec)
        .context("writing .mcp.json")?;
    info!(path = %mcp_path.display(), "wrote .mcp.json");
    Ok((scratch_dir, mcp_path))
}

/// Wire a vt100 parser onto the reader half of `master` (background
/// thread) and return the parser + a shared `Arc<Mutex<Writer>>` the
/// caller can hand both to the ManagedSession (owns it) and any
/// auxiliary task (auto-enter, future input forwarders). Dropping
/// the writer closes CC's stdin and exits CC — see ManagedSession's
/// lifecycle commentary for why we wrap it instead of giving the
/// auxiliary tasks ownership.
fn wire_pty_io(
    master: &(dyn portable_pty::MasterPty + Send),
) -> Result<(Arc<Mutex<Parser>>, SharedWriter, tokio::sync::broadcast::Sender<Vec<u8>>)> {
    let parser = Arc::new(Mutex::new(Parser::new(PTY_ROWS, PTY_COLS, 0)));
    let reader = master.try_clone_reader().map_err(|e| anyhow!("try_clone_reader: {e}"))?;
    // Capacity bounds how far a slow `attach` client may lag before it
    // gets a `Lagged` error and skips ahead — a momentary redraw gap,
    // acceptable for a live terminal view.
    let (output_tx, _) = tokio::sync::broadcast::channel(1024);
    spawn_pty_reader(reader, parser.clone(), output_tx.clone());
    let writer: SharedWriter = Arc::new(Mutex::new(
        master.take_writer().map_err(|e| anyhow!("take_writer: {e}"))?,
    ));
    Ok((parser, writer, output_tx))
}

/// Spawn the per-session parser-plugin watcher and return the handle
/// (so the session can cancel it on destroy / replace it on
/// soft-restart). The registry + restart channel come from the
/// SessionManager — every session shares the same plugin set.
fn spawn_session_watcher(
    mgr:        &SessionManager,
    session_id: &str,
    parser:     Arc<Mutex<Parser>>,
    writer:     SharedWriter,
) -> WatcherHandle {
    spawn_watcher(
        session_id.to_string(),
        parser,
        writer,
        mgr.registry.clone(),
        Some(mgr.restart_tx.clone()),
    )
}

pub async fn create_session(mgr: &SessionManager, args: CreateArgs<'_>) -> Result<String> {
    precheck_create(mgr, &args)?;
    clean_stale_persisted_files(&args.folder);

    // Hub pre-allocates the sessionId AND mints the channel token in
    // one call — we get back both pieces and don't have to wait for
    // CC's MCP to register before knowing the id.
    let pre = preflight(
        args.hub_url, args.pat,
        &args.folder.to_string_lossy(),
        args.machine_id, args.routing_name,
    ).await.context("preflight")?;
    info!(session_id = %pre.session_id, "preflight allocated session");

    // Order matters: the persisted-session file must exist BEFORE CC
    // spawns so the MCP's register frame carries the preflight id and
    // the hub's upsert rebinds to the preflight row instead of minting
    // a sibling.
    write_persisted_session(&args.folder, &pre.session_id, args.routing_name, args.hub_url)?;
    let (scratch_dir, mcp_path) = prepare_mcp_config(args.hub_url, &pre.channel_token).await?;
    // Fresh CC session id for this incarnation — pins the transcript
    // path the token-usage sampler reads.
    let cc_session_id = uuid::Uuid::new_v4().to_string();
    let (master, child) = spawn_cc(&args.folder, &mcp_path, &cc_session_id, args.extra_flags)
        .context("spawning claude")?;

    let (parser, writer, output_tx) = wire_pty_io(&*master)?;
    // Parser-plugin watcher handles known startup prompts (Trust
    // folder, dev channels, MCP, bypass-permissions) AND signals a
    // respawn when --resume / --continue land in a dead-end state.
    // Replaces the older 5s AUTO_ENTER blind pump.
    let watcher = if args.auto_enter {
        info!(session_id = %pre.session_id,
              "AUTO START — parser-plugin watcher will dismiss known prompts");
        Some(spawn_session_watcher(mgr, &pre.session_id, parser.clone(), writer.clone()))
    } else {
        info!(session_id = %pre.session_id,
              "MANUAL START — operator drives prompts; no watcher attached");
        None
    };

    // Token-usage sampler — runs for every session regardless of
    // auto_enter (usage accrues whoever drives the prompts).
    let token_usage_sampler = Some(spawn_token_usage_sampler(
        pre.session_id.clone(), cc_session_id.clone(),
        args.hub_url.to_string(), args.pat.to_string(),
    ));

    mgr.insert(pre.session_id.clone(), ManagedSession {
        _session_id:      pre.session_id.clone(),
        folder:           args.folder.clone(),
        routing_name:     args.routing_name.map(|s| s.to_string()),
        _master:          master,
        _writer:          writer,
        child,
        parser,
        output_tx,
        scratch_dir,
        channel_token_id: pre.channel_token_id,
        extra_flags:      args.extra_flags.to_vec(),
        auto_enter:       args.auto_enter,
        watcher,
        cc_session_id,
        token_usage_sampler,
    });
    Ok(pre.session_id)
}

#[derive(Deserialize)]
struct PersistedIdentity {
    #[serde(rename = "sessionId")]
    session_id: String,
}

/// Read `<cwd>/.claude/clawborrator/identity.json` and return the
/// persisted sessionId. Returns Err if the file is missing or
/// unparseable — caller falls back to the impermanent path.
fn read_persisted_identity(folder: &Path) -> Result<String> {
    let path = folder.join(".claude").join("clawborrator").join("identity.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let parsed: PersistedIdentity = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {} as identity.json", path.display()))?;
    Ok(parsed.session_id)
}

/// Respawn a managed session post-reboot WITHOUT rotating the
/// sessionId. Path: read identity.json (must match the row's
/// sessionId), call /api/v1/sessions/:id/rotate-channel-token to
/// mint a fresh channel token (the old plaintext is gone from
/// memory after the daemon restarted), prepare a new scratch dir +
/// .mcp.json with the new token, spawn CC. The new MCP reads the
/// SAME identity.json on boot and registers with the OLD sessionId
/// + NEW channel token; the hub's UPSERT-on-register rebinds to
/// the existing session row → history / agent.session_id / webhook
/// pins all keep working.
///
/// Inserts into mgr keyed by the OLD sessionId. The OLD scratch
/// dir on disk (from before the reboot) is orphaned; the daemon's
/// startup sweep removes it after `runAutoStartRespawn` runs (once
/// all live scratch dirs are known).
pub async fn respawn_preserving_id_session(
    mgr:          &SessionManager,
    hub_url:      &str,
    pat:          &str,
    session_id:   &str,
    folder:       PathBuf,
    routing_name: Option<&str>,
    auto_enter:   bool,
    extra_flags:  &[String],
) -> Result<String> {
    let on_disk = read_persisted_identity(&folder)
        .with_context(|| "respawn_preserving_id requires <cwd>/.claude/clawborrator/identity.json from a prior session")?;
    if on_disk != session_id {
        bail!(
            "identity.json sessionId ({}) does not match expected ({}); refuse to respawn against a mismatched on-disk identity",
            on_disk, session_id,
        );
    }

    let rotated = rotate_channel_token(hub_url, pat, session_id).await
        .context("rotating channel token")?;
    info!(session_id, channel_token_id = rotated.channel_token_id, "rotated channel token for respawn");

    let (scratch_dir, mcp_path) = prepare_mcp_config(hub_url, &rotated.channel_token).await?;
    let cc_session_id = uuid::Uuid::new_v4().to_string();
    let (master, child) = spawn_cc(&folder, &mcp_path, &cc_session_id, extra_flags)
        .with_context(|| "spawning claude (respawn-preserving-id)")?;
    let (parser, writer, output_tx) = wire_pty_io(&*master)?;
    let watcher = if auto_enter {
        info!(session_id, "respawn AUTO START — attaching parser-plugin watcher");
        Some(spawn_session_watcher(mgr, session_id, parser.clone(), writer.clone()))
    } else {
        None
    };

    let token_usage_sampler = Some(spawn_token_usage_sampler(
        session_id.to_string(), cc_session_id.clone(),
        hub_url.to_string(), pat.to_string(),
    ));

    mgr.insert(session_id.to_string(), ManagedSession {
        _session_id:      session_id.to_string(),
        folder,
        routing_name:     routing_name.map(|s| s.to_string()),
        _master:          master,
        _writer:          writer,
        child,
        parser,
        output_tx,
        scratch_dir,
        channel_token_id: rotated.channel_token_id,
        extra_flags:      extra_flags.to_vec(),
        auto_enter,
        watcher,
        cc_session_id,
        token_usage_sampler,
    });
    info!(session_id, "respawned with preserved sessionId");
    Ok(session_id.to_string())
}

/// Best-effort sweep: walk `~/.clawborrator/sessions/` and remove
/// any scratch dir not currently held by the SessionManager. Run
/// once after `runAutoStartRespawn` completes — at that point every
/// live session has a fresh scratch dir in mgr, so anything else is
/// orphan from a prior daemon run.
pub fn sweep_orphan_scratch_dirs(mgr: &SessionManager) {
    // Sweep only OUR scratch root (shadows-sessions), never desktop_v1's.
    let base = match dirs::home_dir() {
        Some(h) => h.join(".clawborrator").join("shadows-sessions"),
        None    => { warn!("no home dir; skipping orphan scratch sweep"); return; }
    };
    if !base.exists() { return; }
    let live: std::collections::HashSet<PathBuf> = mgr.list_scratch_dirs().into_iter().collect();
    let entries = match std::fs::read_dir(&base) {
        Ok(e) => e,
        Err(e) => { warn!(?e, dir = %base.display(), "could not read scratch base"); return; }
    };
    let mut swept = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() { continue; }
        if live.contains(&path) { continue; }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => { info!(dir = %path.display(), "swept orphan scratch dir"); swept += 1; }
            Err(e) => warn!(?e, dir = %path.display(), "failed to remove orphan scratch dir"),
        }
    }
    info!(swept, base = %base.display(), "orphan-scratch sweep complete");
}

/// Soft-restart: SIGKILL the PTY child, then respawn CC against the
/// EXISTING scratch dir's .mcp.json (which still holds the original
/// channel token) and the EXISTING <cwd>/.claude/clawborrator/
/// identity.json (which still holds the original sessionId). The new
/// CC's MCP reads identity.json on boot and registers with the SAME
/// sessionId; the hub's UPSERT-on-register path rebinds to the
/// existing session row → history preserved, agent.session_id stays
/// bound, webhook routingName matches keep firing, channel token
/// stays alive.
///
/// Eligible only for sessions whose row has preserve_session_id=true
/// — the hub gates the RPC. Unlike restart_session, this does NOT
/// call destroy_session: scratch dir, persisted-session, channel
/// token all survive the cycle. Failure modes:
///   - channel token externally revoked → new MCP fails to register;
///     surfaces as the WS never reaching welcome. Operator escalates
///     to hard reset (POST /restart) which mints a fresh token.
///   - identity.json externally deleted → new MCP mints a sibling
///     sessionId; the swap below leaves the in-mem ManagedSession
///     keyed by the OLD id. Pathological; operator should hard reset.
pub async fn soft_restart_session(
    mgr:         &SessionManager,
    hub_url:     &str,
    pat:         &str,
    session_id:  &str,
    auto_enter:  bool,
    extra_flags: &[String],
) -> Result<()> {
    let entry = mgr.get(session_id)?;
    let (folder, scratch_dir) = {
        let s = entry.lock().unwrap();
        (s.folder.clone(), s.scratch_dir.clone())
    };

    // Kill the old child + cancel the old parser-plugin watcher.
    // Non-blocking; the watcher task observes the cancel via its
    // oneshot and exits cleanly. Logging-only on errors.
    {
        let mut s = entry.lock().unwrap();
        if let Err(e) = s.child.kill() {
            warn!(?e, session_id, "failed to kill old child during soft-restart; continuing");
        }
        if let Some(w) = s.watcher.as_mut() {
            w.cancel();
        }
        // Cancel the old sampler — it posts a reason=final snapshot
        // for the incarnation we are ending, then exits.
        if let Some(t) = s.token_usage_sampler.as_mut() {
            t.cancel();
        }
    }

    let mcp_path = scratch_dir.join(".mcp.json");
    let cc_session_id = uuid::Uuid::new_v4().to_string();
    let (master, child) = spawn_cc(&folder, &mcp_path, &cc_session_id, extra_flags)
        .with_context(|| "spawning claude (soft-restart)")?;
    let (parser, writer, output_tx) = wire_pty_io(&*master)?;
    let new_watcher = if auto_enter {
        info!(session_id, "soft-restart — attaching fresh parser-plugin watcher");
        Some(spawn_session_watcher(mgr, session_id, parser.clone(), writer.clone()))
    } else {
        None
    };

    // Swap new PTY/child/reader/writer + flags + watcher into the
    // existing entry. _session_id, folder, routing_name, scratch_dir,
    // channel_token_id stay the same — that's the whole point of
    // soft-restart. extra_flags + auto_enter get refreshed so a
    // plugin-driven restart that stripped a flag persists in mgr.
    // Fresh sampler for the new incarnation — new cc_session_id means
    // a new transcript jsonl, so it starts a new cumulative series.
    let new_sampler = Some(spawn_token_usage_sampler(
        session_id.to_string(), cc_session_id.clone(),
        hub_url.to_string(), pat.to_string(),
    ));

    {
        let mut s = entry.lock().unwrap();
        s._master             = master;
        s.child               = child;
        s.parser              = parser;
        s.output_tx           = output_tx;
        s._writer             = writer;
        s.extra_flags         = extra_flags.to_vec();
        s.auto_enter          = auto_enter;
        s.watcher             = new_watcher;
        s.cc_session_id       = cc_session_id;
        s.token_usage_sampler = new_sampler;
    }
    info!(session_id, "soft-restarted CC (sessionId preserved)");
    Ok(())
}

pub fn kill_session(mgr: &SessionManager, session_id: &str) -> Result<()> {
    let entry = mgr.remove(session_id)
        .ok_or_else(|| anyhow!("no managed session with id {session_id}"))?;
    let mut s = entry.lock().unwrap();
    s.child.kill().map_err(|e| anyhow!("kill: {e}"))?;
    if let Some(w) = s.watcher.as_mut() { w.cancel(); }
    if let Some(t) = s.token_usage_sampler.as_mut() { t.cancel(); }
    info!(session_id, "killed CC process");
    Ok(())
}

/// Restart a managed session. Snapshots the original create-args
/// (folder + routing_name), tears the existing session down via
/// `destroy_session` so the SPA's old row gets purged AND the
/// daemon-side scratch dir + channel token are cleaned up, then
/// spawns a fresh session in the same folder. The new session
/// gets a new hub session id; the response carries it back.
///
/// We DON'T mint into the same hub session row because the row
/// is keyed by channel-token registration — there's no "rebind"
/// path. Documented behavior: restart = new row in the same
/// folder; the old row is hard-deleted (along with its short
/// pre-kill history). Operators wanting history-preserving
/// restart should use `kill` + manually re-create.
///
/// `auto_enter` and `extra_flags` are forwarded by the hub from the
/// persisted sessions.auto_enter and sessions.extra_flags columns, so
/// a Restart preserves both the prompt-handling mode AND the operator's
/// CLI flags (--model, --add-dir, etc.) instead of silently dropping
/// them. Older hubs that don't pass them default to auto_enter=true
/// and extra_flags=[] via the SessionRestartArgs deserializer.
pub async fn restart_session(
    mgr: &SessionManager,
    hub_url: &str,
    pat: &str,
    machine_id: &str,
    session_id: &str,
    auto_enter: bool,
    extra_flags: &[String],
) -> Result<String> {
    // Snapshot the args BEFORE destroy_session pulls the entry out
    // of the manager.
    let entry = mgr.get(session_id)?;
    let (folder, routing_name) = {
        let s = entry.lock().unwrap();
        (s.folder.clone(), s.routing_name.clone())
    };
    drop(entry);

    // destroy_session runs the full daemon-side cleanup (kill +
    // scratch + revoke). We DO NOT also DELETE the hub row here —
    // that's the orchestrator's job (the hub-side restart handler
    // could call DELETE before forwarding restart, but for now
    // we leave the old row as a short-lived stub. Operator can
    // archive/purge it.)
    destroy_session(mgr, hub_url, pat, session_id).await
        .with_context(|| "destroy phase of restart")?;

    // Now spawn fresh in the same folder. The new sessionId is
    // returned to the hub, which forwards it to the SPA so the
    // operator can be auto-selected onto the new row.
    create_session(mgr, CreateArgs {
        hub_url,
        pat,
        machine_id,
        folder,
        routing_name: routing_name.as_deref(),
        extra_flags,
        auto_enter,
    }).await
}

/// Tear a managed session down completely. Steps:
///   1. Kill the CC process (best-effort — ignore "already dead").
///   2. Remove the daemon-managed scratch dir (the one holding
///      .mcp.json with the soon-to-be-revoked token).
///   3. Revoke the channel token via REST so the hub doesn't
///      accumulate orphaned cw_live_… rows.
/// Hub's DELETE /api/v1/sessions/:id wraps this — it calls
/// `session.destroy` first, then does its own DB cascade. So the
/// session row itself is NOT touched here; the daemon's job is
/// purely to clean up daemon-side state + the channel token.
pub async fn destroy_session(mgr: &SessionManager, hub_url: &str, pat: &str, session_id: &str) -> Result<()> {
    // Removing from the manager first transfers ownership of the
    // scratch_dir + channel_token_id out of the map, which lets us
    // hold them across the async revoke without keeping the lock.
    let entry = mgr.remove(session_id)
        .ok_or_else(|| anyhow!("no managed session with id {session_id}"))?;
    let (scratch_dir, channel_token_id, folder) = {
        let mut s = entry.lock().unwrap();
        // best-effort kill; child may already be dead from a prior
        // session.kill or natural exit. Don't propagate the error.
        let _ = s.child.kill();
        if let Some(w) = s.watcher.as_mut() { w.cancel(); }
        // Cancel the sampler — it posts a reason=final snapshot, then
        // exits. Best-effort: if the hub then deletes the session row
        // the snapshot rows cascade away, which is correct.
        if let Some(t) = s.token_usage_sampler.as_mut() { t.cancel(); }
        (s.scratch_dir.clone(), s.channel_token_id, s.folder.clone())
    };
    if scratch_dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&scratch_dir) {
            warn!(?e, dir = %scratch_dir.display(), "failed to remove scratch dir");
        } else {
            info!(dir = %scratch_dir.display(), "removed scratch dir");
        }
    }
    // Wipe the per-folder persisted-session files so a future create
    // in this same folder doesn't pick this dead session's id back up
    // from MCP's loadPersistedSessionId() path. Also nukes the legacy
    // <0.2.4 paths in case an older daemon/MCP build left them.
    let clawborrator_dir = folder.join(".claude").join("clawborrator");
    let identity = clawborrator_dir.join("identity.json");
    if identity.exists() { let _ = std::fs::remove_file(&identity); }
    let runtime  = clawborrator_dir.join("runtime.json");
    if runtime.exists()  { let _ = std::fs::remove_file(&runtime); }
    let legacy_identity = clawborrator_dir.join("session.json");
    if legacy_identity.exists() { let _ = std::fs::remove_file(&legacy_identity); }
    let legacy_runtime  = folder.join(".claude").join("clawborrator.session.json");
    if legacy_runtime.exists()  { let _ = std::fs::remove_file(&legacy_runtime); }
    revoke_channel_token(hub_url, pat, channel_token_id).await;
    info!(session_id, channel_token_id, "session destroyed");
    Ok(())
}

/// Type raw bytes into a managed session's PTY. The bytes are
/// written through the same Arc<Mutex<Writer>> the auto-enter
/// task uses — operator's keystrokes from the SPA's PIP land
/// here. UTF-8 string in, raw bytes out; the SPA is responsible
/// for translating special keys (Enter → "\r", Esc → "\x1b",
/// arrows → "\x1b[A" etc.) before posting.
pub fn input_session(mgr: &SessionManager, session_id: &str, bytes: &[u8]) -> Result<()> {
    let entry = mgr.get(session_id)?;
    let s = entry.lock().unwrap();
    let mut w = s._writer.lock().map_err(|_| anyhow!("writer mutex poisoned"))?;
    w.write_all(bytes).map_err(|e| anyhow!("write: {e}"))?;
    w.flush().map_err(|e| anyhow!("flush: {e}"))?;
    Ok(())
}

pub fn screenshot_session(mgr: &SessionManager, session_id: &str) -> Result<serde_json::Value> {
    let entry = mgr.get(session_id)?;
    let s = entry.lock().unwrap();
    let parser = s.parser.lock().unwrap();
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let (cur_row, cur_col) = screen.cursor_position();
    // Walk every cell collecting text + ANSI attributes. Adjacent
    // cells with identical (fg, bg, bold, italic, underline, inverse)
    // collapse into one styled run, so a typical CC frame ends up
    // ~20 runs × 24 rows on the wire (vs. cols×rows individual cells).
    //
    // Padding every row to `cols` cells with a default-styled space
    // for None / empty preserves the layout exactly as vt100 sees it.
    // Trimming trailing whitespace was tempting but it collapses
    // box-drawing layouts that draw two columns of boxes side-by-side
    // (CC's startup banner).
    let mut lines: Vec<serde_json::Value> = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let mut row_runs: Vec<serde_json::Value> = Vec::new();
        let mut cur_style: Option<SpanStyle> = None;
        let mut cur_text = String::new();
        for c in 0..cols {
            let (ch, style) = match screen.cell(r, c) {
                Some(cell) => {
                    let glyph = cell.contents();
                    let g = if glyph.is_empty() { " ".to_string() } else { glyph };
                    (g, SpanStyle::from_cell(cell))
                }
                None => (" ".to_string(), SpanStyle::default()),
            };
            match &cur_style {
                Some(prev) if *prev == style => {
                    cur_text.push_str(&ch);
                }
                _ => {
                    if let Some(prev) = cur_style.take() {
                        if !cur_text.is_empty() {
                            row_runs.push(prev.to_json(std::mem::take(&mut cur_text)));
                        }
                    }
                    cur_style = Some(style);
                    cur_text = ch;
                }
            }
        }
        if let Some(prev) = cur_style.take() {
            if !cur_text.is_empty() {
                row_runs.push(prev.to_json(cur_text));
            }
        }
        lines.push(serde_json::Value::Array(row_runs));
    }
    Ok(serde_json::json!({
        "rows":   rows,
        "cols":   cols,
        "lines":  lines,
        "cursor": { "row": cur_row, "col": cur_col },
    }))
}

// Per-cell visual attributes captured off the vt100 parser. Equality
// on this struct drives the per-row run-length encoding: cells with
// identical attrs collapse into one styled span on the wire.
#[derive(Default, PartialEq, Clone)]
struct SpanStyle {
    fg: Option<serde_json::Value>,
    bg: Option<serde_json::Value>,
    b: bool, i: bool, u: bool, inv: bool,
}

impl SpanStyle {
    fn from_cell(cell: &vt100::Cell) -> Self {
        SpanStyle {
            fg: color_to_json(cell.fgcolor()),
            bg: color_to_json(cell.bgcolor()),
            b:   cell.bold(),
            i:   cell.italic(),
            u:   cell.underline(),
            inv: cell.inverse(),
        }
    }

    // Build the on-wire object. Attribute flags + colors are omitted
    // when unset so a default-styled span is just `{"t": "..."}`.
    fn to_json(&self, text: String) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("t".into(), serde_json::Value::String(text));
        if let Some(fg) = &self.fg { m.insert("fg".into(), fg.clone()); }
        if let Some(bg) = &self.bg { m.insert("bg".into(), bg.clone()); }
        if self.b   { m.insert("b".into(),   true.into()); }
        if self.i   { m.insert("i".into(),   true.into()); }
        if self.u   { m.insert("u".into(),   true.into()); }
        if self.inv { m.insert("inv".into(), true.into()); }
        serde_json::Value::Object(m)
    }
}

// Map vt100's Color enum to a wire-friendly JSON value:
//   Default → None (field omitted entirely)
//   Idx(n)  → number 0..255 (the standard ANSI palette index)
//   Rgb(..) → [r, g, b] (24-bit truecolor)
// Renderers map Idx 0..15 against a fixed dark-theme palette of their
// choosing; 16..231 follows the standard 6×6×6 color cube; 232..255
// is the standard greyscale ramp. RGB passes through unchanged.
fn color_to_json(c: vt100::Color) -> Option<serde_json::Value> {
    match c {
        vt100::Color::Default     => None,
        vt100::Color::Idx(n)      => Some(serde_json::Value::from(n)),
        vt100::Color::Rgb(r, g, b) => Some(serde_json::json!([r, g, b])),
    }
}
