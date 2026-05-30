// Local IPC — lets short-lived `clawborrator-supervisor {sessions,
// attach,end,new}` CLI invocations talk to the already-running daemon
// over a per-user local socket (Unix-domain socket on macOS/Linux,
// named pipe on Windows). The daemon owns the live PTYs; these
// subcommands are thin clients of it.
//
// Wire protocol — newline-delimited JSON:
//   client → daemon:  one `Request` line
//   daemon → client:  one `Response` line
// For `attach`, the daemon's response line is `{"t":"attached"}` and
// the connection then becomes a raw bidirectional byte pipe — live
// PTY output daemon→client, keystrokes client→daemon — until either
// side closes (the client detaches with Ctrl-], or the session ends).
//
// All four commands route through the daemon because it is the single
// authority on live sessions: it owns the PTY masters, the vt100
// parsers, and the hub-side calls that `new` needs.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use interprocess::local_socket::{
    tokio::{prelude::*, Stream},
    ListenerOptions, Name,
};
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::sessions::SessionManager;
use crate::spawn::{create_session, input_session, kill_session, CreateArgs};

/// Ctrl-] — the key that detaches an `attach` session (telnet's
/// escape char). Leaves the daemon's session running.
const DETACH_BYTE: u8 = 0x1d;

// ─── wire protocol ──────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    List,
    End { id: String },
    New {
        folder:       String,
        #[serde(default)] routing_name: Option<String>,
        #[serde(default)] flags:        Vec<String>,
    },
    Attach {
        id: String,
        /// The attaching terminal's size. The daemon resizes the
        /// session PTY to match. 0 means "unknown" — keep the default.
        #[serde(default)] cols: u16,
        #[serde(default)] rows: u16,
    },
}

/// One managed session, as reported by `sessions`.
#[derive(Serialize, Deserialize)]
pub struct SessionInfo {
    pub id:           String,
    pub folder:       String,
    pub routing_name: Option<String>,
    pub alive:        bool,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Response {
    Sessions { sessions: Vec<SessionInfo> },
    Created  { session_id: String },
    Ok,
    /// Followed immediately by the raw PTY byte stream.
    Attached,
    Error    { message: String },
}

// ─── socket name ────────────────────────────────────────────────────

/// The per-user IPC endpoint. Unix: a socket file under
/// `~/.clawborrator/` (same dir as the config, easy to find/clean).
/// Windows: a named pipe in the namespaced namespace.
///
/// DISTINCT from desktop_v1's socket (`daemon.sock` /
/// `clawborrator-supervisor-daemon.sock`) so a shadows-desktop daemon and
/// an upstream desktop_v1 daemon can run side by side on one machine
/// without their CLIs (sessions/attach/new/end) crossing wires.
fn ipc_name() -> Result<Name<'static>> {
    #[cfg(unix)]
    {
        let path = dirs::home_dir()
            .ok_or_else(|| anyhow!("could not resolve home dir"))?
            .join(".clawborrator")
            .join("shadows-desktop-daemon.sock");
        path.to_fs_name::<GenericFilePath>()
            .map_err(|e| anyhow!("building socket name: {e}"))
    }
    #[cfg(windows)]
    {
        "shadows-desktop-daemon.sock"
            .to_ns_name::<GenericNamespaced>()
            .map_err(|e| anyhow!("building pipe name: {e}"))
    }
}

// ─── daemon side ────────────────────────────────────────────────────

/// Static bits the `new` handler needs to drive `create_session`.
pub struct IpcConfig {
    pub hub_url:    String,
    pub pat:        String,
    pub machine_id: String,
}

/// Run the IPC listener. Spawned as a background task by `run_daemon`;
/// loops forever accepting client connections.
pub async fn serve(mgr: Arc<SessionManager>, cfg: Arc<IpcConfig>) -> Result<()> {
    let listener = ListenerOptions::new()
        .name(ipc_name()?)
        // Replace a corpse socket left by a previous daemon that exited
        // without cleaning up (crash / SIGKILL).
        .try_overwrite(true)
        .create_tokio()
        .context("creating IPC listener")?;
    info!("IPC listener ready");

    loop {
        match listener.accept().await {
            Ok(conn) => {
                let mgr = mgr.clone();
                let cfg = cfg.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(conn, mgr, cfg).await {
                        warn!(error = %e, "IPC connection handler failed");
                    }
                });
            }
            Err(e) => warn!(error = %e, "IPC accept failed"),
        }
    }
}

/// Write one newline-terminated JSON response. `conn` is shared by
/// reference so the caller can keep reading concurrently.
async fn send_response(conn: &Stream, resp: &Response) -> Result<()> {
    let mut json = serde_json::to_string(resp)?;
    json.push('\n');
    let mut w: &Stream = conn;
    w.write_all(json.as_bytes()).await.context("writing IPC response")?;
    w.flush().await.ok();
    Ok(())
}

async fn handle_conn(conn: Stream, mgr: Arc<SessionManager>, cfg: Arc<IpcConfig>) -> Result<()> {
    // Read exactly one request line. The BufReader is scoped so it's
    // dropped before the `attach` branch needs to own `conn` — the
    // client sends nothing after the request until it sees our reply,
    // so no buffered bytes are lost.
    let req: Request = {
        let mut reader = BufReader::new(&conn);
        let mut line = String::new();
        if reader.read_line(&mut line).await.context("reading IPC request")? == 0 {
            return Ok(()); // client closed without sending anything
        }
        serde_json::from_str(line.trim()).context("parsing IPC request")?
    };

    match req {
        Request::List => {
            let sessions = mgr
                .list_sessions()
                .into_iter()
                .map(|s| SessionInfo {
                    id:           s.id,
                    folder:       s.folder.display().to_string(),
                    routing_name: s.routing_name,
                    alive:        s.alive,
                })
                .collect();
            send_response(&conn, &Response::Sessions { sessions }).await?;
        }
        Request::End { id } => {
            let resp = match kill_session(&mgr, &id) {
                Ok(())  => Response::Ok,
                Err(e)  => Response::Error { message: e.to_string() },
            };
            send_response(&conn, &resp).await?;
        }
        Request::New { folder, routing_name, flags } => {
            let resp = match create_session(&mgr, CreateArgs {
                hub_url:      &cfg.hub_url,
                pat:          &cfg.pat,
                machine_id:   &cfg.machine_id,
                folder:       PathBuf::from(folder),
                routing_name: routing_name.as_deref(),
                extra_flags:  &flags,
                auto_enter:   true,
            }).await {
                Ok(session_id) => Response::Created { session_id },
                Err(e)         => Response::Error { message: e.to_string() },
            };
            send_response(&conn, &resp).await?;
        }
        Request::Attach { id, cols, rows } => {
            handle_attach(conn, mgr, &id, cols, rows).await?;
        }
    }
    Ok(())
}

/// Daemon side of an `attach`: snapshot the session's current screen,
/// subscribe to its live PTY-output broadcast, then proxy bytes both
/// ways until either end closes.
async fn handle_attach(
    conn: Stream,
    mgr:  Arc<SessionManager>,
    id:   &str,
    cols: u16,
    rows: u16,
) -> Result<()> {
    // Resolve the session up front. Subscribe BEFORE snapshotting the
    // screen so no output is missed between the two (a few duplicated
    // bytes just trigger a harmless redraw).
    let (mut rx, screen) = match mgr.get(id) {
        Ok(entry) => {
            let s = entry.lock().unwrap();
            // Resize the session's PTY + vt100 parser to the attaching
            // terminal so Claude Code redraws for that exact geometry.
            // Terminal-agnostic — the client just reports its size.
            if cols > 0 && rows > 0 {
                let _ = s._master.resize(portable_pty::PtySize {
                    rows, cols, pixel_width: 0, pixel_height: 0,
                });
                s.parser.lock().unwrap().set_size(rows, cols);
            }
            let rx = s.output_tx.subscribe();
            let screen = s.parser.lock().unwrap().screen().contents_formatted();
            (rx, screen)
        }
        Err(e) => {
            send_response(&conn, &Response::Error { message: e.to_string() }).await?;
            return Ok(());
        }
    };

    send_response(&conn, &Response::Attached).await?;

    let (mut read_half, mut write_half) = conn.split();
    // Initial paint: clear the client's terminal, then replay the
    // session's current screen so the attach starts in sync.
    write_half.write_all(b"\x1b[2J\x1b[H").await?;
    write_half.write_all(&screen).await?;
    write_half.flush().await?;

    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            // Live PTY output → attached client.
            msg = rx.recv() => match msg {
                Ok(bytes) => {
                    if write_half.write_all(&bytes).await.is_err() { break; }
                    let _ = write_half.flush().await;
                }
                // Slow client fell behind — skip ahead, next full frame
                // from CC will resync the display.
                Err(RecvError::Lagged(_)) => continue,
                // Session ended or was soft-restarted (its sender
                // dropped) — end the attach.
                Err(RecvError::Closed) => break,
            },
            // Client keystrokes → session PTY.
            r = read_half.read(&mut buf) => match r {
                Ok(0) => break, // client detached / disconnected
                Ok(n) => {
                    if input_session(&mgr, id, &buf[..n]).is_err() { break; }
                }
                Err(_) => break,
            },
        }
    }

    // Restore the daemon's default PTY geometry on detach so hub
    // screenshots keep their expected size.
    if let Ok(entry) = mgr.get(id) {
        let s = entry.lock().unwrap();
        let _ = s._master.resize(crate::sessions::fresh_pty_size());
        s.parser.lock().unwrap().set_size(crate::sessions::PTY_ROWS, crate::sessions::PTY_COLS);
    }
    Ok(())
}

// ─── client side ────────────────────────────────────────────────────

async fn connect() -> Result<Stream> {
    Stream::connect(ipc_name()?)
        .await
        .context("connecting to the daemon's IPC socket — is the supervisor daemon running?")
}

/// Send one request, read one response. Used by the non-streaming
/// commands (list / end / new).
async fn request_response(req: &Request) -> Result<Response> {
    let conn = connect().await?;
    {
        let mut json = serde_json::to_string(req)?;
        json.push('\n');
        let mut w: &Stream = &conn;
        w.write_all(json.as_bytes()).await.context("writing IPC request")?;
        w.flush().await.ok();
    }
    let mut reader = BufReader::new(&conn);
    let mut line = String::new();
    if reader.read_line(&mut line).await.context("reading IPC response")? == 0 {
        bail!("daemon closed the connection without responding");
    }
    serde_json::from_str(line.trim()).context("parsing IPC response")
}

pub async fn client_list() -> Result<Vec<SessionInfo>> {
    match request_response(&Request::List).await? {
        Response::Sessions { sessions } => Ok(sessions),
        Response::Error { message }     => bail!("{message}"),
        _                               => bail!("unexpected response to `list`"),
    }
}

pub async fn client_end(id: String) -> Result<()> {
    match request_response(&Request::End { id }).await? {
        Response::Ok                => Ok(()),
        Response::Error { message } => bail!("{message}"),
        _                           => bail!("unexpected response to `end`"),
    }
}

pub async fn client_new(
    folder:       String,
    routing_name: Option<String>,
    flags:        Vec<String>,
) -> Result<String> {
    match request_response(&Request::New { folder, routing_name, flags }).await? {
        Response::Created { session_id } => Ok(session_id),
        Response::Error { message }      => bail!("{message}"),
        _                                => bail!("unexpected response to `new`"),
    }
}

// Windows consoles do not deliver special keys (arrows, Home, End,
// PageUp/Down, function keys) as VT escape sequences on stdin unless
// ENABLE_VIRTUAL_TERMINAL_INPUT is set on the console input handle.
// crossterm's raw mode clears line / echo / processed input — enough
// for printable characters and Enter — but leaves that flag off, so
// the attach pump (which forwards stdin bytes verbatim) would silently
// drop arrow keys on Windows. enable_vt_input() turns the flag on and
// returns the prior console mode; restore_console_mode() reverts it.
#[cfg(windows)]
fn enable_vt_input() -> Option<u32> {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_INPUT,
        STD_INPUT_HANDLE,
    };
    // SAFETY: standard Win32 console calls. We read the current input
    // mode, write it back with one extra flag set, and restore the
    // captured value via restore_console_mode().
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode: u32 = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return None; // stdin is piped / redirected — not a console
        }
        SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_INPUT);
        Some(mode)
    }
}

#[cfg(windows)]
fn restore_console_mode(saved: Option<u32>) {
    if let Some(mode) = saved {
        use windows_sys::Win32::System::Console::{
            GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE,
        };
        // SAFETY: restoring the exact mode captured by enable_vt_input().
        unsafe {
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), mode);
        }
    }
}

/// Attach the local terminal to a managed session: handshake, put the
/// terminal in raw mode, then proxy bytes until the user hits Ctrl-]
/// or the session ends. The terminal is always restored on the way out.
pub async fn client_attach(id: String) -> Result<()> {
    let conn = connect().await?;

    // Report our real terminal size so the daemon resizes the session
    // PTY to match — terminal-agnostic, unlike a client-side resize
    // escape (Windows consoles and several Linux terminals ignore
    // those). (0, 0) when stdout isn't a TTY → daemon keeps its default.
    let (cols, rows) = crossterm::terminal::size().unwrap_or((0, 0));

    // Send the attach request and read the one-line ack with a
    // BufReader we KEEP — the daemon starts streaming the screen
    // immediately after the ack, and `read_line` may have already
    // pulled some of those bytes into the buffer.
    {
        let mut json = serde_json::to_string(&Request::Attach { id: id.clone(), cols, rows })?;
        json.push('\n');
        let mut w: &Stream = &conn;
        w.write_all(json.as_bytes()).await.context("writing attach request")?;
        w.flush().await.ok();
    }
    let mut reader = BufReader::new(&conn);
    let mut line = String::new();
    if reader.read_line(&mut line).await.context("reading attach ack")? == 0 {
        bail!("daemon closed the connection without responding");
    }
    match serde_json::from_str::<Response>(line.trim()).context("parsing attach ack")? {
        Response::Attached          => {}
        Response::Error { message } => bail!("{message}"),
        _                           => bail!("unexpected response to `attach`"),
    }

    eprintln!("Attached to {id} — press Ctrl-] to detach (the session keeps running).");

    crossterm::terminal::enable_raw_mode().context("entering raw terminal mode")?;
    // Windows: also flip on ENABLE_VIRTUAL_TERMINAL_INPUT so the console
    // emits arrows / Home / End / PageUp-Down as VT escape sequences on
    // stdin. Without it the raw-byte pump never sees those keys. Restore
    // the console mode BEFORE disable_raw_mode() so crossterm's own
    // raw-mode teardown lands on the original, pre-attach mode.
    #[cfg(windows)]
    let saved_console_mode = enable_vt_input();
    let result = attach_pump(&conn, reader).await;
    #[cfg(windows)]
    restore_console_mode(saved_console_mode);
    let _ = crossterm::terminal::disable_raw_mode();
    eprintln!("\r\n[detached]");
    result
}

/// The attach byte pump: socket→stdout and stdin→socket, concurrently,
/// until EOF on either side or the detach key. `reader` already holds
/// any screen bytes the ack-read buffered, so it must be the same
/// BufReader used for the handshake.
async fn attach_pump(conn: &Stream, mut reader: BufReader<&Stream>) -> Result<()> {
    let mut sock_w: &Stream = conn;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut sbuf = [0u8; 4096]; // from the socket
    let mut kbuf = [0u8; 4096]; // from the keyboard

    loop {
        tokio::select! {
            r = reader.read(&mut sbuf) => match r {
                Ok(0) => break, // daemon closed — session ended
                Ok(n) => {
                    stdout.write_all(&sbuf[..n]).await?;
                    stdout.flush().await?;
                }
                Err(e) => return Err(e).context("reading from daemon"),
            },
            r = stdin.read(&mut kbuf) => match r {
                Ok(0) => break, // stdin EOF
                Ok(n) => {
                    if let Some(pos) = kbuf[..n].iter().position(|&b| b == DETACH_BYTE) {
                        // Forward anything typed before Ctrl-], then stop.
                        if pos > 0 {
                            sock_w.write_all(&kbuf[..pos]).await?;
                            sock_w.flush().await.ok();
                        }
                        break;
                    }
                    sock_w.write_all(&kbuf[..n]).await?;
                    sock_w.flush().await.ok();
                }
                Err(e) => return Err(e).context("reading stdin"),
            },
        }
    }
    Ok(())
}
