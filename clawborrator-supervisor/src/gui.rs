// First-run setup wizard (egui / eframe) — Windows + macOS.
//
// Shown ONLY when the daemon is launched interactively (a user
// double-click) AND there is no cached token yet. The installed
// autostart entry (Task Scheduler on Windows, a launchd LaunchAgent on
// macOS) runs the exe with `--background`, which skips this and goes
// straight to the tray daemon. So this window is the pair-then-install
// onboarding for a fresh machine: prompt the shadows app URL, run the
// device flow, show the code, wait for approval, then offer "install +
// start the background task" which hands off to the tray daemon and
// exits.
//
// Threading: eframe owns the main thread (the egui event loop). The
// device flow (async) runs on a worker thread with its own current-
// thread tokio runtime, reporting progress back over an mpsc channel.
// The worker wakes the UI via a cloned egui Context (request_repaint).

use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use eframe::egui;
use url::Url;

use crate::oauth::{self, PollStep};

const WIN_W: f32 = 500.0;
const WIN_H: f32 = 460.0;

/// First-run vs. re-pair. First-run offers to install + start the
/// autostart entry after pairing; re-pair (machine deleted hub-side, the
/// daemon is already running) just writes a fresh token and tells the
/// user the daemon will reconnect on its own.
#[derive(Clone, Copy, PartialEq)]
pub enum WizardMode {
    FirstRun,
    Repair,
}

/// Open the setup window and block until the user finishes or closes it.
pub fn run_setup_wizard(default_shadows_url: String, mode: WizardMode) -> Result<()> {
    let title = match mode {
        WizardMode::FirstRun => "Relay setup",
        WizardMode::Repair   => "Relay — re-pair this machine",
    };
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([WIN_W, WIN_H])
        .with_resizable(false);
    // Explicitly set the window / Dock icon. Without this, eframe falls
    // back to its built-in default logo (a lowercase "e"), which it
    // pushes to the macOS Dock — overriding even the .app bundle icon.
    if let Some(icon) = relay_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        title,
        options,
        Box::new(move |_cc| {
            // Runs on the main thread after eframe has created its
            // NSApplication, so this overrides eframe's default Dock icon.
            #[cfg(target_os = "macos")]
            set_macos_dock_icon();
            Ok(Box::new(Wizard::new(default_shadows_url, mode)) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|e| anyhow!("gui failed: {e}"))
}

/// Force the macOS Dock icon to the Relay molecule. The Dock icon is
/// `NSApplication.applicationIconImage`; eframe sets it from its viewport
/// IconData (defaulting to its built-in "e" logo). Setting it directly,
/// on the main thread after eframe init, makes it unambiguous and
/// independent of how the process was launched (bundle vs. bare binary).
#[cfg(target_os = "macos")]
fn set_macos_dock_icon() {
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;
    let Some(mtm) = MainThreadMarker::new() else { return };
    let data = NSData::with_bytes(include_bytes!("../assets/app-icon.png"));
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        // setApplicationIconImage is unsafe in objc2 (it takes a raw
        // image ref); the image we built is valid for the call's duration.
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)); }
    }
}

/// Decode the app icon (the full-color Relay molecule) into an eframe
/// IconData for the window / Dock icon, overriding eframe's default "e"
/// logo. Returns None if decode fails — eframe then keeps its default.
fn relay_icon() -> Option<std::sync::Arc<egui::IconData>> {
    let img = image::load_from_memory(include_bytes!("../assets/app-icon.png"))
        .ok()?
        .into_rgba8();
    let (width, height) = img.dimensions();
    Some(std::sync::Arc::new(egui::IconData { rgba: img.into_raw(), width, height }))
}

enum Stage {
    EnterUrl,
    Pairing { user_code: String, pair_link: String },
    Paired,
    Failed(String),
}

/// Worker -> UI messages.
enum Msg {
    Prompt { user_code: String, pair_link: String },
    Paired,
    Failed(String),
}

/// Prereq-installer worker -> UI messages (macOS + Windows auto-install).
#[cfg(any(target_os = "windows", target_os = "macos"))]
enum InstallMsg {
    Progress(String),
    /// Finished; carries a fresh prereq check to repaint the readout.
    Done(Vec<crate::Prereq>),
    Failed(String),
}

struct Wizard {
    mode:           WizardMode,
    stage:          Stage,
    url_input:      String,
    rx:             Option<Receiver<Msg>>,
    busy:           bool,
    install_status: Option<String>,
    /// Runtime prereqs (claude + node/npm/npx), checked once on reaching
    /// the Paired stage. Empty until then.
    prereqs:        Vec<crate::Prereq>,
    /// Prereq auto-installer state (macOS + Windows).
    #[cfg(any(target_os = "windows", target_os = "macos"))] prereq_rx:         Option<Receiver<InstallMsg>>,
    #[cfg(any(target_os = "windows", target_os = "macos"))] prereq_installing: bool,
    #[cfg(any(target_os = "windows", target_os = "macos"))] prereq_status:     Option<String>,
}

impl Wizard {
    fn new(default_shadows_url: String, mode: WizardMode) -> Self {
        Self {
            mode, stage: Stage::EnterUrl, url_input: default_shadows_url,
            rx: None, busy: false, install_status: None, prereqs: Vec::new(),
            #[cfg(any(target_os = "windows", target_os = "macos"))] prereq_rx: None,
            #[cfg(any(target_os = "windows", target_os = "macos"))] prereq_installing: false,
            #[cfg(any(target_os = "windows", target_os = "macos"))] prereq_status: None,
        }
    }

    /// Kick off the prereq auto-installer on a worker thread (its own
    /// current-thread tokio runtime), streaming progress back over a
    /// channel. macOS only.
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn start_prereq_install(&mut self, ctx: &egui::Context) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.prereq_rx = Some(rx);
        self.prereq_installing = true;
        self.prereq_status = Some("Starting…".into());
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => { let _ = tx.send(InstallMsg::Failed(format!("{e}"))); ctx.request_repaint(); return; }
            };
            let txp = tx.clone();
            let res = rt.block_on(crate::prereq_install::install_missing(move |m| {
                let _ = txp.send(InstallMsg::Progress(m.to_string()));
            }));
            match res {
                Ok(())  => { let _ = tx.send(InstallMsg::Done(crate::check_prereqs())); }
                Err(e)  => { let _ = tx.send(InstallMsg::Failed(format!("{e:#}"))); }
            }
            ctx.request_repaint();
        });
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn drain_prereq(&mut self) {
        let msgs: Vec<InstallMsg> = match &self.prereq_rx {
            Some(rx) => rx.try_iter().collect(),
            None => return,
        };
        for m in msgs {
            match m {
                InstallMsg::Progress(s) => self.prereq_status = Some(s),
                InstallMsg::Done(p)     => { self.prereqs = p; self.prereq_installing = false; self.prereq_status = Some("Prerequisites installed.".into()); }
                InstallMsg::Failed(e)   => { self.prereq_installing = false; self.prereq_status = Some(format!("Install failed: {e}")); }
            }
        }
    }

    /// Two-line prereq readout (Claude Code + Node.js) shown after
    /// pairing, with an install hint for whatever's missing. node/npm/npx
    /// are collapsed into one "Node.js" row since they install together.
    fn render_prereqs(&self, ui: &mut egui::Ui) {
        let green = egui::Color32::from_rgb(60, 170, 90);
        let red   = egui::Color32::from_rgb(200, 90, 60);
        let row = |ui: &mut egui::Ui, ok: bool, label: &str| {
            ui.horizontal(|ui| {
                ui.colored_label(if ok { green } else { red }, if ok { "ready  " } else { "missing" });
                ui.label(label);
            });
        };
        let find = |name: &str| self.prereqs.iter().find(|p| p.name == name);
        let claude_ok = find("claude").map(|p| p.found()).unwrap_or(false);
        let node_ok   = self.prereqs.iter().filter(|p| p.name != "claude").all(|p| p.found());

        ui.label("To run sessions, this machine also needs:");
        ui.add_space(4.0);
        row(ui, claude_ok, "Claude Code CLI");
        if !claude_ok {
            if let Some(p) = find("claude") {
                ui.horizontal(|ui| { ui.add_space(20.0); ui.code(p.hint); });
            }
        }
        row(ui, node_ok, "Node.js  (node, npm, npx)");
        if !node_ok {
            if let Some(p) = find("node") {
                ui.horizontal(|ui| { ui.add_space(20.0); ui.code(p.hint); });
            }
        }
        if !claude_ok || !node_ok {
            ui.add_space(4.0);
            ui.label("Install the missing ones, then start a session — Relay finds them automatically.");
        }
    }

    /// The "Install prerequisites" button + progress, shown under the
    /// readout when something's missing (macOS + Windows auto-install).
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn render_prereq_actions(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.prereq_installing {
            let status = self.prereq_status.clone().unwrap_or_else(|| "Installing…".into());
            ui.add_space(8.0);
            ui.horizontal(|ui| { ui.spinner(); ui.label(status); });
            return;
        }
        if self.prereqs.iter().any(|p| !p.found()) {
            ui.add_space(8.0);
            if ui.button("Install prerequisites").clicked() {
                self.start_prereq_install(ctx);
            }
        }
        if let Some(s) = self.prereq_status.clone() {
            ui.add_space(4.0);
            ui.label(s);
        }
    }

    fn start_pairing(&mut self, ctx: &egui::Context) {
        let shadows_url = self.url_input.trim().trim_end_matches('/').to_string();
        if shadows_url.is_empty() {
            self.stage = Stage::Failed("Enter the shadows app URL.".into());
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.rx = Some(rx);
        self.busy = true;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Err(e) = pair_worker(&shadows_url, &tx) {
                let _ = tx.send(Msg::Failed(format!("{e:#}")));
            }
            ctx.request_repaint();
        });
    }

    fn drain(&mut self) {
        let drained: Vec<Msg> = match &self.rx {
            Some(rx) => rx.try_iter().collect(),
            None => return,
        };
        for m in drained {
            match m {
                Msg::Prompt { user_code, pair_link } => self.stage = Stage::Pairing { user_code, pair_link },
                Msg::Paired                          => { self.prereqs = crate::check_prereqs(); self.stage = Stage::Paired; self.busy = false; }
                Msg::Failed(e)                       => { self.stage = Stage::Failed(e); self.busy = false; }
            }
        }
    }
}

/// The device flow + config persist, run on the worker thread.
fn pair_worker(shadows_url: &str, tx: &Sender<Msg>) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("tokio runtime for pairing")?;
    rt.block_on(async move {
        let client = oauth::device_flow_client()?;
        let label = hostname::get().ok().and_then(|s| s.into_string().ok()).unwrap_or_else(|| "desktop".into());
        let prompt = oauth::request_device_code(&client, shadows_url, &label).await?;
        let _ = tx.send(Msg::Prompt {
            user_code: prompt.user_code.clone(),
            pair_link: prompt.verification_uri_complete.clone(),
        });

        let token_url = Url::parse(shadows_url)?.join("/device/token")?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(prompt.expires_in);
        let mut interval = Duration::from_secs(prompt.interval.max(1));
        loop {
            if tokio::time::Instant::now() > deadline {
                return Err(anyhow!("code expired before approval"));
            }
            tokio::time::sleep(interval).await;
            match oauth::poll_device_token(&client, &token_url, &prompt.device_code, &mut interval).await? {
                PollStep::Approved { token, hub_url } => {
                    let mut cfg = crate::load_or_init_config()?;
                    cfg.token       = Some(token);
                    cfg.hub_url     = Some(hub_url);
                    cfg.shadows_url = Some(shadows_url.to_string());
                    crate::save_config(&cfg)?;
                    let _ = tx.send(Msg::Paired);
                    return Ok(());
                }
                PollStep::KeepPolling => {}
            }
        }
    })
}

/// Install the autostart entry and start it immediately. The entry runs
/// the exe with `--background`, so it comes up as the tray daemon.
///
/// Windows: `install` creates the Task but doesn't run it until logon,
/// so `run_now` (schtasks /Run) kicks it. macOS: `install` bootstraps
/// the LaunchAgent into the GUI domain and RunAtLoad starts it during
/// install itself, so there's nothing extra to do.
fn install_and_run() -> Result<()> {
    let exe = std::env::current_exe().context("locating current exe")?;
    crate::autostart::current().install(&exe).context("installing the background task")?;
    #[cfg(target_os = "windows")]
    crate::autostart::run_now().context("starting the background task")?;
    Ok(())
}

impl eframe::App for Wizard {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        self.drain_prereq();
        // While a worker is running, poll the channel on a timer so the UI
        // advances even if the worker's repaint nudge is missed.
        let mut polling = self.busy;
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        { polling |= self.prereq_installing; }
        if polling {
            ctx.request_repaint_after(Duration::from_millis(400));
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("Relay");
            ui.add_space(10.0);

            match &self.stage {
                Stage::EnterUrl => {
                    match self.mode {
                        WizardMode::FirstRun => ui.label("Pair this machine with your shadows app to get started."),
                        WizardMode::Repair   => ui.label("This machine is no longer registered. Re-pair it to reconnect."),
                    };
                    ui.add_space(12.0);
                    ui.label("Shadows app URL:");
                    ui.text_edit_singleline(&mut self.url_input);
                    ui.add_space(14.0);
                    let label = match self.mode {
                        WizardMode::FirstRun => "Pair this machine",
                        WizardMode::Repair   => "Re-pair this machine",
                    };
                    if ui.button(label).clicked() {
                        self.start_pairing(ctx);
                    }
                }
                Stage::Pairing { user_code, pair_link } => {
                    ui.label("In the shadows app (signed in), open \"Pair a machine\" and enter:");
                    ui.add_space(10.0);
                    ui.heading(user_code);
                    ui.add_space(10.0);
                    ui.hyperlink_to("Open the pairing page (code pre-filled)", pair_link.clone());
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Waiting for approval...");
                    });
                }
                Stage::Paired => match self.mode {
                    // First run: offer to install + start the autostart entry,
                    // which brings the tray daemon up.
                    WizardMode::FirstRun => {
                        ui.label("Paired. This machine is now connected to your shadows hub.");
                        ui.add_space(10.0);
                        self.render_prereqs(ui);
                        #[cfg(any(target_os = "windows", target_os = "macos"))]
                        self.render_prereq_actions(ui, ctx);
                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(8.0);
                        ui.label("Install the background task so Relay starts at logon and runs in the tray:");
                        ui.add_space(10.0);
                        if ui.button("Install and start background task").clicked() {
                            match install_and_run() {
                                Ok(()) => {
                                    self.install_status = Some("Installed and started. Closing...".into());
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                                Err(e) => self.install_status = Some(format!("Install failed: {e:#}")),
                            }
                        }
                        if let Some(s) = &self.install_status {
                            ui.add_space(10.0);
                            ui.label(s);
                        }
                    }
                    // Re-pair: the daemon is already running; it reloads the
                    // fresh token on its next reconnect (≤60s). Nothing to
                    // install — just confirm and let the user close.
                    WizardMode::Repair => {
                        ui.label("Re-paired. Relay will reconnect with the new credentials shortly.");
                        ui.add_space(10.0);
                        self.render_prereqs(ui);
                        #[cfg(any(target_os = "windows", target_os = "macos"))]
                        self.render_prereq_actions(ui, ctx);
                        ui.add_space(12.0);
                        if ui.button("Done").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                },
                Stage::Failed(e) => {
                    ui.colored_label(egui::Color32::from_rgb(200, 60, 60), format!("Pairing failed: {e}"));
                    ui.add_space(12.0);
                    if ui.button("Try again").clicked() {
                        self.stage = Stage::EnterUrl;
                        self.rx = None;
                        self.busy = false;
                    }
                }
            }
        });
    }
}
