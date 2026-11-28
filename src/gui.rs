//! The window: a list of outputs you switch on, one power switch, one
//! calibration button. Everything else (how audio is captured, which output
//! the video follows, initial delays, system output, volume) is automatic.

use crate::backend::{self, CapturePermission, DevKind, DeviceInfo, DeviceList};
use crate::calibrate::{self, CalMsg, CalRequest, CalRoute};
use crate::config::{Config, OutputConfig};
use crate::engine::{Engine, ReaderState, RouteStatus, CENTRE_MIN_HZ, MAX_DELAY_MS, MAX_SAFETY_MS, REAR_DELAY_MS};
use crate::plan::{self, Plan};
use crate::record;
use crate::volume;
use eframe::egui::{self, Color32, CornerRadius, FontId, Margin, RichText, Sense, Stroke, TextStyle};
use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Look
// ---------------------------------------------------------------------------

const BG: Color32 = Color32::from_rgb(12, 14, 19);
const PANEL: Color32 = Color32::from_rgb(17, 20, 27);
const CARD: Color32 = Color32::from_rgb(23, 27, 36);
const CARD_OFF: Color32 = Color32::from_rgb(18, 21, 28);
const BORDER: Color32 = Color32::from_rgb(36, 42, 56);
const TRACK: Color32 = Color32::from_rgb(40, 46, 61);
const TEXT: Color32 = Color32::from_rgb(232, 235, 242);
const DIM: Color32 = Color32::from_rgb(132, 141, 160);
const ACCENT: Color32 = Color32::from_rgb(124, 92, 255);
const OK: Color32 = Color32::from_rgb(46, 204, 154);
const WARN: Color32 = Color32::from_rgb(255, 176, 64);
const BAD: Color32 = Color32::from_rgb(255, 92, 92);

fn setup_style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = Color32::from_rgb(9, 11, 15);
    v.faint_bg_color = CARD;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0_f32, TEXT);
    v.hyperlink_color = ACCENT;
    v.slider_trailing_fill = true;
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(10);
    let r = CornerRadius::same(8);
    v.widgets.noninteractive.corner_radius = r;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    for (w, fill) in [
        (&mut v.widgets.inactive, TRACK),
        (&mut v.widgets.hovered, Color32::from_rgb(54, 61, 80)),
        (&mut v.widgets.active, ACCENT),
        (&mut v.widgets.open, Color32::from_rgb(54, 61, 80)),
    ] {
        w.corner_radius = r;
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::NONE;
        w.fg_stroke = Stroke::new(1.5_f32, TEXT);
    }
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        s.text_styles = [
            (TextStyle::Heading, FontId::proportional(26.0)),
            (TextStyle::Body, FontId::proportional(15.0)),
            (TextStyle::Button, FontId::proportional(15.0)),
            (TextStyle::Small, FontId::proportional(12.5)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(10.0, 8.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.interact_size.y = 26.0;
        s.spacing.slider_width = 240.0;
    });
}

/// iOS-style switch.
fn switch(ui: &mut egui::Ui, on: &mut bool, size: egui::Vec2) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let r = size.y / 2.0;
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(r as u8), TRACK.lerp_to_gamma(ACCENT, t));
    let x = egui::lerp((rect.left() + r)..=(rect.right() - r), t);
    p.circle_filled(egui::pos2(x, rect.center().y), r - 3.0, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Level meter, `level` in 0..1.
fn meter(ui: &mut egui::Ui, level: f32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 6.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(3), TRACK);
    if level > 0.0 {
        let mut fill = rect;
        fill.set_width(rect.width() * level.clamp(0.0, 1.0));
        let col = if level > 0.92 {
            BAD
        } else if level > 0.75 {
            WARN
        } else {
            OK
        };
        p.rect_filled(fill, CornerRadius::same(3), col);
    }
}

fn chip(ui: &mut egui::Ui, text: &str, fg: Color32, bg: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| ui.label(RichText::new(text).small().color(fg)))
        .response
}

fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

fn level(peak: f32) -> f32 {
    if peak <= 1e-5 {
        0.0
    } else {
        ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
    }
}

fn gain_of(o: &OutputConfig) -> f32 {
    if o.muted {
        0.0
    } else {
        1.0
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "linux"))]
pub fn run() -> anyhow::Result<()> {
    run_window()
}

/// Linux: the window, with MultiAudio in the tray when there is one.
#[cfg(target_os = "linux")]
pub fn run() -> anyhow::Result<()> {
    run_linux(false)
}

/// Linux: only in the tray, no window (start at login).
#[cfg(target_os = "linux")]
pub fn run_in_tray() -> anyhow::Result<()> {
    run_linux(true)
}

fn native_options() -> eframe::NativeOptions {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("MultiAudio")
        .with_inner_size([820.0, 760.0])
        .with_min_inner_size([560.0, 480.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    // Wayland/X11: lets the desktop match the window to multiaudio.desktop
    #[cfg(target_os = "linux")]
    {
        viewport = viewport.with_app_id("multiaudio");
    }
    eframe::NativeOptions { viewport, ..Default::default() }
}

/// The window alone: closing it quits.
fn run_window() -> anyhow::Result<()> {
    eframe::run_native(
        "MultiAudio",
        native_options(),
        Box::new(|cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(target_os = "linux")]
fn run_linux(hidden: bool) -> anyhow::Result<()> {
    if crate::tray::open_running_instance() {
        println!("MultiAudio è già acceso: apro la sua finestra.");
        return Ok(());
    }
    match crate::tray::Tray::spawn(hidden) {
        Some(tray) => run_with_tray(tray, hidden),
        None => run_window(),
    }
}

/// Linux: the app lives in the tray; the window comes and goes. With the
/// window closed the same work goes on without drawing.
#[cfg(target_os = "linux")]
fn run_with_tray(tray: crate::tray::Tray, hidden: bool) -> anyhow::Result<()> {
    use crate::tray::Cmd;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    let app = Rc::new(RefCell::new(App::new()));
    let tray = Rc::new(RefCell::new(tray));
    let quit = Rc::new(Cell::new(false));
    let mut show = !hidden;
    let mut told = false;
    loop {
        if show {
            show = false;
            let window = InTray { app: app.clone(), tray: tray.clone(), quit: quit.clone() };
            eframe::run_native(
                "MultiAudio",
                native_options(),
                Box::new(move |cc| {
                    setup_style(&cc.egui_ctx);
                    Ok(Box::new(window))
                }),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            if quit.get() {
                break;
            }
            crate::diag::log("finestra chiusa: resta nella barra");
            if !told {
                told = true;
                crate::tray::notify("MultiAudio resta nella barra", "Per chiuderlo del tutto: «Esci» nel suo menu.");
            }
        }
        {
            let mut a = app.borrow_mut();
            let mut t = tray.borrow_mut();
            while let Some(cmd) = t.try_recv() {
                match cmd {
                    Cmd::Open => show = true,
                    Cmd::Quit => quit.set(true),
                    other => a.tray_command(other),
                }
            }
            if quit.get() {
                break;
            }
            let plan = a.before_frame();
            a.after_frame(&plan);
            t.show(a.tray_snapshot());
        }
        if !show {
            std::thread::sleep(Duration::from_millis(60));
        }
    }
    crate::diag::log("chiusura dall'icona nella barra");
    tray.borrow().release();
    app.borrow_mut().shutdown();
    Ok(())
}

/// Linux: the window while MultiAudio lives in the tray. Closing it does
/// not stop anything.
#[cfg(target_os = "linux")]
struct InTray {
    app: std::rc::Rc<std::cell::RefCell<App>>,
    tray: std::rc::Rc<std::cell::RefCell<crate::tray::Tray>>,
    quit: std::rc::Rc<std::cell::Cell<bool>>,
}

#[cfg(target_os = "linux")]
impl eframe::App for InTray {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        use crate::tray::Cmd;
        let mut app = self.app.borrow_mut();
        let mut tray = self.tray.borrow_mut();
        while let Some(cmd) = tray.try_recv() {
            match cmd {
                Cmd::Open => ctx.send_viewport_cmd(egui::ViewportCommand::Focus),
                Cmd::Quit => {
                    self.quit.set(true);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                other => app.tray_command(other),
            }
        }
        eframe::App::update(&mut *app, ctx, frame);
        tray.show(app.tray_snapshot());
    }
    // (no on_exit: the app keeps running; «Esci» shuts it down in run_with_tray)
}

#[cfg(target_os = "linux")]
impl App {
    /// «MultiAudio» stays among the system's outputs while MultiAudio is off
    /// (it is a sound card), so picking an output in the desktop's audio menu
    /// is the switch: another one switches MultiAudio off, «MultiAudio»
    /// switches it on. (Left playing, the engine heard nothing and its
    /// outputs drifted: switching back sounded broken, 2026-11-28.)
    /// Returns whether it switched MultiAudio on or off.
    fn follow_system_choice(&mut self) -> bool {
        let sys = self.system_output.clone();
        let ours = sys.as_deref().is_some_and(backend::is_own_device);
        if self.want_running && self.vout.is_some() && !ours {
            if let Some(s) = &sys {
                crate::diag::log(format!("scelta dal sistema l'uscita «{}»: spengo", backend::display_name(s)));
                self.notice = Some((format!("Hai scelto «{}» come uscita: MultiAudio è spento.", backend::display_name(s)), Instant::now()));
                self.set_running(false);
                return true;
            }
        } else if !self.want_running && ours && self.cfg.outputs.iter().any(|o| o.enabled) {
            crate::diag::log("scelta dal sistema l'uscita «MultiAudio»: accendo");
            self.set_running(true);
            return true;
        }
        false
    }

    fn tray_snapshot(&self) -> crate::tray::Snapshot {
        use crate::tray::OutputItem;
        let mut outputs = Vec::new();
        for d in &self.devices.outputs {
            let enabled = self.cfg.outputs.iter().any(|o| o.id == d.id && o.enabled);
            // the outputs PipeWire shows; raw ALSA devices and virtual ones only if in use
            if !enabled && (backend::is_own_device(&d.id) || d.kind == DevKind::Virtual || !d.id.starts_with("pulse:")) {
                continue;
            }
            // (they are all PipeWire's: no need to say so)
            let name = d.label.strip_prefix("PipeWire · ").unwrap_or(&d.label).to_string();
            outputs.push(OutputItem { id: d.id.clone(), name, enabled, connected: true });
        }
        for o in &self.cfg.outputs {
            if self.devices.output(&o.id).is_none() {
                outputs.push(OutputItem { id: o.id.clone(), name: o.name.clone(), enabled: o.enabled, connected: false });
            }
        }
        crate::tray::Snapshot { running: self.want_running, outputs, autostart: crate::tray::autostart_enabled() }
    }

    fn tray_command(&mut self, cmd: crate::tray::Cmd) {
        use crate::tray::Cmd;
        match cmd {
            Cmd::SetRunning(on) => self.set_running(on),
            Cmd::ToggleOutput(id) => {
                let on = !self.cfg.outputs.iter().any(|o| o.id == id && o.enabled);
                let name = self.devices.output(&id).map(|d| d.label.clone()).unwrap_or_default();
                self.set_output_enabled(&id, &name, on);
            }
            Cmd::SetAutostart(on) => {
                if let Err(e) = crate::tray::set_autostart(on) {
                    self.push_log(format!("Avvio all'accesso: {e}"));
                }
            }
            Cmd::Open | Cmd::Quit => {}
        }
    }
}

/// How long after the engine closed MultiAudio's own output is made or
/// removed (macOS; see `App::sync_vout`).
const VOUT_SETTLE: Duration = Duration::from_millis(500);
/// How long before making MultiAudio's own output of the same devices is
/// tried again (the engine restarts for it).
const VOUT_RETRY: Duration = Duration::from_secs(60);

#[derive(PartialEq, Clone)]
struct Topology {
    routes: Vec<(String, String)>,
    reference: String,
}

fn topology(p: &Plan) -> Topology {
    Topology {
        routes: p.engine_cfg.routes.iter().map(|r| (r.input.clone(), r.output.clone())).collect(),
        reference: p.engine_cfg.reference.clone(),
    }
}

struct App {
    cfg: Config,
    saved: Config,
    dirty_since: Option<Instant>,
    devices: DeviceList,
    device_ids: Vec<String>,
    engine: Option<Engine>,
    engine_topo: Option<Topology>,
    /// Engine route index → index in cfg.outputs (for the running engine).
    route_output: Vec<usize>,
    want_running: bool,
    test_click: bool,
    last_scan: Instant,
    restart_at: Option<Instant>,
    log: Vec<String>,
    calib: Option<Receiver<CalMsg>>,
    calib_log: Vec<String>,
    meters: HashMap<String, f32>,
    input_meter: f32,
    last_signal: Instant,
    system_output: Option<String>,
    main_set_for: Option<String>,
    permission: CapturePermission,
    last_poll: Instant,
    show_virtual: bool,
    /// Short message shown in the footer (text, since).
    notice: Option<(String, Instant)>,
    /// MultiAudio's own output (macOS: "MultiAudio", the system output;
    /// Linux: "MultiAudio 5.1" with surround on) and what it is made of.
    vout: Option<backend::VirtualOut>,
    vout_key: Vec<String>,
    /// When the engine was last closed (see `sync_vout`).
    engine_closed: Option<Instant>,
    /// MultiAudio's output could not be made of this: not tried again
    /// before then.
    vout_failed: Option<(Vec<String>, Instant)>,
    /// MultiAudio's volume got its starting value.
    volume_set: bool,
    /// Largest safety buffer the engine needed so far (ms).
    safety_seen: f32,
    /// Since when the latency MultiAudio's output declares no longer fits.
    vout_misfit_since: Option<Instant>,
    /// The Accessibility prompt (volume keys) was shown in this run.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    keys_asked: bool,
    /// Engine postponed until macOS grants system-audio capture.
    awaiting_permission: bool,
    /// Answer of the permission prompt: 0 none yet, 1 granted, 2 denied.
    permission_answer: std::sync::Arc<std::sync::atomic::AtomicU8>,
    permission_asked: bool,
    /// The last start of "MultiAudio" with a Bluetooth clock did not get
    /// through: real 5.1 with a Bluetooth video output was switched off.
    bt_clock_tripped: bool,
    /// A diagnosis being recorded (see `record`), what came of the last
    /// one, and the watch for one asked from outside (`record::ARMED`:
    /// looked for once a second, taken after two seconds of sound).
    diag: Option<Receiver<Result<std::path::PathBuf, String>>>,
    diag_msg: String,
    diag_looked: Instant,
    diag_armed: bool,
    /// How the capture is set up (`backend::capture_tuning`), as last seen.
    capture_tuning: String,
    sound_since: Option<Instant>,
}

impl App {
    fn new() -> Self {
        backend::cleanup_vout();
        #[cfg(target_os = "macos")]
        crate::machud::enable();
        let mut cfg = Config::load();
        let bt_clock_tripped = backend::bt_clock_tripped();
        if bt_clock_tripped {
            backend::bt_clock_reset();
            cfg.bt_surround = false;
            crate::diag::log("l'ultimo avvio dell'uscita MultiAudio con il clock Bluetooth non è arrivato in fondo: 5.1 vero col Bluetooth spento");
        }
        let devices = backend::list_devices();
        if cfg.calib_mic.is_empty() {
            cfg.calib_mic = guess_mic(&devices);
        }
        let mut app = Self {
            // (after a start that did not get through the change is to be saved)
            saved: if bt_clock_tripped { Config { bt_surround: true, ..cfg.clone() } } else { cfg.clone() },
            cfg,
            dirty_since: None,
            device_ids: devices.all_ids(),
            devices,
            engine: None,
            engine_topo: None,
            route_output: Vec::new(),
            want_running: false,
            test_click: false,
            last_scan: Instant::now(),
            restart_at: None,
            log: Vec::new(),
            calib: None,
            calib_log: Vec::new(),
            meters: HashMap::new(),
            input_meter: 0.0,
            last_signal: Instant::now(),
            system_output: backend::default_output_id(),
            main_set_for: None,
            permission: backend::capture_permission(),
            last_poll: Instant::now(),
            show_virtual: false,
            notice: None,
            vout: None,
            vout_key: Vec::new(),
            engine_closed: None,
            vout_failed: None,
            volume_set: false,
            safety_seen: 0.0,
            vout_misfit_since: None,
            keys_asked: false,
            awaiting_permission: false,
            permission_answer: Default::default(),
            permission_asked: false,
            bt_clock_tripped,
            diag: None,
            diag_msg: String::new(),
            diag_looked: Instant::now(),
            diag_armed: false,
            capture_tuning: backend::capture_tuning(),
            sound_since: None,
        };
        app.sync_names(); // (also asks the system for the names of outputs that are away)
        if app.cfg.autostart && app.cfg.outputs.iter().any(|o| o.enabled) {
            app.want_running = true;
            app.start_engine();
        }
        app
    }

    fn plan(&self) -> Plan {
        plan::make_plan(&self.cfg, &self.devices, self.vout.is_some())
    }

    fn start_engine(&mut self) {
        self.close_engine(); // release devices first
        self.rescan();
        // Opening the system-audio capture before the permission is granted
        // blocks until the user answers the macOS prompt: ask first, start
        // later (and only then take over the system output).
        let plan = plan::make_plan(&self.cfg, &self.devices, false);
        let needs_capture = plan.engine_cfg.routes.iter().any(|r| r.input.starts_with("tap:"));
        let answer = self.permission_answer.load(Relaxed);
        if needs_capture && self.permission != CapturePermission::Granted && answer != 1 {
            self.engine_topo = Some(topology(&plan));
            self.awaiting_permission = true;
            if self.permission == CapturePermission::NotAsked && !self.permission_asked {
                self.permission_asked = true;
                let flag = self.permission_answer.clone();
                crate::diag::log("chiedo il permesso di registrare l'audio di sistema");
                if !backend::request_capture_permission(move |ok| flag.store(if ok { 1 } else { 2 }, Relaxed)) {
                    self.awaiting_permission = false; // cannot ask: try anyway
                }
            } else if self.permission == CapturePermission::NotApplicable {
                self.awaiting_permission = false;
            }
            if self.awaiting_permission {
                return;
            }
        }
        self.awaiting_permission = false;
        if !self.volume_set {
            volume::set(crate::starting_volume(&self.cfg), false);
            self.volume_set = true;
        }
        let had_vout = self.vout.is_some();
        self.sync_vout();
        if self.vout.is_some() && !had_vout {
            self.rescan(); // its capture input now exists
        }
        let mut plan = self.plan();
        if let Some(p) = &plan.problem {
            self.push_log(p.clone());
        }
        if let Some((declared, _)) = self.vout.as_ref().and_then(|v| v.declared_ms()) {
            plan.engine_cfg.anchor_ms = Some(plan::anchor_ms(&self.cfg, &self.devices, plan.main.as_deref(), declared));
        }
        self.apply_system_output(&plan);
        let e = Engine::start(&plan.engine_cfg, &self.devices, self.test_click);
        for (i, s) in e.status.iter().enumerate() {
            if let RouteStatus::Error(msg) = s {
                let name = plan.engine_cfg.routes.get(i).map(|r| r.name.clone()).unwrap_or_default();
                self.push_log(format!("«{name}»: {msg}"));
            }
        }
        self.engine_topo = Some(topology(&plan));
        self.route_output = plan.route_output.clone();
        self.engine = Some(e);
        self.last_signal = Instant::now();
    }

    /// Sets the system output (MultiAudio's own output, or the video output),
    /// once per choice (so the user can still change it by hand afterwards).
    fn apply_system_output(&mut self, plan: &Plan) {
        let Some(main) = plan.system_output.clone() else { return };
        if self.main_set_for.as_ref() == Some(&main) {
            return;
        }
        self.main_set_for = Some(main.clone());
        if self.system_output.as_deref().map(backend::physical_key) == Some(backend::physical_key(&main)) {
            return;
        }
        match backend::set_default_output(&main) {
            Ok(()) => self.system_output = Some(main),
            Err(e) => self.push_log(format!("Imposta «{}» come uscita audio del sistema ({e})", backend::display_name(&main))),
        }
    }

    /// Closes the engine: its streams, the capture of the system's audio.
    fn close_engine(&mut self) {
        if self.engine.take().is_some() {
            self.engine_closed = Some(Instant::now());
        }
    }

    /// Waits until MultiAudio's own output may be touched (see `sync_vout`).
    fn settle(&self) {
        if backend::VOUT_IS_SYSTEM {
            if let Some(left) = self.engine_closed.and_then(|t| VOUT_SETTLE.checked_sub(t.elapsed())) {
                std::thread::sleep(left);
            }
        }
    }

    /// Removes MultiAudio's own output; the engine is closed first.
    fn remove_vout(&mut self) {
        self.close_engine();
        if self.vout.is_some() {
            self.settle();
            self.vout = None;
        }
        self.vout_key.clear();
    }

    /// What MultiAudio's own output should be made of now (empty: there
    /// should be none), and the video output it follows.
    fn vout_wanted(&self) -> (Vec<String>, Option<String>) {
        let base = plan::make_plan(&self.cfg, &self.devices, false);
        let want = self.want_running
            && backend::vout_supported()
            && (backend::VOUT_IS_SYSTEM || self.cfg.surround)
            && base.problem.is_none()
            && base.main.is_some();
        if !want {
            return (Vec::new(), base.main);
        }
        let enabled: Vec<String> = self.cfg.outputs.iter().filter(|o| o.enabled).map(|o| o.id.clone()).collect();
        let bt_clock = self.cfg.surround && self.cfg.bt_surround;
        (backend::vout_members(&self.devices, &enabled, base.main.as_deref(), bt_clock), base.main)
    }

    /// Makes, remakes or removes MultiAudio's own output, as `vout_wanted`
    /// says: on macOS it is there while running (it is the system output),
    /// on Linux while running with surround on.
    /// Only with the engine closed, and a moment after it closed: on
    /// macOS the output borrows the devices the engine plays through, and
    /// the less is going on in the system's audio daemon while the output is
    /// made the better (it froze, for every program, on 2026-11-26, when
    /// surround was switched on with the engine running: what happened in
    /// it is in `mactap::VOUT_QUIET`, which is the actual remedy).
    fn sync_vout(&mut self) {
        debug_assert!(self.engine.is_none());
        let (key, main) = self.vout_wanted();
        if self.vout.is_some() && self.vout_key == key {
            return;
        }
        if key.is_empty() {
            if self.vout.is_some() {
                self.remove_vout();
                self.system_output = backend::default_output_id();
            }
            return;
        }
        self.remove_vout(); // gives the system output back first
        self.settle();
        let safety = self.safety_seen.max(self.cfg.safety_ms);
        let lat = plan::vout_latency_ms(&self.cfg, &self.devices, main.as_deref(), safety);
        match backend::create_vout(&self.devices, &key, lat, main.as_deref()) {
            Ok(h) => {
                self.vout = Some(h);
                self.vout_key = key;
                self.vout_failed = None;
                self.system_output = backend::default_output_id();
                if !backend::VOUT_IS_SYSTEM {
                    if let Some(m) = &main {
                        self.guard_output(m);
                    }
                }
            }
            Err(e) => {
                self.vout_failed = Some((key, Instant::now() + VOUT_RETRY));
                self.push_log(format!("Uscita MultiAudio non disponibile: {e}"));
            }
        }
    }

    /// The system output must not be one of MultiAudio's own devices unless
    /// it is the one wanted (e.g. "MultiAudio 5.1" picked by hand on Linux).
    fn guard_output(&mut self, wanted: &str) {
        if let Some(was) = backend::guard_system_output(wanted) {
            self.system_output = Some(wanted.to_string());
            self.notice = Some((
                format!("«{was}» non va scelta come uscita di sistema (solo nel player, per i film 5.1): ho rimesso «{}».", backend::display_name(wanted)),
                Instant::now(),
            ));
        }
    }

    /// The power switch (window header; Linux: also the tray menu).
    fn set_running(&mut self, on: bool) {
        self.want_running = on;
        crate::diag::log(if on { "acceso dall'utente" } else { "spento dall'utente" });
        if on {
            self.start_engine();
        } else {
            self.stop_engine();
        }
    }

    /// An output's switch (its card; Linux: also the tray menu).
    fn set_output_enabled(&mut self, id: &str, name: &str, on: bool) {
        match self.cfg.outputs.iter().position(|o| o.id == id) {
            Some(i) => self.cfg.outputs[i].enabled = on,
            None if on => self.cfg.outputs.push(OutputConfig { id: id.to_string(), name: name.to_string(), enabled: true, ..Default::default() }),
            None => {}
        }
    }

    fn stop_engine(&mut self) {
        self.remove_vout(); // the previous system output comes back
        self.system_output = backend::default_output_id();
        self.awaiting_permission = false;
        self.engine_topo = None;
        self.restart_at = None;
        self.main_set_for = None;
    }

    fn rescan(&mut self) {
        self.devices = backend::list_devices();
        self.device_ids = self.devices.all_ids();
        self.last_scan = Instant::now();
        self.sync_names();
    }

    /// Every output is called as its device calls itself (see
    /// `backend::known_name`), also in the settings: that is the name shown
    /// while it is away.
    fn sync_names(&mut self) {
        for o in self.cfg.outputs.iter_mut() {
            let real = self.devices.output(&o.id).map(|d| d.label.clone()).or_else(|| backend::known_name(&o.id));
            if let Some(name) = real.filter(|n| !n.is_empty() && *n != o.name) {
                crate::diag::log(format!("l'uscita «{}» ora si chiama come il suo dispositivo: «{name}»", o.name));
                o.name = name;
            }
        }
    }

    fn push_log(&mut self, s: String) {
        crate::diag::log(&s);
        if self.log.last() != Some(&s) {
            self.log.push(s);
        }
        let excess = self.log.len().saturating_sub(30);
        self.log.drain(..excess);
    }

    fn calibrating(&self) -> bool {
        self.calib.is_some()
    }

    fn uses_capture(&self) -> bool {
        self.engine_topo.as_ref().map(|t| t.routes.iter().any(|r| r.0.starts_with("tap:"))).unwrap_or(false)
    }

    fn housekeeping(&mut self, plan: &Plan) {
        let now = Instant::now();
        if self.last_poll.elapsed() > Duration::from_secs(2) {
            self.last_poll = now;
            let sys = backend::default_output_id();
            let changed = sys != self.system_output;
            if changed {
                crate::diag::log(format!(
                    "uscita di sistema: {} → {}",
                    self.system_output.as_deref().map(backend::display_name).unwrap_or_default(),
                    sys.as_deref().map(backend::display_name).unwrap_or_default()
                ));
            }
            self.system_output = sys;
            // (switched on or off by that choice: `plan` is from before it)
            #[cfg(target_os = "linux")]
            let acted = changed && self.follow_system_choice();
            #[cfg(not(target_os = "linux"))]
            let acted = {
                let _ = changed;
                false
            };
            if self.want_running && !acted {
                if let Some(wanted) = plan.system_output.clone() {
                    self.guard_output(&wanted);
                }
            }
            // the latency MultiAudio's output declares must still fit the
            // delays and the buffer, otherwise make the output again
            let declared = self.vout.as_ref().and_then(|v| v.declared_ms());
            match (declared, &self.engine) {
                (Some((d, floor)), Some(e)) if !self.calibrating() => {
                    let safety = e.safety.effective_ms();
                    if plan::vout_needs_rebuild(&self.cfg, &self.devices, plan.main.as_deref(), safety, d, floor) {
                        let since = *self.vout_misfit_since.get_or_insert(now);
                        if now.duration_since(since) >= Duration::from_secs(3) {
                            crate::diag::log(format!("la latenza dichiarata ({d:.0} ms) non va più bene: rifaccio l'uscita MultiAudio"));
                            self.vout_misfit_since = None;
                            self.vout_key.clear(); // sync_vout makes it again
                        }
                    } else {
                        self.vout_misfit_since = None;
                    }
                }
                _ => self.vout_misfit_since = None,
            }
            // volume keys: as soon as macOS allows them
            #[cfg(target_os = "macos")]
            if self.engine.is_some() && !crate::mackeys::running() {
                if crate::mackeys::trusted() {
                    crate::mackeys::start();
                } else if !self.keys_asked {
                    self.keys_asked = true;
                    crate::diag::log("chiedo il permesso Accessibilità per i tasti del volume");
                    crate::mackeys::ask();
                }
            }
            let before = self.permission;
            self.permission = backend::capture_permission();
            if before != self.permission {
                crate::diag::log(format!("permesso cattura: {before:?} → {:?}", self.permission));
            }
            // A capture created before the user clicked "Allow" stays silent:
            // recreate it as soon as the permission arrives.
            if before != CapturePermission::Granted
                && self.permission == CapturePermission::Granted
                && self.engine.is_some()
                && self.uses_capture()
                && !self.calibrating()
            {
                self.push_log("Permesso concesso: riavvio la cattura.".into());
                self.restart_at = Some(now);
            }
        }

        if self.want_running
            && self.engine.is_some()
            && !self.calibrating()
            && !self.awaiting_permission
            && self.restart_at.is_none()
        {
            let want = self.vout_wanted().0;
            let failed = self.vout_failed.as_ref().is_some_and(|(k, until)| *k == want && now < *until);
            if want != self.vout_key && !failed {
                crate::diag::log(format!("uscita MultiAudio: {:?} → {want:?}", self.vout_key));
                // the engine goes first, then the output (see `sync_vout`)
                self.restart_at = Some(now);
            }
        }
        if self.awaiting_permission && self.want_running {
            let answer = self.permission_answer.load(Relaxed);
            if answer == 1 || self.permission == CapturePermission::Granted {
                crate::diag::log("permesso concesso: avvio");
                self.start_engine();
            } else if answer == 2 {
                self.permission = CapturePermission::Denied;
            }
        }
        let mut new_errors = Vec::new();
        let mut failed = false;
        if let Some(e) = &self.engine {
            if let Ok(mut v) = e.errors.lock() {
                new_errors.append(&mut v);
            }
            failed = e.failed.load(Relaxed);
        }
        for m in new_errors {
            self.push_log(m);
        }
        if self.want_running && !self.calibrating() {
            if failed && self.restart_at.is_none() {
                self.push_log("Un dispositivo si è scollegato: riprovo…".into());
                self.restart_at = Some(now + Duration::from_millis(1500));
            }
            if self.engine.is_some() && self.engine_topo.as_ref() != Some(&topology(plan)) && self.restart_at.is_none() {
                crate::diag::log("uscite cambiate: riavvio il motore");
                self.restart_at = Some(now + Duration::from_millis(150));
            }
            let needs = self.engine.as_ref().map(|e| e.needs_devices()).unwrap_or(false);
            if self.last_scan.elapsed() > Duration::from_secs(if needs { 3 } else { 10 }) && self.restart_at.is_none() {
                let before = self.device_ids.clone();
                self.rescan();
                if needs && before != self.device_ids {
                    crate::diag::log("dispositivi cambiati: riavvio il motore");
                    self.restart_at = Some(now);
                }
            }
            if let Some(t) = self.restart_at {
                if now >= t {
                    self.restart_at = None;
                    self.start_engine();
                }
            }
        } else if !self.want_running && self.last_scan.elapsed() > Duration::from_secs(5) {
            self.rescan();
        }

        // a diagnosis: one being recorded, or one asked for from outside
        if let Some(Ok(done)) = self.diag.as_ref().map(|rx| rx.try_recv()) {
            self.diag = None;
            self.diag_msg = match done {
                Ok(dir) => format!("Registrazione salvata in {}", dir.display()),
                Err(e) => format!("Registrazione non riuscita: {e}"),
            };
        }
        if self.diag_looked.elapsed() > Duration::from_secs(1) {
            self.diag_looked = Instant::now();
            self.diag_armed = record::armed();
            // another way of capturing was asked for (the file `cattura`):
            // it is taken up by starting the engine again
            let tuning = backend::capture_tuning();
            if tuning != self.capture_tuning {
                self.capture_tuning = tuning;
                if self.engine.is_some() && self.restart_at.is_none() {
                    crate::diag::log(format!("{}: riavvio il motore", self.capture_tuning));
                    self.restart_at = Some(Instant::now());
                }
            }
        }
        let sound = self.engine.as_ref().is_some_and(|e| e.inputs.iter().any(|i| i.loud_within(300_000_000)));
        self.sound_since = if sound { self.sound_since.or(Some(Instant::now())) } else { None };
        if self.diag_armed && self.diag.is_none() && self.sound_since.is_some_and(|t| t.elapsed() > Duration::from_secs(2)) {
            self.diag_armed = false;
            record::disarm();
            self.start_diagnosis();
        }

        // calibration messages
        let mut finished = false;
        let mut apply = Vec::new();
        let mut heard = Vec::new();
        if let Some(rx) = &self.calib {
            while let Ok(m) = rx.try_recv() {
                match m {
                    CalMsg::Log(s) => {
                        crate::diag::log(format!("calibrazione: {s}"));
                        self.calib_log.push(s)
                    }
                    CalMsg::Done { delays, levels } => {
                        apply = delays;
                        heard = levels;
                        finished = true;
                    }
                    CalMsg::Failed(e) => {
                        self.calib_log.push(format!("✖ Calibrazione non riuscita: {e}"));
                        finished = true;
                    }
                }
            }
        }
        if finished {
            self.calib = None;
            if !apply.is_empty() {
                for (route, d) in apply {
                    if let Some(&o) = self.route_output.get(route) {
                        if let Some(out) = self.cfg.outputs.get_mut(o) {
                            out.delay_ms = d;
                            out.auto_delay = false;
                        }
                    }
                }
                self.calib_log.push("✔ Fatto! Controlla col click di test: devi sentire un solo click.".into());
                crate::diag::log("calibrazione completata");
            }
            // how loud each output is: with surround on, those behind and
            // at the centre must not be louder than the front
            let measured: Vec<(usize, f32)> = heard.iter().filter_map(|(route, db)| self.route_output.get(*route).map(|&o| (o, *db))).collect();
            if !measured.is_empty() {
                let old: Vec<Option<f32>> = self.cfg.outputs.iter().map(|o| o.level_db).collect();
                for (o, l) in self.cfg.outputs.iter_mut().zip(plan::merge_levels(&old, &measured)) {
                    o.level_db = l;
                }
                // (the measure takes the place of what was set by ear)
                for (o, _) in &measured {
                    if let Some(out) = self.cfg.outputs.get_mut(*o) {
                        out.surround_db = 0.0;
                    }
                }
                let with_surround = Config { surround: true, ..self.cfg.clone() };
                for (id, db) in plan::levels(&with_surround, &self.devices).trims {
                    if db < -0.5 {
                        let line = format!("  «{}»: {:.0} dB più forte delle casse davanti, con il surround viene abbassata di altrettanto", backend::display_name(&id), -db);
                        crate::diag::log(format!("calibrazione: {line}"));
                        self.calib_log.push(line);
                    }
                }
            }
        }
    }

    fn push_live_params(&mut self, plan: &Plan) {
        let Some(e) = &self.engine else { return };
        e.safety.user_ms.store(self.cfg.safety_ms.clamp(0.0, MAX_SAFETY_MS));
        self.safety_seen = self.safety_seen.max(e.safety.effective_ms());
        // sound out exactly when players show the picture (MultiAudio's output)
        let anchor = match self.vout.as_ref().and_then(|v| v.declared_ms()) {
            Some((declared, _)) => plan::anchor_ms(&self.cfg, &self.devices, plan.main.as_deref(), declared),
            None => f32::NAN,
        };
        e.safety.anchor_ms.store(anchor);
        e.test_click.store(self.test_click, Relaxed);
        // Rear and centre speakers count only if they play (not when switched
        // on but disconnected) and can be heard.
        let place = |o: &OutputConfig| plan::placement(&self.cfg, o);
        let playing = e.route_ctl.iter().enumerate().filter(|(_, c)| c.is_some()).filter_map(|(i, _)| self.route_output.get(i)).filter_map(|&o| self.cfg.outputs.get(o));
        let places = crate::engine::Places::of(playing.map(|o| (place(o), !o.muted && o.volume > 0.0))).with_stereo(&self.cfg);
        // (and one that is louder than the front is turned down to it)
        let running: Vec<(&std::sync::Arc<crate::engine::RouteCtl>, &OutputConfig)> =
            e.route_ctl.iter().enumerate().filter_map(|(i, c)| Some((c.as_ref()?, self.cfg.outputs.get(*self.route_output.get(i)?)?))).collect();
        let heard: Vec<crate::engine::Heard> =
            running.iter().map(|(_, o)| crate::engine::Heard { place: place(o), level_db: o.level_db, by_ear_db: o.surround_db }).collect();
        let trims = crate::engine::level_trims(&heard, self.cfg.match_levels);
        for ((ctl, _), db) in running.iter().zip(trims) {
            ctl.trim.store(crate::engine::db_to_gain(db));
        }
        for (i, ctl) in e.route_ctl.iter().enumerate() {
            let (Some(ctl), Some(&o)) = (ctl, self.route_output.get(i)) else { continue };
            if let Some(out) = self.cfg.outputs.get(o) {
                ctl.delay_ms.store((out.delay_ms - plan.idle_wait_ms).clamp(0.0, MAX_DELAY_MS));
                ctl.gain.store(gain_of(out));
                ctl.level.store(out.volume.clamp(0.0, 1.0));
                places.store(ctl, place(out));
            }
        }
    }

    fn autosave(&mut self) {
        if self.cfg == self.saved {
            self.dirty_since = None;
            return;
        }
        let t = *self.dirty_since.get_or_insert_with(Instant::now);
        if t.elapsed() > Duration::from_millis(700) {
            let changes: Vec<String> = self
                .cfg
                .outputs
                .iter()
                .filter(|o| self.saved.outputs.iter().find(|p| p.id == o.id).map(|p| p.delay_ms != o.delay_ms || p.enabled != o.enabled).unwrap_or(true))
                .map(|o| format!("«{}» {} {:.1} ms{}", o.name, if o.enabled { "accesa" } else { "spenta" }, o.delay_ms, if o.auto_delay { " (auto)" } else { "" }))
                .collect();
            if !changes.is_empty() {
                crate::diag::log(format!("impostazioni: {}", changes.join(", ")));
            }
            match self.cfg.save() {
                Ok(()) => self.saved = self.cfg.clone(),
                Err(e) => self.push_log(format!("Salvataggio configurazione fallito: {e}")),
            }
            self.dirty_since = None;
        }
    }

    /// Records ten seconds of what arrives and of what each output is sent
    /// (see `record`), with the settings in force written next to them.
    fn start_diagnosis(&mut self) {
        let Some(e) = &self.engine else { return };
        let describe = |i: usize| match self.route_output.get(i).and_then(|&o| self.cfg.outputs.get(o)) {
            Some(o) => (
                o.name.clone(),
                format!(
                    "Impostazioni: posizione {}, bassi {:?}, volume {:.0} %{}, misurata {:?} dB, a orecchio {:+.0} dB",
                    if o.centre { "centro" } else if o.rear { "dietro" } else { "davanti" },
                    o.bass,
                    o.volume * 100.0,
                    if o.muted { " (muto)" } else { "" },
                    o.level_db,
                    o.surround_db
                ),
            ),
            None => (format!("uscita {i}"), String::new()),
        };
        let about = format!(
            "surround {}, 5.1 vero col Bluetooth {}, con lo stereo: centro {}, dietro {:?}, pareggio volumi {}, uscita «MultiAudio» {}, volume MultiAudio {:.0} %",
            self.cfg.surround,
            self.cfg.bt_surround,
            if self.cfg.stereo_centre { "voci" } else { "tace" },
            self.cfg.stereo_rear,
            self.cfg.match_levels,
            self.vout.as_ref().map(|v| format!("{} canali", v.channels())).unwrap_or_else(|| "assente".into()),
            volume::get().0 * 100.0
        );
        self.diag = Some(record::start(record::tracks(e, describe), about));
        self.diag_msg = format!("Registro {:.0} secondi…", record::SECS);
    }

    fn start_calibration(&mut self) {
        let Some(e) = &self.engine else { return };
        let routes: Vec<CalRoute> = (0..e.route_ctl.len())
            // Linux: the "MultiAudio 5.1" routes duplicate the normal ones
            .filter(|&i| {
                e.route_input.get(i).cloned().flatten().map(|inp| backend::VOUT_IS_SYSTEM || inp.id != backend::VOUT_INPUT).unwrap_or(false)
            })
            .filter_map(|i| {
                let o = self.cfg.outputs.get(*self.route_output.get(i)?)?;
                if o.muted {
                    return None;
                }
                Some(CalRoute {
                    index: i,
                    name: o.name.clone(),
                    ctl: e.route_ctl.get(i).cloned().flatten()?,
                    input: e.route_input.get(i).cloned().flatten()?,
                    delay_ms: o.delay_ms,
                    extra_ms: e.route_ctl.get(i).cloned().flatten().map(|c| c.extra_delay_ms()).unwrap_or(0.0),
                })
            })
            .collect();
        // With an output of the list left out (away, switched off, muted)
        // the others keep their place against it: the least delayed stays
        // where it is.
        let least = routes.iter().map(|r| r.delay_ms).fold(f32::INFINITY, f32::min);
        let base_ms = if routes.len() < self.cfg.outputs.len() && least.is_finite() { least } else { 0.0 };
        let reference = e.reference.clone().map(|rc| {
            let label = self.engine_topo.as_ref().map(|t| backend::display_name(&t.reference)).unwrap_or_default();
            (label, rc)
        });
        let (tx, rx) = std::sync::mpsc::channel();
        self.calib_log.clear();
        self.calib_log.push("Calibrazione in corso: silenzio per qualche secondo…".into());
        self.test_click = false;
        calibrate::spawn(CalRequest { mic: self.cfg.calib_mic.clone(), routes, reference, base_ms }, tx);
        self.calib = Some(rx);
    }

    // ------------------------------------------------------------------ UI

    fn ui_header(&mut self, ui: &mut egui::Ui, plan: &Plan) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("MultiAudio").heading().strong());
                ui.horizontal(|ui| {
                    let on = self.cfg.outputs.iter().filter(|o| o.enabled).count();
                    if let Some(p) = &plan.problem {
                        dot(ui, BAD);
                        ui.label(RichText::new(p).color(BAD));
                    } else if self.awaiting_permission {
                        dot(ui, WARN);
                        ui.label(RichText::new("In attesa del permesso di macOS…").color(WARN));
                    } else if self.engine.is_some() {
                        dot(ui, OK);
                        // (outputs switched on but not connected do not count)
                        let on = self.cfg.outputs.iter().filter(|o| o.enabled && self.devices.output(&o.id).is_some()).count();
                        let mut s = format!("Attivo · {on} {}", if on == 1 { "uscita" } else { "uscite" });
                        if let Some(m) = &plan.main {
                            s.push_str(&format!(" · video su «{}»", backend::display_name(m)));
                        }
                        if self.cfg.surround {
                            s.push_str(" · surround");
                        }
                        ui.label(RichText::new(s).color(DIM));
                    } else if on == 0 {
                        dot(ui, DIM);
                        ui.label(RichText::new("Accendi le uscite che vuoi sentire").color(DIM));
                    } else {
                        dot(ui, DIM);
                        ui.label(RichText::new("Spento").color(DIM));
                    }
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut on = self.want_running;
                if switch(ui, &mut on, egui::vec2(64.0, 34.0)).changed() {
                    self.set_running(on);
                }
                if volume::active() && self.engine.is_some() {
                    ui.add_space(18.0);
                    ui_volume(ui);
                }
            });
        });

        #[cfg(target_os = "macos")]
        if volume::active() && self.engine.is_some() && !crate::mackeys::running() {
            ui.add_space(6.0);
            egui::Frame::new().fill(Color32::from_rgb(33, 29, 58)).corner_radius(CornerRadius::same(10)).inner_margin(10.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("🔊 Tasti del volume:").strong());
                    ui.label("consenti MultiAudio in Privacy e sicurezza → «Accessibilità» (intanto c'è il cursore qui sopra).");
                    if ui.button("Apri Impostazioni").clicked() {
                        crate::mackeys::open_settings();
                    }
                });
            });
        }

        if (self.awaiting_permission || (self.engine.is_some() && self.uses_capture()))
            && matches!(self.permission, CapturePermission::Denied | CapturePermission::NotAsked)
        {
            ui.add_space(6.0);
            egui::Frame::new().fill(Color32::from_rgb(70, 45, 14)).corner_radius(CornerRadius::same(10)).inner_margin(10.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("⚠ Serve il permesso «Registrazione audio di sistema».").color(WARN).strong());
                    ui.label(if self.permission == CapturePermission::NotAsked {
                        "Clicca «Consenti» nella finestra di macOS: MultiAudio parte da solo."
                    } else {
                        "Attivalo per MultiAudio nelle Impostazioni di Sistema."
                    });
                    if ui.button("Apri Impostazioni").clicked() {
                        backend::open_permission_settings();
                    }
                });
            });
        }
    }

    fn ui_outputs(&mut self, ui: &mut egui::Ui, plan: &Plan) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("USCITE").small().strong().color(DIM));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut on = self.cfg.surround;
                if switch(ui, &mut on, egui::vec2(40.0, 22.0))
                    .on_hover_text("Surround: con una sorgente 5.1 ogni uscita suona il suo canale (davanti, centro, dietro). Con una sorgente stereo, che è quasi tutto quello che arriva da un browser, le uscite suonano come a surround spento e la cassa centrale le voci: si cambia sulle schede delle uscite, «Con lo stereo».")
                    .changed()
                {
                    self.cfg.surround = on;
                    crate::diag::log(if on { "surround acceso" } else { "surround spento" });
                }
                ui.label(RichText::new("Surround").color(if self.cfg.surround { TEXT } else { DIM }));
            });
        });
        if self.cfg.surround && !self.cfg.outputs.iter().any(|o| o.enabled && (o.rear || o.centre)) {
            ui.label(RichText::new("Surround acceso: scegli quali uscite stanno «Dietro» o al «Centro» (sulla loro scheda).").small().color(WARN));
        }
        // the bass has to come out somewhere
        let any_plays = self.cfg.outputs.iter().any(|o| o.enabled && self.devices.output(&o.id).is_some());
        if any_plays && !plan::places(&self.cfg, &self.devices).front_full {
            ui.label(RichText::new("Nessuna uscita davanti ha «Bassi: Tutti»: i bassi li suonano lo stesso quelle davanti.").small().color(WARN));
        }
        // with surround on the outputs have to be as loud as each other
        let lv = plan::levels(&self.cfg, &self.devices);
        if let Some((name, front, db)) = &lv.weak_front {
            ui.label(
                RichText::new(format!(
                    "«{front}» (davanti) suona molto meno di «{name}», che per non suonare più forte del davanti è abbassata di {db:.0} dB: il surround esce piano. Per sentirlo forte metti davanti le casse più potenti."
                ))
                .small()
                .color(WARN),
            );
        } else if lv.unmeasured {
            ui.label(
                RichText::new("Se le casse dietro o al centro suonano più forte di quelle davanti, il suono sembra venire da lì: «Calibra col microfono» le pareggia da solo, oppure abbassale a orecchio con «Livello» sulla loro scheda.")
                    .small()
                    .color(WARN),
            );
        }
        // (macOS: the Bluetooth clock; Linux's output does not depend on it)
        let bt_video = backend::VOUT_IS_SYSTEM
            && cfg!(target_os = "macos")
            && plan.main.as_deref().and_then(|m| self.devices.output(m)).map(|d| d.kind == DevKind::Bluetooth).unwrap_or(false);
        if self.bt_clock_tripped && !self.cfg.bt_surround {
            ui.label(
                RichText::new("L'ultima volta l'uscita «MultiAudio» a 6 canali con le casse Bluetooth non è partita: per sicurezza il 5.1 vero con il video sul Bluetooth è stato spento (si riaccende dalle impostazioni avanzate).")
                    .small()
                    .color(WARN),
            );
        }
        if self.cfg.surround && backend::VOUT_IS_SYSTEM && self.vout.is_some() {
            let ch = self.vout.as_ref().map(|v| v.channels()).unwrap_or(0);
            let txt = if ch >= 6 {
                "Film in 5.1: arrivano in 5.1 vero da soli (l'uscita «MultiAudio» ha 6 canali). Lo stereo usa l'upmix."
            } else if ch >= 4 {
                "L'uscita «MultiAudio» ha 4 canali: i film 5.1 arrivano in quadrifonico. Lo stereo usa l'upmix."
            } else {
                "L'uscita «MultiAudio» è stereo (servono più uscite collegate per il 5.1): si usa l'upmix."
            };
            ui.label(RichText::new(txt).small().color(DIM));
        } else if self.cfg.surround && self.vout.is_some() {
            ui.label(
                RichText::new("Film in 5.1: nel player (VLC…) scegli l'uscita audio «MultiAudio 5.1». Il resto dell'audio usa l'upmix.")
                    .small()
                    .color(DIM),
            );
        } else if self.cfg.surround && backend::VOUT_IS_SYSTEM && self.engine.is_some() {
            let txt = if !bt_video {
                "Con questa uscita del video il 5.1 vero non è disponibile: si usa l'upmix."
            } else if self.cfg.bt_surround {
                "Per il 5.1 vero serve un'altra uscita collegata oltre alle casse Bluetooth: si usa l'upmix."
            } else {
                "5.1 vero con il video sul Bluetooth spento (impostazioni avanzate): si usa l'upmix."
            };
            ui.label(RichText::new(txt).small().color(DIM));
        } else if self.cfg.surround && !backend::vout_supported() {
            ui.label(RichText::new("Su questo sistema il 5.1 vero non è disponibile senza driver: si usa l'upmix.").small().color(DIM));
        }
        // Connected outputs (virtual ones only if wanted or already used),
        // then configured outputs that are not connected.
        let mut rows: Vec<(String, Option<DeviceInfo>)> = Vec::new();
        for d in &self.devices.outputs {
            let configured_on = self.cfg.outputs.iter().any(|o| o.id == d.id && o.enabled);
            if d.kind == DevKind::Virtual && !self.show_virtual && !configured_on {
                continue;
            }
            rows.push((d.id.clone(), Some(d.clone())));
        }
        for o in &self.cfg.outputs {
            if self.devices.output(&o.id).is_none() {
                rows.push((o.id.clone(), None));
            }
        }
        let mut remove = None;
        for (id, dev) in rows {
            if let Some(r) = self.ui_output_card(ui, plan, &id, dev.as_ref()) {
                remove = Some(r);
            }
            ui.add_space(2.0);
        }
        if let Some(pos) = remove.and_then(|id| self.cfg.outputs.iter().position(|o| o.id == id)) {
            self.cfg.outputs.remove(pos);
            // the running routes keep pointing at their own outputs
            for o in self.route_output.iter_mut() {
                *o = match (*o).cmp(&pos) {
                    std::cmp::Ordering::Less => *o,
                    std::cmp::Ordering::Equal => usize::MAX,
                    std::cmp::Ordering::Greater => *o - 1,
                };
            }
        }
    }

    /// Returns Some(id) if the user asked to forget a disconnected output.
    fn ui_output_card(&mut self, ui: &mut egui::Ui, plan: &Plan, id: &str, dev: Option<&DeviceInfo>) -> Option<String> {
        let idx = self.cfg.outputs.iter().position(|o| o.id == id);
        let enabled = idx.map(|i| self.cfg.outputs[i].enabled).unwrap_or(false);
        let is_main = plan.main.as_deref() == Some(id) && enabled;
        let by_system = is_main && plan.main_by_system;
        let name = dev.map(|d| d.label.clone()).or_else(|| idx.map(|i| self.cfg.outputs[i].name.clone())).unwrap_or_default();
        let kind = dev.map(|d| d.kind).unwrap_or_default();

        // engine state of this output
        let route = idx.and_then(|o| self.route_output.iter().position(|&x| x == o));
        let ctl = route.and_then(|r| self.engine.as_ref().and_then(|e| e.route_ctl.get(r).cloned().flatten()));
        let status = route.and_then(|r| self.engine.as_ref().and_then(|e| e.status.get(r).cloned()));
        let peak = if by_system {
            self.input_meter
        } else {
            let p = ctl.as_ref().map(|c| c.st_peak.swap(0.0)).unwrap_or(0.0);
            let m = self.meters.entry(id.to_string()).or_insert(0.0);
            *m = (*m * 0.85).max(level(p));
            *m
        };

        let mut forget = None;
        let fill = if enabled { CARD } else { CARD_OFF };
        let stroke = if is_main { Stroke::new(1.0_f32, ACCENT.gamma_multiply(0.6)) } else { Stroke::new(1.0_f32, BORDER) };
        egui::Frame::new().fill(fill).stroke(stroke).corner_radius(CornerRadius::same(14)).inner_margin(14.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let mut on = enabled;
                if dev.is_some() && switch(ui, &mut on, egui::vec2(44.0, 24.0)).changed() {
                    self.set_output_enabled(id, &name, on);
                }
                ui.label(RichText::new(kind.icon()).size(18.0));
                ui.label(RichText::new(&name).size(17.0).strong().color(if enabled { TEXT } else { DIM }));
                let mut info = kind.label().to_string();
                if let Some(l) = dev.and_then(|d| d.latency_ms) {
                    if !info.is_empty() {
                        info.push_str(" · ");
                    }
                    info.push_str(&format!("{l:.0} ms"));
                }
                if !info.is_empty() {
                    chip(ui, &info, DIM, TRACK);
                }
                if is_main {
                    chip(ui, "🎬 video", Color32::WHITE, ACCENT);
                }
                let place = idx.map(|i| &self.cfg.outputs[i]).filter(|o| o.rear || o.centre).map(|o| o.centre);
                if let (true, Some(centre)) = (self.cfg.surround && enabled && !by_system, place) {
                    chip(ui, if centre { "centro" } else { "dietro" }, OK, OK.gamma_multiply(0.18));
                    // (not while it stands in for a missing front)
                    if ctl.as_ref().is_some_and(|c| c.centre.load(Relaxed) || c.rear.load(Relaxed)) {
                        let discrete = idx
                            .map(|o| {
                                self.route_output.iter().enumerate().filter(|(_, &x)| x == o).any(|(r, _)| {
                                    self.engine.as_ref().and_then(|e| e.route_ctl.get(r).cloned().flatten()).map(|c| c.st_discrete.load(Relaxed)).unwrap_or(false)
                                })
                            })
                            .unwrap_or(false);
                        let voices_here = self.cfg.stereo_centre;
                        let rear_all = self.cfg.stereo_rear == crate::config::StereoRear::All;
                        let (txt, tip) = match (discrete, centre) {
                            (true, false) => ("5.1", "Sta suonando i canali surround veri della sorgente 5.1"),
                            (true, true) => ("5.1", "Sta suonando il canale centrale vero della sorgente 5.1"),
                            (false, false) if rear_all => ("stereo", "La sorgente è stereo: questa uscita la suona com'è, insieme a quelle davanti, come a surround spento. Si cambia qui sotto, «Con lo stereo»."),
                            (false, false) => ("ambiente", "La sorgente è stereo: questa uscita suona solo quello che cambia tra sinistra e destra, senza le voci, un attimo dopo il davanti. Si cambia qui sotto, «Con lo stereo»."),
                            (false, true) if voices_here => ("voci", "La sorgente non ha un canale centrale: questa cassa suona quello che sinistra e destra hanno in comune (le voci), insieme alle casse davanti"),
                            (false, true) => ("tace: stereo", "La sorgente non ha un canale centrale: questa cassa suona solo con le sorgenti 5.1. Si cambia qui sotto, «Con lo stereo»."),
                        };
                        chip(ui, txt, DIM, TRACK).on_hover_text(tip);
                    }
                }
                // turned down to the level of the front (see `engine::level_trims`)
                let trim_db = ctl.as_ref().map(|c| 20.0 * c.trim.load().max(1e-6).log10()).filter(|db| *db < -0.5);
                if let (true, Some(db)) = (enabled && !by_system, trim_db) {
                    chip(ui, &format!("−{:.0} dB", -db), DIM, TRACK).on_hover_text(format!(
                        "Con il surround suona {:.0} dB più piano, per non suonare più forte delle casse davanti: è quanto la calibrazione col microfono l'ha sentita più forte di loro, più quello che hai messo tu in «Livello» qui sotto.",
                        -db
                    ));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if dev.is_none() {
                        if ui.small_button("Dimentica").clicked() {
                            forget = Some(id.to_string());
                        }
                        ui.label(RichText::new("non collegata").color(DIM));
                        return;
                    }
                    if !enabled || self.engine.is_none() {
                        return;
                    }
                    // (muted is easy to do by mistake and hard to see: said in words)
                    let muted = idx.is_some_and(|i| self.cfg.outputs[i].muted);
                    let (txt, col) = if by_system {
                        ("suonata dal sistema".to_string(), OK)
                    } else {
                        match (&status, ctl.as_ref().map(|c| c.state())) {
                            (Some(RouteStatus::Running), Some(ReaderState::Running)) if muted => ("in muto".to_string(), WARN),
                            (Some(RouteStatus::Running), Some(ReaderState::Running)) => ("in uscita".to_string(), OK),
                            (Some(RouteStatus::Running), _) => ("in attesa di audio".to_string(), WARN),
                            (Some(RouteStatus::Missing(_)), _) => ("non collegata".to_string(), WARN),
                            (Some(RouteStatus::Error(e)), _) => (format!("errore: {e}"), BAD),
                            _ => (String::new(), DIM),
                        }
                    };
                    meter(ui, peak, 90.0);
                    ui.label(RichText::new(txt).small().color(col));
                });
            });

            let Some(i) = idx else { return };
            if !enabled || dev.is_none() {
                return;
            }
            ui.add_space(4.0);
            if by_system {
                ui.label(
                    RichText::new("La suona direttamente il sistema: il video si sincronizza su di lei, il volume si regola con i tasti del computer.")
                        .small()
                        .color(DIM),
                );
                return;
            }
            let surround = self.cfg.surround;
            let running = self.engine.is_some();
            // (what the engine applies now, calibration and ear together)
            let surround_trim_db = ctl.as_ref().map(|c| 20.0 * c.trim.load().max(1e-6).log10());
            let idle_wait_ms = plan.idle_wait_ms;
            let (stereo_centre, stereo_rear) = (&mut self.cfg.stereo_centre, &mut self.cfg.stereo_rear);
            let o = &mut self.cfg.outputs[i];
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Ritardo").color(DIM));
                let before = o.delay_ms;
                let slider = ui.add(
                    egui::Slider::new(&mut o.delay_ms, 0.0..=MAX_DELAY_MS)
                        .suffix(" ms")
                        .fixed_decimals(1)
                        .logarithmic(true)
                        .smallest_positive(1.0),
                );
                // The delays are those with every output of the list
                // playing. While the slowest one is away the others do not
                // wait for it: said here, for who wonders, and nowhere else
                // (2026-11-27, the user on a line that said it under
                // "Uscite": "che senso ha dirmelo? deve scegliere la più
                // lenta e basta").
                if idle_wait_ms >= 5.0 {
                    slider.on_hover_text(format!(
                        "Ora suona con {:.0} ms: l'uscita più lenta dell'elenco non sta suonando e le altre non la aspettano.",
                        (before - idle_wait_ms).max(0.0)
                    ));
                }
                for (lbl, d) in [("−10", -10.0), ("−1", -1.0), ("+1", 1.0), ("+10", 10.0)] {
                    if ui.small_button(lbl).clicked() {
                        o.delay_ms = ((o.delay_ms + d) * 10.0).round() / 10.0;
                    }
                }
                o.delay_ms = o.delay_ms.clamp(0.0, MAX_DELAY_MS);
                if (o.delay_ms - before).abs() > f32::EPSILON {
                    o.auto_delay = false;
                }
                if o.auto_delay {
                    chip(ui, "auto", ACCENT, ACCENT.gamma_multiply(0.18));
                } else if ui.small_button("↺ auto").on_hover_text("Torna al ritardo stimato automaticamente").clicked() {
                    o.auto_delay = true;
                }
            });
            // the volume acts on the devices: only while MultiAudio is on
            ui.add_enabled_ui(running, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Volume ").color(DIM));
                    let mut pct = o.volume * 100.0;
                    let r = ui
                        .add_enabled(!o.muted, egui::Slider::new(&mut pct, 0.0..=100.0).suffix(" %").fixed_decimals(0))
                        .on_hover_text(
                            "Il volume di questa uscita rispetto al volume di MultiAudio, impostato sul dispositivo stesso quando ce l'ha (anche le Bluetooth). I tasti del volume e il cursore in alto alzano e abbassano tutte le uscite insieme.",
                        );
                    if r.changed() {
                        o.volume = (pct / 100.0).clamp(0.0, 1.0);
                    }
                    let icon = if o.muted { "🔇" } else { "🔊" };
                    if ui.button(icon).on_hover_text(if o.muted { "Riattiva" } else { "Muto" }).clicked() {
                        o.muted = !o.muted;
                    }
                    if !running {
                        ui.label(RichText::new("si regola con MultiAudio acceso").small().color(DIM));
                    }
                });
            });
            if surround {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Posizione").color(DIM));
                    let (centre, rear) = (o.centre, o.rear && !o.centre);
                    if ui.selectable_label(!centre && !rear, "Davanti").on_hover_text("Stereo normale").clicked() {
                        (o.centre, o.rear) = (false, false);
                    }
                    if ui
                        .selectable_label(centre, "Centro")
                        .on_hover_text(
                            "Cassa centrale (le voci): suona il canale centrale se la sorgente è 5.1, e le voci di una sorgente stereo (si cambia qui sotto, «Con lo stereo»); i bassi che non suona restano davanti.",
                        )
                        .clicked()
                    {
                        (o.centre, o.rear) = (true, false);
                    }
                    if ui
                        .selectable_label(rear, "Dietro")
                        .on_hover_text("Canali surround se la sorgente è 5.1. Con una sorgente stereo decide «Con lo stereo», qui sotto.")
                        .clicked()
                    {
                        (o.centre, o.rear) = (false, true);
                    }
                });
                // Stereo sources (most of what plays): what a centre speaker
                // and the rear ones do with them (`Config::stereo_centre`,
                // `config::StereoRear`). The front always plays them whole.
                if o.centre {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Con lo stereo").color(DIM));
                        if ui
                            .selectable_label(*stereo_centre, "Voci")
                            .on_hover_text("Con una sorgente stereo questa cassa suona quello che sinistra e destra hanno in comune (le voci), insieme alle casse davanti.")
                            .clicked()
                        {
                            *stereo_centre = true;
                        }
                        if ui
                            .selectable_label(!*stereo_centre, "Tace")
                            .on_hover_text("Con una sorgente stereo questa cassa tace: suona solo il canale centrale delle sorgenti 5.1.")
                            .clicked()
                        {
                            *stereo_centre = false;
                        }
                    });
                } else if o.rear {
                    use crate::config::StereoRear;
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Con lo stereo").color(DIM));
                        if ui
                            .selectable_label(*stereo_rear == StereoRear::All, "Tutto")
                            .on_hover_text("Con una sorgente stereo (YouTube, i film nel browser…) queste casse suonano lo stereo com'è, insieme a quelle davanti: come a surround spento. È il suono più pulito.")
                            .clicked()
                        {
                            *stereo_rear = StereoRear::All;
                        }
                        if ui
                            .selectable_label(*stereo_rear == StereoRear::Ambience, "Ambiente")
                            .on_hover_text(format!(
                                "Con una sorgente stereo queste casse suonano solo quello che cambia tra sinistra e destra (ambiente, riverbero, effetti ai lati), {REAR_DELAY_MS:.0} ms dopo il davanti. Le voci, che stanno al centro, da qui non escono. Con l'audio compresso (YouTube, Netflix) in quello che resta c'è anche il fruscio della compressione, e si sente tanto più quanto queste casse sono più forti di quelle davanti."
                            ))
                            .clicked()
                        {
                            *stereo_rear = StereoRear::Ambience;
                        }
                    });
                }
                // How loud it plays with surround on: a rear or centre
                // output louder than the front ones is most of what is heard.
                if o.centre || o.rear {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Livello ").color(DIM));
                        let tip = "Quanto suona questa uscita con il surround acceso (senza surround non conta). Se è più forte delle casse davanti il suono sembra venire da qui. «Calibra col microfono» la pareggia da sola; qui la correggi a orecchio.";
                        // (never above what its volume gives: the sum with
                        // what the calibration found stops at 0 dB)
                        let total = surround_trim_db.unwrap_or(o.surround_db).min(0.0);
                        if ui.add_enabled(total > -crate::engine::MAX_TRIM_DB, egui::Button::new("−3").small()).on_hover_text(tip).clicked() {
                            o.surround_db -= 3.0;
                        }
                        if ui.add_enabled(total < -0.5 || surround_trim_db.is_none(), egui::Button::new("+3").small()).on_hover_text(tip).clicked() {
                            o.surround_db += 3.0;
                        }
                        o.surround_db = o.surround_db.clamp(-crate::engine::MAX_TRIM_DB, crate::engine::MAX_TRIM_DB);
                        let txt = if total < -0.5 { format!("{:.0} dB", total).replace('-', "−") } else { "pieno".to_string() };
                        ui.label(RichText::new(txt).color(if total < -0.5 { TEXT } else { DIM })).on_hover_text(tip);
                        if o.surround_db != 0.0 {
                            let how = format!("a orecchio {}{:.0} dB", if o.surround_db > 0.0 { "+" } else { "−" }, o.surround_db.abs());
                            ui.label(RichText::new(how).small().color(DIM));
                            if ui.small_button("↺").on_hover_text("Toglie la correzione a orecchio").clicked() {
                                o.surround_db = 0.0;
                            }
                        }
                    });
                }
            }
            ui.horizontal_wrapped(|ui| {
                use crate::config::Bass;
                ui.label(RichText::new("Bassi").color(DIM));
                let (centre, rear) = (surround && o.centre, surround && o.rear && !o.centre);
                let now = o.bass_as(rear, centre);
                let rest = "Quello che sta sotto lo suonano le uscite davanti che hanno tutti i bassi.";
                let full = if centre {
                    format!("Suona tutto il canale centrale dei film 5.1; dallo stereo prende il centro da {CENTRE_MIN_HZ:.0} Hz in su. Per una cassa centrale grande.")
                } else if rear {
                    "Suona tutto, e anche i bassi del davanti (allineati con il sub): casse grandi.".to_string()
                } else {
                    "Suona tutto: casse grandi, o con il subwoofer. Prende anche i bassi che le altre uscite non suonano.".to_string()
                };
                for (b, label, tip) in [
                    (Bass::Full, "Tutti", full),
                    (Bass::Less, "Pochi", format!("Suona da circa {:.0} Hz in su: casse medie. {rest}", Bass::Less.hz())),
                    (Bass::Off, "Niente", format!("Suona da circa {:.0} Hz in su: casse piccole (portatile, smart speaker). {rest}", Bass::Off.hz())),
                ] {
                    if ui.selectable_label(now == b, label).on_hover_text(tip).clicked() {
                        o.bass = Some(b);
                    }
                }
            });
        });
        forget
    }

    fn ui_actions(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let can = self.engine.is_some() && !self.calibrating();
            let cal = egui::Button::new(RichText::new("🎯  Calibra col microfono").strong().color(Color32::WHITE))
                .fill(ACCENT)
                .corner_radius(CornerRadius::same(10))
                .min_size(egui::vec2(0.0, 38.0));
            if ui
                .add_enabled(can, cal)
                .on_hover_text("Ogni uscita suona un breve fischio: il microfono misura il ritardo e il volume di ognuna. I ritardi li imposta da solo; con il surround abbassa le uscite dietro e al centro che suonano più forte di quelle davanti.\nMetti il microfono dove ti siedi.")
                .clicked()
            {
                self.start_calibration();
            }
            let click = egui::Button::new(RichText::new("🔊  Click di test"))
                .selected(self.test_click)
                .corner_radius(CornerRadius::same(10))
                .min_size(egui::vec2(0.0, 38.0));
            if ui
                .add_enabled(self.engine.is_some(), click)
                .on_hover_text("Un click al secondo su tutte le uscite: devi sentirne uno solo.")
                .clicked()
            {
                self.test_click = !self.test_click;
            }
            if self.calibrating() {
                ui.spinner();
            }
        });
        if self.engine.is_none() {
            ui.label(RichText::new("Accendi MultiAudio (interruttore in alto) per calibrare.").small().color(DIM));
        } else if !self.calibrating() && self.calib_log.is_empty() {
            ui.label(RichText::new("Prima di calibrare: metti in pausa la musica e alza un po' il volume.").small().color(DIM));
        }
        if !self.calib_log.is_empty() {
            ui.add_space(4.0);
            egui::Frame::new().fill(PANEL).corner_radius(CornerRadius::same(10)).inner_margin(10.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                for l in &self.calib_log {
                    ui.label(RichText::new(l).small().monospace());
                }
            });
        }
    }

    fn ui_advanced(&mut self, ui: &mut egui::Ui, plan: &Plan) {
        ui.add_space(10.0);
        egui::CollapsingHeader::new(RichText::new("⚙  Impostazioni avanzate").color(DIM)).show(ui, |ui| {
            egui::Grid::new("adv").num_columns(2).spacing([16.0_f32, 10.0_f32]).show(ui, |ui| {
                ui.label("Microfono per la calibrazione");
                let mics: Vec<DeviceInfo> = self.devices.inputs.iter().filter(|d| !d.label.contains('⟲')).cloned().collect();
                device_combo(ui, "mic", &mut self.cfg.calib_mic, &mics, "— scegli —");
                ui.end_row();

                ui.label("Uscita per il video");
                let auto = match &plan.main {
                    Some(m) if self.cfg.video_output.is_empty() => format!("Automatica ({})", backend::display_name(m)),
                    _ => "Automatica (la più lenta)".to_string(),
                };
                let current = if self.cfg.video_output.is_empty() { auto.clone() } else { backend::display_name(&self.cfg.video_output) };
                egui::ComboBox::from_id_salt("video").width(260.0).selected_text(current).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.cfg.video_output, String::new(), auto);
                    for o in self.cfg.outputs.iter().filter(|o| o.enabled) {
                        ui.selectable_value(&mut self.cfg.video_output, o.id.clone(), backend::display_name(&o.id));
                    }
                });
                ui.end_row();

                ui.label("Buffer di sicurezza");
                ui.horizontal(|ui| {
                    ui.add(egui::Slider::new(&mut self.cfg.safety_ms, 5.0..=MAX_SAFETY_MS).suffix(" ms").fixed_decimals(0));
                    if let Some(e) = &self.engine {
                        let min = e.safety.min_ms.load();
                        if min > self.cfg.safety_ms {
                            ui.label(RichText::new(format!("(in uso {min:.0} ms)")).small().color(DIM));
                        }
                    }
                });
                ui.end_row();

                ui.label("");
                ui.checkbox(&mut self.cfg.autostart, "Accendi all'apertura dell'app");
                ui.end_row();
                if backend::VOUT_IS_SYSTEM && cfg!(target_os = "macos") && backend::vout_supported() {
                    ui.label("");
                    let before = self.cfg.bt_surround;
                    ui.checkbox(&mut self.cfg.bt_surround, "5.1 vero anche con il video sul Bluetooth").on_hover_text(
                        "Con il surround acceso l'uscita «MultiAudio» a 6 canali c'è anche quando il video segue le casse Bluetooth: sono loro a darle il tempo, così i player ritardano il video come sempre.\nSpento: l'uscita di sistema restano le casse Bluetooth e il surround è ricavato dallo stereo.",
                    );
                    if self.cfg.bt_surround != before {
                        crate::diag::log(if before { "5.1 vero con il video sul Bluetooth: spento" } else { "5.1 vero con il video sul Bluetooth: acceso" });
                    }
                    ui.end_row();
                }
                ui.label("");
                let before = self.cfg.match_levels;
                ui.checkbox(&mut self.cfg.match_levels, "Pareggia il volume delle casse (surround)").on_hover_text(
                    "Con il surround acceso le uscite dietro e al centro che al microfono della calibrazione suonavano più forte di quelle davanti vengono abbassate al loro livello, così il suono resta davanti.\nServe aver fatto «Calibra col microfono» con tutte le uscite accese. Spento: conta solo il «Livello» messo a orecchio sulle schede.",
                );
                if self.cfg.match_levels != before {
                    crate::diag::log(if before { "pareggio del volume delle casse: spento" } else { "pareggio del volume delle casse: acceso" });
                }
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.show_virtual, "Mostra dispositivi virtuali");
                ui.end_row();
                ui.label("");
                ui.horizontal(|ui| {
                    if ui.button("⟳ Aggiorna dispositivi").clicked() {
                        crate::diag::log("aggiornamento dispositivi chiesto dall'utente");
                        if self.want_running {
                            self.start_engine();
                        } else {
                            self.rescan();
                        }
                    }
                    if ui.button("📄 Apri registro").on_hover_text("Il registro diagnostico: utile se qualcosa si desincronizza").clicked() {
                        backend::reveal(&crate::diag::path());
                    }
                    let rec = egui::Button::new(format!("⏺ Registra {:.0} s", record::SECS));
                    if ui
                        .add_enabled(self.engine.is_some() && self.diag.is_none(), rec)
                        .on_hover_text("Per capire un suono che non va: salva pochi secondi di quello che MultiAudio riceve dal computer e di quello che manda a ogni uscita, in file audio accanto al registro. Premilo mentre suona quello che si sente male. Ogni registrazione sostituisce la precedente; non esce dal computer.")
                        .clicked()
                    {
                        self.start_diagnosis();
                    }
                });
                ui.end_row();
                if !self.diag_msg.is_empty() {
                    ui.label("");
                    ui.label(RichText::new(&self.diag_msg).small().color(DIM));
                    ui.end_row();
                }
            });
            ui.add_space(6.0);
            ui.label(RichText::new(HELP).small().color(DIM));
            if !self.log.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new("Messaggi").small().strong().color(DIM));
                for l in self.log.iter().rev() {
                    ui.label(RichText::new(l).small().color(DIM));
                }
            }
        });
    }

    fn ui_footer(&mut self, ui: &mut egui::Ui, plan: &Plan) {
        if let Some((text, since)) = &self.notice {
            if since.elapsed() < Duration::from_secs(12) {
                ui.label(RichText::new(format!("⚠ {text}")).small().color(WARN));
            } else {
                self.notice = None;
            }
        }
        ui.horizontal_wrapped(|ui| {
            let sys = self.system_output.clone().unwrap_or_default();
            let sys_name = if sys.is_empty() { "?".to_string() } else { backend::display_name(&sys) };
            ui.label(RichText::new(format!("🔈 Uscita di sistema: {sys_name}")).small().color(DIM));
            if let Some(wanted) = &plan.system_output {
                if self.engine.is_some() && backend::physical_key(wanted) != backend::physical_key(&sys) {
                    let why = if backend::is_own_device(wanted) { "tutto l'audio deve passare da lì" } else { "per sincronizzare il video" };
                    ui.label(RichText::new(format!("⚠ dovrebbe essere «{}»: {why}", backend::display_name(wanted))).small().color(WARN));
                    if ui.small_button("Imposta").clicked() {
                        match backend::set_default_output(wanted) {
                            Ok(()) => self.system_output = Some(wanted.clone()),
                            Err(e) => self.push_log(format!("Uscita di sistema: {e}")),
                        }
                    }
                } else if self.engine.is_some() && volume::active() {
                    #[cfg(target_os = "macos")]
                    let keys = crate::mackeys::running();
                    #[cfg(not(target_os = "macos"))]
                    let keys = false;
                    let txt = if keys {
                        "· volume: tasti del computer o cursore in alto; quello di ogni uscita nella sua scheda"
                    } else {
                        "· volume: cursore in alto; quello di ogni uscita nella sua scheda"
                    };
                    ui.label(RichText::new(txt).small().color(DIM));
                } else if self.engine.is_some() {
                    ui.label(RichText::new("· volume: tasti del computer per tutte; quello di ogni uscita nella sua scheda").small().color(DIM));
                }
            }
            if self.engine.is_some() && self.last_signal.elapsed() > Duration::from_secs(10) {
                ui.label(RichText::new("· nessun suono catturato: fai partire qualcosa").small().color(DIM));
            }
        });
    }
}

impl App {
    /// The work of a frame before drawing it (Linux: also done with the
    /// window closed, MultiAudio in the tray). Returns the plan to draw.
    fn before_frame(&mut self) -> Plan {
        let plan = self.plan();
        self.housekeeping(&plan);
        if volume::active() {
            let (l, _, _) = volume::get();
            if self.cfg.volume.map(|v| (v - l).abs() > 1e-4).unwrap_or(true) {
                self.cfg.volume = Some(l);
            }
        }
        plan::fill_auto_delays(&mut self.cfg, &self.devices);
        let plan = self.plan();

        // level of the captured system audio
        let mut p = 0f32;
        if let Some(e) = &self.engine {
            for inp in &e.inputs {
                p = p.max(inp.peak.swap(0.0));
            }
        }
        if p > 1e-4 {
            self.last_signal = Instant::now();
        }
        self.input_meter = (self.input_meter * 0.85).max(level(p));
        plan
    }

    /// The work of a frame after drawing it.
    fn after_frame(&mut self, plan: &Plan) {
        self.push_live_params(plan);
        self.autosave();
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let plan = self.before_frame();

        egui::TopBottomPanel::top("header")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(20, 14)))
            .show(ctx, |ui| self.ui_header(ui, &plan));
        egui::TopBottomPanel::bottom("footer")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(20, 8)))
            .show(ctx, |ui| self.ui_footer(ui, &plan));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(20, 10)))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.ui_outputs(ui, &plan);
                    self.ui_actions(ui);
                    self.ui_advanced(ui, &plan);
                    ui.add_space(10.0);
                });
            });
        self.after_frame(&plan);
        ctx.request_repaint_after(Duration::from_millis(60));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        crate::diag::log("chiusura dell'app");
        self.shutdown();
    }
}

/// MultiAudio's volume (all outputs, each times its own).
fn ui_volume(ui: &mut egui::Ui) {
    let (mut level, muted, _) = volume::get();
    let pct = if muted { "muto".to_string() } else { format!("{:.0} %", level * 100.0) };
    ui.add_sized([44.0, 20.0], egui::Label::new(RichText::new(pct).small().color(DIM)));
    ui.scope(|ui| {
        ui.spacing_mut().slider_width = 150.0;
        let r = ui.add(egui::Slider::new(&mut level, 0.0..=1.0).show_value(false));
        if r.changed() {
            volume::set(level, false);
        }
    });
    let icon = if muted || level <= 0.0 { "🔇" } else if level < 0.5 { "🔈" } else { "🔊" };
    if ui
        .add(egui::Button::new(RichText::new(icon).size(16.0)).frame(false))
        .on_hover_text(if muted { "Riattiva" } else { "Muto" })
        .clicked()
    {
        volume::set(level, !muted);
    }
}

impl App {
    /// Stops everything; MultiAudio's output goes away and the previous
    /// system output comes back.
    fn shutdown(&mut self) {
        self.remove_vout();
        if self.cfg != self.saved {
            let _ = self.cfg.save();
            self.saved = self.cfg.clone();
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.remove_vout();
        if self.cfg != self.saved {
            let _ = self.cfg.save();
        }
    }
}

// ---------------------------------------------------------------------------

fn device_combo(ui: &mut egui::Ui, id_salt: impl std::hash::Hash, value: &mut String, list: &[DeviceInfo], empty: &str) {
    let current = list.iter().find(|d| &d.id == value).map(|d| d.label.clone()).unwrap_or_else(|| {
        if value.is_empty() {
            empty.to_string()
        } else {
            format!("⚠ {} (non collegato)", backend::display_name(value))
        }
    });
    egui::ComboBox::from_id_salt(id_salt).width(260.0).selected_text(current).show_ui(ui, |ui| {
        for d in list {
            ui.selectable_value(value, d.id.clone(), &d.label);
        }
    });
}

fn guess_mic(devs: &DeviceList) -> String {
    let skip = ["⟲", "BlackHole", "CABLE", "Soundflower", "Monitor"];
    devs.inputs.iter().find(|d| !skip.iter().any(|k| d.label.contains(k))).map(|d| d.id.clone()).unwrap_or_default()
}

#[cfg(target_os = "macos")]
const HELP: &str = "\
Come funziona: quando è acceso, MultiAudio crea la sua uscita audio «MultiAudio» e la fa diventare l'uscita di sistema: tutto quello che suona il computer (anche il 5.1 dei film) arriva lì, e MultiAudio lo manda alle uscite accese, ognuna col suo ritardo. Nessun driver da installare; quando lo spegni torna l'uscita di prima.
L'uscita «MultiAudio» dichiara la latenza dell'uscita più lenta: i player ritardano il video da soli, anche Netflix & co. Se il video va su AirPlay (o su Bluetooth senza surround) l'uscita di sistema resta quella: i player leggono la sua latenza, e MultiAudio prende l'audio del computer da sé.
I ritardi partono stimati dalla latenza dichiarata dai dispositivi («auto»); «Calibra col microfono» li misura davvero, e misura anche quanto suona forte ogni uscita (serve al surround).
Il volume: i tasti del computer alzano e abbassano tutte le uscite insieme; il cursore Volume di ogni uscita la regola rispetto alle altre. MultiAudio muove il volume dei dispositivi che lo hanno e regola da sé gli altri.";

#[cfg(target_os = "linux")]
const HELP: &str = "\
Come funziona: quando è acceso, MultiAudio fa diventare «MultiAudio» l'uscita di sistema: tutto quello che suona il computer (anche il 5.1 dei film) arriva lì, e MultiAudio lo manda alle uscite accese, ognuna col suo ritardo e la sua posizione. Quando lo spegni torna l'uscita di prima.
L'uscita «MultiAudio» dichiara la latenza dell'uscita più lenta: i player ritardano il video da soli. Serve il modulo del kernel snd-aloop (vedi README): senza, l'uscita c'è lo stesso ma non dichiara la latenza e il video può andare avanti all'audio.
I ritardi partono stimati («auto»: il Bluetooth dal ritardo che dichiara la cassa, le uscite col filo circa 20 ms); «Calibra col microfono» li misura davvero, e misura anche quanto suona forte ogni uscita (serve al surround).
Il volume: i tasti del computer alzano e abbassano tutte le uscite insieme; il cursore Volume di ogni uscita la regola rispetto alle altre.";

#[cfg(target_os = "windows")]
const HELP: &str = "\
Come funziona: MultiAudio cattura da sé l'audio del computer (nessun driver da installare) e lo manda alle uscite accese, ognuna col suo ritardo.
L'uscita più lenta (di solito Bluetooth o AirPlay) diventa l'uscita di sistema: i player leggono la sua latenza e ritardano il video da soli, anche Netflix & co.
I ritardi partono stimati dalla latenza dichiarata dai dispositivi («auto»); «Calibra col microfono» li misura davvero, e misura anche quanto suona forte ogni uscita (serve al surround).
Il volume: i tasti del computer alzano e abbassano tutte le uscite insieme; il cursore Volume di ogni uscita la regola rispetto alle altre.";
