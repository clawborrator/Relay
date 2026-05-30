// Linux autostart via systemd USER service. Writes a unit file to
// ~/.config/systemd/user/clawborrator-supervisor.service, runs
// daemon-reload, enables the unit, and prints a hint about
// `loginctl enable-linger` (which lets the service start at boot
// before the user logs in, and survives logouts).
//
// User-level (not system-level) was an explicit operator choice:
//   - No root needed to install / iterate
//   - Daemon runs as the operator's user, has their HOME and PATH,
//     can find their Claude Code auth tokens and scratch dirs
//   - systemctl --user is the natural ops surface
//
// systemd-user runs on every distro shipping a modern systemd
// (Debian / Ubuntu / Fedora / Arch / openSUSE). For non-systemd
// distros (Void / Alpine / Gentoo musl), the XDG autostart
// .desktop fallback would be a separate impl behind a flag; not
// shipped today.
//
// Settings of note in the unit file:
//   - After/Wants=network-online.target — daemon dials the hub via
//     WSS on startup; require network before kicking it off.
//   - Restart=on-failure, RestartSec=5 — covers process crashes;
//     the in-proc reconnect loop handles WS-level blips on its own.
//   - StandardOutput=journal, StandardError=journal — journald
//     captures the daemon's stdout/stderr. Tail via
//     `journalctl --user -u clawborrator-supervisor -f`.
//   - WantedBy=default.target — user-service equivalent of
//     multi-user.target; default.target is the user-session target.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

use super::{AutostartProvider, AutostartStatus};

const UNIT_NAME: &str = "clawborrator-supervisor.service";

pub struct LinuxAutostart;

impl AutostartProvider for LinuxAutostart {
    fn install(&self, exe: &Path) -> Result<()> {
        let exe_abs = fs::canonicalize(exe)
            .with_context(|| format!("could not canonicalize exe path: {}", exe.display()))?;
        let unit_path = user_unit_path()?;
        let unit_dir = unit_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("unit_path has no parent: {}", unit_path.display()))?;
        fs::create_dir_all(unit_dir)
            .with_context(|| format!("could not create {}", unit_dir.display()))?;

        let unit_contents = render_unit(&exe_abs);
        fs::write(&unit_path, unit_contents)
            .with_context(|| format!("could not write {}", unit_path.display()))?;
        info!(unit = %unit_path.display(), exe = %exe_abs.display(), "unit file written");

        run_systemctl_user(&["daemon-reload"]).context("systemctl --user daemon-reload failed")?;
        run_systemctl_user(&["enable", UNIT_NAME])
            .with_context(|| format!("systemctl --user enable {UNIT_NAME} failed"))?;

        eprintln!("Installed {UNIT_NAME} into {}", unit_path.display());
        eprintln!();
        eprintln!("To start it now:");
        eprintln!("    systemctl --user start {UNIT_NAME}");
        eprintln!();
        eprintln!("To watch its logs live:");
        eprintln!("    journalctl --user -u {UNIT_NAME} -f");
        eprintln!();
        eprintln!("If you haven't already enabled linger for boot-time start (before any");
        eprintln!("user login + survives logouts):");
        eprintln!("    sudo loginctl enable-linger \"$USER\"");
        eprintln!("(Linger needs to run before `install-task` on a fresh server install,");
        eprintln!(" otherwise the user systemd manager isn't running and daemon-reload");
        eprintln!(" fails with \"No medium found\". Re-login over SSH after enable-linger.)");
        Ok(())
    }

    fn uninstall(&self) -> Result<()> {
        let unit_path = user_unit_path()?;

        // Disable first (idempotent — succeeds even if the unit was
        // never enabled). If the unit file is already gone, this
        // emits a "no such file" error; we treat that as success.
        match run_systemctl_user(&["disable", UNIT_NAME]) {
            Ok(_) => {}
            Err(e) => warn!(error = %e, "systemctl --user disable returned non-zero; continuing"),
        }
        // Stop too — disable doesn't stop an already-running unit.
        match run_systemctl_user(&["stop", UNIT_NAME]) {
            Ok(_) => {}
            Err(e) => warn!(error = %e, "systemctl --user stop returned non-zero; continuing"),
        }

        if unit_path.exists() {
            fs::remove_file(&unit_path)
                .with_context(|| format!("could not remove {}", unit_path.display()))?;
            info!(unit = %unit_path.display(), "unit file removed");
        } else {
            warn!(unit = %unit_path.display(), "unit file already absent; nothing to remove");
        }

        // daemon-reload after the file is gone so systemd forgets it.
        let _ = run_systemctl_user(&["daemon-reload"]);

        eprintln!();
        eprintln!("Removed {UNIT_NAME}.");
        eprintln!("If you previously ran `sudo loginctl enable-linger \"$USER\"`, the linger");
        eprintln!("setting is unchanged; disable it with `sudo loginctl disable-linger \"$USER\"`");
        eprintln!("if you wanted to fully revert.");
        Ok(())
    }

    fn status(&self) -> Result<AutostartStatus> {
        let unit_path = user_unit_path()?;
        if !unit_path.exists() {
            return Ok(AutostartStatus::NotInstalled);
        }
        let enabled = run_systemctl_user(&["is-enabled", UNIT_NAME])
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        let active = run_systemctl_user(&["is-active", UNIT_NAME])
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        Ok(AutostartStatus::Installed {
            details: format!(
                "{} (enabled={}, active={})",
                unit_path.display(),
                enabled,
                active
            ),
        })
    }

    fn facility_name(&self) -> &'static str {
        "systemd user service"
    }
}

fn user_unit_path() -> Result<PathBuf> {
    // XDG_CONFIG_HOME wins if set, otherwise ~/.config. dirs::config_dir()
    // already does that resolution; just append systemd/user/<unit>.
    let cfg = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("could not resolve XDG_CONFIG_HOME / ~/.config"))?;
    Ok(cfg.join("systemd").join("user").join(UNIT_NAME))
}

fn render_unit(exe: &Path) -> String {
    let exec = exe.display();
    // PATH explicitly includes %h/.local/bin (the user home), which
    // is where the official Claude Code installer (`curl -fsSL
    // https://claude.ai/install.sh | bash`) drops the `claude` binary.
    // systemd's default user-service PATH is just /usr/local/sbin:
    // /usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin — which doesn't
    // include ~/.local/bin, so the supervisor's spawn-claude path
    // would fail with "No such file or directory" even though the
    // operator's interactive shell sees `which claude` fine.
    // (%h expands to the user's home dir at service-start.)
    format!(
        "# Auto-generated by `clawborrator-supervisor install-task`.\n\
         # Edit by hand at your own risk; running install-task again overwrites this file.\n\
         [Unit]\n\
         Description=clawborrator desktop supervisor daemon\n\
         Documentation=https://github.com/clawborrator/desktop_v1\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         Environment=\"PATH=%h/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\"\n\
         ExecStart={exec}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         StandardOutput=journal\n\
         StandardError=journal\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

// Read this process's effective UID by parsing /proc/self/status.
// Linux-only, no new dep. Used to synthesize XDG_RUNTIME_DIR when
// the parent shell didn't populate it (common after a fresh SSH
// login on minimal server installs whose PAM stack doesn't include
// pam_systemd, or when linger was enabled in this shell but env
// hasn't been refreshed).
fn current_uid() -> Option<u32> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("Uid:\t") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

fn run_systemctl_user(args: &[&str]) -> Result<std::process::Output> {
    let mut cmd = Command::new("systemctl");
    cmd.arg("--user").args(args);

    // If XDG_RUNTIME_DIR isn't already set in the parent env,
    // synthesize /run/user/<uid> and pass it to the child. This
    // covers the case where the operator's SSH session didn't
    // trigger pam_systemd (some server installs don't ship it in
    // the SSH PAM chain) OR they just enabled linger and haven't
    // re-logged in yet. systemctl --user uses XDG_RUNTIME_DIR to
    // locate the user manager's bus socket; without it, libdbus
    // fails with "No medium found".
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        if let Some(uid) = current_uid() {
            let dir = format!("/run/user/{uid}");
            if std::path::Path::new(&dir).exists() {
                cmd.env("XDG_RUNTIME_DIR", &dir);
            }
        }
    }

    let out = cmd
        .output()
        .context("could not exec systemctl --user (is systemd present on this system?)")?;
    if !out.status.success() {
        // Surface stderr in the error chain; the caller decides whether
        // to treat non-zero as a hard fail or a soft "already absent".
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr_trim = stderr.trim();

        // "No medium found" is libdbus speak for "the user systemd
        // manager isn't running for this UID, so there's no bus
        // socket to connect to". Standard symptom over fresh SSH
        // on a server install — PAM didn't trigger pam_systemd to
        // start the user manager because no graphical login has
        // happened. Fix is `sudo loginctl enable-linger $USER` then
        // re-login (or export XDG_RUNTIME_DIR for the current shell).
        // Detect by stderr content and emit the actionable hint
        // instead of the cryptic systemd-internal message.
        if stderr_trim.contains("No medium found") || stderr_trim.contains("Failed to connect to bus") {
            bail!(
                "systemctl --user {} can't connect to a user systemd manager.\n\
                 \n\
                 Your user systemd manager isn't running yet (common on a fresh SSH session\n\
                 to a server install). To start it:\n\
                 \n\
                     sudo loginctl enable-linger \"$USER\"\n\
                 \n\
                 Then either re-login over SSH (cleanest) OR export the runtime dir for\n\
                 the current shell:\n\
                 \n\
                     export XDG_RUNTIME_DIR=/run/user/$(id -u)\n\
                 \n\
                 Then re-run `clawborrator-supervisor install-task`.\n\
                 \n\
                 (Raw systemctl error: {})",
                args.join(" "),
                stderr_trim
            );
        }

        bail!(
            "systemctl --user {} exited with code {:?}: {}",
            args.join(" "),
            out.status.code(),
            stderr_trim
        );
    }
    Ok(out)
}
