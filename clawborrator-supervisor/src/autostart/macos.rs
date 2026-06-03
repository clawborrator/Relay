// macOS autostart via a launchd LaunchAgent. Writes a plist to
// ~/Library/LaunchAgents/com.clawborrator.supervisor.plist, then
// bootstraps it into the per-user GUI domain so the daemon launches
// at every login (and launchd relaunches it if it crashes).
//
// Per-user (a LaunchAgent, not a system-wide LaunchDaemon) was an
// explicit operator choice, mirroring the Linux systemd-user model:
//   - No root / sudo needed to install or iterate.
//   - The daemon runs as the operator's user, in their GUI login
//     session, with their HOME — so it can find Claude Code auth
//     tokens + scratch dirs AND, because it's a GUI-session agent,
//     render the menu-bar tray icon.
//   - `launchctl` against the `gui/<uid>` domain is the natural ops
//     surface.
//
// Notes on the plist:
//   - RunAtLoad=true — start as soon as it's bootstrapped, and at
//     every subsequent login.
//   - KeepAlive/SuccessfulExit=false — relaunch on a crash, but a
//     clean exit (tray Quit) stays exited. Equivalent to systemd's
//     Restart=on-failure.
//   - EnvironmentVariables/PATH — launchd hands an agent only a
//     minimal PATH (/usr/bin:/bin:/usr/sbin:/sbin). install-task
//     composes a fuller one: ~/.local/bin (the `claude` install dir)
//     and the Homebrew bin dirs, THEN the PATH this install-task run
//     inherited from the operator's shell. Snapshotting the shell
//     PATH is what makes nvm / asdf / fnm / volta node installs work
//     — their bin dirs have no fixed location, so a hardcoded list
//     can't cover them. Re-run install-task after changing your PATH
//     to refresh the snapshot.
//   - StandardErrorPath — early-crash diagnostics (anything printed
//     before tracing's file logger inits) land here; the daemon's
//     own rolling log stays separate, under ~/Library/Application
//     Support/clawborrator/.
//   - launchd does NOT expand `~` in plist paths, so every path is
//     written absolute.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{anyhow, bail, Context, Result};
use tracing::{info, warn};

use super::{AutostartProvider, AutostartStatus};

const LABEL: &str = "com.clawborrator.supervisor";

pub struct MacosAutostart;

impl AutostartProvider for MacosAutostart {
    fn install(&self, exe: &Path) -> Result<()> {
        let exe_abs = fs::canonicalize(exe)
            .with_context(|| format!("could not canonicalize exe path: {}", exe.display()))?;
        let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home dir"))?;

        let plist_path = plist_path(&home);
        let plist_dir = plist_path
            .parent()
            .ok_or_else(|| anyhow!("plist_path has no parent: {}", plist_path.display()))?;
        fs::create_dir_all(plist_dir)
            .with_context(|| format!("could not create {}", plist_dir.display()))?;

        // launchd captures the daemon's stdout/stderr here for
        // early-crash diagnostics; create the dir up front so the
        // streams aren't silently dropped.
        let log_dir = home.join("Library").join("Logs").join("clawborrator");
        fs::create_dir_all(&log_dir)
            .with_context(|| format!("could not create {}", log_dir.display()))?;

        let plist = render_plist(&exe_abs, &home);
        fs::write(&plist_path, plist)
            .with_context(|| format!("could not write {}", plist_path.display()))?;
        info!(plist = %plist_path.display(), exe = %exe_abs.display(), "LaunchAgent plist written");

        let uid = current_uid()?;
        let domain = format!("gui/{uid}");
        let service = format!("gui/{uid}/{LABEL}");
        let plist_str = plist_path
            .to_str()
            .ok_or_else(|| anyhow!("plist path is not valid UTF-8: {}", plist_path.display()))?;

        // bootout any stale instance first so bootstrap doesn't fail
        // with "service already bootstrapped". A clean first install
        // has nothing to boot out — ignore the result.
        let _ = run_launchctl(&["bootout", &service]);

        let out = run_launchctl(&["bootstrap", &domain, plist_str])?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!(
                "launchctl bootstrap {domain} failed: {}\n\
                 \n\
                 (If this says \"Could not find domain\", run `install-task` from a\n\
                 normal GUI login session — not over a plain SSH shell; a LaunchAgent\n\
                 needs the Aqua session to exist.)",
                stderr.trim()
            );
        }

        // RunAtLoad already starts it on bootstrap; kickstart is a
        // belt-and-braces nudge in case the agent was bootstrapped
        // in a disabled state. Non-fatal.
        let _ = run_launchctl(&["kickstart", &service]);

        let support_dir = home
            .join("Library")
            .join("Application Support")
            .join("clawborrator");
        eprintln!("Installed LaunchAgent {LABEL} at {}", plist_path.display());
        eprintln!();
        eprintln!("The daemon is running now and will relaunch at every login.");
        eprintln!();
        eprintln!("Daemon log (daily-rolled):");
        eprintln!("    tail -f \"{}\"/supervisor.log.*", support_dir.display());
        eprintln!();
        eprintln!("launchd's capture of early stdout/stderr:");
        eprintln!("    tail -f \"{}\"/launchd.*.log", log_dir.display());
        Ok(())
    }

    fn uninstall(&self) -> Result<()> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home dir"))?;
        let plist_path = plist_path(&home);
        let uid = current_uid()?;
        let service = format!("gui/{uid}/{LABEL}");

        // bootout stops + unloads the agent. Idempotent: a not-loaded
        // agent makes this exit non-zero ("No such process"), which
        // we treat as success.
        match run_launchctl(&["bootout", &service]) {
            Ok(o) if o.status.success() => info!(service = %service, "agent booted out"),
            Ok(o) => warn!(
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "launchctl bootout returned non-zero; continuing (agent likely not loaded)"
            ),
            Err(e) => warn!(error = %e, "launchctl bootout failed to exec; continuing"),
        }

        if plist_path.exists() {
            fs::remove_file(&plist_path)
                .with_context(|| format!("could not remove {}", plist_path.display()))?;
            info!(plist = %plist_path.display(), "plist removed");
        } else {
            warn!(plist = %plist_path.display(), "plist already absent; nothing to remove");
        }

        eprintln!();
        eprintln!("Removed LaunchAgent {LABEL}.");
        Ok(())
    }

    fn status(&self) -> Result<AutostartStatus> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home dir"))?;
        let plist_path = plist_path(&home);
        if !plist_path.exists() {
            return Ok(AutostartStatus::NotInstalled);
        }
        let uid = current_uid()?;
        let service = format!("gui/{uid}/{LABEL}");
        // `launchctl print <service>` exits 0 only when the agent is
        // bootstrapped into the domain. The plist can linger on disk
        // after a manual `bootout`, so distinguish the two states.
        let loaded = run_launchctl(&["print", &service])
            .map(|o| o.status.success())
            .unwrap_or(false);
        Ok(AutostartStatus::Installed {
            details: format!(
                "{} (launchd: {})",
                plist_path.display(),
                if loaded { "loaded" } else { "plist present, not loaded" }
            ),
        })
    }

    fn facility_name(&self) -> &'static str {
        "launchd LaunchAgent"
    }
}

fn plist_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// The current process's effective UID, as a string for the
/// `gui/<uid>` launchctl domain target. Shelled out to `id -u` to
/// avoid pulling in `libc` just for `getuid()`.
fn current_uid() -> Result<String> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .context("could not exec `id -u`")?;
    if !out.status.success() {
        bail!("`id -u` exited with {:?}", out.status.code());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_launchctl(args: &[&str]) -> Result<Output> {
    Command::new("launchctl")
        .args(args)
        .output()
        .context("could not exec launchctl")
}

/// XML-escape — strict allow-list. The interpolated values are file
/// paths under the user's home; the universe of characters needing
/// escaping is small, but a home dir / username with an `&` would
/// otherwise produce a malformed plist that launchd rejects.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn render_plist(exe: &Path, home: &Path) -> String {
    let log_dir = home.join("Library").join("Logs").join("clawborrator");
    let local_bin = home.join(".local").join("bin").display().to_string();

    // Compose the agent's PATH: ~/.local/bin + the Homebrew bin dirs,
    // then the PATH this install-task invocation inherited from the
    // operator's shell. The snapshot is what makes nvm / asdf / fnm /
    // volta node installs reachable — their bin dirs aren't in any
    // fixed location, but they're on the shell PATH at install time.
    // If the shell PATH is somehow empty, fall back to the launchd
    // default so the agent still has the system tools.
    let mut path = format!("{local_bin}:/opt/homebrew/bin:/usr/local/bin");
    match std::env::var("PATH") {
        Ok(p) if !p.is_empty() => { path.push(':'); path.push_str(&p); }
        _ => path.push_str(":/usr/bin:/bin:/usr/sbin:/sbin"),
    }

    let exec = xml_escape(&exe.display().to_string());
    let path = xml_escape(&path);
    let stdout_log = xml_escape(&log_dir.join("launchd.stdout.log").display().to_string());
    let stderr_log = xml_escape(&log_dir.join("launchd.stderr.log").display().to_string());

    // No literal `{`/`}` in plist XML, so a raw format string is safe.
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- Auto-generated by `clawborrator-supervisor install-task`. -->
<!-- Edit by hand at your own risk; install-task overwrites this file. -->
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exec}</string>
    <!-- `--background` marks this as the autostart launch so the daemon
         goes straight to the tray and never re-opens the first-run
         setup wizard (parity with the Windows Task Scheduler entry). -->
    <string>--background</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path}</string>
  </dict>
  <key>StandardOutPath</key>
  <string>{stdout_log}</string>
  <key>StandardErrorPath</key>
  <string>{stderr_log}</string>
</dict>
</plist>
"#
    )
}
