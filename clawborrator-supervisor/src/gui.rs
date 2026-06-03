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
const WIN_H: f32 = 380.0;

/// Open the setup window and block until the user finishes or closes it.
pub fn run_first_run_wizard(default_shadows_url: String) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([WIN_W, WIN_H])
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "Relay setup",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(Wizard::new(default_shadows_url)) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|e| anyhow!("gui failed: {e}"))
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

struct Wizard {
    stage:          Stage,
    url_input:      String,
    rx:             Option<Receiver<Msg>>,
    busy:           bool,
    install_status: Option<String>,
}

impl Wizard {
    fn new(default_shadows_url: String) -> Self {
        Self { stage: Stage::EnterUrl, url_input: default_shadows_url, rx: None, busy: false, install_status: None }
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
                Msg::Paired                          => { self.stage = Stage::Paired; self.busy = false; }
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
        // While pairing, poll the channel on a timer so the UI advances
        // even if the worker's repaint nudge is missed.
        if self.busy {
            ctx.request_repaint_after(Duration::from_millis(400));
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("Relay");
            ui.add_space(10.0);

            match &self.stage {
                Stage::EnterUrl => {
                    ui.label("Pair this machine with your shadows app to get started.");
                    ui.add_space(12.0);
                    ui.label("Shadows app URL:");
                    ui.text_edit_singleline(&mut self.url_input);
                    ui.add_space(14.0);
                    if ui.button("Pair this machine").clicked() {
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
                Stage::Paired => {
                    ui.label("Paired. This machine is now connected to your shadows hub.");
                    ui.add_space(12.0);
                    ui.label("Install the background task so shadows-desktop starts at logon and runs in the tray:");
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
