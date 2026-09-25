//! Resumable Claude Code conversations on this machine, reported to the
//! paired shadows app (PairWave) so a user can pick one — including ones
//! started outside PairWave, in the CLI or the desktop app — and resume it
//! as a managed session (`--resume <id>`, see spawn.rs).
//!
//! What's sent, per conversation: its id, working folder, git branch, a
//! title (Claude Code's own `custom-title` when set, else the first prompt,
//! truncated) and last-activity time. Never message bodies beyond that
//! title, tool output or file contents. Only transcripts touched in the
//! last `MAX_AGE_DAYS`, newest `MAX_CONVERSATIONS`.
//!
//! Opt out with `RELAY_NO_CONVERSATION_LIST=1`.
//!
//! Transcripts can be many MB, so only the head and tail of each file are
//! read, and results are cached by (path, size, mtime) between scans.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::sessions::SessionManager;
use crate::statusline::{claude_config_dir, ReporterConfig};

const TICK: Duration = Duration::from_secs(120);
const HEARTBEAT: Duration = Duration::from_secs(15 * 60);
const MAX_CONVERSATIONS: usize = 200;
const MAX_AGE_DAYS: u64 = 30;
const HEAD_BYTES: u64 = 256 * 1024;
const TAIL_BYTES: u64 = 256 * 1024;
const TITLE_MAX: usize = 120;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    pub session_id: String,
    pub cwd: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// Epoch seconds of the transcript's last write.
    pub last_activity: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report<'a> {
    machine_id: &'a str,
    daemon_version: &'a str,
    conversations: &'a [Conversation],
}

fn one_line(s: &str) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > TITLE_MAX {
        let cut: String = flat.chars().take(TITLE_MAX - 1).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

/// Text of a user prompt, skipping tool results and injected blocks
/// (`<command-name>`, `<system-reminder>`, …) that aren't something a person typed.
fn prompt_text(d: &Value) -> Option<String> {
    if d.get("isSidechain").and_then(Value::as_bool) == Some(true) || d.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let content = d.pointer("/message/content")?;
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let t = unwrap_channel(text.trim());
    if t.is_empty() || t.starts_with('<') {
        return None;
    }
    Some(one_line(t))
}

/// Prompts sent through PairWave arrive wrapped as
/// `<channel source="clawborrator" …>text</channel>`; use the text inside.
fn unwrap_channel(t: &str) -> &str {
    if !t.starts_with("<channel") {
        return t;
    }
    let Some(open_end) = t.find('>') else { return t };
    let inner = &t[open_end + 1..];
    inner.strip_suffix("</channel>").unwrap_or(inner).trim()
}

#[derive(Default)]
struct Parsed {
    cwd: Option<String>,
    branch: Option<String>,
    custom_title: Option<String>,
    first_prompt: Option<String>,
}

fn scan_lines(text: &str, p: &mut Parsed, from_tail: bool) {
    for line in text.lines() {
        let Ok(d) = serde_json::from_str::<Value>(line) else { continue };
        match d.get("type").and_then(Value::as_str) {
            Some("custom-title") => {
                if let Some(t) = d.get("customTitle").and_then(Value::as_str) {
                    if !t.trim().is_empty() {
                        p.custom_title = Some(one_line(t)); // last one wins
                    }
                }
            }
            Some("user") => {
                if p.cwd.is_none() || from_tail {
                    if let Some(c) = d.get("cwd").and_then(Value::as_str) {
                        if p.cwd.is_none() {
                            p.cwd = Some(c.to_string());
                        }
                    }
                }
                if let Some(b) = d.get("gitBranch").and_then(Value::as_str) {
                    if !b.is_empty() && (from_tail || p.branch.is_none()) {
                        p.branch = Some(b.to_string());
                    }
                }
                if p.first_prompt.is_none() && !from_tail {
                    p.first_prompt = prompt_text(&d);
                }
            }
            _ => {}
        }
    }
}

fn read_range(f: &mut std::fs::File, start: u64, len: u64) -> String {
    let mut buf = Vec::with_capacity(len as usize);
    if f.seek(SeekFrom::Start(start)).is_ok() {
        let _ = f.take(len).read_to_end(&mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Parse one transcript. None when it has no folder or nothing to call it.
pub fn parse_transcript(path: &Path, size: u64, mtime: u64) -> Option<Conversation> {
    let session_id = path.file_stem()?.to_str()?.to_string();
    uuid::Uuid::parse_str(&session_id).ok()?;
    let mut f = std::fs::File::open(path).ok()?;
    let mut p = Parsed::default();
    scan_lines(&read_range(&mut f, 0, HEAD_BYTES), &mut p, false);
    if size > HEAD_BYTES {
        let start = size.saturating_sub(TAIL_BYTES).max(HEAD_BYTES);
        // The first line of the tail chunk is usually cut mid-way; serde skips it.
        scan_lines(&read_range(&mut f, start, size - start), &mut p, true);
    }
    let title = p.custom_title.or(p.first_prompt)?;
    Some(Conversation { session_id, cwd: p.cwd?, title, git_branch: p.branch, last_activity: mtime })
}

/// Recent transcripts under `<config>/projects/*/*.jsonl`, newest first.
fn recent_transcripts(root: &Path, now: SystemTime) -> Vec<(PathBuf, u64, u64)> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(root.join("projects")) else { return out };
    let cutoff = now.checked_sub(Duration::from_secs(MAX_AGE_DAYS * 86_400)).unwrap_or(SystemTime::UNIX_EPOCH);
    for dir in dirs.flatten() {
        let Ok(files) = std::fs::read_dir(dir.path()) else { continue };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = f.metadata() else { continue };
            let Ok(modified) = meta.modified() else { continue };
            if !meta.is_file() || modified < cutoff {
                continue;
            }
            let secs = modified.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            out.push((path, meta.len(), secs));
        }
    }
    out.sort_by(|a, b| b.2.cmp(&a.2));
    out
}

/// Parse cache keyed by path, invalidated by (size, mtime).
#[derive(Default)]
pub struct Scanner {
    cache: HashMap<PathBuf, (u64, u64, Option<Conversation>)>,
}

impl Scanner {
    pub fn scan(&mut self, root: &Path, exclude: &HashSet<String>, now: SystemTime) -> Vec<Conversation> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for (path, size, mtime) in recent_transcripts(root, now) {
            if out.len() >= MAX_CONVERSATIONS {
                break;
            }
            seen.insert(path.clone());
            let conv = match self.cache.get(&path) {
                Some((s, m, c)) if *s == size && *m == mtime => c.clone(),
                _ => {
                    let c = parse_transcript(&path, size, mtime);
                    self.cache.insert(path.clone(), (size, mtime, c.clone()));
                    c
                }
            };
            if let Some(c) = conv {
                if !exclude.contains(&c.session_id) {
                    out.push(c);
                }
            }
        }
        self.cache.retain(|p, _| seen.contains(p));
        out
    }
}

fn disabled() -> bool {
    std::env::var("RELAY_NO_CONVERSATION_LIST").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false)
}

/// Every TICK, scan and POST the list to `<shadows_url>/api/relay/conversations`
/// when it changed (or on the heartbeat). Best-effort, like the usage reporter.
pub fn spawn_conversation_reporter(mgr: Arc<SessionManager>, cfg: ReporterConfig) {
    if disabled() {
        info!("conversation list reporting disabled (RELAY_NO_CONVERSATION_LIST)");
        return;
    }
    tokio::spawn(async move {
        let client = match reqwest::Client::builder().timeout(Duration::from_secs(20)).build() {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "conversation reporter: http client");
                return;
            }
        };
        let Some(root) = claude_config_dir() else { return };
        let mut scanner = Scanner::default();
        let mut last_sent: Option<Vec<Conversation>> = None;
        let mut last_post = Instant::now() - HEARTBEAT;
        let mut unsupported_until: Option<Instant> = None;
        info!("conversation reporter started");
        loop {
            tokio::time::sleep(Duration::from_secs(20)).await;
            if unsupported_until.is_some_and(|t| Instant::now() < t) {
                tokio::time::sleep(TICK).await;
                continue;
            }
            let exclude: HashSet<String> = mgr.list_live_cc_session_ids().into_iter().collect();
            let root2 = root.clone();
            let (s, current) = match tokio::task::spawn_blocking(move || {
                let v = scanner.scan(&root2, &exclude, SystemTime::now());
                (scanner, v)
            })
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "conversation scan panicked");
                    return;
                }
            };
            scanner = s;
            let changed = last_sent.as_ref() != Some(&current);
            if changed || last_post.elapsed() >= HEARTBEAT {
                if let Some((shadows_url, token)) = (cfg.creds)() {
                    let url = format!("{}/api/relay/conversations", shadows_url.trim_end_matches('/'));
                    let body = Report { machine_id: &cfg.machine_id, daemon_version: cfg.daemon_version, conversations: &current };
                    match client.post(&url).bearer_auth(&token).json(&body).send().await {
                        Ok(r) if r.status().is_success() => {
                            debug!(conversations = current.len(), "conversations reported");
                            last_sent = Some(current);
                            last_post = Instant::now();
                        }
                        Ok(r) if r.status().as_u16() == 404 => unsupported_until = Some(Instant::now() + HEARTBEAT),
                        Ok(r) => warn!(status = %r.status(), "conversation report rejected"),
                        Err(e) => warn!(error = %e, "conversation report failed"),
                    }
                }
            }
            tokio::time::sleep(TICK).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, lines.join("\n")).unwrap();
        p
    }

    const ID: &str = "52a8edad-7201-4fc2-b04e-f493f683614e";

    #[test]
    fn prefers_latest_custom_title_and_reads_folder() {
        let dir = std::env::temp_dir().join(format!("relay-conv-{}-a", std::process::id()));
        let p = write(&dir, &format!("{ID}.jsonl"), &[
            r#"{"type":"user","cwd":"/w/app","gitBranch":"main","message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#,
            r#"{"type":"user","cwd":"/w/app","gitBranch":"main","message":{"role":"user","content":[{"type":"text","text":"Fix the   login\nbug"}]}}"#,
            r#"{"type":"custom-title","customTitle":"Old name"}"#,
            r#"{"type":"assistant","message":{"content":"secret reply"}}"#,
            r#"{"type":"custom-title","customTitle":"Login bug fix"}"#,
        ]);
        let c = parse_transcript(&p, std::fs::metadata(&p).unwrap().len(), 42).unwrap();
        assert_eq!(c.session_id, ID);
        assert_eq!(c.cwd, "/w/app");
        assert_eq!(c.title, "Login bug fix");
        assert_eq!(c.git_branch.as_deref(), Some("main"));
        assert!(!serde_json::to_string(&c).unwrap().contains("secret"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn falls_back_to_first_prompt_and_skips_unnamed() {
        let dir = std::env::temp_dir().join(format!("relay-conv-{}-b", std::process::id()));
        let p = write(&dir, &format!("{ID}.jsonl"), &[
            r#"{"type":"user","cwd":"/w","message":{"role":"user","content":"Add a healthz endpoint"}}"#,
        ]);
        assert_eq!(parse_transcript(&p, 10, 1).unwrap().title, "Add a healthz endpoint");
        let q = write(&dir, "0b7c8a1e-1111-4222-8333-944455556666.jsonl", &[r#"{"type":"assistant"}"#]);
        assert!(parse_transcript(&q, 10, 1).is_none());
        let r = write(&dir, "not-a-uuid.jsonl", &[r#"{"type":"user","cwd":"/w","message":{"content":"x"}}"#]);
        assert!(parse_transcript(&r, 10, 1).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scanner_excludes_live_sessions_and_old_files() {
        let root = std::env::temp_dir().join(format!("relay-conv-{}-c", std::process::id()));
        let proj = root.join("projects").join("-w");
        write(&proj, &format!("{ID}.jsonl"), &[r#"{"type":"user","cwd":"/w","message":{"content":"hello"}}"#]);
        let mut s = Scanner::default();
        let now = SystemTime::now();
        assert_eq!(s.scan(&root, &HashSet::new(), now).len(), 1);
        let ex: HashSet<String> = [ID.to_string()].into_iter().collect();
        assert!(s.scan(&root, &ex, now).is_empty());
        let later = now + Duration::from_secs((MAX_AGE_DAYS + 1) * 86_400);
        assert!(s.scan(&root, &HashSet::new(), later).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unwraps_prompts_sent_through_pairwave() {
        let dir = std::env::temp_dir().join(format!("relay-conv-{}-d", std::process::id()));
        let p = write(&dir, &format!("{ID}.jsonl"), &[
            r#"{"type":"user","cwd":"/w","message":{"role":"user","content":"<channel source=\"clawborrator\" chat_id=\"x\" sender=\"remote\">\nDoes anything need to be done before the next meeting?\n</channel>"}}"#,
        ]);
        assert_eq!(parse_transcript(&p, 10, 1).unwrap().title, "Does anything need to be done before the next meeting?");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_titles_are_truncated() {
        let t = one_line(&"word ".repeat(100));
        assert!(t.chars().count() <= TITLE_MAX && t.ends_with('…'));
    }
}

#[cfg(test)]
mod live {
    /// `cargo test -p shadows-desktop -- --ignored scan_this_machine --nocapture`
    #[test]
    #[ignore]
    fn scan_this_machine() {
        let root = super::claude_config_dir().unwrap();
        let t = std::time::Instant::now();
        let v = super::Scanner::default().scan(&root, &Default::default(), std::time::SystemTime::now());
        println!("{} conversations in {:?}", v.len(), t.elapsed());
        for c in v.iter().take(8) {
            println!("{} | {} | {} | {:?}", c.session_id, c.title, c.cwd, c.git_branch);
        }
    }
}
