//! Updates the shadows app (PairWave) asks for.
//!
//! Every `TICK` the daemon checks in at `<shadows_url>/api/relay/checkin`
//! with its machine id and version (bearer = the paired token, like the
//! usage and conversation reports). The reply says whether to update:
//! `updateRequested` once ("Update now" in PairWave) or `autoUpdate` for
//! every new release.
//!
//! The reply only ever says *whether* to update. What gets installed is the
//! latest release from GitHub, through the same path as `relay update`
//! (update.rs); nothing about the download comes from the shadows app.
//! Relay waits until none of its sessions is mid-reply, since the restart
//! ends them (they come back on their own, resume_state.rs).
//!
//! Opt out with `RELAY_NO_REMOTE_UPDATE=1`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::sessions::SessionManager;
use crate::statusline::ReporterConfig;
use crate::update::{self, Release};

const FIRST: Duration = Duration::from_secs(45);
const TICK: Duration = Duration::from_secs(120);
/// How long an auto-update reuses a release lookup before asking GitHub again.
const RELEASE_TTL: Duration = Duration::from_secs(60 * 60);
/// Back off this long when the shadows app doesn't have the endpoint yet.
const UNSUPPORTED_BACKOFF: Duration = Duration::from_secs(30 * 60);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Checkin<'a> {
    machine_id: &'a str,
    daemon_version: &'a str,
}

#[derive(Deserialize, Default, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Reply {
    #[serde(default)]
    pub update_requested: bool,
    #[serde(default)]
    pub auto_update: bool,
}

fn disabled() -> bool {
    std::env::var("RELAY_NO_REMOTE_UPDATE").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false)
}

/// What to do this tick. PURE.
#[derive(Debug, PartialEq)]
pub enum Decision {
    Nothing,
    /// There's an update to install, but a session is mid-reply.
    Wait,
    Install,
}

pub fn decide(wanted: bool, newer: Option<&str>, failed: Option<&str>, requested: bool, busy: bool) -> Decision {
    let Some(v) = newer else { return Decision::Nothing };
    // A failed auto-update isn't retried for the same version; asking again is.
    if !wanted || (!requested && failed == Some(v)) {
        return Decision::Nothing;
    }
    if busy { Decision::Wait } else { Decision::Install }
}

pub fn spawn_update_checkin(mgr: Arc<SessionManager>, cfg: ReporterConfig) {
    if disabled() {
        info!("remote updates disabled (RELAY_NO_REMOTE_UPDATE)");
        return;
    }
    tokio::spawn(async move {
        let Some(mut client) = crate::statusline::report_client(Duration::from_secs(20)) else { return };
        // An "Update now" stays wanted until it's installed (or found moot),
        // even though the shadows app hands it over only once.
        let mut requested = false;
        let mut failed: Option<String> = None;
        let mut release: Option<(Instant, Option<Release>)> = None;
        tokio::time::sleep(FIRST).await;
        loop {
            let reply = match (cfg.creds)() {
                None => None,
                Some((shadows_url, token)) => {
                    let url = format!("{}/api/relay/checkin", shadows_url.trim_end_matches('/'));
                    let body = Checkin { machine_id: &cfg.machine_id, daemon_version: cfg.daemon_version };
                    match client.post(&url).bearer_auth(&token).json(&body).send().await {
                        Ok(r) if r.status().is_success() => r.json::<Reply>().await.ok(),
                        Ok(r) if r.status().as_u16() == 404 => {
                            debug!("shadows app has no check-in endpoint yet");
                            tokio::time::sleep(UNSUPPORTED_BACKOFF).await;
                            continue;
                        }
                        Ok(r) => {
                            warn!(status = %r.status(), "update check-in rejected");
                            None
                        }
                        Err(e) => {
                            debug!(error = ?e, "update check-in failed");
                            if let Some(c) = crate::statusline::report_client(Duration::from_secs(20)) { client = c; }
                            None
                        }
                    }
                }
            };
            let reply = reply.unwrap_or_default();
            if reply.update_requested {
                info!("update requested from the shadows app");
                requested = true;
            }
            let wanted = requested || reply.auto_update;
            if wanted {
                // Asked explicitly: look again now. Auto: at most hourly.
                let stale = match &release {
                    Some((at, _)) => reply.update_requested || at.elapsed() >= RELEASE_TTL,
                    None => true,
                };
                if stale {
                    match update::latest_release().await {
                        Ok(r) => release = Some((Instant::now(), r)),
                        Err(e) => warn!(error = %e, "couldn't look up the latest Relay release"),
                    }
                }
                let newer = release
                    .as_ref()
                    .and_then(|(_, r)| r.as_ref())
                    .filter(|r| update::is_newer(&r.version, update::CURRENT));
                if requested && newer.is_none() && release.is_some() {
                    info!("already on the latest Relay; nothing to update");
                    requested = false;
                }
                match decide(wanted, newer.map(|r| r.version.as_str()), failed.as_deref(), requested, mgr.any_busy()) {
                    Decision::Nothing => {}
                    Decision::Wait => debug!("update waiting until no session is mid-reply"),
                    Decision::Install => {
                        let r = newer.cloned().expect("decided to install a release");
                        info!(version = %r.version, requested, "installing Relay update");
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        // install_and_restart runs its own runtime; keep it off this one.
                        let r2 = r.clone();
                        std::thread::spawn(move || {
                            let _ = tx.send(update::install_and_restart(&r2).map_err(|e| e.to_string()));
                        });
                        match rx.await {
                            // The restart stops this daemon; on Windows it's up to us to go.
                            Ok(Ok(())) => {
                                if cfg!(target_os = "windows") {
                                    std::process::exit(0);
                                }
                            }
                            Ok(Err(e)) => {
                                warn!(error = %e, version = %r.version, "Relay update failed");
                                failed = Some(r.version.clone());
                                requested = false;
                            }
                            Err(_) => warn!("Relay update thread ended unexpectedly"),
                        }
                    }
                }
            }
            tokio::time::sleep(TICK).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_only_when_wanted_newer_and_idle() {
        assert_eq!(decide(true, Some("0.5.0"), None, true, false), Decision::Install);
        assert_eq!(decide(true, Some("0.5.0"), None, false, true), Decision::Wait);
        assert_eq!(decide(false, Some("0.5.0"), None, false, false), Decision::Nothing);
        assert_eq!(decide(true, None, None, true, false), Decision::Nothing);
    }

    #[test]
    fn a_failed_auto_update_waits_for_the_next_release_or_an_explicit_ask() {
        assert_eq!(decide(true, Some("0.5.0"), Some("0.5.0"), false, false), Decision::Nothing);
        assert_eq!(decide(true, Some("0.5.1"), Some("0.5.0"), false, false), Decision::Install);
        assert_eq!(decide(true, Some("0.5.0"), Some("0.5.0"), true, false), Decision::Install);
    }

    #[test]
    fn a_working_session_is_busy() {
        assert!(crate::sessions::screen_is_busy("✻ Thinking… (12s · ↓ 1.2k tokens · esc to interrupt)"));
        assert!(!crate::sessions::screen_is_busy("❯ \n  ⏵⏵ auto mode on (shift+tab to cycle)"));
    }

    #[test]
    fn reads_the_reply() {
        let r: Reply = serde_json::from_str(r#"{"updateRequested":true,"autoUpdate":false}"#).unwrap();
        assert_eq!(r, Reply { update_requested: true, auto_update: false });
        let r: Reply = serde_json::from_str("{}").unwrap();
        assert_eq!(r, Reply::default());
    }
}
