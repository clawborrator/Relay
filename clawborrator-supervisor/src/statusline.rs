//! Real usage limits via Claude Code's status line.
//!
//! Claude Code knows the signed-in account's plan usage (the 5-hour and
//! 7-day `rate_limits`, with reset times), the model, and exact context-window
//! use, and hands all of it as JSON on stdin to the configured `statusLine`
//! command on every update. None of that reaches the hub, so Relay:
//!
//! 1. spawns each session with `--settings <scratch>/relay-settings.json`,
//!    whose `statusLine.command` is `relay statusline --out <scratch>/statusline.json`;
//! 2. `relay statusline` (this module, [`run_statusline`]) saves that JSON
//!    atomically, then prints the user's OWN status line if they configured
//!    one (by running it with the same stdin), so their terminal is unchanged;
//! 3. the daemon's [`spawn_usage_reporter`] posts the latest per-session
//!    snapshot to the shadows app (PairWave) it paired with, authenticated
//!    with this machine's paired token. The hub is not involved.
//!
//! Only non-identifying usage fields are forwarded (model, context %, rate
//! limits, cost). Paths, transcript locations and workspace info in the
//! status-line JSON stay on the machine.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::sessions::SessionManager;

pub const STATUSLINE_FILE: &str = "statusline.json";
pub const SETTINGS_FILE: &str = "relay-settings.json";

/// Cap on what we read from stdin — the status-line JSON is a few KB.
const MAX_INPUT: u64 = 1 << 20;
/// How long a user's own status-line command may run before we give up.
const PASSTHROUGH_TIMEOUT: Duration = Duration::from_secs(3);

// ─── `relay statusline` (runs as Claude Code's statusLine command) ───────

/// Entry point for `relay statusline --out <path>`. Never fails loudly:
/// anything written to stderr or a non-zero exit would surface in the
/// user's Claude Code UI, so errors are swallowed.
pub fn run_statusline(out: &Path) {
    let mut input = String::new();
    let _ = std::io::stdin().take(MAX_INPUT).read_to_string(&mut input);
    if serde_json::from_str::<Value>(&input).is_ok() {
        let _ = write_atomic(out, input.as_bytes());
    }
    // The user's own line wins; if they have none (or it prints nothing,
    // e.g. a hook that only renders inside another tool), show ours.
    let line = user_status_line(&input)
        .filter(|l| !l.trim().is_empty())
        .unwrap_or_else(|| default_status_line(&input));
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(line.as_bytes());
    let _ = stdout.flush();
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Claude Code's config dir: $CLAUDE_CONFIG_DIR, else ~/.claude.
pub(crate) fn claude_config_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        let p = PathBuf::from(d);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    dirs::home_dir().map(|h| h.join(".claude"))
}

/// The statusLine command the user configured themselves, if any. Checks the
/// project's settings (cwd = the session folder) then the user's, in Claude
/// Code's precedence order. Our own command is skipped so we never recurse.
fn user_status_command() -> Option<String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(".claude").join("settings.local.json"));
        candidates.push(cwd.join(".claude").join("settings.json"));
    }
    if let Some(dir) = claude_config_dir() {
        candidates.push(dir.join("settings.json"));
    }
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(cmd) = v.pointer("/statusLine/command").and_then(Value::as_str) else { continue };
        if cmd.trim().is_empty() || cmd.contains(" statusline --out ") {
            continue;
        }
        return Some(cmd.to_string());
    }
    None
}

/// Run the user's own status-line command with the same stdin and return its
/// output. The whole lifecycle (writing stdin, reading stdout, exiting) shares
/// one PASSTHROUGH_TIMEOUT deadline; a command that overruns is killed, so a
/// slow or wedged script can never stall Claude Code's UI.
fn user_status_line(input: &str) -> Option<String> {
    let cmd = user_status_command()?;
    let mut child = spawn_status_command(&cmd)?;
    let deadline = Instant::now() + PASSTHROUGH_TIMEOUT;

    // Feed stdin on its own thread: a command that never reads it must not
    // block us once the pipe buffer fills.
    if let Some(mut stdin) = child.stdin.take() {
        let data = input.as_bytes().to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });

    let out = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok();
    // Output is in (or we timed out); give the process the rest of the
    // deadline to exit, then kill it either way.
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                kill_tree(&mut child);
                break;
            }
        }
    }
    out
}

/// Spawn a status-line command the way Claude Code runs it: through bash
/// (`sh` on unix; on Windows Claude Code uses Git Bash, so prefer
/// CLAUDE_CODE_GIT_BASH_PATH / `bash` and fall back to `cmd` only if no bash
/// exists). On unix the command gets its own process group so a timeout can
/// kill everything it started, not just the shell.
fn spawn_status_command(cmd: &str) -> Option<std::process::Child> {
    use std::process::{Command, Stdio};
    let configure = |mut c: Command| -> Command {
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        c
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut c = Command::new("sh");
        c.args(["-c", cmd]).process_group(0);
        configure(c).spawn().ok()
    }
    #[cfg(windows)]
    {
        let bash = std::env::var_os("CLAUDE_CODE_GIT_BASH_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("bash"));
        let mut c = Command::new(&bash);
        c.args(["-c", cmd]);
        if let Ok(child) = configure(c).spawn() {
            return Some(child);
        }
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        configure(c).spawn().ok()
    }
}

/// Kill a timed-out status command and everything it spawned.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // Negative pid = the whole process group (see process_group(0)).
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &format!("-{}", child.id())])
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Compact default line when the user has none: "Opus 5.5 · ctx 34% · 5h 12% · 7d 40%".
pub fn default_status_line(input: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(input) else { return String::new() };
    let mut parts: Vec<String> = Vec::new();
    if let Some(m) = v.pointer("/model/display_name").and_then(Value::as_str) {
        parts.push(m.to_string());
    }
    if let Some(p) = v.pointer("/context_window/used_percentage").and_then(Value::as_f64) {
        parts.push(format!("ctx {:.0}%", p));
    }
    if let Some(p) = v.pointer("/rate_limits/five_hour/used_percentage").and_then(Value::as_f64) {
        parts.push(format!("5h {:.0}%", p));
    }
    if let Some(p) = v.pointer("/rate_limits/seven_day/used_percentage").and_then(Value::as_f64) {
        parts.push(format!("7d {:.0}%", p));
    }
    parts.join(" · ")
}

// ─── Spawn-side settings file ────────────────────────────────────────────

/// Write `<scratch>/relay-settings.json` pointing CC's statusLine at this
/// binary. Returned path is passed to `claude --settings <path>`.
pub fn write_settings(scratch_dir: &Path) -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let out = scratch_dir.join(STATUSLINE_FILE);
    let command = format!(
        "{} statusline --out {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&out.to_string_lossy()),
    );
    let settings = serde_json::json!({
        "statusLine": { "type": "command", "command": command, "padding": 0 }
    });
    std::fs::create_dir_all(scratch_dir)?;
    let path = scratch_dir.join(SETTINGS_FILE);
    std::fs::write(&path, serde_json::to_vec_pretty(&settings)?)?;
    Ok(path)
}

/// Quote a path for the shell CC runs statusLine commands through
/// (`sh -c` on unix, `cmd /C` on Windows).
fn shell_quote(s: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", s.replace('"', ""))
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

// ─── Daemon-side reporter ────────────────────────────────────────────────

#[derive(Serialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub used_percentage: f64,
    /// As Claude Code reports it (epoch seconds today; PairWave also accepts ISO).
    pub resets_at: Option<Value>,
}

#[derive(Serialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsage {
    pub hub_session_id: String,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub context_used_percentage: Option<f64>,
    pub context_window_size: Option<u64>,
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub session_cost_usd: Option<f64>,
    /// Claude Code's conversation id for this incarnation, so the shadows
    /// app can resume the session in place later.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cc_session_id: Option<String>,
}

fn window(v: &Value, key: &str) -> Option<Window> {
    let w = v.pointer(&format!("/rate_limits/{key}"))?;
    Some(Window {
        used_percentage: w.get("used_percentage")?.as_f64()?,
        resets_at: w.get("resets_at").cloned().filter(|r| !r.is_null()),
    })
}

/// Pull the forwardable fields out of one status-line JSON document.
pub fn extract_usage(hub_session_id: &str, raw: &str) -> Option<SessionUsage> {
    let v: Value = serde_json::from_str(raw).ok()?;
    Some(SessionUsage {
        hub_session_id: hub_session_id.to_string(),
        cc_session_id: v.get("session_id").and_then(Value::as_str).map(str::to_string),
        model_id: v.pointer("/model/id").and_then(Value::as_str).map(str::to_string),
        model_name: v.pointer("/model/display_name").and_then(Value::as_str).map(str::to_string),
        context_used_percentage: v.pointer("/context_window/used_percentage").and_then(Value::as_f64),
        context_window_size: v.pointer("/context_window/context_window_size").and_then(Value::as_u64),
        five_hour: window(&v, "five_hour"),
        seven_day: window(&v, "seven_day"),
        session_cost_usd: v.pointer("/cost/total_cost_usd").and_then(Value::as_f64),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report<'a> {
    machine_id: &'a str,
    daemon_version: &'a str,
    sessions: &'a [SessionUsage],
}

/// Current pairing credentials: (shadows_url, token). Re-read every tick so a
/// re-pair (the repair wizard rewrites the config without restarting the
/// daemon) takes effect without a restart. None = not paired / disabled.
pub type CredsFn = Arc<dyn Fn() -> Option<(String, String)> + Send + Sync>;

pub struct ReporterConfig {
    pub creds: CredsFn,
    pub machine_id: String,
    pub daemon_version: &'static str,
}

const TICK: Duration = Duration::from_secs(30);
/// Re-send unchanged data this often so PairWave can tell "fresh" from "stale".
const HEARTBEAT: Duration = Duration::from_secs(5 * 60);

/// Poll each live session's statusline.json and POST changes to
/// `<shadows_url>/api/relay/usage`. Best-effort: failures are logged and
/// retried on the next tick; a 404 (older PairWave without the endpoint)
/// backs off to the heartbeat interval.
pub fn spawn_usage_reporter(mgr: Arc<SessionManager>, cfg: ReporterConfig) {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder().timeout(Duration::from_secs(15)).build() {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "usage reporter: http client");
                return;
            }
        };
        let mut last_sent: Vec<SessionUsage> = Vec::new();
        let mut last_post = Instant::now() - HEARTBEAT;
        let mut unsupported_until: Option<Instant> = None;
        info!("usage reporter started");
        loop {
            tokio::time::sleep(TICK).await;
            if unsupported_until.is_some_and(|t| Instant::now() < t) {
                continue;
            }
            let mut current: Vec<SessionUsage> = Vec::new();
            for (sid, dir) in mgr.list_session_scratch_dirs() {
                let Ok(raw) = std::fs::read_to_string(dir.join(STATUSLINE_FILE)) else { continue };
                if let Some(u) = extract_usage(&sid, &raw) {
                    current.push(u);
                }
            }
            if current.is_empty() {
                continue;
            }
            let Some((shadows_url, token)) = (cfg.creds)() else { continue };
            let url = format!("{}/api/relay/usage", shadows_url.trim_end_matches('/'));
            current.sort_by(|a, b| a.hub_session_id.cmp(&b.hub_session_id));
            let changed = current != last_sent;
            if !changed && last_post.elapsed() < HEARTBEAT {
                continue;
            }
            let body = Report { machine_id: &cfg.machine_id, daemon_version: cfg.daemon_version, sessions: &current };
            match client.post(&url).bearer_auth(&token).json(&body).send().await {
                Ok(r) if r.status().is_success() => {
                    debug!(sessions = current.len(), "usage reported");
                    last_sent = current;
                    last_post = Instant::now();
                }
                Ok(r) if r.status().as_u16() == 404 => {
                    // PairWave without the endpoint yet: check back later.
                    unsupported_until = Some(Instant::now() + HEARTBEAT);
                }
                Ok(r) => warn!(status = %r.status(), "usage report rejected"),
                Err(e) => warn!(error = %e, "usage report failed"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "session_id": "cc-1", "transcript_path": "/secret/path.jsonl",
      "model": {"id": "claude-opus-5-5", "display_name": "Opus 5.5"},
      "workspace": {"current_dir": "/Users/x/private-repo"},
      "cost": {"total_cost_usd": 1.25},
      "context_window": {"context_window_size": 1000000, "used_percentage": 34.5},
      "rate_limits": {
        "five_hour": {"used_percentage": 12, "resets_at": 1790000000},
        "seven_day": {"used_percentage": 40.5, "resets_at": 1790500000}
      }
    }"#;

    #[test]
    fn extracts_only_usage_fields() {
        let u = extract_usage("hub-1", SAMPLE).unwrap();
        assert_eq!(u.model_id.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(u.context_used_percentage, Some(34.5));
        assert_eq!(u.context_window_size, Some(1_000_000));
        assert_eq!(u.cc_session_id.as_deref(), Some("cc-1"));
        assert_eq!(u.five_hour.as_ref().unwrap().used_percentage, 12.0);
        assert_eq!(u.seven_day.as_ref().unwrap().resets_at, Some(serde_json::json!(1790500000)));
        let json = serde_json::to_string(&u).unwrap();
        assert!(!json.contains("secret"), "paths must not be forwarded");
        assert!(!json.contains("private-repo"));
    }

    #[test]
    fn tolerates_missing_rate_limits() {
        let u = extract_usage("hub-1", r#"{"model":{"id":"claude-sonnet-5"}}"#).unwrap();
        assert!(u.five_hour.is_none() && u.seven_day.is_none());
        assert!(extract_usage("hub-1", "not json").is_none());
    }

    #[test]
    fn default_line_is_compact() {
        assert_eq!(default_status_line(SAMPLE), "Opus 5.5 · ctx 34% · 5h 12% · 7d 40%");
        assert_eq!(default_status_line("{}"), "");
    }

    #[test]
    fn settings_file_points_at_this_binary() {
        let dir = std::env::temp_dir().join(format!("relay-sl-test-{}", std::process::id()));
        let path = write_settings(&dir).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let cmd = v.pointer("/statusLine/command").and_then(Value::as_str).unwrap();
        assert!(cmd.contains(" statusline --out "));
        assert!(cmd.contains(STATUSLINE_FILE));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
