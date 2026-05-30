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

/// Run the shadows device-authorization pairing flow against
/// `shadows_url`. Returns (token, hub_url) once the user approves the
/// code in the shadows web UI.
pub async fn run_device_flow(shadows_url: &str, machine_label: &str) -> Result<(String, String)> {
    let client = reqwest::Client::builder()
        .user_agent(format!("{APP_NAME}/{}", env!("CARGO_PKG_VERSION")))
        .build()?;

    // Step 1: request a device_code + user_code.
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

    // Step 2: tell the operator what to do.
    eprintln!();
    eprintln!("============================================================");
    eprintln!("To pair this machine, open the shadows app and sign in, then");
    eprintln!("go to Pair a machine and enter this code:");
    eprintln!();
    eprintln!("    {}", dc.user_code);
    eprintln!();
    eprintln!("Or open this single-link shortcut (code pre-filled):");
    eprintln!();
    eprintln!("    {}", dc.verification_uri_complete);
    eprintln!();
    eprintln!("(pairing page: {})", dc.verification_uri);
    eprintln!("Waiting for approval (expires in {}s)...", dc.expires_in);
    eprintln!("============================================================");

    // Step 3: poll /device/token until approved / expired.
    let token_url = Url::parse(shadows_url)?.join("/device/token")?;
    let interval = Duration::from_secs(dc.interval.max(1));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(dc.expires_in);
    let mut effective_interval = interval;

    loop {
        if tokio::time::Instant::now() > deadline {
            bail!("device code expired before approval completed");
        }
        tokio::time::sleep(effective_interval).await;

        let resp = client.post(token_url.clone())
            .json(&DevicePollBody { device_code: &dc.device_code })
            .send()
            .await
            .context("POST /device/token")?;

        if resp.status().is_success() {
            let body: DevicePollSuccess = resp.json().await.context("parsing /device/token success response")?;
            eprintln!();
            eprintln!("Approved. Connecting to {}.", body.hub_url);
            return Ok((body.access_token, body.hub_url));
        }

        // RFC 8628-style typed errors. authorization_pending + slow_down
        // are normal "keep polling" states; the rest are terminal.
        let body: DevicePollError = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                warn!(?e, "could not parse /device/token error body; treating as transient");
                continue;
            }
        };
        match body.error.as_str() {
            "authorization_pending" => continue,
            "slow_down"             => { effective_interval += Duration::from_secs(5); continue; }
            "expired_token"         => bail!("device code expired; restart `login` to mint a fresh code"),
            "access_denied"         => bail!("approval denied"),
            "invalid_grant"         => bail!("device code invalid or already consumed; restart `login`"),
            other                   => bail!("unexpected device flow error: {other}"),
        }
    }
}
