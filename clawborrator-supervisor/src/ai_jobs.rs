//! AI jobs the shadows app (PairWave) runs on the user's own Claude Code.
//!
//! PairWave's AI features (weekly reports, session summaries, second opinions
//! on approvals, operator notes) can use the user's Claude plan instead of an
//! API key. The daemon long-polls `<shadows_url>/api/relay/ai-jobs` (bearer =
//! the paired token), and for each job runs a one-shot, text-only Claude Code:
//!
//!   claude -p --tools "" --strict-mcp-config --setting-sources project
//!          --no-session-persistence --output-format json ...
//!
//! in an empty temp folder, so it has no tools, no MCP servers, no project
//! settings or CLAUDE.md and leaves no transcript. The reply text goes back
//! to `/api/relay/ai-jobs/result`. One job at a time.
//!
//! Opt out with `RELAY_NO_AI_JOBS=1`.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use crate::statusline::ReporterConfig;

const FIRST: Duration = Duration::from_secs(20);
/// Longer than the server's 25 s hold.
const POLL_TIMEOUT: Duration = Duration::from_secs(40);
const RETRY: Duration = Duration::from_secs(10);
/// Back off this long when the shadows app doesn't have the endpoint yet.
const UNSUPPORTED_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// PairWave gives up on a job after 120 s.
const RUN_TIMEOUT: Duration = Duration::from_secs(110);
const MAX_SYSTEM: usize = 8 * 1024;
const MAX_PROMPT: usize = 200_000;
const MAX_RESULT: usize = 64_000;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Poll<'a> {
    machine_id: &'a str,
}

#[derive(Deserialize, Debug)]
pub struct Job {
    pub id: String,
    #[serde(default)]
    pub system: String,
    pub prompt: String,
    #[serde(default)]
    pub model: String,
}

#[derive(Deserialize)]
struct PollReply {
    job: Option<Job>,
}

#[derive(Serialize)]
struct Outcome<'a> {
    id: &'a str,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn disabled() -> bool {
    std::env::var("RELAY_NO_AI_JOBS").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false)
}

/// A model alias or id safe to pass as an argument; anything else → sonnet. PURE.
pub fn safe_model(m: &str) -> &str {
    // Must start with a letter, so it can't be read as a flag.
    let ok = m.len() <= 40
        && m.starts_with(|c: char| c.is_ascii_lowercase())
        && m.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.');
    if ok { m } else { "sonnet" }
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[derive(Deserialize)]
struct CcOutput {
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    is_error: bool,
}

/// The reply text from `claude -p --output-format json`, or why it failed. PURE.
pub fn parse_output(stdout: &str, stderr: &str, exited_ok: bool) -> Result<String, String> {
    match serde_json::from_str::<CcOutput>(stdout.trim()) {
        Ok(o) if !o.is_error && exited_ok => o.result.map(|r| clip(&r, MAX_RESULT)).ok_or_else(|| "Claude Code returned no text".into()),
        Ok(o) => Err(clip(o.result.as_deref().filter(|r| !r.is_empty()).unwrap_or("Claude Code reported an error"), 300)),
        Err(_) => {
            let why = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
            Err(clip(if why.is_empty() { "Claude Code failed" } else { why }, 300))
        }
    }
}

async fn run_job(job: &Job) -> Result<String, String> {
    if job.prompt.len() > MAX_PROMPT || job.system.len() > MAX_SYSTEM {
        return Err("job too large".into());
    }
    let claude = crate::find_on_path("claude").ok_or("Claude Code (`claude`) isn't installed on this machine")?;
    let dir: PathBuf = std::env::temp_dir().join(format!("relay-ai-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("temp folder: {e}"))?;

    let mut cmd = tokio::process::Command::new(&claude);
    cmd.current_dir(&dir)
        .args(["-p", "--tools", "", "--strict-mcp-config", "--setting-sources", "project", "--no-session-persistence"])
        .args(["--output-format", "json", "--model", safe_model(&job.model)])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if !job.system.is_empty() {
        cmd.arg("--system-prompt").arg(&job.system);
    }
    // Same PATH sessions get, so a GUI-launched daemon finds Claude Code's deps.
    if let Some(home) = dirs::home_dir() {
        let mut dirs = crate::spawn::session_path_prepend_dirs(&home);
        if let Some(existing) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&existing));
        }
        if let Ok(joined) = std::env::join_paths(&dirs) {
            cmd.env("PATH", joined);
        }
    }

    let out = async {
        let mut child = cmd.spawn().map_err(|e| format!("couldn't start Claude Code: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(job.prompt.as_bytes()).await.map_err(|e| format!("stdin: {e}"))?;
        }
        child.wait_with_output().await.map_err(|e| format!("Claude Code: {e}"))
    };
    let res = match tokio::time::timeout(RUN_TIMEOUT, out).await {
        Err(_) => Err("Claude Code took too long".to_string()),
        Ok(Err(e)) => Err(e),
        Ok(Ok(o)) => parse_output(&String::from_utf8_lossy(&o.stdout), &String::from_utf8_lossy(&o.stderr), o.status.success()),
    };
    let _ = std::fs::remove_dir_all(&dir);
    res
}

pub fn spawn_ai_jobs(cfg: ReporterConfig) {
    if disabled() {
        info!("AI jobs disabled (RELAY_NO_AI_JOBS)");
        return;
    }
    tokio::spawn(async move {
        let Some(mut client) = crate::statusline::report_client(POLL_TIMEOUT) else { return };
        tokio::time::sleep(FIRST).await;
        loop {
            let Some((shadows_url, token)) = (cfg.creds)() else {
                tokio::time::sleep(RETRY * 6).await;
                continue;
            };
            let base = shadows_url.trim_end_matches('/').to_string();
            let poll = client
                .post(format!("{base}/api/relay/ai-jobs"))
                .bearer_auth(&token)
                .json(&Poll { machine_id: &cfg.machine_id })
                .send()
                .await;
            let job = match poll {
                Ok(r) if r.status().is_success() => match r.json::<PollReply>().await {
                    Ok(p) => p.job,
                    Err(e) => {
                        debug!(error = ?e, "bad AI job reply");
                        tokio::time::sleep(RETRY).await;
                        continue;
                    }
                },
                Ok(r) if r.status().as_u16() == 404 => {
                    debug!("shadows app has no AI job endpoint yet");
                    tokio::time::sleep(UNSUPPORTED_BACKOFF).await;
                    continue;
                }
                Ok(r) => {
                    warn!(status = %r.status(), "AI job poll rejected");
                    tokio::time::sleep(RETRY * 6).await;
                    continue;
                }
                Err(e) => {
                    debug!(error = ?e, "AI job poll failed");
                    if let Some(c) = crate::statusline::report_client(POLL_TIMEOUT) {
                        client = c;
                    }
                    tokio::time::sleep(RETRY).await;
                    continue;
                }
            };
            let Some(job) = job else { continue };
            info!(id = %job.id, "running an AI job for the shadows app");
            let res = run_job(&job).await;
            if let Err(e) = &res {
                warn!(id = %job.id, error = %e, "AI job failed");
            }
            let outcome = match &res {
                Ok(text) => Outcome { id: &job.id, ok: true, text: Some(text.clone()), error: None },
                Err(e) => Outcome { id: &job.id, ok: false, text: None, error: Some(e.clone()) },
            };
            if let Err(e) = client.post(format!("{base}/api/relay/ai-jobs/result")).bearer_auth(&token).json(&outcome).send().await {
                warn!(error = ?e, "couldn't send the AI job result");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_is_sanitized() {
        assert_eq!(safe_model("sonnet"), "sonnet");
        assert_eq!(safe_model("claude-sonnet-5-5"), "claude-sonnet-5-5");
        assert_eq!(safe_model("--dangerously-skip-permissions"), "sonnet");
        assert_eq!(safe_model("x; rm -rf"), "sonnet");
        assert_eq!(safe_model(""), "sonnet");
    }

    #[test]
    fn parses_claude_output() {
        let ok = r#"{"type":"result","is_error":false,"result":"{\"a\":1}"}"#;
        assert_eq!(parse_output(ok, "", true), Ok("{\"a\":1}".to_string()));
        let err = r#"{"type":"result","is_error":true,"result":"Not logged in"}"#;
        assert_eq!(parse_output(err, "", false), Err("Not logged in".to_string()));
        assert_eq!(parse_output("", "boom", false), Err("boom".to_string()));
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Runs the real Claude Code: `cargo test ai_jobs::live -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn runs_claude_code_without_tools() {
        let job = Job {
            id: "t".into(),
            system: "Reply with a single JSON object and nothing else.".into(),
            prompt: "Return {\"sum\": 2+2 as a number, \"tools\": the names of any tools you can call, as an array}.".into(),
            model: "sonnet".into(),
        };
        let text = run_job(&job).await.expect("claude ran");
        eprintln!("{text}");
        assert!(text.contains("\"sum\""));
    }
}
