//! Is Claude Code on this machine signed in?
//!
//! `claude auth status` says "loggedIn" even when the saved token has expired,
//! so the only real test is a request. Relay sends a tiny one (Haiku, no
//! tools, same isolation as ai_jobs.rs) 2 minutes after start and then every
//! 6 hours, every 15 minutes while signed out. AI jobs update the state too,
//! and a session screen showing Claude Code's own sign-in error triggers an
//! early check. The last result rides along in each check-in
//! (remote_update.rs), so the shadows app can warn before something fails.
//!
//! Opt out of the periodic request with `RELAY_NO_AUTH_CHECK=1`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tracing::{info, warn};

use crate::sessions::SessionManager;

const FIRST: Duration = Duration::from_secs(120);
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const WHILE_SIGNED_OUT: Duration = Duration::from_secs(15 * 60);
/// How often session screens are scanned for a sign-in error.
const SCAN: Duration = Duration::from_secs(120);
/// Don't re-check more often than this because of a screen.
const SCREEN_RECHECK: Duration = Duration::from_secs(10 * 60);

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AuthState {
    pub ok: bool,
    /// RFC 3339.
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

static STATE: Mutex<Option<AuthState>> = Mutex::new(None);
static LAST_CHECK: Mutex<Option<Instant>> = Mutex::new(None);

pub fn record(ok: bool, error: Option<String>) {
    let prev = STATE.lock().unwrap().as_ref().map(|s| s.ok);
    if prev != Some(ok) {
        if ok { info!("Claude Code is signed in") } else { warn!(error = ?error, "Claude Code isn't signed in") }
    }
    *STATE.lock().unwrap() = Some(AuthState { ok, at: chrono::Utc::now().to_rfc3339(), error });
}

pub fn snapshot() -> Option<AuthState> {
    STATE.lock().unwrap().clone()
}

/// Does a Claude Code error mean it isn't signed in? PURE.
pub fn is_auth_error(e: &str) -> bool {
    let l = e.to_ascii_lowercase();
    ["authenticat", "oauth", "not logged in", "/login", "401", "invalid api key", "credential"].iter().any(|k| l.contains(k))
}

/// Does a session screen show Claude Code's own sign-in error? PURE.
pub fn screen_signed_out(screen: &str) -> bool {
    let l = screen.to_ascii_lowercase();
    l.contains("token has expired") || l.contains("please run /login") || l.contains("invalid api key") || l.contains("failed to authenticate")
}

fn disabled() -> bool {
    std::env::var("RELAY_NO_AUTH_CHECK").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false)
}

/// Send Claude Code a tiny request and record whether it's signed in.
pub async fn check_now() -> AuthState {
    *LAST_CHECK.lock().unwrap() = Some(Instant::now());
    let job = crate::ai_jobs::Job {
        id: "auth-check".into(),
        kind: String::new(),
        system: "Reply with the single word ok.".into(),
        prompt: "ok?".into(),
        model: "haiku".into(),
    };
    match crate::ai_jobs::run_job(&job).await {
        Ok(_) => record(true, None),
        Err(e) if is_auth_error(&e) => record(false, Some(e)),
        // Couldn't tell (offline, Claude Code missing, timeout): keep what we knew.
        Err(e) => warn!(error = %e, "Claude Code sign-in check inconclusive"),
    }
    snapshot().unwrap_or(AuthState { ok: false, at: chrono::Utc::now().to_rfc3339(), error: Some("not checked yet".into()) })
}

pub fn spawn_auth_health(mgr: Arc<SessionManager>) {
    if disabled() {
        info!("Claude Code sign-in check disabled (RELAY_NO_AUTH_CHECK)");
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(FIRST).await;
        check_now().await;
        let mut next = Instant::now() + EVERY;
        loop {
            tokio::time::sleep(SCAN).await;
            let signed_in = snapshot().map(|s| s.ok).unwrap_or(false);
            let due = Instant::now() >= next;
            let recent = LAST_CHECK.lock().unwrap().map(|t| t.elapsed() < SCREEN_RECHECK).unwrap_or(false);
            let screen_says = signed_in && !recent && mgr.any_screen(screen_signed_out);
            if due || screen_says {
                let s = check_now().await;
                next = Instant::now() + if s.ok { EVERY } else { WHILE_SIGNED_OUT };
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_sign_in_errors() {
        assert!(is_auth_error("Failed to authenticate. API Error: 401 OAuth access token has expired."));
        assert!(is_auth_error("Invalid API key · Please run /login"));
        assert!(!is_auth_error("Claude Code took too long"));
        assert!(screen_signed_out("  ⎿  API Error: 401 · OAuth token has expired. Please run /login"));
        assert!(!screen_signed_out("> write the tests\n  esc to interrupt"));
    }
}
