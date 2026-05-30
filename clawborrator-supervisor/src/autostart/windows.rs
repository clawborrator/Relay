// Windows autostart via Task Scheduler. Generates a 1.2-schema Task
// XML, writes it to a temp file as UTF-16 LE BOM (schtasks input
// quirk), then shells out to schtasks.exe for create / delete / query.
//
// Per-user task scope ("HKCU"-equivalent). No admin elevation needed.
// Trigger is LogonTrigger bound to the current user's USERID, so the
// task runs at every logon for that user, in their interactive
// session, with their environment — which is what the supervisor
// needs (it spawns Claude Code subprocesses that depend on the user's
// PATH and home directory).
//
// Settings of note (all overrides of schtasks-shorthand defaults):
//   - ExecutionTimeLimit=PT0S — default is 72h, would kill the daemon.
//   - MultipleInstancesPolicy=IgnoreNew — relogon doesn't double-spawn.
//   - StopIfGoingOnBatteries=false — daemon should run on battery.
//   - RestartOnFailure 1m × 999 — covers process crashes; the in-proc
//     reconnect loop handles WS-level blips on its own.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

use super::{AutostartProvider, AutostartStatus};

const TASK_NAME: &str = "Clawborrator Supervisor";

pub struct WindowsAutostart;

impl AutostartProvider for WindowsAutostart {
    fn install(&self, exe: &Path) -> Result<()> {
        let xml      = render_task_xml(exe)?;
        let xml_path = write_temp_xml(&xml)?;
        let out = run_schtasks(&["/Create", "/TN", TASK_NAME, "/XML", xml_path_str(&xml_path)?, "/F"])?;
        cleanup_temp(&xml_path);
        require_success(&out, "schtasks /Create")?;
        info!(task = TASK_NAME, exe = %exe.display(), "task installed");
        Ok(())
    }

    fn uninstall(&self) -> Result<()> {
        let out = run_schtasks(&["/Delete", "/TN", TASK_NAME, "/F"])?;
        if !out.status.success() {
            // Not-installed is success-shaped for an idempotent uninstall.
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("does not exist") || stderr.contains("cannot find") {
                warn!(task = TASK_NAME, "task was not installed; nothing to remove");
                return Ok(());
            }
            bail!("schtasks /Delete failed: {}", stderr.trim());
        }
        info!(task = TASK_NAME, "task removed");
        Ok(())
    }

    fn status(&self) -> Result<AutostartStatus> {
        let out = run_schtasks(&["/Query", "/TN", TASK_NAME])?;
        if out.status.success() {
            Ok(AutostartStatus::Installed { details: format!("Task Scheduler entry: \"{TASK_NAME}\"") })
        } else {
            Ok(AutostartStatus::NotInstalled)
        }
    }

    fn facility_name(&self) -> &'static str { "Windows Task Scheduler" }
}

// ─── helpers ───────────────────────────────────────────────────────

fn run_schtasks(args: &[&str]) -> Result<Output> {
    Command::new("schtasks.exe")
        .args(args)
        .output()
        .context("invoking schtasks.exe")
}

fn require_success(out: &Output, what: &str) -> Result<()> {
    if out.status.success() { return Ok(()); }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    bail!("{what} failed: {} {}", stderr.trim(), stdout.trim());
}

fn xml_path_str(p: &Path) -> Result<&str> {
    p.to_str().context("temp xml path is not valid UTF-8")
}

fn cleanup_temp(p: &Path) {
    let _ = std::fs::remove_file(p);
}

/// Resolve `DOMAIN\user` for the current process. Falls back to bare
/// `user` if USERDOMAIN isn't set (rare; mostly only on stripped CI
/// images).
fn current_user_id() -> String {
    let user   = whoami::username();
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    if domain.is_empty() { user } else { format!("{domain}\\{user}") }
}

/// XML-escape — strict allow-list. The values we interpolate are user
/// names and file paths; the universe of characters that need escaping
/// is small. Belt-and-braces in case a username has an apostrophe or
/// a path is unusually shaped.
fn xml_escape(s: &str) -> String {
    s.replace('&',  "&amp;")
     .replace('<',  "&lt;")
     .replace('>',  "&gt;")
     .replace('"',  "&quot;")
     .replace('\'', "&apos;")
}

fn render_task_xml(exe: &Path) -> Result<String> {
    let user = xml_escape(&current_user_id());
    let exe_path = exe.to_str().context("exe path is not valid UTF-8")?;
    let exe_dir  = exe.parent().and_then(Path::to_str).unwrap_or("");
    let exe_path_xml = xml_escape(exe_path);
    let exe_dir_xml  = xml_escape(exe_dir);

    Ok(format!(r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>{user}</Author>
    <Description>clawborrator-supervisor — connects this machine to the clawborrator hub at user logon.</Description>
    <URI>\{TASK_NAME}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <DisallowStartOnRemoteAppSession>false</DisallowStartOnRemoteAppSession>
    <UseUnifiedSchedulingEngine>true</UseUnifiedSchedulingEngine>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe_path_xml}</Command>
      <WorkingDirectory>{exe_dir_xml}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#))
}

/// Write the XML as UTF-16 LE with BOM. Some `schtasks.exe` builds
/// reject UTF-8 input despite accepting `encoding="UTF-8"` in the
/// XML header; UTF-16 LE BOM is the universally-accepted format.
fn write_temp_xml(xml: &str) -> Result<PathBuf> {
    let mut path = std::env::temp_dir();
    path.push(format!("clawborrator-supervisor-task-{}.xml", std::process::id()));

    let bytes: Vec<u8> = std::iter::once(0xFEFFu16)
        .chain(xml.encode_utf16())
        .flat_map(|u| u.to_le_bytes())
        .collect();

    std::fs::write(&path, bytes).with_context(|| format!("writing temp task xml to {path:?}"))?;
    Ok(path)
}
