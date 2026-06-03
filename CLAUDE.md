# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with this repository.

## Project Overview

**Relay** (`shadows-desktop` crate, v0.3.0) — A desktop daemon that registers a machine with a [clawborrator](https://github.com/clawborrator) hub and runs Claude Code sessions on it, controlled from the [shadows](https://github.com/clawborrator/shadows) web app. The distributed binary is named `relay` (`relay.exe` on Windows); the crate package stays `shadows-desktop` to keep the fork diff small.

Fork of `desktop_v1` (`clawborrator-supervisor`). The only behavioral difference is authentication: instead of pairing against the hub's GitHub OAuth, it pairs against the **shadows app** (Google/Zoho SSO). Shadows brokers a hub token for the user's shadow principal. Everything downstream (the `/supervisor` WebSocket, session spawn/kill/restart, channel-token plumbing) is unchanged from desktop_v1.

## Build & Run

```bash
# Build
cargo build --release
# Binary: target/release/relay  (relay.exe on Windows)

# First-run pairing (opens GUI wizard on Windows/macOS; interactive on Linux)
relay login [--shadows-url https://shadows-app.fly.dev]

# Run the daemon (uses cached token + hub URL from pairing)
relay

# Autostart management
relay install-task     # add to Task Scheduler / launchd
relay uninstall-task
relay task-status

# Unpair
relay logout
```

Cargo 1.75+ required (workspace pins `rust-version = "1.75"`).

## Architecture

### Crate structure (`clawborrator-supervisor/src/`)

| Module | Purpose |
|--------|---------|
| `main.rs` | CLI entry point (`relay`), WS daemon loop, reconnect with exponential backoff |
| `oauth.rs` | **The key divergence from desktop_v1.** Device flow targets `shadows` app instead of GitHub. Polls `shadows /device/token`, gets back `{ access_token, hub_url }`. |
| `auth.rs` | Token cache load/save; `~/.clawborrator/shadows-desktop.json` |
| `gui.rs` | **Setup wizard** (egui/eframe, Windows + macOS). `run_setup_wizard(url, WizardMode)` — `WizardMode::FirstRun` offers install+start after pairing; `WizardMode::Repair` just refreshes the token for an already-running daemon. Sets the Relay molecule as the Dock/window icon. |
| `autostart/` | Platform-specific autostart: `windows.rs` (Task Scheduler "Relay"), macOS via `launchd` LaunchAgent (passes `--background` to skip the wizard). Linux: `relay.service` systemd-user unit. |
| `build.rs` | Windows-only: embeds `assets/app-icon.ico` into `relay.exe` via `winresource`. No-op on macOS/Linux. |
| `sessions.rs` | Session lifecycle, `SharingPolicy` (allowed_roots, max_concurrent_sessions) |
| `spawn.rs` | `create_session`, `destroy_session`, `restart_session`, etc. + `sweep_orphan_scratch_dirs` |
| `ipc.rs` | Per-install IPC socket (distinct from desktop_v1 — coexists on the same machine) |
| `tray/` | System-tray icon (Windows + macOS) |
| `parser_plugins/` | Terminal output parsers |
| `logging.rs` | Tracing subscriber setup |
| `status.rs` | `TrayStatus` / `TrayStatusUpdater` |
| `token_usage.rs` | Claude token tracking |

### Config file

`~/.clawborrator/shadows-desktop.json` caches: access token, learned hub URL, `machine_id` (stable per-install UUID), and the shadows URL used for pairing. The re-pair wizard (`WizardMode::Repair`) overwrites this file with a fresh token without restarting the daemon.

**Desktop-sharing guardrails** (from desktop_v1): `allowed_roots` and `max_concurrent_sessions` in the config file control what a shared user may spawn.

### Pairing flow

```
1. relay login
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
- `relay-macos-arm64.dmg` — macOS only ships the `.dmg` (raw binary excluded; it's unsigned and Gatekeeper-blocked)
- `relay-windows-x64.exe`
- `relay-linux-x64`

Creating a release: `git tag v0.3.0 && git push origin v0.3.0`

macOS `.dmg` packaging scripts are in `packaging/macos/`. The `.dmg` is unsigned unless `MACOS_CERT_P12` / notarization secrets are configured.

## Relationship to desktop_v1

Kept as a thin fork. Divergence confined to `src/oauth.rs` (device flow targets shadows, returns hub URL) and `src/auth.rs` + login plumbing in `src/main.rs`. Crate dir stays `clawborrator-supervisor/` to keep the fork diff small; the binary is named `relay` (clap `command_name` set explicitly in `main.rs`).

Upstream WS/session fixes can be pulled from desktop_v1 with minimal conflict since the auth modules are the only changed surface area.

## Key constants (`main.rs`)

| Constant | Value | Purpose |
|----------|-------|---------|
| `DAEMON_VERSION` | from Cargo.toml (currently `0.3.0`) | Sent in the `hello` WS frame |
| `DEFAULT_SHADOWS_URL` | `https://shadows-app.fly.dev` | Default pairing target |
| `PING_INTERVAL` | 30s | WS keepalive |
| `LIVENESS_TIMEOUT` | 90s | No-frame deadline before forced reconnect |
| `CONNECT_TIMEOUT` | 20s | WS connect + handshake cap |
| `RECONNECT_BACKOFF_INITIAL` | 1s | Backoff start |
| `RECONNECT_BACKOFF_MAX` | 60s | Backoff cap |

## IPC socket

Uses a **distinct** IPC socket path from `desktop_v1` (different `application_name`/socket path in `ipc.rs`) and a separate scratch directory so both daemons can coexist on the same machine during migration.
