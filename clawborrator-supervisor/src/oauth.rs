// Device-authorization pairing for shadows-desktop.
//
// Unlike the upstream desktop_v1 (which device-flows against the hub's
// GitHub OAuth), shadows-desktop pairs against the SHADOWS app. The user
// is already authenticated to shadows with Google/Zoho; they enter the
// user_code there, shadows brokers a cw_app_ token for their hub shadow
// principal, and hands it back here ALONG WITH the hub URL to connect to.
//
// So this flow returns (token, hub_url): shadows tells the daemon both
// which credential to use and which hub to dial. The hub never sees
// Google/Zoho. See hub_v1/docs/SHADOWS-SCOPE.md.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;
use url::Url;

const APP_NAME: &str = "shadows-desktop";

#[derive(Serialize, Debug)]
struct DeviceCodeBody<'a> {
    machine_label: &'a str,
}

#[derive(Deserialize, Debug)]
struct DeviceCodeResp {
    device_code:               String,
    user_code:                 String,
    verification_uri:          String,
    verification_uri_complete: String,
    expires_in:                u64,
    interval:                  u64,
}

#[derive(Serialize, Debug)]
struct DevicePollBody<'a> {
    device_code: &'a str,
}

#[derive(Deserialize, Debug)]
struct DevicePollSuccess {
    access_token: String,
    #[allow(dead_code)]
    token_type:   String,
    // shadows tells us which hub this token is for.
    hub_url:      String,
}

#[derive(Deserialize, Debug)]
struct DevicePollError {
    error: String,
}

/// What the user must do to pair, plus the bits the caller needs to poll.
/// Returned by `request_device_code` so a GUI can render the code itself
/// instead of the console.
pub struct PairingPrompt {
    /// Secret long code, used by the caller to poll /device/token.
    pub device_code:               String,
    /// Short code the user types into the shadows web UI.
    pub user_code:                 String,
    /// Pairing page URL (no code pre-filled).
    pub verification_uri:          String,
    /// Single-link shortcut with the code pre-filled.
    pub verification_uri_complete: String,
    /// Suggested seconds between polls.
    pub interval:                  u64,
    /// Seconds until the code expires.
    pub expires_in:                u64,
}

/// Outcome of a single /device/token poll.
pub enum PollStep {
    /// Approved - here's the brokered token + the hub URL shadows chose.
    Approved { token: String, hub_url: String },
    /// authorization_pending / slow_down / transient parse error: keep going.
    KeepPolling,
}

/// The reqwest client the device flow uses (shared user-agent).
pub fn device_flow_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("{APP_NAME}/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building device-flow http client")
}

/// Step 1: request a device_code + user_code from the shadows app.
pub async fn request_device_code(
    client: &reqwest::Client,
    shadows_url: &str,
    machine_label: &str,
) -> Result<PairingPrompt> {
    let code_url = Url::parse(shadows_url)?.join("/device/code")?;
    let code_resp = client.post(code_url)
        .json(&DeviceCodeBody { machine_label })
        .send()
        .await
        .context("POST /device/code")?;
    if !code_resp.status().is_success() {
        let status = code_resp.status();
        let body = code_resp.text().await.unwrap_or_default();
        bail!("device-code request failed: {status} {body}");
    }
    let dc: DeviceCodeResp = code_resp.json().await.context("parsing /device/code response")?;
    Ok(PairingPrompt {
        device_code:               dc.device_code,
        user_code:                 dc.user_code,
        verification_uri:          dc.verification_uri,
        verification_uri_complete: dc.verification_uri_complete,
        interval:                  dc.interval,
        expires_in:                dc.expires_in,
    })
}

/// Step 3 (one iteration): POST /device/token and classify the result.
/// Terminal errors (expired / denied / invalid) return Err; `slow_down`
/// bumps `effective_interval`.
pub async fn poll_device_token(
    client: &reqwest::Client,
    token_url: &Url,
    device_code: &str,
    effective_interval: &mut Duration,
) -> Result<PollStep> {
    let resp = client.post(token_url.clone())
        .json(&DevicePollBody { device_code })
        .send()
        .await
        .context("POST /device/token")?;

    if resp.status().is_success() {
        let body: DevicePollSuccess = resp.json().await.context("parsing /device/token success response")?;
        return Ok(PollStep::Approved { token: body.access_token, hub_url: body.hub_url });
    }

    // RFC 8628-style typed errors. authorization_pending + slow_down are
    // normal "keep polling" states; the rest are terminal.
    let body: DevicePollError = match resp.json().await {
        Ok(b) => b,
        Err(e) => {
            warn!(?e, "could not parse /device/token error body; treating as transient");
            return Ok(PollStep::KeepPolling);
        }
    };
    match body.error.as_str() {
        "authorization_pending" => Ok(PollStep::KeepPolling),
        "slow_down"             => { *effective_interval += Duration::from_secs(5); Ok(PollStep::KeepPolling) }
        "expired_token"         => bail!("device code expired; restart `login` to mint a fresh code"),
        "access_denied"         => bail!("approval denied"),
        "invalid_grant"         => bail!("device code invalid or already consumed; restart `login`"),
        other                   => bail!("unexpected device flow error: {other}"),
    }
}

/// Run the shadows device-authorization pairing flow against
/// `shadows_url` from the CONSOLE (the `login` subcommand). Returns
/// (token, hub_url) once the user approves the code. The GUI path
/// (gui::run_first_run_wizard) reuses the same steps without the
/// eprintln instructions.
pub async fn run_device_flow(shadows_url: &str, machine_label: &str) -> Result<(String, String)> {
    let client = device_flow_client()?;
    let p = request_device_code(&client, shadows_url, machine_label).await?;

    eprintln!();
    eprintln!("============================================================");
    eprintln!("To pair this machine, open the shadows app and sign in, then");
    eprintln!("go to Pair a machine and enter this code:");
    eprintln!();
    eprintln!("    {}", p.user_code);
    eprintln!();
    eprintln!("Or open this single-link shortcut (code pre-filled):");
    eprintln!();
    eprintln!("    {}", p.verification_uri_complete);
    eprintln!();
    eprintln!("(pairing page: {})", p.verification_uri);
    eprintln!("Waiting for approval (expires in {}s)...", p.expires_in);
    eprintln!("============================================================");

    let token_url = Url::parse(shadows_url)?.join("/device/token")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(p.expires_in);
    let mut effective_interval = Duration::from_secs(p.interval.max(1));

    loop {
        if tokio::time::Instant::now() > deadline {
            bail!("device code expired before approval completed");
        }
        tokio::time::sleep(effective_interval).await;
        match poll_device_token(&client, &token_url, &p.device_code, &mut effective_interval).await? {
            PollStep::Approved { token, hub_url } => {
                eprintln!();
                eprintln!("Approved. Connecting to {}.", hub_url);
                return Ok((token, hub_url));
            }
            PollStep::KeepPolling => continue,
        }
    }
}
