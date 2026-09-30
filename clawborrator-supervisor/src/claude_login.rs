//! Sign Claude Code in again from the shadows app ("Sign in again" on the
//! Machines page), for a machine whose login expired.
//!
//! `claude auth login` with the browser disabled prints a sign-in link and
//! waits at "Paste code here". Relay runs it in a PTY, hands the link to the
//! shadows app (an AI job reply, ai_jobs.rs), and the user opens it on any
//! device, approves, and pastes the code shown back into the app, which sends
//! it here. The code only works with this waiting process (PKCE), and the
//! process is killed after 10 minutes if nothing arrives.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const LINK_WAIT: Duration = Duration::from_secs(25);
const CODE_WAIT: Duration = Duration::from_secs(45);
const ABANDON: Duration = Duration::from_secs(10 * 60);

struct Login {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<String>>,
    started: Instant,
}

static LOGIN: Mutex<Option<Login>> = Mutex::new(None);

/// Strip terminal escape sequences. PURE.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            match it.peek() {
                Some('[') => {
                    it.next();
                    while let Some(&n) = it.peek() {
                        it.next();
                        if n.is_ascii_alphabetic() || n == '~' { break; }
                    }
                }
                Some(']') => {
                    // OSC: up to BEL or ESC \.
                    it.next();
                    while let Some(n) = it.next() {
                        if n == '\x07' { break; }
                        if n == '\x1b' { it.next(); break; }
                    }
                }
                _ => { it.next(); }
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

/// The sign-in link Claude Code printed, if any. PURE.
pub fn find_link(output: &str) -> Option<String> {
    let text = strip_ansi(output);
    let i = text.find("https://")?;
    let rest = &text[i..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    let url = &rest[..end];
    (url.contains("oauth") && url.contains("authorize")).then(|| url.to_string())
}

/// A pasted code is `<code>#<state>`-style base64url. PURE.
pub fn valid_code(code: &str) -> bool {
    !code.is_empty() && code.len() <= 1024 && code.chars().all(|c| c.is_ascii_alphanumeric() || "-_.~#".contains(c))
}

pub fn cancel() {
    if let Some(mut l) = LOGIN.lock().unwrap().take() {
        let _ = l.child.kill();
    }
}

/// Start `claude auth login`; returns the sign-in link.
pub async fn start() -> Result<String, String> {
    cancel();
    let claude = crate::find_on_path("claude").ok_or("Claude Code (`claude`) isn't installed on this machine")?;
    let output = Arc::new(Mutex::new(String::new()));
    let out2 = output.clone();
    let login = tokio::task::spawn_blocking(move || -> Result<Login, String> {
        // Wide, so the link isn't wrapped across lines.
        let pty = native_pty_system()
            .openpty(PtySize { rows: 40, cols: 1000, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| format!("openpty: {e}"))?;
        let mut cmd = CommandBuilder::new(&claude);
        cmd.args(["auth", "login"]);
        cmd.cwd(std::env::temp_dir());
        // Print the link instead of opening a browser on this machine.
        cmd.env("BROWSER", if cfg!(windows) { "echo" } else { "/usr/bin/true" });
        cmd.env("TERM", "xterm-256color");
        if let Some(home) = dirs::home_dir() {
            let mut dirs = crate::spawn::session_path_prepend_dirs(&home);
            if let Some(existing) = std::env::var_os("PATH") {
                dirs.extend(std::env::split_paths(&existing));
            }
            if let Ok(joined) = std::env::join_paths(&dirs) {
                cmd.env("PATH", joined);
            }
        }
        let child = pty.slave.spawn_command(cmd).map_err(|e| format!("couldn't start Claude Code: {e}"))?;
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().map_err(|e| format!("pty: {e}"))?;
        let writer = pty.master.take_writer().map_err(|e| format!("pty: {e}"))?;
        std::thread::spawn(move || {
            let _keep_master = pty.master;
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 { break; }
                let mut o = out2.lock().unwrap();
                o.push_str(&String::from_utf8_lossy(&buf[..n]));
                if o.len() > 64 * 1024 {
                    let cut = o.len() - 32 * 1024;
                    let cut = (cut..o.len()).find(|&i| o.is_char_boundary(i)).unwrap_or(0);
                    o.drain(..cut);
                }
            }
        });
        Ok(Login { child, writer, output: output.clone(), started: Instant::now() })
    })
    .await
    .map_err(|e| format!("login task: {e}"))??;

    let output = login.output.clone();
    *LOGIN.lock().unwrap() = Some(login);
    // Kill it if it's abandoned.
    tokio::spawn(async {
        tokio::time::sleep(ABANDON).await;
        let stale = LOGIN.lock().unwrap().as_ref().map(|l| l.started.elapsed() >= ABANDON).unwrap_or(false);
        if stale { cancel(); }
    });

    let deadline = Instant::now() + LINK_WAIT;
    while Instant::now() < deadline {
        if let Some(link) = find_link(&output.lock().unwrap()) {
            return Ok(link);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let seen = strip_ansi(&output.lock().unwrap());
    cancel();
    Err(format!("Claude Code didn't show a sign-in link. It said: {}", tail(&seen)))
}

fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let t = lines[lines.len().saturating_sub(3)..].join(" · ");
    t.chars().take(300).collect()
}

/// Type the pasted code into the waiting `claude auth login`.
pub async fn finish(code: &str) -> Result<(), String> {
    let code = code.trim();
    if !valid_code(code) {
        return Err("That doesn't look like a sign-in code.".into());
    }
    let output = {
        let mut g = LOGIN.lock().unwrap();
        let l = g.as_mut().ok_or("No sign-in is waiting on this machine. Start again.")?;
        l.output.lock().unwrap().clear();
        l.writer.write_all(format!("{code}\r").as_bytes()).map_err(|e| format!("pty: {e}"))?;
        l.writer.flush().ok();
        l.output.clone()
    };
    let deadline = Instant::now() + CODE_WAIT;
    loop {
        let exited = {
            let mut g = LOGIN.lock().unwrap();
            match g.as_mut() {
                Some(l) => l.child.try_wait().ok().flatten(),
                None => return Err("The sign-in was cancelled.".into()),
            }
        };
        if let Some(status) = exited {
            LOGIN.lock().unwrap().take();
            let said = strip_ansi(&output.lock().unwrap());
            return if status.success() { Ok(()) } else { Err(format!("Sign-in failed: {}", tail(&said))) };
        }
        if Instant::now() >= deadline {
            let said = strip_ansi(&output.lock().unwrap());
            cancel();
            return Err(format!("Claude Code didn't finish signing in: {}", tail(&said)));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_link() {
        let out = "\x1b[2mOpening browser to sign in…\x1b[0m\r\nIf the browser didn't open, visit: https://claude.com/cai/oauth/authorize?code=true&client_id=abc&state=x\r\nPaste code here if prompted >";
        assert_eq!(find_link(out).as_deref(), Some("https://claude.com/cai/oauth/authorize?code=true&client_id=abc&state=x"));
        assert_eq!(find_link("see https://example.com/help"), None);
    }

    #[test]
    fn validates_codes() {
        assert!(valid_code("AbC-12_x~.9#state-Xy"));
        assert!(!valid_code("abc; rm -rf /"));
        assert!(!valid_code("a\nb"));
        assert!(!valid_code(""));
    }
}

#[cfg(test)]
mod live {
    /// Starts (then cancels) a real `claude auth login`:
    /// `cargo test claude_login::live -- --ignored`. Doesn't change the login.
    #[tokio::test]
    #[ignore]
    async fn prints_a_link() {
        let link = super::start().await.expect("got a link");
        super::cancel();
        assert!(link.starts_with("https://") && link.contains("authorize"), "{link}");
        eprintln!("link ok ({} chars)", link.len());
        let s = crate::claude_auth::check_now().await;
        eprintln!("signed in: {}", s.ok);
        assert!(s.ok);
    }
}
