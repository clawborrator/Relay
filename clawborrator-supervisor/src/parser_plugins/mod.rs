// VT100 parser plugin system — pattern-driven prompt handling for
// managed CC sessions.
//
// Replaces the stopgap AUTO_ENTER pump (5s blind \r-spam) with a
// reactive watcher: each plugin owns a sentinel + cursor-position
// predicate, runs against the parser's screen snapshot, and emits an
// Action (write bytes to the PTY, or signal the session to respawn
// without a flag). The watcher fires each plugin at most once per
// session lifetime so a settled prompt doesn't get re-poked.
//
// Built-in plugins live in `builtin.rs`; the framework here is
// plugin-agnostic and unit-testable without a PTY.

pub mod builtin;
pub mod watcher;

#[cfg(test)] mod tests;

/// Plain-text snapshot of the terminal handed to each plugin.
/// `text` is the full rendered screen (rows joined with `\n`).
/// `cursor` is `(row, col)` — same convention as vt100's
/// `Screen::cursor_position`. `lines` is `text` pre-split for
/// per-line scanning so plugins don't redo the work.
pub struct ScreenView {
    pub text:   String,
    /// Raw vt100 cursor position `(row, col)`. Exposed for plugins
    /// that want to gate on cursor location rather than parse the
    /// `>` marker out of the rendered text — none of the built-ins
    /// use it today, but the field is part of the public surface.
    #[allow(dead_code)]
    pub cursor: (u16, u16),
    pub lines:  Vec<String>,
}

impl ScreenView {
    pub fn from_text(text: String, cursor: (u16, u16)) -> Self {
        let lines = text.split('\n').map(|s| s.to_string()).collect();
        Self { text, cursor, lines }
    }

    /// First line whose trimmed-left content starts with a cursor
    /// marker (ASCII `>` or U+276F `❯`). This is the
    /// cursor-highlighted option in CC's interactive prompts
    /// ("❯ 1. Yes, I trust this folder"). Returns the line's index
    /// and the option number parsed off it (1, 2, 3…). Returns
    /// None if the screen doesn't contain such a marker line.
    ///
    /// Both markers are supported because CC's TUI has shipped
    /// builds using each — `>` historically, `❯` (heavy
    /// right-pointing angle quotation) in current releases. Plugin
    /// matching MUST tolerate both or it silently breaks across CC
    /// upgrades.
    pub fn highlighted_option(&self) -> Option<(usize, u32)> {
        for (idx, line) in self.lines.iter().enumerate() {
            let trimmed = line.trim_start();
            let rest = match strip_cursor_marker(trimmed) {
                Some(r) => r.trim_start(),
                None    => continue,
            };
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<u32>() {
                return Some((idx, n));
            }
        }
        None
    }

    /// True if any line on the screen has a cursor-marker prefix
    /// (`>` or `❯`) followed by content. Used by plugins where the
    /// highlighted item isn't a numbered option (e.g. the
    /// `--resume` picker, where the selection is a freeform
    /// session title).
    pub fn has_cursor_highlight(&self) -> bool {
        self.lines.iter().any(|l| {
            let t = l.trim_start();
            match strip_cursor_marker(t) {
                Some(rest) => !rest.trim_start().is_empty(),
                None       => false,
            }
        })
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.text.contains(needle)
    }
}

/// Returns the slice after the cursor-marker prefix (`>` or `❯`),
/// or None if the input doesn't start with either marker. Pulled
/// out so plugins + `highlighted_option` share one source of truth
/// for marker recognition.
fn strip_cursor_marker(s: &str) -> Option<&str> {
    s.strip_prefix('>').or_else(|| s.strip_prefix('❯'))
}

/// What a plugin asks the watcher to do when it matches.
/// `WriteBytes` is the common case — one Enter dismisses most CC
/// prompts. `WriteSequence` is for multi-step input where the
/// chunks must NOT be bundled in a single PTY write (e.g.
/// arrow-down + Enter on the Bypass-Permissions prompt; Ink's
/// keypress parser needs to see the screen re-render between
/// navigation and confirm or it eats the Enter). Each tuple is
/// `(delay_ms_before_write, bytes)` — the delay applies BEFORE
/// the corresponding write, so the first chunk's delay is usually
/// 0. `RestartWithoutFlag` signals that the current spawn was
/// passed a flag that put CC into a dead-end state (e.g.
/// `--continue` with no resumable conversation); the session
/// must be killed and respawned without it.
#[derive(Debug, Clone)]
pub enum Action {
    WriteBytes(Vec<u8>),
    WriteSequence(Vec<(u64, Vec<u8>)>),
    RestartWithoutFlag(String),
}

/// One pattern → one action. `name()` is the stable identifier used
/// by the watcher's fire-once set and in tracing. `inspect()` is
/// pure: it must not mutate or touch I/O, so plugins are trivially
/// unit-testable.
pub trait ParserPlugin: Send + Sync {
    fn name(&self) -> &'static str;
    fn inspect(&self, screen: &ScreenView) -> Option<Action>;
}

/// Bundle of plugins fired by the watcher. The default registry is
/// the full built-in set; tests can build a registry with just one
/// plugin to isolate behavior.
pub struct PluginRegistry {
    plugins: Vec<Box<dyn ParserPlugin>>,
}

impl PluginRegistry {
    pub fn new() -> Self { Self { plugins: Vec::new() } }

    pub fn with_defaults() -> Self {
        let mut r = Self::new();
        for p in builtin::default_plugins() {
            r.plugins.push(p);
        }
        r
    }

    /// Append a plugin to the registry. Lets callers extend the
    /// built-in set or build a registry from scratch in tests.
    #[allow(dead_code)]
    pub fn register(&mut self, plugin: Box<dyn ParserPlugin>) {
        self.plugins.push(plugin);
    }

    pub fn plugins(&self) -> &[Box<dyn ParserPlugin>] { &self.plugins }
}

impl Default for PluginRegistry {
    fn default() -> Self { Self::with_defaults() }
}
