# macOS packaging

The release workflow wraps the `shadows-desktop` binary in a proper
`Relay.app` bundle and a drag-to-Applications `.dmg`
(`relay-macos-arm64.dmg`), then attaches the `.dmg` to the GitHub
Release alongside the raw CLI binary.

- `make-app.sh <binary> <version> <out.app>` — builds the bundle
  (display name **Relay**, executable `shadows-desktop`): copies the
  binary into `Contents/MacOS/`, generates `AppIcon.icns` from
  `clawborrator-supervisor/assets/app-icon.png`, writes `Info.plist`,
  and ad-hoc signs it.
- `make-dmg.sh <app> <out.dmg>` — lays the `.app` next to an
  `/Applications` symlink and builds a compressed UDZO disk image.

They're split so a Developer ID signing step can run on the `.app`
*between* the two (you can't re-sign a bundle after it's sealed in a
dmg).

## Gatekeeper: signed vs. unsigned

macOS quarantines anything downloaded from the internet.

**Unsigned (the default today).** The `.dmg` is ad-hoc signed, which is
enough to run locally but is *not* a Developer ID identity. On a machine
that downloaded it, first launch shows **“Relay” can’t be opened
because Apple cannot check it for malicious software.** Two ways
through:

- Right-click the app in `/Applications` → **Open** → **Open** (only
  needed once), or
- strip the quarantine flag:
  `xattr -dr com.apple.quarantine "/Applications/Relay.app"`

A `.dmg` does not avoid this — only notarization does.

**Signed + notarized (clean double-click).** Requires a paid Apple
Developer account ($99/yr). Once the secrets below exist, the release
workflow signs the bundle with your Developer ID, submits the `.dmg` to
Apple’s notary service, and staples the ticket — after which a
downloaded app opens on a plain double-click with no warning. No code
changes needed; the steps are already in `.github/workflows/release.yml`
and stay dormant until the secrets are set.

## Enabling notarization

Add these as **repository secrets** (Settings → Secrets and variables →
Actions). They’re encrypted at rest, masked in logs, unavailable to
fork PRs, and never embedded in the shipped app — only the resulting
public signature is.

| Secret | What it is |
| --- | --- |
| `MACOS_CERT_P12` | base64 of your *Developer ID Application* certificate exported as `.p12` (`base64 -i cert.p12 \| pbcopy`) |
| `MACOS_CERT_PASSWORD` | the password you set when exporting the `.p12` |
| `MACOS_SIGN_IDENTITY` | the identity string, e.g. `Developer ID Application: Your Name (TEAMID)` |
| `MACOS_NOTARY_APPLE_ID` | your Apple ID email |
| `MACOS_NOTARY_PASSWORD` | an [app-specific password](https://support.apple.com/en-us/102654) for that Apple ID |
| `MACOS_NOTARY_TEAM_ID` | your 10-character Team ID |

The signing step runs when `MACOS_CERT_P12` is present; notarization
additionally requires `MACOS_NOTARY_APPLE_ID`. With neither set, the
build still succeeds and ships the unsigned `.dmg`.

## Icons

All under `clawborrator-supervisor/assets/`, four variants for three jobs:

| Asset | Used for | Look |
| --- | --- | --- |
| `app-icon.png` (1024²) | macOS `AppIcon.icns` (Dock/Finder/Applications) | full-color logo on a white tile |
| `app-icon.ico` (multi-size) | Windows `relay.exe` icon (Explorer/taskbar/Alt-Tab), embedded via `build.rs` + `winresource` | same |
| `tray-white.png` (256²) | macOS menu-bar status item (`include_bytes!` in `tray/mod.rs`) | all-white glyph, transparent bg |
| `tray-color.png` (256²) | Windows notification-area tray (`include_bytes!`, cfg-split) | full-color glyph, transparent bg |

Why two tray glyphs: macOS doesn't tint status items, so white reads on
the dark menu bar; Windows doesn't tint tray icons either, so a colored
glyph stays visible on both light and dark taskbars (a white one would
vanish on a light taskbar).

Regenerating from new source art: the helper logic lives in the icon
processing this repo's history records — trim the real logo on an alpha
threshold (ignore faint halos), enlarge to ~84–92%, and re-center. For a
white-background source, flood-fill the background out before making the
transparent tray glyphs.
