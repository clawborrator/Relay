//! Sessions in their own git worktree.
//!
//! Only one managed session may run in a folder (the MCP's
//! `.claude/clawborrator/` sidecars live there). Claude Code's own
//! `--worktree <name>` flag moves CC into `<repo>/.claude/worktrees/<name>`
//! only AFTER the daemon has already claimed and written sidecars into the
//! repo folder, so a second session in the same repo was refused (and would
//! have clobbered the first one's sidecars if it hadn't been).
//!
//! So the daemon handles `--worktree` itself: it creates (or reuses) the
//! worktree at the same place CC would, on a `worktree-<name>` branch, strips
//! the flag, and spawns CC with that worktree as the session folder. Every
//! per-folder file then lives in the worktree, and the repo folder stays free.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Pull `--worktree <name>` / `--worktree=<name>` out of the flags.
/// Returns the name (if any) and the remaining flags.
pub fn take_worktree_flag(flags: &[String]) -> Result<(Option<String>, Vec<String>)> {
    let mut name = None;
    let mut rest = Vec::with_capacity(flags.len());
    let mut i = 0;
    while i < flags.len() {
        let f = &flags[i];
        if f == "--worktree" || f == "-w" {
            let v = flags.get(i + 1).context("--worktree needs a name")?;
            name = Some(v.clone());
            i += 2;
            continue;
        }
        if let Some(v) = f.strip_prefix("--worktree=") {
            name = Some(v.to_string());
            i += 1;
            continue;
        }
        rest.push(f.clone());
        i += 1;
    }
    if let Some(n) = &name {
        if !valid_name(n) {
            bail!("worktree name must be 1-64 letters, digits, '.', '_' or '-': {n}");
        }
    }
    Ok((name, rest))
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && !n.starts_with('.')
        && !n.starts_with('-')
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("running git (is it installed?)")?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Create (or reuse) `<repo>/.claude/worktrees/<name>` for the repo that
/// contains `folder`, and return its path.
pub fn ensure_worktree(folder: &Path, name: &str) -> Result<PathBuf> {
    let top = PathBuf::from(
        git(folder, &["rev-parse", "--show-toplevel"])
            .with_context(|| format!("{} isn't inside a git repository, so it can't have a worktree", folder.display()))?,
    );
    let root = top.join(".claude").join("worktrees");
    let path = root.join(name);
    if path.join(".git").exists() {
        return Ok(path); // already there (e.g. a restart) — reuse it
    }
    std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    // Keep the worktrees out of the repo's own `git status`.
    let ignore = root.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(&ignore, "*\n");
    }
    let branch = format!("worktree-{name}");
    let path_s = path.to_string_lossy().to_string();
    let branch_exists = git(&top, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
    if branch_exists {
        git(&top, &["worktree", "add", &path_s, &branch])?;
    } else {
        git(&top, &["worktree", "add", "-b", &branch, &path_s])?;
    }
    Ok(path)
}

/// Resolve the folder a session should actually run in, and the flags to
/// pass CC. Without `--worktree` this is a no-op.
pub fn resolve(folder: PathBuf, flags: &[String]) -> Result<(PathBuf, Vec<String>)> {
    let (name, rest) = take_worktree_flag(flags)?;
    match name {
        None => Ok((folder, rest)),
        Some(n) => Ok((ensure_worktree(&folder, &n)?, rest)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn strips_the_flag_in_both_forms() {
        let (n, rest) = take_worktree_flag(&s(&["--model", "opus", "--worktree", "pw-a"])).unwrap();
        assert_eq!(n.as_deref(), Some("pw-a"));
        assert_eq!(rest, s(&["--model", "opus"]));
        let (n, rest) = take_worktree_flag(&s(&["--worktree=pw-b", "--effort", "high"])).unwrap();
        assert_eq!(n.as_deref(), Some("pw-b"));
        assert_eq!(rest, s(&["--effort", "high"]));
        let (n, rest) = take_worktree_flag(&s(&["--resume", "x"])).unwrap();
        assert!(n.is_none());
        assert_eq!(rest, s(&["--resume", "x"]));
    }

    #[test]
    fn rejects_unsafe_names() {
        assert!(take_worktree_flag(&s(&["--worktree", "../x"])).is_err());
        assert!(take_worktree_flag(&s(&["--worktree", "-rf"])).is_err());
        assert!(take_worktree_flag(&s(&["--worktree"])).is_err());
    }

    #[test]
    fn creates_and_reuses_a_worktree() {
        let dir = std::env::temp_dir().join(format!("relay-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |a: &[&str]| assert!(Command::new("git").arg("-C").arg(&dir).args(a).output().unwrap().status.success());
        run(&["init", "-q"]);
        run(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"]);
        let p = ensure_worktree(&dir, "pw-test").unwrap();
        assert!(p.ends_with(".claude/worktrees/pw-test"));
        assert!(p.join(".git").exists());
        assert_eq!(ensure_worktree(&dir, "pw-test").unwrap(), p);
        let status = git(&dir, &["status", "--porcelain"]).unwrap();
        assert!(status.is_empty(), "worktrees should be ignored, got: {status}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
