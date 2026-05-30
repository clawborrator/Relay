//! Per-session token-usage sampler.
//!
//! While a managed Claude Code session runs, this samples CC's
//! transcript jsonl once per wall-clock hour and POSTs the cumulative
//! per-model token counts to the hub
//! (POST /api/v1/sessions/:id/token-usage). One final snapshot is
//! posted when the session ends — the sampler's cancel handle fires
//! from destroy / kill / soft-restart.
//!
//! CC writes its transcript to
//!   <claude-config>/projects/<cwd-hash>/<cc-session-id>.jsonl
//! one JSON object per line. spawn_cc passes `--session-id
//! <cc-session-id>` so the filename is known; the cwd-hash is
//! CC-internal, so the file is located by globbing for the basename.
//!
//! Counters are cumulative per (session, model): the sampler sums
//! `message.usage` across every assistant record. Each managed-session
//! restart hands CC a fresh `--session-id`, so a soft-restart or a
//! post-reboot respawn begins a new cumulative series, closed by the
//! prior incarnation's reason=final snapshot. The hub stores rows
//! append-only — see hub_v1 migration 0024.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{Timelike, Utc};
use serde::Serialize;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

/// One model's summed token counters for a session.
#[derive(Clone, Copy, Default)]
struct TokenCounters {
    input:          u64,
    output:         u64,
    cache_read:     u64,
    cache_creation: u64,
}

/// Cancel handle for a per-session token-usage sampler. Mirrors
/// WatcherHandle: cancelling signals the task to post a final
/// snapshot and exit.
pub struct TokenUsageSamplerHandle {
    cancel_tx: Option<oneshot::Sender<()>>,
}

impl TokenUsageSamplerHandle {
    /// Signal the sampler to post a final (reason=final) snapshot and
    /// stop. Idempotent — a second call is a no-op.
    pub fn cancel(&mut self) {
        if let Some(tx) = self.cancel_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Resolve Claude Code's config dir: $CLAUDE_CONFIG_DIR if set,
/// else ~/.claude.
fn claude_config_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        let p = PathBuf::from(d);
        if !p.as_os_str().is_empty() { return Some(p); }
    }
    dirs::home_dir().map(|h| h.join(".claude"))
}

/// Locate CC's transcript for `cc_session_id`. CC stores it at
/// <config>/projects/<cwd-hash>/<cc_session_id>.jsonl; the cwd-hash is
/// CC-internal, so every project dir is scanned for the basename.
fn find_transcript(cc_session_id: &str) -> Option<PathBuf> {
    let projects = claude_config_dir()?.join("projects");
    let file = format!("{cc_session_id}.jsonl");
    for entry in std::fs::read_dir(&projects).ok()?.flatten() {
        let candidate = entry.path().join(&file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Sum per-model token usage from a CC transcript jsonl. Each line is
/// one JSON object; assistant records carry `message.model` and
/// `message.usage`. Malformed lines are skipped. `usage` is per-turn,
/// so the sum across turns is the cumulative session total.
fn sum_transcript(path: &Path) -> HashMap<String, TokenCounters> {
    let mut totals: HashMap<String, TokenCounters> = HashMap::new();
    let raw = match std::fs::read_to_string(path) {
        Ok(s)  => s,
        Err(e) => {
            warn!(?e, path = %path.display(), "token-usage: cannot read transcript");
            return totals;
        }
    };
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v)  => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") { continue; }
        let msg = match v.get("message") { Some(m) => m, None => continue };
        let model = match msg.get("model").and_then(|m| m.as_str()) {
            Some(m) => m.to_string(),
            None    => continue,
        };
        let usage = match msg.get("usage") { Some(u) => u, None => continue };
        let field = |k: &str| usage.get(k).and_then(|n| n.as_u64()).unwrap_or(0);
        let e = totals.entry(model).or_default();
        e.input          += field("input_tokens");
        e.output         += field("output_tokens");
        e.cache_read     += field("cache_read_input_tokens");
        e.cache_creation += field("cache_creation_input_tokens");
    }
    totals
}

#[derive(Serialize)]
struct ModelUsageOut {
    model: String,
    #[serde(rename = "inputTokens")]         input_tokens:          u64,
    #[serde(rename = "outputTokens")]        output_tokens:         u64,
    #[serde(rename = "cacheReadTokens")]     cache_read_tokens:     u64,
    #[serde(rename = "cacheCreationTokens")] cache_creation_tokens: u64,
}

#[derive(Serialize)]
struct SnapshotBody {
    #[serde(rename = "capturedAt")]  captured_at:   String,
    #[serde(rename = "ccSessionId")] cc_session_id: String,
    reason: String,
    models: Vec<ModelUsageOut>,
}

/// Collect the session's current per-model totals and POST one
/// snapshot to the hub. Best-effort: errors are logged, not
/// propagated — the next hourly tick (or the hub's grain unique
/// index) makes a missed or retried post harmless. A snapshot with no
/// model rows (transcript not found yet, or no assistant turns) is
/// skipped — the hub requires at least one model.
async fn post_snapshot(
    hub_url:        &str,
    pat:            &str,
    hub_session_id: &str,
    cc_session_id:  &str,
    reason:         &str,
    captured_at:    String,
) {
    let totals = match find_transcript(cc_session_id) {
        Some(path) => sum_transcript(&path),
        None       => HashMap::new(),
    };
    if totals.is_empty() {
        debug!(hub_session_id, reason, "token-usage: nothing to report this tick");
        return;
    }
    let models: Vec<ModelUsageOut> = totals.into_iter().map(|(model, c)| ModelUsageOut {
        model,
        input_tokens:          c.input,
        output_tokens:         c.output,
        cache_read_tokens:     c.cache_read,
        cache_creation_tokens: c.cache_creation,
    }).collect();
    let body = SnapshotBody {
        captured_at,
        cc_session_id: cc_session_id.to_string(),
        reason: reason.to_string(),
        models,
    };

    let url = format!(
        "{}/api/v1/sessions/{}/token-usage",
        hub_url.trim_end_matches('/'), hub_session_id,
    );
    let result = async {
        let client = reqwest::Client::builder()
            .user_agent(format!("clawborrator-supervisor/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(15))
            .build()?;
        let resp = client.post(&url).bearer_auth(pat).json(&body).send().await?;
        if !resp.status().is_success() {
            let s = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            anyhow::bail!("{s} {txt}");
        }
        anyhow::Ok(())
    }.await;
    match result {
        Ok(())  => info!(hub_session_id, reason, "token-usage: snapshot posted"),
        Err(e)  => warn!(hub_session_id, reason, error = %e, "token-usage: snapshot post failed"),
    }
}

/// Seconds from now until the next top-of-hour (UTC). Never 0: exactly
/// on the hour returns a full 3600 so the loop does not busy-spin.
fn secs_to_next_hour() -> u64 {
    let now = Utc::now();
    let into_hour = now.minute() as u64 * 60 + now.second() as u64;
    3600 - into_hour
}

/// The current top-of-hour (UTC) as RFC3339. Used as an hourly
/// snapshot's capturedAt so buckets align across sessions.
fn top_of_hour_rfc3339() -> String {
    let now = Utc::now();
    now.with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(now)
        .to_rfc3339()
}

/// Spawn the per-session token-usage sampler. Posts a `reason=hourly`
/// snapshot at every wall-clock hour boundary while the session runs,
/// and a `reason=final` snapshot when the returned handle is
/// cancelled. The task owns the hub URL + PAT for its lifetime.
pub fn spawn_token_usage_sampler(
    hub_session_id: String,
    cc_session_id:  String,
    hub_url:        String,
    pat:            String,
) -> TokenUsageSamplerHandle {
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    tokio::spawn(async move {
        info!(hub_session_id = %hub_session_id, cc_session_id = %cc_session_id,
              "token-usage sampler started");
        loop {
            let wait = Duration::from_secs(secs_to_next_hour());
            tokio::select! {
                _ = tokio::time::sleep(wait) => {
                    post_snapshot(&hub_url, &pat, &hub_session_id, &cc_session_id,
                                  "hourly", top_of_hour_rfc3339()).await;
                }
                _ = &mut cancel_rx => {
                    post_snapshot(&hub_url, &pat, &hub_session_id, &cc_session_id,
                                  "final", Utc::now().to_rfc3339()).await;
                    info!(hub_session_id = %hub_session_id, "token-usage sampler stopped");
                    return;
                }
            }
        }
    });
    TokenUsageSamplerHandle { cancel_tx: Some(cancel_tx) }
}
