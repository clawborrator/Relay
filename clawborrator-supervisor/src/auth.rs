// Operator-facing authentication for shadows-desktop. The device-
// authorization pairing flow itself lives in `oauth.rs`; this module
// wraps it with the surrounding lifecycle:
//
//   - login: pair against the SHADOWS app, which returns (token,
//     hub_url). Cache both (+ the shadows_url we paired against) so the
//     daemon knows which hub to dial and which credential to use.
//   - logout: server-revoke the token against the cached hub then clear
//     the local cache.
//   - fetch_user_login: GET <hub>/api/v1/me to confirm a token works
//     and surface the resolved (shadow) login.

use anyhow::{anyhow, bail, Context, Result};
use url::Url;

use crate::oauth;
use crate::Config;

/// Result of `login`.
pub enum LoginOutcome {
    /// A cached token still authenticates against the cached hub.
    /// `cfg` was not mutated; no save needed.
    AlreadyLoggedIn { login: String, hub_url: String },
    /// Paired fresh: minted a token + learned the hub URL. `cfg` has
    /// been mutated; caller should save_config. `login` is the /me
    /// response, or None if /me failed (token still valid for WS
    /// register, just couldn't confirm identity post-mint).
    Authenticated   { login: Option<String>, hub_url: String },
}

/// Pair against the shadows app at `shadows_url`. On success updates
/// `cfg` in place (token + hub_url + shadows_url); the caller saves it.
pub async fn login(cfg: &mut Config, shadows_url: &str, force: bool) -> Result<LoginOutcome> {
    // Cached token still good for the hub it was issued for, and we
    // paired against this same shadows URL? Verify and short-circuit.
    if !force {
        if let (Some(token), Some(hub)) = (cfg.token.clone(), cfg.hub_url.clone()) {
            if cfg.shadows_url.as_deref() == Some(shadows_url) {
                if let Ok(login) = fetch_user_login(&hub, &token).await {
                    return Ok(LoginOutcome::AlreadyLoggedIn { login, hub_url: hub });
                }
            }
        }
    }

    // hostname makes a friendlier label on the approval screen than the
    // bare machine_id uuid; fall back to the machine_id.
    let label = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| cfg.machine_id.clone());

    let (token, hub_url) = oauth::run_device_flow(shadows_url, &label)
        .await
        .with_context(|| "shadows pairing failed")?;

    cfg.token       = Some(token.clone());
    cfg.hub_url     = Some(hub_url.clone());
    cfg.shadows_url = Some(shadows_url.to_string());
    let login = fetch_user_login(&hub_url, &token).await.ok();
    Ok(LoginOutcome::Authenticated { login, hub_url })
}

/// Result of `logout`. The local cache is cleared regardless of the
/// server-side revoke outcome.
pub enum LogoutOutcome {
    Revoked,
    NoCachedToken,
    RevokeFailed(anyhow::Error),
}

/// POST `<hub>/api/v1/auth/logout` to revoke the cached token
/// (best-effort) and clear token + hub_url + shadows_url. machine_id is
/// preserved so a subsequent `login` reuses the same install identity.
pub async fn logout(cfg: &mut Config) -> LogoutOutcome {
    let outcome = match (cfg.token.clone(), cfg.hub_url.clone()) {
        (Some(token), Some(hub)) => match revoke_token(&hub, &token).await {
            Ok(())  => LogoutOutcome::Revoked,
            Err(e)  => LogoutOutcome::RevokeFailed(e),
        },
        _ => LogoutOutcome::NoCachedToken,
    };
    cfg.token       = None;
    cfg.hub_url     = None;
    cfg.shadows_url = None;
    outcome
}

/// GET `<hub>/api/v1/me` — returns `githubLogin` (for a shadow principal
/// this is `shadow:<email>`). Confirms the token works.
pub async fn fetch_user_login(hub_url: &str, token: &str) -> Result<String> {
    let url = Url::parse(hub_url)?.join("/api/v1/me")?;
    let resp = reqwest::Client::new()
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .context("GET /api/v1/me")?;
    if !resp.status().is_success() {
        bail!("hub returned {}", resp.status());
    }
    let body: serde_json::Value = resp.json().await.context("parsing /api/v1/me response")?;
    body.get("githubLogin")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing githubLogin in /me response"))
}

/// POST `<hub>/api/v1/auth/logout` — revokes the bearer token.
async fn revoke_token(hub_url: &str, token: &str) -> Result<()> {
    let url = Url::parse(hub_url)?.join("/api/v1/auth/logout")?;
    let resp = reqwest::Client::new()
        .post(url)
        .bearer_auth(token)
        .send()
        .await
        .context("POST /api/v1/auth/logout")?;
    if !resp.status().is_success() {
        bail!("hub returned {}", resp.status());
    }
    Ok(())
}
