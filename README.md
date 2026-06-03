# Relay

Relay is a desktop app that registers your machine with a
[clawborrator](https://github.com/clawborrator) hub and runs Claude Code
sessions on it, controlled from the
[shadows](https://github.com/clawborrator/shadows) web app.

It's a fork of `desktop_v1` (`clawborrator-supervisor`). The ONLY
behavioral difference is how it authenticates: instead of pairing against
the hub's GitHub OAuth, it pairs against the **shadows app**, where you
sign in with Google or Zoho. shadows brokers a hub token for your shadow
principal and hands it back along with the hub URL. Everything downstream
(the `/supervisor` WebSocket, session spawn/kill/restart, the
channel-token plumbing) is unchanged from desktop_v1.

The hub never sees Google/Zoho — only an email-keyed shadow principal.
The shadows app's SSO is the trust root.

## Install

Grab the latest build from the [Releases](../../releases) page.

### macOS (app)

1. Download `relay-macos-arm64.dmg`, open it, and drag **Relay** to
   Applications.
2. Launch Relay. A setup window walks you through pairing (below); once
   paired it installs a login agent and lives in the menu bar.

The `.dmg` is Developer-ID-signed and notarized, so it opens with a
normal double-click — no Gatekeeper override needed.

### Windows

Download and run `relay-windows-x64.exe`. First launch shows the setup
wizard, then installs a per-user Task Scheduler entry and runs in the
system tray. No admin elevation required.

### Linux / headless

Download `relay-linux-x64` (or the raw `relay-macos-arm64` CLI binary),
mark it executable, and pair from the terminal — Linux runs headless via
a systemd-user service.

### Build from source

```
cargo build --release
# binary: target/release/relay
```

Cargo 1.75+ (the workspace pins the toolchain). Builds on Linux, macOS,
and Windows.

## Pairing

On macOS/Windows the GUI wizard does this on first launch. From the CLI:

```
1. relay login [--shadows-url https://shadows-app.fly.dev]
     -> prints a user code, polls shadows /device/token
2. open the shadows app (already signed in with Google/Zoho),
   go to "Pair a machine", enter the code
3. shadows ensures your hub shadow principal, mints it a cw_app_ token,
   and returns { access_token, hub_url } to the daemon
4. the daemon caches both and connects to <hub_url>/supervisor as your
   shadow principal. Spawn + drive sessions from the shadows app.
```

`--shadows-url` defaults to `https://shadows-app.fly.dev`; point it at
your own shadows instance to pair there.

## CLI

```
relay login [--shadows-url ...]   # pair (one time)
relay                             # run the daemon (menu-bar/tray app on macOS/Windows)
relay install-task                # launch at logon (also uninstall-task / task-status)
relay logout                      # unpair: revoke the token on the hub + clear local cache
relay sessions                    # list managed sessions
relay attach <id>                 # attach a terminal to a session (Ctrl-] to detach)
relay end <id>                    # kill a session
relay new <folder>                # start a session locally
relay prereq-check                # verify claude + node/npm/npx are reachable
```

Global flags (`--shadows-url`, `--hub-url`, `--pat`, `--machine-id`) work
either before or after the subcommand.

## Re-pairing / recovery

If this machine is removed from the shadows app (or its token is
revoked), the menu-bar / tray menu has **"Re-pair this machine…"**. It
re-runs the pairing flow; the running daemon adopts the fresh token on
its next reconnect — no restart needed.

## Node requirement

Sessions run Claude Code, which launches the `clawborrator-mcp` bridge
via Node. Relay adds the usual Node locations to each session's PATH —
`~/.local/bin`, Homebrew, and node version managers (nvm / fnm / volta /
asdf) — so Node is found even when Relay is started by the OS at login
(which doesn't source your shell). Run `relay prereq-check` to confirm
`claude` and `node`/`npm`/`npx` are visible.

## Config

Cached at `~/.clawborrator/shadows-desktop.json`: the token, the learned
hub URL, a stable per-install `machine_id` (survives re-pairing), and the
shadows URL paired against.

### Desktop-sharing guardrails

Carried over from desktop_v1 (see `hub_v1/docs/DESKTOP-SHARING.md`): edit
the config file to set `allowed_roots` (folders a shared user may spawn
sessions under) and `max_concurrent_sessions`.

## Relationship to desktop_v1

Kept as a thin fork so upstream session/WS fixes are easy to pull in. The
divergence is confined to `src/oauth.rs` (the device flow targets shadows
and returns the hub URL) and `src/auth.rs` + the `login` plumbing in
`src/main.rs`. The crate dir is still `clawborrator-supervisor/` to keep
the fork diff against upstream small; the built binary is `relay`.

macOS packaging (`.app` + signed/notarized `.dmg`) and the icon assets
live under `packaging/macos/` and `clawborrator-supervisor/assets/`.
