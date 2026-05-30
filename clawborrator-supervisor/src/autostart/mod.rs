// Auto-start integration. Each OS has its own facility for launching
// a binary at user logon (Task Scheduler on Windows, launchd on macOS,
// systemd-user / autostart .desktop on Linux). The trait gives main.rs
// a single dispatch surface so subcommand code doesn't grow per-OS
// branches; per-OS submodules contain all the platform-specific bits.
//
// Only the Windows impl is real today; macOS / Linux are stubs that
// return NotYetImplemented so the subcommands work cross-platform
// (you can still run `task-status` on macOS without a panic).

use std::path::Path;

use anyhow::Result;

#[cfg(target_os = "windows")] mod windows;
#[cfg(target_os = "macos")]   mod macos;
#[cfg(target_os = "linux")]   mod linux;

#[derive(Debug)]
pub enum AutostartStatus {
    /// Entry exists. `details` is a short OS-specific human description
    /// (task name, plist label, etc.) for the operator to recognise.
    Installed { details: String },
    NotInstalled,
}

pub trait AutostartProvider {
    /// Register an autostart entry that will launch `exe` at user logon.
    fn install(&self, exe: &Path) -> Result<()>;

    /// Remove the autostart entry. Idempotent — succeeds even if the
    /// entry is absent (logged but not an error).
    fn uninstall(&self) -> Result<()>;

    /// Report whether the entry is currently installed.
    fn status(&self) -> Result<AutostartStatus>;

    /// Human-readable label for the platform's autostart facility, used
    /// in user-facing messages ("Task Scheduler entry installed", etc).
    fn facility_name(&self) -> &'static str;
}

/// Resolve the provider for the current OS. Compile-time dispatch keeps
/// the call sites flat — main.rs sees a single `dyn AutostartProvider`.
pub fn current() -> &'static dyn AutostartProvider {
    #[cfg(target_os = "windows")] { &windows::WindowsAutostart }
    #[cfg(target_os = "macos")]   { &macos::MacosAutostart }
    #[cfg(target_os = "linux")]   { &linux::LinuxAutostart }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    { &Unsupported }
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
struct Unsupported;

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
impl AutostartProvider for Unsupported {
    fn install(&self, _exe: &Path) -> Result<()> { anyhow::bail!("autostart not supported on this OS") }
    fn uninstall(&self)              -> Result<()> { anyhow::bail!("autostart not supported on this OS") }
    fn status(&self)                 -> Result<AutostartStatus> { anyhow::bail!("autostart not supported on this OS") }
    fn facility_name(&self) -> &'static str { "(unsupported)" }
}
