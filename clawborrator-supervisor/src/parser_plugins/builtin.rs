// Built-in plugins for the six known CC startup prompts.
//
// Naming convention: each plugin's `name()` is a short kebab-case
// identifier used in tracing + the watcher's fire-once set.
//
// Sentinels: each plugin picks a substring that uniquely identifies
// its prompt (i.e. won't false-match against any other CC screen).
// Where the cursor needs to land on a specific option, the plugin
// also checks `ScreenView::highlighted_option()` so we don't fire
// while CC is mid-render with an indeterminate cursor.
//
// Byte sequences:
//   - Plain Enter:     b"\r"
//   - Arrow-down + Enter: b"\x1b[B\r"
//     (ESC [ B is the standard xterm "Cursor Down" sequence)
//
// All sentinels are matched against the raw `screen.contents()`
// from vt100 — no ANSI residue, just plain text from rendered cells.

use super::{Action, ParserPlugin, ScreenView};

pub fn default_plugins() -> Vec<Box<dyn ParserPlugin>> {
    vec![
        Box::new(NoResume),
        Box::new(NoContinue),
        Box::new(ResumePicker),
        Box::new(ResumeSummary),
        Box::new(TrustFolder),
        Box::new(DevChannels),
        Box::new(McpServer),
        Box::new(BypassPermissions),
        Box::new(EnableAutoMode),
    ]
}

// === Sentinel-only restart plugins ============================

/// `--resume` with no resumable conversation → respawn without
/// `--resume`. CC prints the sentinel then waits on Ctrl+C.
pub struct NoResume;
impl ParserPlugin for NoResume {
    fn name(&self) -> &'static str { "no-resume" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if screen.contains("No conversations found to resume") {
            Some(Action::RestartWithoutFlag("--resume".to_string()))
        } else { None }
    }
}

/// `--continue` with no convo to continue → respawn without
/// `--continue`. CC prints the sentinel then exits.
pub struct NoContinue;
impl ParserPlugin for NoContinue {
    fn name(&self) -> &'static str { "no-continue" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if screen.contains("No conversation found to continue") {
            Some(Action::RestartWithoutFlag("--continue".to_string()))
        } else { None }
    }
}

/// `--resume` with at least one resumable conversation → CC shows
/// the "Resume session" picker with the most-recent session
/// highlighted. We auto-pick it by pressing Enter. Sentinel pair
/// ("Resume session" + the picker footer) keeps this from
/// false-matching anywhere else.
pub struct ResumePicker;
impl ParserPlugin for ResumePicker {
    fn name(&self) -> &'static str { "resume-picker" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if !screen.contains("Resume session") { return None; }
        if !screen.contains("Ctrl+A to show all projects") { return None; }
        // Require a cursor highlight on a non-empty line before
        // firing so we don't poke an empty/loading picker. Uses
        // `has_cursor_highlight` which accepts either `>` or `❯`.
        if !screen.has_cursor_highlight() { return None; }
        Some(Action::WriteBytes(b"\r".to_vec()))
    }
}

/// `--resume` / `--continue` against a long-lived session triggers
/// the "resume from summary?" prompt before the regular picker.
/// Default highlight is option 1 ("Resume from summary (recommended)")
/// which is the safer, cheaper path. We confirm with Enter.
///
/// Sentinel pair keeps this from false-matching: the leading-line
/// "This session is … old and … tokens" framing is unique to this
/// prompt and won't appear in the freeform picker or in a started
/// session.
pub struct ResumeSummary;
impl ParserPlugin for ResumeSummary {
    fn name(&self) -> &'static str { "resume-summary" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if !screen.contains("Resume from summary") { return None; }
        if !screen.contains("Resume full session as-is") { return None; }
        let (_, opt) = screen.highlighted_option()?;
        if opt == 1 { Some(Action::WriteBytes(b"\r".to_vec())) } else { None }
    }
}

// === Enter-on-cursor-1 plugins ================================

/// "Quick safety check: Is this a project you created…" trust-folder
/// prompt. Default highlight is option 1 ("Yes, I trust this
/// folder"); we just need to send Enter.
pub struct TrustFolder;
impl ParserPlugin for TrustFolder {
    fn name(&self) -> &'static str { "trust-folder" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if !screen.contains("Quick safety check: Is this a project") { return None; }
        if !screen.contains("Yes, I trust this folder") { return None; }
        let (_, opt) = screen.highlighted_option()?;
        if opt == 1 { Some(Action::WriteBytes(b"\r".to_vec())) } else { None }
    }
}

/// `--dangerously-load-development-channels` warning. Default
/// highlight is option 1 ("I am using this for local development").
pub struct DevChannels;
impl ParserPlugin for DevChannels {
    fn name(&self) -> &'static str { "dev-channels" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if !screen.contains("WARNING: Loading development channels") { return None; }
        if !screen.contains("I am using this for local development") { return None; }
        let (_, opt) = screen.highlighted_option()?;
        if opt == 1 { Some(Action::WriteBytes(b"\r".to_vec())) } else { None }
    }
}

/// "New MCP server found in .mcp.json" — three-option prompt; we
/// pick option 1 ("Use this and all future MCP servers in this
/// project") because the .mcp.json was placed by the daemon itself
/// and is implicitly trusted for the lifetime of the project.
pub struct McpServer;
impl ParserPlugin for McpServer {
    fn name(&self) -> &'static str { "mcp-server" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if !screen.contains("New MCP server found in .mcp.json") { return None; }
        if !screen.contains("Use this and all future MCP servers") { return None; }
        let (_, opt) = screen.highlighted_option()?;
        if opt == 1 { Some(Action::WriteBytes(b"\r".to_vec())) } else { None }
    }
}

// === Arrow-down-then-Enter plugin =============================

/// "Enable auto mode?" — three-option prompt CC shows on first
/// session start. Default highlight is option 1 ("Yes, and make it
/// my default mode"). We confirm with Enter so:
///   - the session enables auto mode (Claude can self-approve safe
///     tool calls; risky ones are blocked, which is what a managed
///     supervisor session wants since there's no operator at the TUI)
///   - the choice persists so we don't see this prompt every session
///
/// Option 3 ("No, exit") EXITS the entire CC process — we must NOT
/// land on it. The option=1 highlight gate ensures we only Enter
/// when the safe default is selected.
///
/// Sentinel pair keeps this from false-matching: "Enable auto mode?"
/// is the heading, "make it my default mode" is unique to this
/// prompt's option 1.
pub struct EnableAutoMode;
impl ParserPlugin for EnableAutoMode {
    fn name(&self) -> &'static str { "enable-auto-mode" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if !screen.contains("Enable auto mode?") { return None; }
        if !screen.contains("make it my default mode") { return None; }
        let (_, opt) = screen.highlighted_option()?;
        if opt == 1 { Some(Action::WriteBytes(b"\r".to_vec())) } else { None }
    }
}

/// `--dangerously-skip-permissions` warning. CC defaults the highlight
/// to option 1 ("No, exit") to make the dangerous path explicit. We
/// want option 2 ("Yes, I accept"), so navigate down and confirm.
///
/// Byte sequence: SS3 B (`\x1bOB`) is the application-cursor-mode
/// arrow-down — CC's Ink TUI sets DECCKM (`\x1b[?1h`) on boot, so
/// the normal-mode CSI B (`\x1b[B`) is silently discarded.
///
/// Why WriteSequence with a delay: Ink's keypress parser needs the
/// render cycle between arrow-down (highlight moves) and Enter
/// (confirm) to commit the new selection. Bundling both bytes
/// into one PTY write made CC eat the Enter on option 1's
/// already-rendered state, leaving the prompt visible. 150ms is
/// generous — Ink typically re-renders within one event-loop turn
/// (<16ms) but cold-boot CC under load can lag.
pub struct BypassPermissions;
impl ParserPlugin for BypassPermissions {
    fn name(&self) -> &'static str { "bypass-permissions" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if !screen.contains("WARNING: Claude Code running in Bypass Permissions mode") { return None; }
        if !screen.contains("Yes, I accept") { return None; }
        let (_, opt) = screen.highlighted_option()?;
        if opt == 1 {
            Some(Action::WriteSequence(vec![
                (0,   b"\x1bOB".to_vec()),  // SS3 B — app-cursor-mode arrow-down
                (150, b"\r".to_vec()),       // Enter, after Ink re-renders
            ]))
        } else { None }
    }
}
