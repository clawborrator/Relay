//! Self-update from GitHub releases (clawborrator/Relay).
//!
//! The daemon checks the latest release at start and every few hours. When
//! a newer version exists the tray shows "Update to Relay vX.Y.Z" (the
//! label says how many running sessions the restart will end); headless
//! installs log it and `relay update` does the same thing from a shell.
//!
//! Installing replaces this install in place and restarts it through the
//! same facility that started it:
//!   macOS   — Relay.app is swapped for the one in the release .dmg, then
//!             `launchctl kickstart -k` restarts the LaunchAgent (or the
//!             bundle is reopened when it isn't running under launchd).
//!   Linux   — the binary is replaced (rename over a running file is fine),
//!             then `systemctl --user restart relay.service` (or the caller
//!             restarts it by hand when not running under systemd).
//!   Windows — the running .exe is renamed aside (allowed while running),
//!             the new one written in its place, and a detached helper
//!             re-runs the "Relay" scheduled task once this process exits.
//!
//! Downloads come over HTTPS from GitHub, the same place the installers
//! are published. Restarting ends the sessions this daemon is running.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

const REPO: &str = "clawborrator/Relay";
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version:   String,
    /// Download URL for this platform's asset, when the release has one.
    pub asset_url: Option<String>,
    pub page_url:  String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Installing(String),
    Failed(String),
}

/// Shared update state: written by the checker / installer threads, read
/// by the tray to render its menu item.
#[derive(Clone)]
pub struct Updater(Arc<Mutex<Phase>>);

impl Updater {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Phase::Idle)))
    }

    pub fn phase(&self) -> Phase {
        self.0.lock().unwrap().clone()
    }

    fn set(&self, p: Phase) {
        *self.0.lock().unwrap() = p;
    }

    /// Check now, on a background thread. `quiet`: a failed periodic check
    /// shouldn't replace a known state with an error in the menu.
    pub fn check_in_background(&self, quiet: bool) {
        if matches!(self.phase(), Phase::Checking | Phase::Installing(_)) {
            return;
        }
        let prev = self.phase();
        if !quiet {
            self.set(Phase::Checking);
        }
        let me = self.clone();
        std::thread::spawn(move || match block_on(latest_release()) {
            Ok(Some(r)) if is_newer(&r.version, CURRENT) => {
                info!(latest = %r.version, current = CURRENT, "Relay update available");
                me.set(Phase::Available(r));
            }
            Ok(_) => me.set(Phase::UpToDate),
            Err(e) => {
                warn!(error = %e, "update check failed");
                me.set(if quiet { prev } else { Phase::Failed("Couldn't check for updates".into()) });
            }
        });
    }

    /// Periodic background checks for the daemon's lifetime.
    pub fn start_periodic(&self) {
        let me = self.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(60));
            if !matches!(me.phase(), Phase::Available(_) | Phase::Installing(_)) {
                me.check_in_background(true);
            }
            std::thread::sleep(CHECK_EVERY);
        });
    }

    /// Install the available release on a background thread, then restart.
    pub fn install_in_background(&self) {
        let Phase::Available(release) = self.phase() else { return };
        self.set(Phase::Installing(release.version.clone()));
        let me = self.clone();
        std::thread::spawn(move || {
            match install_and_restart(&release) {
                // The restart stops this daemon; on Windows it's up to us to go.
                Ok(()) => {
                    if cfg!(target_os = "windows") {
                        std::process::exit(0);
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Relay update failed");
                    me.set(Phase::Failed(format!("Update failed: {e}")));
                }
            }
        });
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("building update runtime")
        .block_on(f)
}

fn client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("relay/{CURRENT}"))
        .timeout(timeout)
        .build()
        .context("building HTTP client")
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft:    bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets:   Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name:                 String,
    browser_download_url: String,
}

/// This platform's release asset name, if Relay publishes one.
fn asset_name() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("relay-macos-arm64.dmg")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("relay-linux-x64")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("relay-windows-x64.exe")
    } else {
        None
    }
}

pub async fn latest_release() -> Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let r = client(Duration::from_secs(20))?
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    if r.status().as_u16() == 404 {
        return Ok(None);
    }
    let r: GhRelease = r.error_for_status()?.json().await?;
    if r.draft || r.prerelease {
        return Ok(None);
    }
    let asset_url = asset_name()
        .and_then(|n| r.assets.iter().find(|a| a.name == n))
        .map(|a| a.browser_download_url.clone());
    Ok(Some(Release {
        version: r.tag_name.trim_start_matches('v').to_string(),
        asset_url,
        page_url: r.html_url,
    }))
}

fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next().unwrap_or(Some(0))?, it.next().unwrap_or(Some(0))?))
}

/// True when `latest` is a strictly newer version than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse(latest), parse(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

async fn download(url: &str, to: &Path) -> Result<()> {
    let bytes = client(Duration::from_secs(600))?
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    if bytes.len() < 1024 {
        bail!("download was unexpectedly small ({} bytes)", bytes.len());
    }
    std::fs::write(to, &bytes).with_context(|| format!("writing {}", to.display()))?;
    Ok(())
}

/// A fresh, private staging dir under the user's own cache dir (never a
/// shared /tmp, where another user could pre-create it and swap the
/// download). The leaf is a new random name created exclusively.
fn scratch_dir() -> Result<PathBuf> {
    let base = dirs::cache_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".cache")))
        .ok_or_else(|| anyhow!("could not resolve a cache dir"))?
        .join("relay-updates");
    std::fs::create_dir_all(&base).with_context(|| format!("creating {}", base.display()))?;
    let d = base.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir(&d).with_context(|| format!("creating {}", d.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(d)
}

/// Download + install `release` over this install, then restart the Relay
/// daemon into it. When called from the daemon itself, a successful
/// restart ends this process.
pub fn install_and_restart(release: &Release) -> Result<()> {
    let url = release
        .asset_url
        .as_deref()
        .ok_or_else(|| anyhow!("no download for this platform; get it from {}", release.page_url))?;
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = scratch_dir()?;
    info!(version = %release.version, "downloading Relay update");
    let file = dir.join(asset_name().unwrap_or("relay-update"));
    block_on(download(url, &file))?;
    platform::install(&exe, &file, &dir)?;
    info!(version = %release.version, "Relay update installed; restarting");
    platform::restart(&exe)
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::process::Command;

    fn bundle_of(exe: &Path) -> Option<PathBuf> {
        let contents = exe.parent()?.parent()?;
        let bundle = contents.parent()?;
        (bundle.extension()? == "app").then(|| bundle.to_path_buf())
    }

    fn run(cmd: &mut Command) -> Result<String> {
        let out = cmd.output()?;
        if !out.status.success() {
            bail!("{:?}: {}", cmd, String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    pub fn install(exe: &Path, dmg: &Path, dir: &Path) -> Result<()> {
        let bundle = bundle_of(exe).ok_or_else(|| anyhow!("this Relay isn't running from Relay.app, so it can't update itself"))?;
        let mnt = dir.join("mnt");
        std::fs::create_dir_all(&mnt)?;
        run(Command::new("hdiutil").args(["attach", "-nobrowse", "-quiet", "-readonly", "-mountpoint"]).arg(&mnt).arg(dmg))?;
        let staged = bundle.with_extension("app.new");
        let result = (|| -> Result<()> {
            let app = std::fs::read_dir(&mnt)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .find(|p| p.extension().is_some_and(|x| x == "app"))
                .ok_or_else(|| anyhow!("no .app inside the update disk image"))?;
            let _ = std::fs::remove_dir_all(&staged);
            run(Command::new("ditto").arg(&app).arg(&staged))?;
            Ok(())
        })();
        let _ = Command::new("hdiutil").args(["detach", "-quiet"]).arg(&mnt).status();
        result?;
        // Swap: old aside, new in place, old removed.
        let old = bundle.with_extension("app.old");
        let _ = std::fs::remove_dir_all(&old);
        std::fs::rename(&bundle, &old).context("moving the old Relay.app aside")?;
        if let Err(e) = std::fs::rename(&staged, &bundle) {
            let _ = std::fs::rename(&old, &bundle);
            return Err(e).context("moving the new Relay.app into place");
        }
        let _ = std::fs::remove_dir_all(&old);
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    pub fn restart(exe: &Path) -> Result<()> {
        let uid = run(Command::new("id").arg("-u"))?.trim().to_string();
        let target = format!("gui/{uid}/com.clawborrator.supervisor");
        let under_launchd = Command::new("launchctl").args(["print", &target]).output().is_ok_and(|o| o.status.success());
        if under_launchd {
            // Stops the running daemon (possibly this process) and starts
            // the updated agent again.
            run(Command::new("launchctl").args(["kickstart", "-k", &target]))?;
            return Ok(());
        }
        let _ = exe;
        bail!("updated; quit Relay and open it again to run the new version")
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    pub fn install(exe: &Path, bin: &Path, dir: &Path) -> Result<()> {
        std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755))?;
        let staged = exe.with_extension("new");
        std::fs::copy(bin, &staged).with_context(|| format!("writing {}", staged.display()))?;
        std::fs::rename(&staged, exe).with_context(|| format!("replacing {}", exe.display()))?;
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    pub fn restart(_exe: &Path) -> Result<()> {
        // Same `systemctl --user` wrapper as autostart (sets XDG_RUNTIME_DIR
        // for SSH sessions without it); a non-zero exit is an Err.
        use crate::autostart::run_systemctl_user;
        if run_systemctl_user(&["is-active", "--quiet", "relay.service"]).is_ok() {
            // Stops the running daemon (possibly this process) and starts
            // the updated binary.
            run_systemctl_user(&["restart", "relay.service"])
                .context("updated, but restarting relay.service failed; restart Relay to run the new version")?;
            return Ok(());
        }
        bail!("updated; restart Relay to run the new version")
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    pub fn install(exe: &Path, new_exe: &Path, dir: &Path) -> Result<()> {
        // Stage the complete new exe next to the old one first, so a
        // failed write never touches the install. Then swap: a running
        // .exe can't be overwritten but can be renamed.
        let staged = exe.with_extension("new.exe");
        let _ = std::fs::remove_file(&staged);
        if let Err(e) = std::fs::copy(new_exe, &staged) {
            let _ = std::fs::remove_file(&staged);
            return Err(e).context("writing the new relay.exe");
        }
        let old = exe.with_extension("old.exe");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old).context("moving the running relay.exe aside")?;
        if let Err(e) = std::fs::rename(&staged, exe) {
            let _ = std::fs::rename(&old, exe);
            return Err(e).context("moving the new relay.exe into place");
        }
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    pub fn restart(exe: &Path) -> Result<()> {
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // After a short wait (so a daemon calling this has exited), stop
        // any still-running daemon — the scheduled task, or one started by
        // hand (any other process of this exe, e.g. when `relay update` ran
        // from a shell) — then start the task again, or the exe directly
        // when there's no task.
        let image = exe.file_name().and_then(|n| n.to_str()).unwrap_or("relay.exe");
        let script = format!(
            "ping -n 4 127.0.0.1 >nul & schtasks /End /TN Relay >nul 2>&1 & taskkill /F /FI \"IMAGENAME eq {image}\" /FI \"PID ne {pid}\" >nul 2>&1 & ping -n 3 127.0.0.1 >nul & schtasks /Run /TN Relay >nul 2>&1 || start \"\" \"{exe}\" --background",
            pid = std::process::id(),
            exe = exe.display()
        );
        let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
        let spawned = Command::new("cmd")
            .args(["/c", &script])
            .creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
            .or_else(|_| Command::new("cmd").args(["/c", &script]).creation_flags(flags).spawn());
        spawned.context("starting the restart helper")?;
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod platform {
    use super::*;
    pub fn install(_: &Path, _: &Path, _: &Path) -> Result<()> {
        bail!("self-update isn't supported on this platform")
    }
    pub fn restart(_: &Path) -> Result<()> {
        bail!("self-update isn't supported on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.4.5", "0.4.4"));
        assert!(is_newer("v0.5.0", "0.4.9"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("0.4.4", "0.4.4"));
        assert!(!is_newer("0.4.3", "0.4.4"));
        assert!(!is_newer("garbage", "0.4.4"));
        assert!(!is_newer("0.4.5-rc.1", "0.4.5"));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_install_test {
    use super::*;

    /// Downloads the real latest .dmg and installs it over a throwaway
    /// Relay.app in a temp dir. Network + ~20 MB, so opt-in:
    /// `cargo test -- --ignored installs_latest_dmg_into_a_bundle`.
    #[test]
    #[ignore]
    fn installs_latest_dmg_into_a_bundle() {
        let root = std::env::temp_dir().join(format!("relay-upd-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let exe = root.join("Relay.app/Contents/MacOS/relay");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"old").unwrap();
        let r = block_on(latest_release()).unwrap().unwrap();
        let dir = root.join("work");
        std::fs::create_dir_all(&dir).unwrap();
        let dmg = dir.join("relay.dmg");
        block_on(download(r.asset_url.as_deref().unwrap(), &dmg)).unwrap();
        platform::install(&exe, &dmg, &dir).unwrap();
        let out = std::process::Command::new(&exe).arg("--version").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("relay {}", r.version));
        assert!(!root.join("Relay.app.old").exists() && !root.join("Relay.app.new").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
