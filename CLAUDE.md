# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with this repository.

## Project Overview

**shadows-desktop** — A desktop daemon (v0.2.0) that registers a machine with a [clawborrator](https://github.com/clawborrator) hub and runs Claude Code sessions on it, controlled from the [shadows](https://github.com/clawborrator/shadows) web app.

Fork of `desktop_v1` (`clawborrator-supervisor`). The only behavioral difference is authentication: instead of pairing against the hub's GitHub OAuth, it pairs against the **shadows app** (Google/Zoho SSO). Shadows brokers a hub token for the user's shadow principal. Everything downstream (the `/supervisor` WebSocket, session spawn/kill/restart, channel-token plumbing) is unchanged from desktop_v1.

## Build & Run

```bash
# Build
cargo build --release
# Binary: target/release/shadows-desktop

# First-run pairing (opens a GUI wizard on Windows/macOS, or runs interactively on Linux)
shadows-desktop login [--shadows-url https://shadows-app.fly.dev]

# Run the daemon (uses cached token + hub URL from pairing)
shadows-desktop

# Autostart management
shadows-desktop install-task     # add to Task Scheduler / launchd
shadows-desktop uninstall-task
shadows-desktop task-status

# Unpair
shadows-desktop logout
```

Cargo 1.75+ required (workspace pins `rust-version = "1.75"`).

## Architecture

### Crate structure (`clawborrator-supervisor/src/`)

| Module | Purpose |
|--------|---------|
| `main.rs` | CLI entry point, WS daemon loop, reconnect with exponential backoff |
| `oauth.rs` | **The key divergence from desktop_v1.** Device flow targets `shadows` app instead of GitHub. Polls `shadows /device/token`, gets back `{ access_token, hub_url }`. |
| `auth.rs` | Token cache load/save; `~/.clawborrator/shadows-desktop.json` |
| `gui.rs` | **First-run setup wizard** (egui/eframe, Windows + macOS only). Opens when launched interactively with no cached token. Prompts for the shadows URL, runs the device flow, shows the user code, waits for approval, then offers "install + start background task". |
| `autostart/` | Platform-specific autostart: `windows.rs` (Task Scheduler via `schtasks.exe`), macOS via `launchd` LaunchAgent. Registered under `--background` flag to skip the GUI. |
| `sessions.rs` | Session lifecycle, `SharingPolicy` (allowed_roots, max_concurrent_sessions) |
| `spawn.rs` | `create_session`, `destroy_session`, `restart_session`, etc. + `sweep_orphan_scratch_dirs` |
| `ipc.rs` | Per-install IPC socket (distinct from desktop_v1 — coexists on the same machine) |
| `tray/` | System-tray icon (Windows + macOS) |
| `parser_plugins/` | Terminal output parsers |
| `logging.rs` | Tracing subscriber setup |
| `status.rs` | `TrayStatus` / `TrayStatusUpdater` |
| `token_usage.rs` | Claude token tracking |

### Config file

`~/.clawborrator/shadows-desktop.json` caches: access token, learned hub URL, `machine_id` (stable per-install UUID), and the shadows URL used for pairing.

**Desktop-sharing guardrails** (from desktop_v1): `allowed_roots` and `max_concurrent_sessions` in the config file control what a shared user may spawn.

### Pairing flow

```
1. shadows-desktop login
     → prints user_code, polls shadows /device/token
2. User opens shadows app (already signed in with Google/Zoho),
   goes to "Pair a machine", enters the code
3. Shadows mints a hub token for the user's shadow principal,
   returns { access_token, hub_url }
4. Daemon caches both and connects to <hub_url>/supervisor
```

The hub only sees the shadow principal (email-keyed); it never sees Google/Zoho credentials.

## CI & Release

### CI (`.github/workflows/ci.yml`)
Runs on every push/PR to `main`. `cargo check --locked` + `cargo build --locked` on Linux only (fast check). Full cross-platform smoke test is available via workflow_dispatch on the release workflow.

### Release (`.github/workflows/release.yml`)
Triggered by a `v*` tag push. Builds native binaries for:
- `shadows-desktop-macos-arm64` + `.dmg` (unsigned unless `MACOS_CERT_P12` secret is set)
- `shadows-desktop-windows-x64.exe`
- `shadows-desktop-linux-x64`

Creating a release: `git tag v0.2.0 && git push origin v0.2.0`

macOS `.dmg` packaging scripts are in `packaging/macos/`.

## Relationship to desktop_v1

Kept as a thin fork. Divergence confined to `src/oauth.rs` (device flow targets shadows, returns hub URL) and `src/auth.rs` + login plumbing in `src/main.rs`. Crate dir stays `clawborrator-supervisor/` to keep the fork diff small; the binary is named `shadows-desktop`.

Upstream WS/session fixes can be pulled from desktop_v1 with minimal conflict since the auth modules are the only changed surface area.

## Key constants (`main.rs`)

| Constant | Value | Purpose |
|----------|-------|---------|
| `DAEMON_VERSION` | from Cargo.toml (currently `0.2.0`) | Sent in the `hello` WS frame |
| `DEFAULT_SHADOWS_URL` | `https://shadows-app.fly.dev` | Default pairing target |
| `PING_INTERVAL` | 30s | WS keepalive |
| `LIVENESS_TIMEOUT` | 90s | No-frame deadline before forced reconnect |
| `CONNECT_TIMEOUT` | 20s | WS connect + handshake cap |
| `RECONNECT_BACKOFF_INITIAL` | 1s | Backoff start |
| `RECONNECT_BACKOFF_MAX` | 60s | Backoff cap |

## IPC socket

Uses a **distinct** IPC socket path from `desktop_v1` (different `application_name`/socket path in `ipc.rs`) and a separate scratch directory so both daemons can coexist on the same machine during migration.
