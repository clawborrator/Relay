// Tracing setup. The release Windows build is `windows_subsystem =
// "windows"` so it has no console; logs MUST go to a file. The file
// path is platform-conventional:
//
//   Windows: %LOCALAPPDATA%\clawborrator\supervisor.log
//   macOS:   ~/Library/Application Support/clawborrator/supervisor.log
//   Linux:   ~/.local/share/clawborrator/supervisor.log
//
// File-only writer in all build modes keeps the setup tight. Debug
// builds run from a console see the same log via `tail -f` if they
// want; we avoid a tee writer until there's a real reason to add one.
//
// The returned WorkerGuard MUST be held for the lifetime of the
// process; dropping it flushes pending log lines and shuts the
// background writer thread down. main.rs binds it to a top-level
// variable.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

pub struct LogHandle {
    pub log_path: PathBuf,
    _guard:       WorkerGuard,
}

pub fn init() -> Result<LogHandle> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating log dir {dir:?}"))?;

    // Daily rolling, names like `supervisor.log.2026-05-09`. Old files
    // accumulate; aggressive pruning is a future call. The user can
    // sweep the dir manually if it grows.
    let appender = tracing_appender::rolling::daily(&dir, "supervisor.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let log_path = dir.join("supervisor.log");

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(writer)
        .with_ansi(false)
        .init();

    Ok(LogHandle { log_path, _guard: guard })
}

fn log_dir() -> Result<PathBuf> {
    let base = if cfg!(target_os = "windows") {
        dirs::data_local_dir()
    } else {
        dirs::data_dir()
    };
    Ok(base
        .ok_or_else(|| anyhow!("could not resolve OS data directory"))?
        .join("clawborrator"))
}
