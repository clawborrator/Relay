// Auto-install of the session prerequisites (Node + Claude Code), driven
// by the setup wizard's "Install prerequisites" button and the
// `install-prereqs` CLI command. Both are user-space — no admin/sudo:
//
//   - Node: the official prebuilt release is downloaded and extracted
//     into ~/.clawborrator/node/, whose bin/ is on every session's PATH
//     (see spawn::session_path_prepend_dirs). Relay manages this Node, so
//     a fresh machine needs nothing preinstalled.
//   - Claude Code: its official install script (curl … | bash), which
//     drops a native `claude` binary into ~/.local/bin.
//
// All platforms. The wizard surfaces this on macOS + Windows; the CLI
// (`install-prereqs`) works everywhere.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Relay-managed Node root (`~/.clawborrator/node`).
pub(crate) fn managed_node_root(home: &Path) -> PathBuf {
    home.join(".clawborrator").join("node")
}

/// The dir of the managed Node binaries, added to every session's PATH so
/// node/npm/npx resolve there when nothing else provides them. The Windows
/// `.zip` puts node.exe at the package root; unix tarballs use `bin/`.
pub(crate) fn managed_node_bin_dir(home: &Path) -> PathBuf {
    #[cfg(windows)]
    { managed_node_root(home) }
    #[cfg(not(windows))]
    { managed_node_root(home).join("bin") }
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home dir"))
}

fn arch_tag() -> Result<&'static str> {
    if cfg!(target_arch = "aarch64") {
        Ok("arm64")
    } else if cfg!(target_arch = "x86_64") {
        Ok("x64")
    } else {
        Err(anyhow!("unsupported CPU architecture for a Node download"))
    }
}

fn os_tag() -> &'static str {
    if cfg!(target_os = "macos") { "darwin" }
    else if cfg!(target_os = "windows") { "win" }
    else { "linux" }
}

/// Newest Node LTS version string (e.g. "v22.20.0") from the official
/// dist index. The array is newest-first; the first entry whose `lts`
/// field is a codename string (rather than `false`) is the latest LTS.
async fn latest_lts(client: &reqwest::Client) -> Result<String> {
    let txt = client
        .get("https://nodejs.org/dist/index.json")
        .send().await?
        .error_for_status()?
        .text().await?;
    let arr: Vec<serde_json::Value> =
        serde_json::from_str(&txt).context("parsing the Node dist index")?;
    for e in arr {
        if e.get("lts").map(|l| l.is_string()).unwrap_or(false) {
            if let Some(v) = e.get("version").and_then(|v| v.as_str()) {
                return Ok(v.to_string());
            }
        }
    }
    Err(anyhow!("no LTS release found in the Node dist index"))
}

/// Download + extract the official Node release into the managed dir.
/// Replaces any prior managed Node so re-running upgrades cleanly.
pub(crate) async fn install_node<F: Fn(&str)>(progress: &F) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("relay/", env!("CARGO_PKG_VERSION")))
        .build()?;

    progress("Resolving the latest Node LTS…");
    let ver = latest_lts(&client).await?;
    let (os, arch) = (os_tag(), arch_tag()?);
    // Windows ships a .zip (node.exe at the root); unix a .tar.gz (bin/).
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let name = format!("node-{ver}-{os}-{arch}");
    let url = format!("https://nodejs.org/dist/{ver}/{name}.{ext}");

    progress(&format!("Downloading Node {ver} ({os}-{arch})…"));
    let bytes = client
        .get(&url).send().await?
        .error_for_status().with_context(|| format!("downloading {url}"))?
        .bytes().await?;

    let tmp = std::env::temp_dir().join(format!("{name}-{}.{ext}", std::process::id()));
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing {tmp:?}"))?;

    let root = managed_node_root(&home_dir()?);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).with_context(|| format!("creating {root:?}"))?;

    progress("Extracting Node…");
    // --strip-components=1 drops the top-level `node-vX-os-arch/` dir, so
    // the layout lands directly under <root>. `tar` ships with macOS,
    // Linux, and Windows 10+ (bsdtar, which also reads .zip).
    let mut tar = std::process::Command::new("tar");
    if ext == "zip" { tar.arg("-xf"); } else { tar.arg("-xzf"); }
    let out = tar
        .arg(&tmp)
        .arg("--strip-components=1")
        .arg("-C").arg(&root)
        .output()
        .context("running tar to extract Node")?;
    let _ = std::fs::remove_file(&tmp);
    if !out.status.success() {
        return Err(anyhow!("tar failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }

    let node_exe = if cfg!(windows) { "node.exe" } else { "node" };
    let node_bin = managed_node_bin_dir(&home_dir()?).join(node_exe);
    if !node_bin.is_file() {
        return Err(anyhow!("Node binary missing after extract: {node_bin:?}"));
    }
    progress(&format!("Node {ver} installed."));
    Ok(())
}

/// Run Claude Code's official installer (user-space). Unix uses the
/// install.sh script; Windows uses the PowerShell bootstrap. Both are the
/// vendor's documented one-liners.
pub(crate) fn install_claude<F: Fn(&str)>(progress: &F) -> Result<()> {
    progress("Installing Claude Code…");
    #[cfg(windows)]
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", "irm https://claude.ai/install.ps1 | iex"])
        .output()
        .context("running the Claude Code installer")?;
    #[cfg(not(windows))]
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("curl -fsSL https://claude.ai/install.sh | bash")
        .output()
        .context("running the Claude Code installer")?;
    if !out.status.success() {
        return Err(anyhow!(
            "Claude Code installer failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    progress("Claude Code installed.");
    Ok(())
}

/// Install whatever the prereq check reports missing. No-op (beyond a
/// progress note) when everything is already present.
pub(crate) async fn install_missing<F: Fn(&str)>(progress: F) -> Result<()> {
    let prereqs = crate::check_prereqs();
    let node_missing =
        prereqs.iter().any(|p| matches!(p.name, "node" | "npm" | "npx") && !p.found());
    let claude_missing = prereqs.iter().any(|p| p.name == "claude" && !p.found());

    if node_missing {
        install_node(&progress).await?;
    }
    if claude_missing {
        install_claude(&progress)?;
    }
    if !node_missing && !claude_missing {
        progress("All prerequisites already present.");
    }
    Ok(())
}
