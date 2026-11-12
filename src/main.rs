//! vban-emitter-gui: a mixer-style window for setting up and running the Linux vban_emitter, sending any number of
//! VBAN streams, and installing them as a user service. `vban-emitter-gui --run [--config FILE]` is the headless mode
//! the service (and the window's Start) uses.

mod config;
mod emitter;
mod meter;
mod ping;
mod service;
mod tray;

use config::{Config, Stream, BACKENDS, DEFAULT_MONITOR, FORMATS, RATES};
use eframe::egui::{self, Color32, RichText, Stroke};
use emitter::{Owner, Running, Source};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BG: Color32 = Color32::from_rgb(0x17, 0x1b, 0x22);
const PANEL: Color32 = Color32::from_rgb(0x21, 0x27, 0x31);
const STRIP: Color32 = Color32::from_rgb(0x2a, 0x32, 0x3f);
const ROW: Color32 = Color32::from_rgb(0x22, 0x29, 0x34);
const ROW_SELECTED: Color32 = Color32::from_rgb(0x2c, 0x3a, 0x4b);
const FIELD: Color32 = Color32::from_rgb(0x12, 0x16, 0x1c);
const EDGE: Color32 = Color32::from_rgb(0x3a, 0x45, 0x57);
const CYAN: Color32 = Color32::from_rgb(0x3f, 0xc1, 0xd9);
const AMBER: Color32 = Color32::from_rgb(0xe0, 0xa4, 0x3a);
const GREEN: Color32 = Color32::from_rgb(0x4c, 0xd9, 0x64);
const RED: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4d);
const TEXT: Color32 = Color32::from_rgb(0xd7, 0xdd, 0xe6);
const DIM: Color32 = Color32::from_rgb(0x8a, 0x94, 0xa6);
const OFF: Color32 = Color32::from_gray(70);

const LOG_LINES: usize = 500;

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let config_path = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(config::default_path);
    if args.iter().any(|a| a == "--run") {
        std::process::exit(emitter::run_service(&config_path));
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "vban-emitter-gui [--tray] [--config FILE]  open the window (--tray: start hidden in the tray)\n\
             vban-emitter-gui --run [--config FILE]     send every stream that's on (what the service does)"
        );
        return Ok(());
    }
    let in_tray = args.iter().any(|a| a == "--tray");
    let Some(instance) = tray::claim_single_instance(in_tray) else { return Ok(()) };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(tray::TITLE)
            .with_app_id("vban-emitter-gui")
            .with_visible(!in_tray)
            .with_icon(Arc::new(egui::IconData { rgba: tray::window_icon_rgba(64), width: 64, height: 64 }))
            .with_inner_size([1000.0, 800.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        tray::TITLE,
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || tray::show_on_request(instance, ctx));
            Ok(Box::new(App::new(cc, config_path, in_tray)))
        }),
    );
    tray::release_single_instance();
    result
}

type Log = Arc<Mutex<VecDeque<String>>>;

fn push(log: &Log, line: String) {
    let mut l = log.lock().unwrap();
    l.push_back(line);
    while l.len() > LOG_LINES {
        l.pop_front();
    }
}

/// What a background thread keeps up to date.
#[derive(Default)]
struct Watched {
    service: service::Status,
    emitters: Vec<Running>,
    service_log: Vec<String>,
    old_units: Vec<service::OldUnit>,
    busy: bool,
}

/// The runner (`--run`) the window's Start began.
struct LocalRun {
    child: Child,
    stop_asked: Option<Instant>,
}

#[derive(PartialEq, Clone, Copy)]
enum LogTab {
    Window,
    Service,
}

struct App {
    path: PathBuf,
    cfg: Config,
    saved: Config,
    /// The stream the sound device list is showing.
    selected: usize,
    /// The stream whose extra settings (⚙) are open.
    gear: Option<usize>,
    sources: Vec<Source>,
    sources_error: Option<String>,
    run: Option<LocalRun>,
    here_pid: Arc<Mutex<Option<u32>>>,
    log: Log,
    watched: Arc<Mutex<Watched>>,
    tab: LogTab,
    message: Option<(String, Color32)>,
    /// Whether the chosen vban_emitter has -x (buffer size); checked at start and when the Program field is left.
    bufsize_ok: bool,
    /// Live level meters on every source, and the test tone.
    meters: meter::Meters,
    sinks: Vec<(String, String)>,
    default_sink: String,
    tone_sink: String,
    tone_playing: Arc<Mutex<bool>>,
    /// The tray icon is up, so closing the window hides it there rather than exiting.
    tray_mode: bool,
    /// Set by the tray's Exit.
    exit_requested: Arc<AtomicBool>,
    quitting: bool,
    /// Hidden in the tray: the meters are paused.
    hidden: bool,
    /// Who answers VBAN pings at each stream's destination.
    pings: Arc<Mutex<ping::Shared>>,
}

impl App {
    fn new(cc: &eframe::CreationContext, path: PathBuf, in_tray: bool) -> App {
        style(&cc.egui_ctx);
        let cfg = Config::load(&path);
        let here_pid = Arc::new(Mutex::new(None));
        let watched = Arc::new(Mutex::new(Watched::default()));
        {
            let (w, ctx, here) = (watched.clone(), cc.egui_ctx.clone(), here_pid.clone());
            std::thread::spawn(move || loop {
                let status = service::status();
                let emitters = emitter::running(*here.lock().unwrap());
                let tail = service::log_tail(LOG_LINES);
                let old = service::old_units();
                {
                    let mut w = w.lock().unwrap();
                    w.service = status;
                    w.emitters = emitters;
                    w.service_log = tail;
                    w.old_units = old;
                }
                ctx.request_repaint();
                std::thread::sleep(Duration::from_millis(1500));
            });
        }
        let log: Log = Default::default();
        push(&log, format!("[gui] settings file: {}", path.display()));
        let mut app = App {
            path,
            saved: cfg.clone(),
            cfg,
            selected: 0,
            gear: None,
            sources: Vec::new(),
            sources_error: None,
            run: None,
            here_pid,
            log,
            watched,
            tab: LogTab::Window,
            message: None,
            bufsize_ok: false,
            meters: Default::default(),
            sinks: Vec::new(),
            default_sink: String::new(),
            tone_sink: String::new(),
            tone_playing: Default::default(),
            tray_mode: false,
            exit_requested: Default::default(),
            quitting: false,
            hidden: in_tray,
            pings: Default::default(),
        };
        ping::start(app.pings.clone());
        app.bufsize_ok = emitter::supports_bufsize(&app.cfg.program);
        match tray::start(&cc.egui_ctx, app.exit_requested.clone()) {
            Ok(()) => app.tray_mode = true,
            Err(e) => {
                push(&app.log, format!("[gui] no tray icon ({e}), so closing the window will exit"));
                if in_tray {
                    app.hidden = false;
                    tray::show_window(&cc.egui_ctx);
                }
            }
        }
        if !app.hidden {
            app.refresh_devices(&cc.egui_ctx);
        }
        app
    }

    /// Closing the window: hide to the tray (meters paused), or exit when there's no tray or Exit was asked for.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if self.exit_requested.swap(false, Ordering::SeqCst) {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if self.tray_mode && !self.quitting && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            tray::hide_window(ctx);
            self.meters.stop();
            self.hidden = true;
        }
        if tray::take_shown() && self.hidden {
            self.hidden = false;
            self.refresh_devices(ctx);
        }
    }

    /// Lists the sources and outputs again and restarts the meters on them.
    fn refresh_devices(&mut self, ctx: &egui::Context) {
        match emitter::list_sources() {
            Ok(s) => {
                self.sources = s;
                self.sources_error = None;
            }
            Err(e) => self.sources_error = Some(e),
        }
        self.sinks = emitter::list_sinks().unwrap_or_default();
        self.default_sink = emitter::default_monitor().map(|m| m.trim_end_matches(".monitor").to_string()).unwrap_or_default();
        let names: Vec<String> = self.sources.iter().map(|s| s.name.clone()).collect();
        self.meters.start(&names, ctx.clone());
    }

    fn save(&mut self) -> bool {
        match self.cfg.save(&self.path) {
            Ok(()) => {
                self.saved = self.cfg.clone();
                true
            }
            Err(e) => {
                self.message = Some((format!("Can't save {}: {e}", self.path.display()), RED));
                false
            }
        }
    }

    fn running_here(&self) -> bool {
        self.run.is_some()
    }

    /// Sends every stream that's on from the window, by running this program's `--run` mode.
    fn start(&mut self, ctx: &egui::Context) {
        let problems = self.cfg.problems();
        if !problems.is_empty() {
            self.message = Some((problems.join("   "), RED));
            return;
        }
        if !self.save() {
            return;
        }
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("vban-emitter-gui"));
        let mut cmd = Command::new(exe);
        cmd.args(["--run", "--config"]).arg(&self.path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match emitter::dies_with_us(&mut cmd).spawn() {
            Ok(c) => c,
            Err(e) => {
                self.message = Some((format!("Can't start: {e}"), RED));
                return;
            }
        };
        *self.here_pid.lock().unwrap() = Some(child.id());
        for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
            child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>)]
        .into_iter()
        .flatten()
        {
            let (log, ctx) = (self.log.clone(), ctx.clone());
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        push(&log, line);
                        ctx.request_repaint();
                    }
                }
            });
        }
        self.run = Some(LocalRun { child, stop_asked: None });
        self.message = None;
        self.tab = LogTab::Window;
    }

    fn stop(&mut self) {
        if let Some(run) = &mut self.run {
            if run.stop_asked.is_none() {
                unsafe { libc::kill(run.child.id() as i32, libc::SIGTERM) };
                run.stop_asked = Some(Instant::now());
                push(&self.log, "[gui] stopping".into());
            }
        }
    }

    /// Notices the runner exiting, and kills it if it ignores a stop for 2 s.
    fn check_run(&mut self) {
        let Some(run) = &mut self.run else { return };
        match run.child.try_wait() {
            Ok(Some(status)) => {
                push(&self.log, format!("[gui] stopped ({status})"));
                if run.stop_asked.is_none() {
                    self.message = Some(("Sending stopped on its own; see the log.".into(), AMBER));
                    self.tab = LogTab::Window;
                }
                self.run = None;
                *self.here_pid.lock().unwrap() = None;
            }
            Ok(None) => {
                if run.stop_asked.map_or(false, |t| t.elapsed() > Duration::from_secs(2)) {
                    let _ = run.child.kill();
                }
            }
            Err(e) => push(&self.log, format!("[gui] {e}")),
        }
    }

    fn service_action(&mut self, ctx: &egui::Context, what: &str) {
        let what = what.to_string();
        if what == "install" || what == "restart" || what == "start" {
            let problems = self.cfg.problems();
            if !problems.is_empty() {
                self.message = Some((problems.join("   "), RED));
                return;
            }
            if !self.save() {
                return;
            }
            if self.running_here() {
                self.stop();
            }
        }
        let (log, ctx, watched, path) = (self.log.clone(), ctx.clone(), self.watched.clone(), self.path.clone());
        let here = self.here_pid.clone();
        watched.lock().unwrap().busy = true;
        self.tab = LogTab::Window;
        std::thread::spawn(move || {
            let lines = match what.as_str() {
                "install" => service::install(&path),
                "remove" => service::remove(),
                old if old.starts_with("retire:") => service::retire(&old["retire:".len()..]),
                verb => service::control(verb),
            };
            for l in lines {
                push(&log, format!("[service] {l}"));
            }
            // Re-read everything now, and once more after the emitters have had a moment to start or stop.
            for pause in [0u64, 700] {
                std::thread::sleep(Duration::from_millis(pause));
                let (status, emitters, old) = (service::status(), emitter::running(*here.lock().unwrap()), service::old_units());
                let mut w = watched.lock().unwrap();
                w.service = status;
                w.emitters = emitters;
                w.old_units = old;
                w.busy = false;
                ctx.request_repaint();
            }
        });
    }

    /// Fills the selected stream (and the backend) from a running emitter's command line.
    fn import(&mut self, r: &Running) {
        let i = self.selected.min(self.cfg.streams.len() - 1);
        let (mut s, backend) = self.cfg.streams[i].from_args(&r.args);
        if backend != "pulseaudio" {
            if let Some(src) = emitter::current_source_of(r.pid) {
                s.source = src;
            }
        }
        if BACKENDS.contains(&backend.as_str()) {
            self.cfg.backend = backend;
        }
        let name = s.label();
        self.cfg.streams[i] = s;
        self.message = Some((format!("Copied emitter {} into stream {name}. Save, Start or Install to use it.", r.pid), CYAN));
    }

    fn short_source(&self, source: &str) -> String {
        let full = source_label(source, &self.sources, &self.cfg.backend);
        let full = full.strip_prefix("Monitor of ").map(|s| format!("{s} (output)")).unwrap_or(full);
        if full.chars().count() > 34 { format!("{}…", full.chars().take(33).collect::<String>()) } else { full }
    }

    /// The meter level for a source setting (the default output's monitor for DEFAULT_MONITOR).
    fn level_of(&self, source: &str, levels: &std::collections::HashMap<String, [f32; 2]>) -> Option<[f32; 2]> {
        let name = if source == DEFAULT_MONITOR { format!("{}.monitor", self.default_sink) } else { source.to_string() };
        if source.is_empty() { None } else { Some(levels.get(&name).copied().unwrap_or_default()) }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_close(ctx);
        self.check_run();
        if self.running_here() {
            ctx.request_repaint_after(Duration::from_millis(400));
        }
        self.selected = self.selected.min(self.cfg.streams.len().saturating_sub(1));
        {
            let mut targets: Vec<(std::net::Ipv4Addr, u16)> =
                self.cfg.streams.iter().filter(|s| s.on).filter_map(|s| Some((s.ip.trim().parse().ok()?, s.port))).collect();
            targets.sort();
            targets.dedup();
            self.pings.lock().unwrap().targets = targets;
        }
        let (svc, emitters, busy, old_units) = {
            let w = self.watched.lock().unwrap();
            (w.service.clone(), w.emitters.clone(), w.busy, w.old_units.clone())
        };

        egui::TopBottomPanel::top("head").frame(egui::Frame::new().fill(PANEL).inner_margin(10)).show(ctx, |ui| {
            self.header(ui, ctx, &svc, &emitters);
        });

        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .default_height(130.0)
            .min_height(80.0)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(8))
            .show(ctx, |ui| self.log_view(ui));

        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(10)).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some((m, c)) = self.message.clone() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(m).color(c));
                        if ui.small_button("✕").clicked() {
                            self.message = None;
                        }
                    });
                    ui.add_space(4.0);
                }
                strip(ui, "INPUT  ·  CAPTURE", |ui| self.input_strip(ui));
                ui.add_space(8.0);
                strip(ui, "VBAN OUTGOING STREAMS", |ui| self.streams_strip(ui, &emitters));
                if matches!(self.cfg.backend.as_str(), "pulseaudio" | "alsa") {
                    ui.add_space(8.0);
                    let title = format!("SOUND DEVICES  ·  CLICK ONE TO SEND IT ON {}", self.cfg.streams[self.selected].label().to_uppercase());
                    strip(ui, &title, |ui| self.devices_strip(ui, &emitters));
                }
                ui.add_space(8.0);
                strip(ui, "USER SERVICE  ·  START AT LOGIN", |ui| self.service_strip(ui, ctx, &svc, busy));
                ui.add_space(8.0);
                strip(ui, "EMITTERS ON THIS COMPUTER", |ui| self.emitters_strip(ui, ctx, &emitters, &old_units, busy));
            });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.meters.stop();
        if let Some(run) = &mut self.run {
            unsafe { libc::kill(run.child.id() as i32, libc::SIGTERM) };
            for _ in 0..20 {
                if matches!(run.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = run.child.kill();
        }
    }
}

impl App {
    fn header(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, svc: &service::Status, emitters: &[Running]) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("VBAN").size(26.0).strong().color(CYAN));
            ui.label(RichText::new("EMITTER").size(26.0).color(TEXT));
            ui.label(RichText::new("GUI").size(14.0).color(DIM));
            ui.add_space(14.0);
            let ours = |o: Owner| emitters.iter().filter(|r| r.owner == o).count();
            let plural = |n: usize| if n == 1 { "1 stream".to_string() } else { format!("{n} streams") };
            let (on, what) = if self.running_here() {
                (true, format!("ON AIR · {} · this window", plural(ours(Owner::Here))))
            } else if svc.running() {
                (true, format!("ON AIR · {} · service", plural(ours(Owner::Service))))
            } else {
                (false, "STOPPED".to_string())
            };
            led(ui, if on { GREEN } else { OFF }, 9.0);
            ui.label(RichText::new(what).size(16.0).strong().color(if on { GREEN } else { DIM }));
            let busy = self.watched.lock().unwrap().busy;
            if busy {
                ui.spinner();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let size = egui::vec2(120.0, 36.0);
                // The big button drives whatever is in charge: the window's own run, else the service when it's
                // installed, else a run from the window.
                let stopping = self.run.as_ref().map_or(false, |r| r.stop_asked.is_some());
                if self.running_here() || svc.running() {
                    let b = egui::Button::new(RichText::new(if stopping { "STOPPING…" } else { "■  STOP" }).size(16.0).strong().color(Color32::WHITE))
                        .fill(RED)
                        .min_size(size);
                    let tip = if self.running_here() { "Stop sending from this window" } else { "Stop the service (it still starts at the next login if enabled)" };
                    if ui.add_enabled(!stopping && !busy, b).on_hover_text(tip).clicked() {
                        if self.running_here() {
                            self.stop();
                        } else {
                            self.service_action(ctx, "stop");
                        }
                    }
                } else {
                    let b = egui::Button::new(RichText::new("▶  START").size(16.0).strong().color(Color32::BLACK)).fill(GREEN).min_size(size);
                    let tip = if svc.installed {
                        "Save the settings and start the service (sends every stream that's on)"
                    } else {
                        "Send every stream that's on, from this window (until you stop it or close the window)"
                    };
                    if ui.add_enabled(!busy, b).on_hover_text(tip).clicked() {
                        if svc.installed {
                            self.service_action(ctx, "restart");
                        } else {
                            self.start(ctx);
                        }
                    }
                }
                ui.add_space(8.0);
                let exit_tip = if self.tray_mode {
                    "Close the app. Closing the window only hides it to the tray; the service keeps sending either way"
                } else {
                    "Close the app. The service keeps sending"
                };
                if ui.button("Exit").on_hover_text(exit_tip).clicked() {
                    self.exit_requested.store(true, Ordering::SeqCst);
                    ctx.request_repaint();
                }
                let dirty = self.cfg != self.saved;
                if ui.add_enabled(dirty, egui::Button::new("Save settings")).clicked() && self.save() {
                    self.message = Some((format!("Saved to {}", self.path.display()), CYAN));
                }
                if dirty {
                    ui.label(RichText::new("● unsaved").color(AMBER));
                }
            });
        });
    }

    fn input_strip(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(caption("Backend"));
            egui::ComboBox::from_id_salt("backend").width(130.0).selected_text(&self.cfg.backend).show_ui(ui, |ui| {
                for b in BACKENDS {
                    ui.selectable_value(&mut self.cfg.backend, b.to_string(), *b);
                }
            });
            ui.add_space(16.0);
            ui.label(caption("Program"));
            let r = ui
                .add(egui::TextEdit::singleline(&mut self.cfg.program).desired_width(260.0))
                .on_hover_text("The vban_emitter program to run");
            if r.lost_focus() {
                self.bufsize_ok = emitter::supports_bufsize(&self.cfg.program);
            }
            if self.cfg.program != config::BUNDLED && std::path::Path::new(config::BUNDLED).exists() {
                if ui
                    .small_button("Use the included one")
                    .on_hover_text(format!("Switch to {}, the vban_emitter (v2.2.0) that comes with this app", config::BUNDLED))
                    .clicked()
                {
                    self.cfg.program = config::BUNDLED.into();
                    self.bufsize_ok = emitter::supports_bufsize(&self.cfg.program);
                }
            }
            ui.add_space(16.0);
            let note = match self.cfg.backend.as_str() {
                "pulseaudio" => "Records each stream's sound device directly. Right for almost every setup.",
                "alsa" => "Records through ALSA, then moves each stream to its sound device with pactl.",
                "pipe" => "Reads raw samples from a pipe (set the path with ⚙ on each stream).",
                _ => "Reads raw samples from a file (set the path with ⚙ on each stream).",
            };
            ui.label(RichText::new(note).small().color(DIM));
        });
        if let Some(e) = &self.sources_error {
            ui.label(RichText::new(e).small().color(AMBER));
        }
    }

    fn streams_strip(&mut self, ui: &mut egui::Ui, emitters: &[Running]) {
        let levels = self.meters.levels.lock().unwrap().clone();
        let count = self.cfg.streams.len();
        let (mut select, mut remove) = (None, None);
        for i in 0..count {
            let selected = i == self.selected;
            let live = emitters.iter().find(|r| {
                r.owner != Owner::Other && r.stream_name() == Some(self.cfg.streams[i].name.trim())
            });
            let src_label = self.short_source(&self.cfg.streams[i].source);
            let level = self.level_of(&self.cfg.streams[i].source, &levels);
            let backend = self.cfg.backend.clone();
            let show_source = matches!(backend.as_str(), "pulseaudio" | "alsa");
            let gear_open = self.gear == Some(i);
            let bufsize_ok = self.bufsize_ok;
            let (answer, port_busy) = {
                let p = self.pings.lock().unwrap();
                let ip: Option<std::net::Ipv4Addr> = self.cfg.streams[i].ip.trim().parse().ok();
                let answer = ip.and_then(|ip| p.answers.get(&ip).cloned()).filter(|a| a.at.elapsed() < Duration::from_secs(8));
                (answer, p.busy_ports.contains(&self.cfg.streams[i].port))
            };
            let s = &mut self.cfg.streams[i];
            let frame = egui::Frame::new()
                .fill(if selected { ROW_SELECTED } else { ROW })
                .stroke(Stroke::new(if selected { 1.5_f32 } else { 1.0_f32 }, if selected { CYAN } else { EDGE }))
                .corner_radius(5)
                .inner_margin(egui::Margin::symmetric(8, 5));
            let mut toggle_gear = false;
            let resp = frame
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let (c, tip) = match (live, &answer) {
                            (Some(r), Some(a)) => (GREEN, format!("Sending (vban_emitter {}), and {} answers at {}", r.pid, a.who, s.ip.trim())),
                            (Some(r), None) if port_busy => (
                                AMBER,
                                format!("Sending (vban_emitter {}), but the receiver can't be checked: port {} is in use on this computer", r.pid, s.port),
                            ),
                            (Some(r), None) => (
                                AMBER,
                                format!("Sending (vban_emitter {}), but nothing answers at {}: receiver off, blocked, or not a VB-Audio app", r.pid, s.ip.trim()),
                            ),
                            (None, _) if s.on => (OFF, "On, but not being sent right now (press Start)".to_string()),
                            (None, _) => (Color32::from_gray(45), "Off".to_string()),
                        };
                        led(ui, c, 6.0).on_hover_text(tip);
                        ui.checkbox(&mut s.on, "").on_hover_text("Send this stream (with Start, or by the service)");
                        ui.add(egui::TextEdit::singleline(&mut s.name).char_limit(16).desired_width(84.0).text_color(AMBER))
                            .on_hover_text("Stream name: must match the receiver's");
                        ui.label(RichText::new("to").color(DIM));
                        ui.add(egui::TextEdit::singleline(&mut s.ip).hint_text("IP address").desired_width(100.0).text_color(CYAN));
                        ui.label(RichText::new(":").color(DIM));
                        ui.add(egui::DragValue::new(&mut s.port).range(1..=65535).speed(1.0));
                        egui::ComboBox::from_id_salt(("rate", i)).width(72.0).selected_text(rate_label(s.rate)).show_ui(ui, |ui| {
                            for r in RATES {
                                ui.selectable_value(&mut s.rate, *r, rate_label(*r));
                            }
                        });
                        ui.add(egui::DragValue::new(&mut s.channels).range(1..=256).speed(0.1).suffix(" ch"));
                        egui::ComboBox::from_id_salt(("format", i)).width(72.0).selected_text(format_label(&s.format)).show_ui(ui, |ui| {
                            for f in FORMATS {
                                ui.selectable_value(&mut s.format, f.to_string(), format_label(f));
                            }
                        });
                        if show_source {
                            ui.label(RichText::new("from").color(DIM));
                            let b = ui
                                .add(egui::Button::new(RichText::new(&src_label).small()).min_size(egui::vec2(150.0, 0.0)))
                                .on_hover_text("Pick this stream's sound device in the list below");
                            if b.clicked() {
                                select = Some(i);
                            }
                            if let Some(l) = level {
                                level_meter(ui, l, 56.0);
                            }
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add_enabled(count > 1, egui::Button::new(RichText::new("−").strong())).on_hover_text("Remove this stream").clicked() {
                                remove = Some(i);
                            }
                            if ui.selectable_label(gear_open, "⚙").on_hover_text("More settings").clicked() {
                                toggle_gear = true;
                            }
                        });
                    });
                    if gear_open {
                        ui.horizontal(|ui| {
                            ui.add_space(30.0);
                            ui.label(caption("Channel map"));
                            ui.add(egui::TextEdit::singleline(&mut s.channel_map).hint_text("all (e.g. 1,2)").desired_width(90.0))
                                .on_hover_text("Which device channels to send, such as 1,2. Empty sends them all.");
                            if bufsize_ok || s.buffer_size > 0 {
                                ui.label(caption("Buffer"));
                                ui.add(
                                    egui::DragValue::new(&mut s.buffer_size)
                                        .range(0..=65536)
                                        .speed(16.0)
                                        .custom_formatter(|v, _| if v < 1.0 { "default".into() } else { format!("{v:.0}") }),
                                )
                                .on_hover_text("-x: audio buffer size. Bigger is steadier, smaller has less delay. 0 = the emitter's default (1024).");
                                if !bufsize_ok {
                                    ui.label(RichText::new("needs vban 2.2.0").small().color(AMBER));
                                }
                            }
                            ui.label(caption("Log"));
                            egui::ComboBox::from_id_salt(("loglevel", i)).width(90.0).selected_text(level_label(s.log_level)).show_ui(ui, |ui| {
                                for l in 0..=4u8 {
                                    ui.selectable_value(&mut s.log_level, l, level_label(l));
                                }
                            });
                            if backend != "pulseaudio" {
                                let (name, hint) = match backend.as_str() {
                                    "alsa" => ("ALSA device", "default"),
                                    "pipe" => ("Pipe", "/path/to/fifo"),
                                    _ => ("File", "/path/to/file"),
                                };
                                ui.label(caption(name));
                                ui.add(egui::TextEdit::singleline(&mut s.device).hint_text(hint).desired_width(140.0));
                            }
                        });
                    }
                    if s.on {
                        for p in s.problems(&backend) {
                            ui.label(RichText::new(format!("⚠ {p}")).small().color(AMBER));
                        }
                    }
                })
                .response;
            if toggle_gear {
                self.gear = if gear_open { None } else { Some(i) };
            }
            // A click anywhere in a row selects it.
            if ui.input(|inp| inp.pointer.primary_pressed()) && ui.rect_contains_pointer(resp.rect) {
                select = Some(i);
            }
            ui.add_space(3.0);
        }
        ui.horizontal(|ui| {
            if ui.button(RichText::new("+  Add stream").strong()).on_hover_text("Another stream, starting from the selected one's settings").clicked() {
                let base = self.cfg.streams[self.selected].clone();
                let name = self.cfg.free_name();
                self.cfg.streams.push(Stream { name, on: true, ..base });
                select = Some(self.cfg.streams.len() - 1);
            }
            let on = self.cfg.streams.iter().filter(|s| s.on).count();
            ui.label(RichText::new(format!("{on} of {} on", self.cfg.streams.len())).small().color(DIM));
            if self.running_here() || self.watched.lock().unwrap().service.running() {
                if self.cfg != self.saved {
                    ui.label(RichText::new("· changes apply when you Stop and Start, or Restart the service").small().color(AMBER));
                }
            }
        });
        if let Some(i) = remove {
            self.cfg.streams.remove(i);
            self.gear = None;
            if self.selected >= i && self.selected > 0 {
                self.selected -= 1;
            }
        } else if let Some(i) = select {
            self.selected = i;
        }
    }

    fn devices_strip(&mut self, ui: &mut egui::Ui, emitters: &[Running]) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("🔊 = what an output is playing (pick one to send the desktop's sound)   🎤 = a microphone or line-in")
                    .small()
                    .color(DIM),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("⟳ Refresh").on_hover_text("List the sound devices again").clicked() {
                    self.refresh_devices(ui.ctx());
                }
            });
        });
        if let Some(e) = self.meters.error.lock().unwrap().clone() {
            ui.label(RichText::new(e).color(AMBER));
        }
        ui.add_space(4.0);
        let levels = self.meters.levels.lock().unwrap().clone();
        let default_monitor = format!("{}.monitor", self.default_sink);
        let sel = self.selected;
        let current = self.cfg.streams[sel].source.clone();
        let sel_name = self.cfg.streams[sel].label();
        // Other streams that use each source setting.
        let users = |value: &str| -> String {
            let names: Vec<String> = self
                .cfg
                .streams
                .iter()
                .enumerate()
                .filter(|(i, s)| *i != sel && s.source == value)
                .map(|(_, s)| s.label())
                .collect();
            if names.is_empty() { String::new() } else { format!("also: {}", names.join(", ")) }
        };
        let mut pick = None;
        let row = |ui: &mut egui::Ui, value: &str, title: String, sub: String, level: Option<[f32; 2]>, pick: &mut Option<String>| {
            let chosen = current == value;
            ui.vertical(|ui| {
                ui.set_min_width(330.0);
                let title = if chosen { RichText::new(title).strong().color(CYAN) } else { RichText::new(title).strong() };
                if ui.selectable_label(chosen, title).on_hover_text("Send this").clicked() {
                    *pick = Some(value.to_string());
                }
                if !sub.is_empty() {
                    ui.label(RichText::new(sub).small().monospace().color(DIM));
                }
            });
            match level {
                Some(l) => {
                    level_meter(ui, l, 220.0);
                    ui.label(RichText::new(db_text(l[0].max(l[1]))).monospace().small().color(DIM));
                }
                None => {
                    ui.label("");
                    ui.label("");
                }
            }
            ui.horizontal(|ui| {
                if chosen {
                    ui.label(RichText::new(format!("✔ {sel_name}")).color(CYAN));
                } else if ui.button("Use").on_hover_text(format!("Send this on {sel_name}")).clicked() {
                    *pick = Some(value.to_string());
                }
                let u = users(value);
                if !u.is_empty() {
                    ui.label(RichText::new(u).small().color(DIM));
                }
            });
            ui.end_row();
        };
        egui::Grid::new("meters").num_columns(4).spacing([12.0, 6.0]).striped(true).show(ui, |ui| {
            let followed = self.sources.iter().find(|s| s.name == default_monitor).map(|s| s.description.clone());
            row(
                ui,
                DEFAULT_MONITOR,
                "🔊 Whatever the default output plays".into(),
                followed.map_or_else(String::new, |d| format!("now: {d}")),
                Some(levels.get(&default_monitor).copied().unwrap_or_default()),
                &mut pick,
            );
            for s in &self.sources {
                let mut sub = s.name.clone();
                if s.name == default_monitor {
                    sub.push_str("  · default output");
                }
                let title = format!("{} {}", if s.monitor { "🔊" } else { "🎤" }, s.description);
                row(ui, &s.name, title, sub, Some(levels.get(&s.name).copied().unwrap_or_default()), &mut pick);
            }
            let other = if self.cfg.backend == "pulseaudio" { "🎤 The default input" } else { "Wherever ALSA records from (no move)" };
            row(ui, "", other.into(), String::new(), None, &mut pick);
        });
        if let Some(p) = pick {
            self.cfg.streams[sel].source = p.clone();
            // If this stream is being sent right now, switch the running emitter over too (not saved).
            let name = self.cfg.streams[sel].name.trim().to_string();
            if let Some(r) = emitters.iter().find(|r| r.owner != Owner::Other && r.stream_name() == Some(name.as_str())) {
                let (pid, log, ctx) = (r.pid, self.log.clone(), ui.ctx().clone());
                push(&log, format!("[gui] {name}: switching the running stream over (not saved yet)"));
                std::thread::spawn(move || {
                    emitter::move_stream(pid, &p, &|m| push(&log, m));
                    ctx.request_repaint();
                });
            }
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(caption("Test tone on"));
            let shown = self
                .sinks
                .iter()
                .find(|s| s.0 == self.tone_sink)
                .map_or_else(|| "the default output".to_string(), |s| s.1.clone());
            egui::ComboBox::from_id_salt("tonesink").width(260.0).selected_text(shown).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.tone_sink, String::new(), "the default output");
                for (name, desc) in &self.sinks {
                    ui.selectable_value(&mut self.tone_sink, name.clone(), desc).on_hover_text(name);
                }
            });
            let playing = *self.tone_playing.lock().unwrap();
            if ui.add_enabled(!playing, egui::Button::new(if playing { "♪ playing…" } else { "▶ Play tone" })).clicked() {
                *self.tone_playing.lock().unwrap() = true;
                let (flag, log, ctx) = (self.tone_playing.clone(), self.log.clone(), ui.ctx().clone());
                meter::play_tone(&self.tone_sink, move |r| {
                    if let Err(e) = r {
                        push(&log, format!("[gui] test tone: {e}"));
                    }
                    *flag.lock().unwrap() = false;
                    ctx.request_repaint();
                });
            }
            ui.label(RichText::new("three beeps: the meter that moves is that output").small().color(DIM));
        });
    }

    fn service_strip(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, svc: &service::Status, busy: bool) {
        ui.horizontal(|ui| {
            let (color, text) = if !svc.installed {
                (OFF, "not installed".to_string())
            } else if svc.running() {
                (GREEN, format!("running · {}", svc.active))
            } else if svc.active == "failed" {
                (RED, "failed".to_string())
            } else {
                (Color32::from_gray(90), svc.active.clone())
            };
            led(ui, color, 7.0);
            ui.label(RichText::new(text).strong().color(TEXT));
            if svc.installed {
                ui.label(RichText::new(if svc.enabled == "enabled" { "· starts at login" } else { "· doesn't start at login" }).color(DIM));
            }
            ui.label(RichText::new(format!("({}, sends every stream that's on)", service::UNIT)).small().color(DIM));
            if busy {
                ui.spinner();
            }
        });
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!busy, |ui| {
                let label = if svc.installed { "Reinstall & restart" } else { "Install & start" };
                if ui.button(label).on_hover_text("Save the settings, write the service, start it now and at every login").clicked() {
                    self.service_action(ctx, "install");
                }
                ui.add_enabled_ui(svc.installed, |ui| {
                    if svc.running() {
                        if ui.button("Restart (apply settings)").clicked() {
                            self.service_action(ctx, "restart");
                        }
                        if ui.button("Stop").clicked() {
                            self.service_action(ctx, "stop");
                        }
                    } else if ui.button("Start").clicked() {
                        self.service_action(ctx, "start");
                    }
                    if svc.enabled == "enabled" {
                        if ui.button("Disable at login").clicked() {
                            self.service_action(ctx, "disable");
                        }
                    } else if ui.button("Enable at login").clicked() {
                        self.service_action(ctx, "enable");
                    }
                    if ui.button(RichText::new("Remove").color(RED)).on_hover_text("Stop the service and delete it").clicked() {
                        self.service_action(ctx, "remove");
                    }
                });
            });
        });
        if svc.running() && self.cfg != self.saved {
            ui.label(RichText::new("The service uses the saved settings: Restart to apply your changes.").small().color(AMBER));
        }
        ui.label(RichText::new("Commands:").small().color(DIM));
        for s in self.cfg.streams.iter().filter(|s| s.on) {
            ui.label(RichText::new(self.cfg.command_line(s)).monospace().small().color(TEXT));
        }
    }

    fn emitters_strip(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, emitters: &[Running], old: &[service::OldUnit], busy: bool) {
        if !old.is_empty() {
            ui.label(RichText::new("Other VBAN services (made by hand or by another tool)").strong());
            let mut retire = None;
            egui::Grid::new("oldunits").num_columns(3).spacing([12.0, 4.0]).show(ui, |ui| {
                for u in old {
                    let on = u.enabled == "enabled" || u.active == "active";
                    ui.label(RichText::new(&u.name).monospace().color(if on { AMBER } else { DIM }));
                    ui.label(RichText::new(format!("{} · {}", u.enabled, u.active)).small().color(DIM));
                    if on {
                        if ui
                            .add_enabled(!busy, egui::Button::new("Disable & stop"))
                            .on_hover_text("systemctl --user disable --now. Its files are kept, so it can be turned back on.")
                            .clicked()
                        {
                            retire = Some(u.name.clone());
                        }
                    } else {
                        ui.label(RichText::new("off").small().color(DIM));
                    }
                    ui.end_row();
                }
            });
            ui.label(
                RichText::new("To let this app stand in for an older setup: Copy settings from its emitter below, Disable & stop the old service, Stop its emitter, then Install & start here.")
                    .small()
                    .color(DIM),
            );
            if let Some(name) = retire {
                self.service_action(ctx, &format!("retire:{name}"));
            }
            ui.add_space(6.0);
        }
        if emitters.is_empty() {
            ui.label(RichText::new("No vban_emitter is running.").color(DIM));
            return;
        }
        let others = emitters.iter().filter(|r| r.owner == Owner::Other).count();
        let mut import = None;
        let mut kill = None;
        egui::Grid::new("emitters").num_columns(4).spacing([12.0, 4.0]).striped(true).show(ui, |ui| {
            for r in emitters {
                let who = match r.owner {
                    Owner::Here => RichText::new("this window").color(GREEN),
                    Owner::Service => RichText::new("service").color(GREEN),
                    Owner::Other => RichText::new("other").color(AMBER),
                };
                ui.label(who);
                ui.label(RichText::new(format!("{}  vban_emitter {}", r.pid, r.args.join(" "))).monospace().small());
                if ui.small_button("Copy settings").on_hover_text("Fill the selected stream from this emitter's command line").clicked() {
                    import = Some(r.clone());
                }
                if r.owner == Owner::Other {
                    if ui.small_button("Stop").on_hover_text("Send it SIGTERM").clicked() {
                        kill = Some(r.pid);
                    }
                } else {
                    ui.label("");
                }
                ui.end_row();
            }
        });
        if let Some(r) = import {
            self.import(&r);
        }
        if let Some(pid) = kill {
            let ok = unsafe { libc::kill(pid as i32, libc::SIGTERM) } == 0;
            push(&self.log, if ok {
                format!("[gui] sent SIGTERM to vban_emitter {pid}")
            } else {
                format!("[gui] couldn't stop vban_emitter {pid}: {}", std::io::Error::last_os_error())
            });
        }
        if others > 0 {
            ui.label(
                RichText::new("“other” emitters were started outside this app (a script or another service). Sending the same stream twice to one place will clash.")
                    .small()
                    .color(DIM),
            );
        }
    }

    fn log_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("LOG").strong().color(CYAN));
            ui.selectable_value(&mut self.tab, LogTab::Window, "This window");
            ui.selectable_value(&mut self.tab, LogTab::Service, "Service");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.tab == LogTab::Window && ui.small_button("Clear").clicked() {
                    self.log.lock().unwrap().clear();
                }
                if self.tab == LogTab::Service {
                    ui.label(RichText::new(service::log_path().display().to_string()).small().color(DIM));
                }
            });
        });
        let lines: Vec<String> = match self.tab {
            LogTab::Window => self.log.lock().unwrap().iter().cloned().collect(),
            LogTab::Service => self.watched.lock().unwrap().service_log.clone(),
        };
        egui::Frame::new().fill(FIELD).inner_margin(6).corner_radius(4).show(ui, |ui| {
            egui::ScrollArea::both().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                if lines.is_empty() {
                    ui.label(RichText::new("(nothing yet)").monospace().color(DIM));
                }
                for l in &lines {
                    let c = if l.starts_with("[gui]") || l.starts_with("[service]") {
                        CYAN
                    } else if l.contains("FATAL") || l.contains("ERROR") || l.contains("rror") {
                        RED
                    } else {
                        TEXT
                    };
                    ui.label(RichText::new(l).monospace().size(12.0).color(c));
                }
            });
        });
    }
}

fn db_text(peak: f32) -> String {
    if peak < 0.001 {
        "silent".into()
    } else {
        format!("{:>5.1} dB", 20.0 * peak.log10())
    }
}

/// Two thin bars (left, right), -60 dB to 0 dB, green then amber then red.
fn level_meter(ui: &mut egui::Ui, level: [f32; 2], width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 16.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2.0, FIELD);
    let to_x = |db: f32| rect.left() + rect.width() * ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    for (i, l) in level.iter().enumerate() {
        let top = rect.top() + 2.0 + i as f32 * 6.5;
        let db = if *l > 0.0 { 20.0 * l.log10() } else { -100.0 };
        let end = to_x(db);
        for (from, to, color) in [(-60.0, -12.0, GREEN), (-12.0, -3.0, AMBER), (-3.0, 0.0, RED)] {
            let (a, b) = (to_x(from), to_x(to).min(end));
            if b > a {
                p.rect_filled(egui::Rect::from_min_max(egui::pos2(a, top), egui::pos2(b, top + 5.0)), 1.0, color);
            }
        }
    }
    if width > 100.0 {
        for db in [-48.0, -36.0, -24.0, -12.0, -3.0] {
            let x = to_x(db);
            p.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], Stroke::new(1.0_f32, Color32::from_black_alpha(140)));
        }
    }
}

fn caption(s: &str) -> RichText {
    RichText::new(s).color(DIM)
}

fn led(ui: &mut egui::Ui, color: Color32, r: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(r * 2.0 + 4.0, r * 2.0 + 4.0), egui::Sense::hover());
    let p = ui.painter();
    if color == GREEN || color == RED || color == AMBER {
        p.circle_filled(rect.center(), r + 3.0, color.gamma_multiply(0.25));
    }
    p.circle_filled(rect.center(), r, color);
    p.circle_stroke(rect.center(), r, Stroke::new(1.0_f32, Color32::from_black_alpha(120)));
    resp
}

/// A titled, rounded mixer strip.
fn strip(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new().fill(STRIP).stroke(Stroke::new(1.0_f32, EDGE)).corner_radius(6).inner_margin(10).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.label(RichText::new(title).size(12.0).strong().color(CYAN));
        ui.add_space(2.0);
        let r = ui.available_rect_before_wrap();
        ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0_f32, EDGE));
        ui.add_space(6.0);
        body(ui);
    });
}

fn source_label(source: &str, list: &[Source], backend: &str) -> String {
    if source == DEFAULT_MONITOR {
        "Desktop (default output)".into()
    } else if source.is_empty() {
        if backend == "pulseaudio" { "Default input" } else { "Where ALSA records" }.into()
    } else {
        list.iter().find(|s| s.name == source).map_or_else(|| source.to_string(), |s| s.description.clone())
    }
}

fn rate_label(r: u32) -> String {
    if r % 1000 == 0 { format!("{} kHz", r / 1000) } else { format!("{:.1} kHz", r as f32 / 1000.0) }
}

fn format_label(f: &str) -> String {
    match f {
        "8I" => "8-bit",
        "16I" => "16-bit",
        "24I" => "24-bit",
        "32I" => "32-bit",
        "32F" => "32 float",
        "64F" => "64 float",
        "12I" => "12-bit",
        "10I" => "10-bit",
        other => other,
    }
    .to_string()
}

fn level_label(l: u8) -> &'static str {
    match l {
        0 => "0 · fatal",
        1 => "1 · errors",
        2 => "2 · warnings",
        3 => "3 · info",
        _ => "4 · debug",
    }
}

fn style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = FIELD;
    v.faint_bg_color = Color32::from_rgb(0x30, 0x39, 0x48);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.selection.bg_fill = Color32::from_rgb(0x1f, 0x6f, 0x80);
    v.selection.stroke = Stroke::new(1.0_f32, CYAN);
    v.widgets.inactive.bg_fill = Color32::from_rgb(0x36, 0x40, 0x50);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x36, 0x40, 0x50);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, EDGE);
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x42, 0x4f, 0x63);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x42, 0x4f, 0x63);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, CYAN);
    v.widgets.active.bg_fill = Color32::from_rgb(0x1f, 0x6f, 0x80);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(0x1f, 0x6f, 0x80);
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(7.0, 6.0);
        s.spacing.button_padding = egui::vec2(9.0, 4.0);
    });
}
