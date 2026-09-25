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
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
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
/// How far into a transcript to look for its folder + first prompt.
const HEAD_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Lines longer than this (big attachments, tool output) are skipped unparsed.
const MAX_LINE: usize = 512 * 1024;
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
    if d.get("isSidechain").and_then(Value::as_bool) == Some(true) {
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
    // Injected turns are marked isMeta, except that prompts sent through
    // PairWave are too (they arrive as <channel> blocks) and are exactly
    // what someone typed.
    let is_channel = text.trim_start().starts_with("<channel");
    if d.get("isMeta").and_then(Value::as_bool) == Some(true) && !is_channel {
        return None;
    }
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

/// Only user turns and title records matter; skip everything else unparsed.
fn interesting(line: &str) -> bool {
    line.len() <= MAX_LINE && (line.contains("\"type\":\"user\"") || line.contains("\"custom-title\""))
}

fn scan_line(line: &str, p: &mut Parsed, from_tail: bool) {
    {
        if !interesting(line) {
            return;
        }
        let Ok(d) = serde_json::from_str::<Value>(line) else { return };
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

/// Stream lines from the start until the folder and first prompt are known.
/// Returns how many bytes were consumed.
fn scan_head(f: &mut std::fs::File, p: &mut Parsed) -> u64 {
    let mut reader = BufReader::with_capacity(64 * 1024, f.take(HEAD_MAX_BYTES));
    let mut buf = Vec::new();
    let mut consumed = 0u64;
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => consumed += n as u64,
        }
        if buf.len() <= MAX_LINE {
            scan_line(&String::from_utf8_lossy(&buf), p, false);
        }
        if p.cwd.is_some() && p.first_prompt.is_some() {
            break;
        }
    }
    consumed
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
    let head = scan_head(&mut f, &mut p);
    if size > head {
        let start = size.saturating_sub(TAIL_BYTES).max(head);
        // The first line of the tail chunk is usually cut mid-way; serde skips it.
        for line in read_range(&mut f, start, size - start).lines() {
            scan_line(line, &mut p, true);
        }
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
        let Some(mut client) = crate::statusline::report_client(Duration::from_secs(20)) else { return };
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
                        Err(e) => {
                            warn!(error = ?e, "conversation report failed; reconnecting next time");
                            if let Some(c) = crate::statusline::report_client(Duration::from_secs(20)) { client = c; }
                        }
                    }
                }
            }
            tokio::time::sleep(TICK).await;
        }
    });
}

// ---------- history upload on resume ----------

const HISTORY_MAX_ITEMS: usize = 400;
const HISTORY_TEXT_MAX: usize = 20_000;
const HISTORY_INPUT_MAX: usize = 8_000;
const HISTORY_MAX_FILE: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HistoryItem {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

/// The conversation id in `--resume <id>` / `--resume=<id>` / `-r <id>`.
pub fn resume_source(flags: &[String]) -> Option<String> {
    let mut it = flags.iter();
    while let Some(f) = it.next() {
        let id = if f == "--resume" || f == "-r" {
            it.next().cloned()
        } else {
            f.strip_prefix("--resume=").map(str::to_string)
        };
        if let Some(id) = id {
            if uuid::Uuid::parse_str(&id).is_ok() {
                return Some(id);
            }
        }
    }
    None
}

/// `<config>/projects/*/<id>.jsonl`
pub(crate) fn find_transcript(root: &Path, id: &str) -> Option<PathBuf> {
    let name = format!("{id}.jsonl");
    std::fs::read_dir(root.join("projects")).ok()?.flatten().map(|d| d.path().join(&name)).find(|p| p.is_file())
}

/// The visible conversation: typed prompts, Claude's replies and tool calls
/// (name + input). Tool results, attachments and sidechains are left out.
pub fn history_items(path: &Path) -> Vec<HistoryItem> {
    let mut out: Vec<HistoryItem> = Vec::new();
    let Ok(f) = std::fs::File::open(path) else { return out };
    let mut reader = BufReader::with_capacity(64 * 1024, f.take(HISTORY_MAX_FILE));
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if buf.len() > 4 * MAX_LINE {
            continue;
        }
        let line = String::from_utf8_lossy(&buf);
        let Ok(d) = serde_json::from_str::<Value>(&line) else { continue };
        if d.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let at = d.get("timestamp").and_then(Value::as_str).map(str::to_string);
        match d.get("type").and_then(Value::as_str) {
            Some("user") => {
                if let Some(text) = prompt_full_text(&d) {
                    out.push(HistoryItem { kind: "user", text: Some(clip(&text, HISTORY_TEXT_MAX)), tool: None, input: None, at });
                }
            }
            Some("assistant") => {
                let Some(Value::Array(parts)) = d.pointer("/message/content") else { continue };
                for p in parts {
                    match p.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let t = p.get("text").and_then(Value::as_str).unwrap_or("").trim();
                            if !t.is_empty() {
                                out.push(HistoryItem { kind: "claude", text: Some(clip(t, HISTORY_TEXT_MAX)), tool: None, input: None, at: at.clone() });
                            }
                        }
                        Some("tool_use") => {
                            let name = p.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                            if name.is_empty() {
                                continue;
                            }
                            let input = p.get("input").map(|v| clip(&v.to_string(), HISTORY_INPUT_MAX));
                            out.push(HistoryItem { kind: "tool", text: None, tool: Some(name), input, at: at.clone() });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if out.len() > HISTORY_MAX_ITEMS {
        out.drain(..out.len() - HISTORY_MAX_ITEMS);
    }
    out
}

/// Like `prompt_text`, but the whole prompt rather than a one-line title.
fn prompt_full_text(d: &Value) -> Option<String> {
    let content = d.pointer("/message/content")?;
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let is_channel = text.trim_start().starts_with("<channel");
    if d.get("isMeta").and_then(Value::as_bool) == Some(true) && !is_channel {
        return None;
    }
    let t = unwrap_channel(text.trim());
    if t.is_empty() || t.starts_with('<') {
        return None;
    }
    Some(t.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryUpload<'a> {
    machine_id: &'a str,
    hub_session_id: &'a str,
    source_session_id: &'a str,
    items: &'a [HistoryItem],
}

/// After a `--resume` spawn: send the resumed conversation's earlier history
/// to `<shadows_url>/api/relay/history` so the new session shows it. The
/// local session row may not exist yet when this runs, so a 404 is retried.
pub async fn upload_history(shadows_url: String, token: String, machine_id: String, hub_session_id: String, source: String) {
    if disabled() {
        return;
    }
    let Some(root) = claude_config_dir() else { return };
    let src = source.clone();
    let items = match tokio::task::spawn_blocking(move || find_transcript(&root, &src).map(|p| history_items(&p))).await {
        Ok(Some(items)) if !items.is_empty() => items,
        _ => return,
    };
    let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(60)).build() else { return };
    let url = format!("{}/api/relay/history", shadows_url.trim_end_matches('/'));
    let body = HistoryUpload { machine_id: &machine_id, hub_session_id: &hub_session_id, source_session_id: &source, items: &items };
    for attempt in 0..10u32 {
        tokio::time::sleep(Duration::from_secs(if attempt == 0 { 5 } else { 10 })).await;
        match client.post(&url).bearer_auth(&token).json(&body).send().await {
            Ok(r) if r.status().is_success() => {
                info!(items = items.len(), "resumed conversation history uploaded");
                return;
            }
            Ok(r) if r.status().as_u16() == 404 => continue,
            Ok(r) => {
                warn!(status = %r.status(), "history upload rejected");
                return;
            }
            Err(e) => warn!(error = %e, "history upload failed"),
        }
    }
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
            r#"{"type":"user","isMeta":true,"cwd":"/w","message":{"role":"user","content":"<channel source=\"clawborrator\" chat_id=\"x\" sender=\"remote\">\nDoes anything need to be done before the next meeting?\n</channel>"}}"#,
        ]);
        assert_eq!(parse_transcript(&p, 10, 1).unwrap().title, "Does anything need to be done before the next meeting?");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_the_prompt_after_a_huge_first_line() {
        let dir = std::env::temp_dir().join(format!("relay-conv-{}-e", std::process::id()));
        let big = format!(r#"{{"type":"attachment","data":"{}"}}"#, "x".repeat(700_000));
        let p = write(&dir, &format!("{ID}.jsonl"), &[
            &big,
            r#"{"type":"user","cwd":"/w/big","message":{"role":"user","content":"After the attachment"}}"#,
        ]);
        let size = std::fs::metadata(&p).unwrap().len();
        let c = parse_transcript(&p, size, 1).unwrap();
        assert_eq!((c.cwd.as_str(), c.title.as_str()), ("/w/big", "After the attachment"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_keeps_prompts_replies_and_tools_only() {
        let dir = std::env::temp_dir().join(format!("relay-conv-{}-h", std::process::id()));
        let p = write(&dir, &format!("{ID}.jsonl"), &[
            r#"{"type":"user","timestamp":"2026-09-24T10:00:00Z","message":{"role":"user","content":"Fix the login bug"}}"#,
            r#"{"type":"assistant","timestamp":"2026-09-24T10:00:05Z","message":{"content":[{"type":"text","text":"Looking."},{"type":"tool_use","name":"Read","input":{"file_path":"/w/a.ts"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"SECRET FILE BODY"}]}}"#,
            r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"subagent prompt"}}"#,
            r#"{"type":"attachment","data":"blob"}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<channel source=\"clawborrator\">\nnow add tests\n</channel>"}}"#,
        ]);
        let items = history_items(&p);
        let kinds: Vec<_> = items.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, ["user", "claude", "tool", "user"]);
        assert_eq!(items[2].tool.as_deref(), Some("Read"));
        assert_eq!(items[3].text.as_deref(), Some("now add tests"));
        assert_eq!(items[0].at.as_deref(), Some("2026-09-24T10:00:00Z"));
        assert!(!serde_json::to_string(&items).unwrap().contains("SECRET"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_the_resume_source() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(resume_source(&s(&["--model", "opus", "--resume", ID])).as_deref(), Some(ID));
        assert_eq!(resume_source(&s(&[&format!("--resume={ID}")])).as_deref(), Some(ID));
        assert_eq!(resume_source(&s(&["-r", ID])).as_deref(), Some(ID));
        assert!(resume_source(&s(&["--resume", "not-a-uuid"])).is_none());
        assert!(resume_source(&s(&["--continue"])).is_none());
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
    fn history_of_this_machine() {
        let root = super::claude_config_dir().unwrap();
        let id = std::env::var("RELAY_TEST_CONV").unwrap_or_default();
        let p = super::find_transcript(&root, &id).expect("transcript");
        let items = super::history_items(&p);
        let bytes = serde_json::to_string(&items).unwrap().len();
        println!("{} items, {} bytes", items.len(), bytes);
        for i in items.iter().take(4) {
            println!("{} {:?} {:?}", i.kind, i.text.as_deref().map(|t| &t[..t.len().min(70)]), i.tool);
        }
    }

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
