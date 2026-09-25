//! Auto-resume across Relay restarts.
//!
//! Restarting Relay (an update, a crash, a reboot) ends the Claude Code
//! processes it runs. Sessions the app marked `autoStart` + `preserveSessionId`
//! are respawned by the hub on reconnect under the SAME hub session id
//! (`session.respawn_preserving_id`), but each spawn pins a fresh Claude Code
//! session id, so the conversation itself would start over.
//!
//! So the daemon remembers, per hub session, the Claude Code session id it
//! last spawned (`~/.clawborrator/relay-sessions.json`), and the respawn adds
//! `--resume <that id>` (spawn_cc adds `--fork-session`), replacing any older
//! `--resume` carried in the row's original flags. A session with no
//! transcript yet just starts fresh.
//!
//! Ending a session on purpose (tray End, `relay end`) turns the hub's
//! `autoStart` off so it stays ended.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const MAX_ENTRIES: usize = 300;

#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    cc: String,
    at: String,
}

static LOCK: Mutex<()> = Mutex::new(());

fn state_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".clawborrator").join("relay-sessions.json"))
}

fn load() -> BTreeMap<String, Entry> {
    state_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Remember the Claude Code session id just spawned for `hub_sid`.
pub fn record(hub_sid: &str, cc_sid: &str) {
    let _g = LOCK.lock().unwrap();
    let Some(path) = state_path() else { return };
    let mut map = load();
    map.insert(hub_sid.to_string(), Entry { cc: cc_sid.to_string(), at: chrono::Utc::now().to_rfc3339() });
    if map.len() > MAX_ENTRIES {
        let mut by_age: Vec<(String, String)> = map.iter().map(|(k, v)| (v.at.clone(), k.clone())).collect();
        by_age.sort();
        for (_, k) in by_age.into_iter().take(map.len() - MAX_ENTRIES) {
            map.remove(&k);
        }
    }
    let tmp = path.with_extension("json.tmp");
    let ok = serde_json::to_vec_pretty(&map)
        .ok()
        .and_then(|b| std::fs::write(&tmp, b).ok())
        .and_then(|_| std::fs::rename(&tmp, &path).ok());
    if ok.is_none() {
        warn!(path = %path.display(), "could not save relay-sessions.json");
    }
}

fn is_resume_flag(f: &str) -> bool {
    matches!(f, "--resume" | "-r" | "--continue" | "-c" | "--fork-session")
        || f.starts_with("--resume=")
        || f.starts_with("--continue=")
}

/// PURE: `flags` with any resume/continue/fork flags replaced by
/// `--resume <cc_sid>`.
pub fn with_resume(flags: &[String], cc_sid: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(flags.len() + 2);
    let mut i = 0;
    while i < flags.len() {
        let f = flags[i].as_str();
        if is_resume_flag(f) {
            // `--resume <id>` / `-r <id>` take a value (unless it's another flag).
            let takes_value = matches!(f, "--resume" | "-r");
            i += 1;
            if takes_value && flags.get(i).is_some_and(|v| !v.starts_with('-')) {
                i += 1;
            }
            continue;
        }
        out.push(flags[i].clone());
        i += 1;
    }
    out.push("--resume".into());
    out.push(cc_sid.to_string());
    out
}

/// Flags for respawning `hub_sid` after a restart: resume its last
/// conversation when there's a transcript for it, else `flags` unchanged.
pub fn respawn_flags(hub_sid: &str, flags: &[String]) -> Vec<String> {
    let Some(entry) = load().get(hub_sid).cloned() else { return flags.to_vec() };
    let has_transcript = crate::statusline::claude_config_dir()
        .and_then(|root| crate::conversations::find_transcript(&root, &entry.cc))
        .is_some();
    if !has_transcript {
        return flags.to_vec();
    }
    info!(session_id = hub_sid, cc_session_id = %entry.cc, "respawn: resuming the previous conversation");
    with_resume(flags, &entry.cc)
}

/// Tell the hub not to bring this session back on the next restart
/// (it was ended on purpose). Best effort, off the caller's thread.
pub fn stop_auto_restart_in_background(hub_url: String, token: String, hub_sid: String) {
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        rt.block_on(async move {
            let url = format!("{}/api/v1/sessions/{}", hub_url.trim_end_matches('/'), hub_sid);
            let Ok(client) = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build() else { return };
            match client.patch(&url).bearer_auth(&token).json(&serde_json::json!({ "autoStart": false })).send().await {
                Ok(r) if r.status().is_success() => info!(session_id = %hub_sid, "auto-restart turned off for ended session"),
                Ok(r) => warn!(session_id = %hub_sid, status = %r.status(), "could not turn off auto-restart"),
                Err(e) => warn!(session_id = %hub_sid, error = ?e, "could not turn off auto-restart"),
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::with_resume;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn replaces_older_resume_flags() {
        assert_eq!(with_resume(&s(&["--model", "opus"]), "new"), s(&["--model", "opus", "--resume", "new"]));
        assert_eq!(with_resume(&s(&["--resume", "old", "--effort", "high"]), "new"), s(&["--effort", "high", "--resume", "new"]));
        assert_eq!(with_resume(&s(&["--resume=old", "--fork-session"]), "new"), s(&["--resume", "new"]));
        assert_eq!(with_resume(&s(&["-c", "--model", "x"]), "new"), s(&["--model", "x", "--resume", "new"]));
        assert_eq!(with_resume(&s(&["--resume", "--model", "x"]), "new"), s(&["--model", "x", "--resume", "new"]));
    }
}
