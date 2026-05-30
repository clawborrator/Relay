// Plugin tests — exercises each built-in against the screen-text
// fixtures the operator captured. Each fixture is the rendered
// screen verbatim (no ANSI). Cursor positions in the fixtures are
// set to (0,0) since the plugins consult
// `ScreenView::highlighted_option()` which parses the `>` marker
// out of the text itself — independent of vt100's cursor.

use super::builtin::*;
use super::{Action, ParserPlugin, ScreenView};

fn screen(text: &str) -> ScreenView {
    ScreenView::from_text(text.to_string(), (0, 0))
}

// === Fixtures ===

const TRUST_FOLDER: &str = "Accessing workspace:

 C:\\temp\\a

 Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source
 project, or work from your team). If not, take a moment to review what's in this folder first.

 Claude Code'll be able to read, edit, and execute files here.

 Security guide

 > 1. Yes, I trust this folder
   2. No, exit

 Enter to confirm · Esc to cancel";

const DEV_CHANNELS: &str = "  WARNING: Loading development channels

  --dangerously-load-development-channels is for local channel development only. Do not use this option to run
  channels you have downloaded off the internet.

  Please use --channels to run a list of approved channels.

  Channels: server:clawborrator

  > 1. I am using this for local development
    2. Exit

  Enter to confirm · Esc to cancel";

const MCP_SERVER: &str = "New MCP server found in .mcp.json: MongoDB

  MCP servers may execute code or access system resources. All tool calls require approval. Learn more in the MCP
  documentation.

  > 1. Use this and all future MCP servers in this project
    2. Use this MCP server
    3. Continue without using this MCP server

  Enter to confirm · Esc to cancel";

const BYPASS_PERMISSIONS: &str = "  WARNING: Claude Code running in Bypass Permissions mode

  In Bypass Permissions mode, Claude Code will not ask for your approval before running potentially dangerous
  commands.
  This mode should only be used in a sandboxed container/VM that has restricted internet access and can easily be
  restored if damaged.

  By proceeding, you accept all responsibility for actions taken while running in Bypass Permissions mode.

  https://code.claude.com/docs/en/security

  > 1. No, exit
    2. Yes, I accept

  Enter to confirm · Esc to cancel";

const NO_RESUME: &str = "No conversations found to resume.
Press Ctrl+C to exit and start a new conversation.";

const NO_CONTINUE: &str = "No conversation found to continue";

const RESUME_PICKER: &str = "Resume session
  ╭──────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
  │ ⌕ Search…                                                                                                        │
  ╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
    a

  > asdf
    6 seconds ago · HEAD · 11.5KB

    Ctrl+A to show all projects · Ctrl+B to only show current branch · Space to preview · Ctrl+R to rename · Type to
    search · Esc to cancel";

const RESUME_SUMMARY: &str = "This session is 1h 34m old and 290.3k tokens.

  Resuming the full session will consume a substantial portion of your usage limits. We recommend resuming from a summary.

  > 1. Resume from summary (recommended)
    2. Resume full session as-is
    3. Don't ask me again

  Enter to confirm · Esc to cancel";

const RESUME_SUMMARY_LIVE: &str = "This session is 1h 34m old and 290.3k tokens.

  Resuming the full session will consume a substantial portion of your usage limits. We recommend resuming from a summary.

  ❯ 1. Resume from summary (recommended)
    2. Resume full session as-is
    3. Don't ask me again

  Enter to confirm · Esc to cancel";

// In-session AskUserQuestion tool prompt — looks like a startup
// prompt (numbered options with `>` cursor on option 1) but is
// triggered mid-session, not at boot. We must NOT auto-dismiss
// these. Differentiated by the `Enter to select · ↑/↓ to navigate`
// footer (startup prompts say `Enter to confirm · Esc to cancel`)
// and by question-specific text that won't match any built-in
// plugin's sentinel pair. Captured 2026-05-12 from fileId=100.
const ASK_USER_QUESTION: &str = "□ Beverage

Which type of beverage do you prefer?

> 1. Coffee
     Bold, caffeinated, ideal for mornings.
  2. Tea
     Lighter caffeine, wide variety of flavors.
  3. Water
     Plain and hydrating.
  4. Soda
     Sweet and fizzy.
  5. Type something.

  6. Chat about this

Enter to select · ↑/↓ to navigate · Esc to cancel";

const UNRELATED: &str = "claude > some unrelated screen content without prompts";

// === Helpers ===

fn assert_matches_enter(plugin: &dyn ParserPlugin, text: &str) {
    let s = screen(text);
    match plugin.inspect(&s) {
        Some(Action::WriteBytes(b)) => assert_eq!(b, b"\r"),
        other => panic!("{} did not return Enter for fixture; got {:?}", plugin.name(), other),
    }
}

fn assert_matches_down_enter(plugin: &dyn ParserPlugin, text: &str) {
    let s = screen(text);
    match plugin.inspect(&s) {
        Some(Action::WriteSequence(chunks)) => {
            // Expect: SS3 B (app-cursor arrow-down) first, then
            // Enter after a delay > 0. The watcher dispatches each
            // chunk with its `delay_ms` before-write.
            assert_eq!(chunks.len(), 2, "expected 2-chunk sequence (↓ then Enter)");
            assert_eq!(chunks[0].1, b"\x1bOB", "first chunk should be SS3 B (app-cursor ↓)");
            assert!(chunks[1].0 > 0, "second chunk should have a delay so Ink re-renders before Enter");
            assert_eq!(chunks[1].1, b"\r", "second chunk should be Enter");
        }
        other => panic!("{} did not return ↓+Enter sequence; got {:?}", plugin.name(), other),
    }
}

fn assert_matches_restart(plugin: &dyn ParserPlugin, text: &str, flag: &str) {
    let s = screen(text);
    match plugin.inspect(&s) {
        Some(Action::RestartWithoutFlag(f)) => assert_eq!(f, flag),
        other => panic!("{} did not return RestartWithoutFlag for fixture; got {:?}", plugin.name(), other),
    }
}

fn assert_no_match(plugin: &dyn ParserPlugin, text: &str) {
    let s = screen(text);
    assert!(plugin.inspect(&s).is_none(),
            "{} false-matched against `{}`", plugin.name(), text.lines().next().unwrap_or(""));
}

// === TrustFolder ===

#[test] fn trust_folder_fires() { assert_matches_enter(&TrustFolder, TRUST_FOLDER); }
#[test] fn trust_folder_ignores_dev_channels() { assert_no_match(&TrustFolder, DEV_CHANNELS); }
#[test] fn trust_folder_ignores_mcp() { assert_no_match(&TrustFolder, MCP_SERVER); }
#[test] fn trust_folder_ignores_bypass() { assert_no_match(&TrustFolder, BYPASS_PERMISSIONS); }
#[test] fn trust_folder_ignores_no_resume() { assert_no_match(&TrustFolder, NO_RESUME); }
#[test] fn trust_folder_ignores_unrelated() { assert_no_match(&TrustFolder, UNRELATED); }

// === DevChannels ===

#[test] fn dev_channels_fires() { assert_matches_enter(&DevChannels, DEV_CHANNELS); }
#[test] fn dev_channels_ignores_trust() { assert_no_match(&DevChannels, TRUST_FOLDER); }
#[test] fn dev_channels_ignores_mcp() { assert_no_match(&DevChannels, MCP_SERVER); }
#[test] fn dev_channels_ignores_bypass() { assert_no_match(&DevChannels, BYPASS_PERMISSIONS); }
#[test] fn dev_channels_ignores_unrelated() { assert_no_match(&DevChannels, UNRELATED); }

// === McpServer ===

#[test] fn mcp_fires() { assert_matches_enter(&McpServer, MCP_SERVER); }
#[test] fn mcp_ignores_trust() { assert_no_match(&McpServer, TRUST_FOLDER); }
#[test] fn mcp_ignores_dev_channels() { assert_no_match(&McpServer, DEV_CHANNELS); }
#[test] fn mcp_ignores_bypass() { assert_no_match(&McpServer, BYPASS_PERMISSIONS); }
#[test] fn mcp_ignores_unrelated() { assert_no_match(&McpServer, UNRELATED); }

// === BypassPermissions ===

#[test] fn bypass_fires_with_down_enter() { assert_matches_down_enter(&BypassPermissions, BYPASS_PERMISSIONS); }
#[test] fn bypass_ignores_trust() { assert_no_match(&BypassPermissions, TRUST_FOLDER); }
#[test] fn bypass_ignores_dev_channels() { assert_no_match(&BypassPermissions, DEV_CHANNELS); }
#[test] fn bypass_ignores_mcp() { assert_no_match(&BypassPermissions, MCP_SERVER); }
#[test] fn bypass_ignores_unrelated() { assert_no_match(&BypassPermissions, UNRELATED); }
#[test] fn bypass_skips_when_cursor_already_on_option_2() {
    // If the operator manually moved cursor to option 2 before the
    // plugin fired, we should NOT also press ↓ — that would move
    // off the right answer. The plugin gates on opt == 1 so this
    // case returns None.
    let text = BYPASS_PERMISSIONS.replace("> 1. No, exit\n    2. Yes, I accept",
                                          "  1. No, exit\n  > 2. Yes, I accept");
    assert_no_match(&BypassPermissions, &text);
}

// === NoResume / NoContinue ===

#[test] fn no_resume_fires() { assert_matches_restart(&NoResume, NO_RESUME, "--resume"); }
#[test] fn no_resume_ignores_continue_text() { assert_no_match(&NoResume, NO_CONTINUE); }
#[test] fn no_resume_ignores_unrelated() { assert_no_match(&NoResume, UNRELATED); }

#[test] fn no_continue_fires() { assert_matches_restart(&NoContinue, NO_CONTINUE, "--continue"); }
#[test] fn no_continue_ignores_resume_text() { assert_no_match(&NoContinue, NO_RESUME); }
#[test] fn no_continue_ignores_unrelated() { assert_no_match(&NoContinue, UNRELATED); }

// === ResumePicker ===

#[test] fn resume_picker_fires() { assert_matches_enter(&ResumePicker, RESUME_PICKER); }
#[test] fn resume_picker_ignores_no_resume_text() { assert_no_match(&ResumePicker, NO_RESUME); }
#[test] fn resume_picker_ignores_unrelated() { assert_no_match(&ResumePicker, UNRELATED); }
#[test] fn resume_picker_skips_when_no_highlight() {
    // Picker UI rendered but no session highlighted yet (e.g. mid-load).
    let text = RESUME_PICKER.replace("  > asdf", "    asdf");
    assert_no_match(&ResumePicker, &text);
}
#[test] fn resume_picker_ignores_no_resume_sentinel() {
    // "Resume session" appears but the picker chrome is missing
    // (Ctrl+A hint). Don't fire — could be a different screen.
    let stripped = RESUME_PICKER.replace("Ctrl+A to show all projects", "");
    assert_no_match(&ResumePicker, &stripped);
}

// === ResumeSummary ===

#[test] fn resume_summary_fires() { assert_matches_enter(&ResumeSummary, RESUME_SUMMARY); }
#[test] fn resume_summary_fires_with_heavy_angle_marker() {
    assert_matches_enter(&ResumeSummary, RESUME_SUMMARY_LIVE);
}
#[test] fn resume_summary_ignores_resume_picker() { assert_no_match(&ResumeSummary, RESUME_PICKER); }
#[test] fn resume_summary_ignores_no_resume() { assert_no_match(&ResumeSummary, NO_RESUME); }
#[test] fn resume_summary_ignores_trust() { assert_no_match(&ResumeSummary, TRUST_FOLDER); }
#[test] fn resume_summary_ignores_unrelated() { assert_no_match(&ResumeSummary, UNRELATED); }
#[test] fn resume_summary_skips_when_cursor_already_on_full_resume() {
    // Operator manually moved cursor to option 2 — don't press
    // Enter on a non-default selection that has a different
    // billing impact.
    let text = RESUME_SUMMARY.replace("> 1. Resume from summary",
                                      "  1. Resume from summary")
                              .replace("2. Resume full session",
                                       "> 2. Resume full session");
    assert_no_match(&ResumeSummary, &text);
}

// === Cross-plugin isolation: only the right plugin fires per fixture ===

#[test] fn each_fixture_matches_exactly_one_plugin() {
    let plugins = super::builtin::default_plugins();
    let cases: &[(&str, &str)] = &[
        ("trust-folder",       TRUST_FOLDER),
        ("dev-channels",       DEV_CHANNELS),
        ("mcp-server",         MCP_SERVER),
        ("bypass-permissions", BYPASS_PERMISSIONS),
        ("no-resume",          NO_RESUME),
        ("no-continue",        NO_CONTINUE),
        ("resume-picker",      RESUME_PICKER),
        ("resume-summary",     RESUME_SUMMARY),
    ];
    for (expected, text) in cases {
        let s = screen(text);
        let matches: Vec<&'static str> = plugins.iter()
            .filter(|p| p.inspect(&s).is_some())
            .map(|p| p.name())
            .collect();
        assert_eq!(matches, vec![*expected],
                   "expected only `{}` to match its fixture, got {:?}", expected, matches);
    }
}

// === No-plugin-fires defense: AskUserQuestion in-session prompt ===

#[test] fn no_builtin_plugin_fires_on_ask_user_question() {
    // In-session AskUserQuestion tool calls render a startup-shaped
    // prompt (`> 1. Foo` cursor + numbered options) but must NEVER
    // be auto-dismissed — the operator is being asked something
    // and the supervisor stepping in would corrupt the conversation.
    // Every built-in plugin must guard with a sentinel pair tight
    // enough to ignore this fixture. Regression captured from
    // fileId=100 on 2026-05-12.
    let s = screen(ASK_USER_QUESTION);
    let plugins = super::builtin::default_plugins();
    let matches: Vec<&'static str> = plugins.iter()
        .filter(|p| p.inspect(&s).is_some())
        .map(|p| p.name())
        .collect();
    assert!(matches.is_empty(),
            "AskUserQuestion prompt false-matched plugin(s): {:?}", matches);
}

// === ScreenView::highlighted_option parser ===

#[test] fn highlighted_option_parses_basic() {
    let s = screen("  > 1. Foo\n    2. Bar\n");
    assert_eq!(s.highlighted_option(), Some((0, 1)));
}
#[test] fn highlighted_option_parses_option_2() {
    let s = screen("    1. Foo\n  > 2. Bar\n");
    assert_eq!(s.highlighted_option(), Some((1, 2)));
}
#[test] fn highlighted_option_handles_no_marker() {
    let s = screen("    1. Foo\n    2. Bar\n");
    assert_eq!(s.highlighted_option(), None);
}
#[test] fn highlighted_option_accepts_heavy_angle_marker() {
    // Current CC builds use ❯ (U+276F) instead of `>`. Plugins
    // must match both — regression captured 2026-05-11 from a
    // live stuck-at-dev-channels screen.
    let s = screen("  ❯ 1. Foo\n    2. Bar\n");
    assert_eq!(s.highlighted_option(), Some((0, 1)));
}
#[test] fn highlighted_option_accepts_heavy_angle_option_2() {
    let s = screen("    1. Foo\n  ❯ 2. Bar\n");
    assert_eq!(s.highlighted_option(), Some((1, 2)));
}

#[test] fn has_cursor_highlight_matches_both_markers() {
    assert!(screen("  > asdf").has_cursor_highlight());
    assert!(screen("  ❯ asdf").has_cursor_highlight());
    assert!(!screen("    asdf").has_cursor_highlight());
    assert!(!screen("  > ").has_cursor_highlight());     // empty after marker
    assert!(!screen("  ❯ ").has_cursor_highlight());     // empty after marker
}

// === Live-CC fixtures (heavy-angle ❯ marker) ===

const DEV_CHANNELS_LIVE: &str = "  WARNING: Loading development channels

  --dangerously-load-development-channels is for local channel development only. Do not use this option to run
  channels you have downloaded off the internet.

  Please use --channels to run a list of approved channels.

  Channels: server:clawborrator

  ❯ 1. I am using this for local development
    2. Exit

  Enter to confirm · Esc to cancel";

#[test] fn dev_channels_fires_with_heavy_angle_marker() {
    // Verbatim screen captured from a stuck v0.2.10 watcher — the
    // bug that motivated this regression test.
    assert_matches_enter(&DevChannels, DEV_CHANNELS_LIVE);
}

#[test] fn trust_folder_fires_with_heavy_angle_marker() {
    let text = TRUST_FOLDER.replace("> 1. Yes, I trust this folder",
                                    "❯ 1. Yes, I trust this folder");
    assert_matches_enter(&TrustFolder, &text);
}

#[test] fn mcp_fires_with_heavy_angle_marker() {
    let text = MCP_SERVER.replace("> 1. Use this and all future MCP servers",
                                  "❯ 1. Use this and all future MCP servers");
    assert_matches_enter(&McpServer, &text);
}

#[test] fn bypass_fires_with_heavy_angle_marker() {
    let text = BYPASS_PERMISSIONS.replace("> 1. No, exit", "❯ 1. No, exit");
    assert_matches_down_enter(&BypassPermissions, &text);
}

#[test] fn resume_picker_fires_with_heavy_angle_marker() {
    let text = RESUME_PICKER.replace("> asdf", "❯ asdf");
    assert_matches_enter(&ResumePicker, &text);
}
