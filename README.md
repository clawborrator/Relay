# shadows-desktop

A desktop daemon that registers your machine with a [clawborrator](https://github.com/clawborrator)
hub and runs Claude Code sessions on it, controlled from the
[shadows](https://github.com/clawborrator/shadows) web app.

This is a fork of `desktop_v1` (`clawborrator-supervisor`). The ONLY
difference is how it authenticates: instead of pairing against the hub's
GitHub OAuth, it pairs against the **shadows app**, where you sign in with
Google or Zoho. shadows brokers a hub token for your shadow principal and
hands it back along with the hub URL. Everything downstream (the
`/supervisor` WebSocket, session spawn/kill/restart, the channel-token
plumbing) is unchanged from desktop_v1.

See `hub_v1/docs/SHADOWS-SCOPE.md` and `HUB-ABB-SHADOW-PRINCIPAL.md` for
the design.

## How pairing works

```
1. shadows-desktop login --shadows-url https://shadows-app.fly.dev
     -> prints a user code, polls shadows /device/token
2. you open the shadows app (already signed in with Google/Zoho),
   go to "Pair a machine", enter the code
3. shadows ensures your hub shadow principal, mints it a cw_app_ token,
   and returns { access_token, hub_url } to the daemon
4. the daemon caches both and connects to <hub_url>/supervisor as your
   shadow principal. Spawn + drive sessions from the shadows app.
```

The hub never sees Google/Zoho; it only sees an email-keyed shadow
principal. The shadows app's SSO is the trust root.

## Build

```
cargo build --release
# binary: target/release/shadows-desktop
```

Cargo 1.75+ (workspace pins the toolchain). Builds on Linux, macOS,
Windows (same platform support as desktop_v1).

## Usage

```
# Pair (one time). --shadows-url defaults to https://shadows-app.fly.dev.
shadows-desktop login [--shadows-url https://your-shadows.example.com]

# Run the daemon (uses the cached token + hub URL from pairing).
shadows-desktop

# Launch at logon (optional).
shadows-desktop install-task     # uninstall-task / task-status to manage

# Unpair (revokes the token on the hub, clears local cache).
shadows-desktop logout
```

Config (token, learned hub URL, machine_id, the shadows URL paired
against) is cached at `~/.clawborrator/shadows-desktop.json`. The
`machine_id` is a stable per-install uuid that survives re-pairing.

### Desktop-sharing guardrails

Carried over from desktop_v1 (see `hub_v1/docs/DESKTOP-SHARING.md`): edit
`~/.clawborrator/shadows-desktop.json` to set `allowed_roots` (folders a
shared user may spawn sessions under) and `max_concurrent_sessions`.

## Relationship to desktop_v1

Kept as a thin fork so upstream session/WS fixes are easy to pull in. The
divergence is confined to `src/oauth.rs` (the device flow targets shadows
and returns the hub URL) and `src/auth.rs` + the `login` plumbing in
`src/main.rs`. The crate dir is still `clawborrator-supervisor/` to keep
the fork diff against upstream small; the built binary is `shadows-desktop`.
