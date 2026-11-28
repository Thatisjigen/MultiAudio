#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod backend;
mod calibrate;
mod config;
mod diag;
mod dsp;
mod engine;
mod gui;
#[cfg(target_os = "macos")]
mod mactap;
#[cfg(target_os = "macos")]
mod machud;
#[cfg(target_os = "macos")]
mod mackeys;
mod plan;
mod record;
mod volume;
#[cfg(target_os = "windows")]
mod winvol;

use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    dsp::now_ns(); // start the shared clock
    diag::init();
    // A crash must not leave the system output on a device nobody feeds.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        backend::emergency_restore();
        default_hook(info);
    }));
    // …nor an exit that skips the normal clean-up (e.g. ⌘Q on macOS)
    #[cfg(unix)]
    {
        extern "C" fn at_exit() {
            backend::emergency_restore();
        }
        extern "C" {
            fn atexit(cb: extern "C" fn()) -> std::ffi::c_int;
        }
        unsafe {
            atexit(at_exit);
        }
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--list") => list(),
        Some("--headless") => headless(),
        Some("--test-tones") => {
            // diagnostic: multiaudio --test-tones <device id> [seconds]
            let id = args.get(1).cloned().unwrap_or_default();
            let secs = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4.0);
            backend::play_test_tones(&id, secs)
        }
        Some("--cleanup") => {
            // after a crash: remove MultiAudio's output, give the system output back
            let cfg = config::Config::load();
            let devs = backend::list_devices();
            let main = plan::main_output(&cfg, &devs);
            backend::cleanup_vout();
            std::thread::sleep(Duration::from_millis(300));
            let current = backend::default_output_id();
            if current.as_deref().map(backend::is_own_device).unwrap_or(true) || current.as_deref() != main.as_deref() {
                if let Some(m) = &main {
                    match backend::set_default_output(m) {
                        Ok(()) => println!("Uscita di sistema: {}", backend::display_name(m)),
                        Err(e) => eprintln!("! {e}"),
                    }
                }
            }
            Ok(())
        }
        Some("--config-path") => {
            println!("{}", config::Config::path().display());
            Ok(())
        }
        Some("--help") | Some("-h") => {
            println!(
                "MultiAudio {}\n\n  multiaudio              interfaccia grafica\n  multiaudio --list       elenca i dispositivi (id da usare nel config)\n  multiaudio --headless   esegue il config salvato senza interfaccia\n  multiaudio --config-path  mostra dove si trova il config\n  multiaudio --cleanup    dopo un crash: toglie l'uscita MultiAudio e rimette quella di prima\n\nVariabile MULTIAUDIO_CONFIG per usare un altro file di configurazione.",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        _ => {
            #[cfg(unix)]
            {
                stop_on_signals();
                // the window may be closed or busy: act from a thread
                std::thread::spawn(|| loop {
                    if STOP.load(Relaxed) {
                        backend::emergency_restore();
                        std::process::exit(0);
                    }
                    std::thread::sleep(Duration::from_millis(200));
                });
            }
            gui::run()
        }
    }
}

fn list() -> anyhow::Result<()> {
    use std::io::Write;
    let d = backend::list_devices();
    let mut out = std::io::stdout().lock();
    // Ignore write errors (e.g. output piped into `head`).
    let _ = writeln!(out, "INGRESSI:");
    for i in &d.inputs {
        let _ = writeln!(out, "  {:<50} {}ch  id={}", i.label, i.channels, i.id);
    }
    let _ = writeln!(out, "USCITE:");
    for o in &d.outputs {
        let lat = o.latency_ms.map(|l| format!("  latenza dichiarata {l:.0} ms")).unwrap_or_default();
        let _ = writeln!(out, "  {:<40} {:<10} {}ch  id={}{lat}", o.label, o.kind.label(), o.channels, o.id);
    }
    if let Some(def) = backend::default_output_id() {
        let _ = writeln!(out, "USCITA DI SISTEMA: {}", backend::display_name(&def));
    }
    // MULTIAUDIO_DEBUG: who is playing, how each output declares its latency
    #[cfg(target_os = "macos")]
    if std::env::var_os("MULTIAUDIO_DEBUG").is_some() {
        let _ = writeln!(out, "SUONANO ORA (pid): {:?}", backend::playing_processes());
        let mut ids: Vec<String> = d.outputs.iter().map(|o| o.id.clone()).collect();
        ids.push(backend::vout_output_id());
        for id in ids {
            let Some(uid) = id.strip_prefix("cpal:coreaudio:") else { continue };
            if let Some((rate, lat, safety, buf, streams)) = mactap::latency_parts(uid) {
                let ms = |f: u32| f as f64 / rate * 1000.0;
                let _ = writeln!(
                    out,
                    "LATENZA {}: dispositivo {:.1} ms, sicurezza {:.1} ms, buffer {:.1} ms, flussi {:?} ms ({rate:.0} Hz)",
                    backend::display_name(&id),
                    ms(lat),
                    ms(safety),
                    ms(buf),
                    streams.iter().map(|s| (ms(*s) * 10.0).round() / 10.0).collect::<Vec<_>>()
                );
            }
        }
    }
    if let Some(v) = backend::system_volume() {
        let db = if v.gain > 0.0 { 20.0 * v.gain.log10() } else { f32::NEG_INFINITY };
        let _ = writeln!(out, "VOLUME DI SISTEMA: {db:.1} dB{}", if v.muted { " (muto)" } else { "" });
    }
    let perm = match backend::capture_permission() {
        backend::CapturePermission::Granted => "concesso",
        backend::CapturePermission::Denied => "negato",
        backend::CapturePermission::NotAsked => "non ancora chiesto",
        backend::CapturePermission::NotApplicable => "",
    };
    if !perm.is_empty() {
        let _ = writeln!(out, "PERMESSO REGISTRAZIONE AUDIO DI SISTEMA: {perm}");
    }
    Ok(())
}

/// Set by SIGINT / SIGTERM: stop cleanly (the system output must come back).
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
fn stop_on_signals() {
    extern "C" fn on_signal(_: std::ffi::c_int) {
        STOP.store(true, Relaxed);
    }
    extern "C" {
        fn signal(sig: std::ffi::c_int, handler: extern "C" fn(std::ffi::c_int)) -> usize;
    }
    unsafe {
        signal(2, on_signal); // SIGINT
        signal(15, on_signal); // SIGTERM
    }
}

/// MultiAudio's volume when it starts: the last one used (every output
/// lines up with it), else that of the system output.
fn starting_volume(cfg: &config::Config) -> f32 {
    cfg.volume.or_else(backend::system_volume_slider).unwrap_or(0.5)
}

fn headless() -> anyhow::Result<()> {
    let mut cfg = config::Config::load();
    println!("Config: {}", config::Config::path().display());
    // for tests: leave the system output alone (no "MultiAudio" output)
    let keep_output = std::env::var_os("MULTIAUDIO_KEEP_SYSTEM_OUTPUT").is_some();
    let print_status = |p: &plan::Plan, e: &engine::Engine| {
        if let Some(m) = &p.main {
            println!("  video su: {}{}", backend::display_name(m), if p.main_by_system { " (suonata dal sistema)" } else { "" });
        }
        if cfg!(target_os = "macos") {
            let (l, m, _) = volume::get();
            println!("  volume MultiAudio: {:.0} %{}", l * 100.0, if m { " (muto)" } else { "" });
        }
        for inp in &e.inputs {
            let pk = inp.peak.swap(0.0);
            let db = if pk > 0.0 { 20.0 * pk.log10() } else { f32::NEG_INFINITY };
            let per_ch: Vec<String> = inp
                .ch_peak
                .iter()
                .map(|c| {
                    let v = c.swap(0.0);
                    if v > 0.0 { format!("{:.0}", 20.0 * v.log10()) } else { "-".into() }
                })
                .collect();
            println!(
                "  ingresso {}: picco {:.1} dBFS (canali: {}), in ritardo {}, buchi {}",
                backend::display_name(&inp.id),
                db,
                per_ch.join(" "),
                inp.late.count.load(Relaxed),
                inp.holes.count.load(Relaxed)
            );
        }
        for (i, r) in p.engine_cfg.routes.iter().enumerate() {
            let extra = e.route_ctl[i]
                .as_ref()
                .map(|c| {
                    let pk = c.st_peak.swap(0.0);
                    format!(
                        " [{:?}{}] picco {} dBFS, buffer {:.1} ms, deriva {:+.0} ppm, interruzioni {}, chiamate in ritardo {}, volume sistema {:.2}",
                        c.state(),
                        if c.st_discrete.load(Relaxed) { ", 5.1 vero" } else { "" },
                        if pk > 0.0 { format!("{:.0}", 20.0 * pk.log10()) } else { "-".into() },
                        c.st_buffer_ms.load(),
                        c.st_ppm.load(),
                        c.st_underruns.load(Relaxed),
                        c.st_late.count.load(Relaxed),
                        c.sys_gain.load()
                    )
                })
                .unwrap_or_default();
            println!("  {} ({} ms): {:?}{}", r.name, r.delay_ms, e.status[i], extra);
        }
    };
    if backend::capture_permission() == backend::CapturePermission::NotAsked {
        println!("Chiedo il permesso di registrare l'audio di sistema: rispondi alla finestra di macOS…");
        let (tx, rx) = std::sync::mpsc::channel();
        if backend::request_capture_permission(move |ok| {
            let _ = tx.send(ok);
        }) {
            match rx.recv_timeout(Duration::from_secs(120)) {
                Ok(true) => println!("Permesso concesso."),
                Ok(false) => println!("Permesso negato: le uscite resteranno mute."),
                Err(_) => println!("Nessuna risposta: continuo comunque."),
            }
        }
    }
    backend::cleanup_vout();
    #[cfg(unix)]
    stop_on_signals();

    // (the window is where this is put right: here it is only respected)
    let bt_clock_tripped = backend::bt_clock_tripped();
    if bt_clock_tripped {
        eprintln!("! l'ultimo avvio dell'uscita MultiAudio con il clock Bluetooth non è arrivato in fondo: niente 5.1 vero col Bluetooth");
    }
    let mut devices = backend::list_devices();
    let mut vout: Option<backend::VirtualOut> = None;
    let mut vout_key: Vec<String> = Vec::new();
    let mut volume_set = false;
    let mut safety_seen = cfg.safety_ms;
    let mut rebuild_ticks = 0;
    let mut eng: Option<engine::Engine> = None;
    let mut p = plan::make_plan(&cfg, &devices, false);
    let mut restart = true;
    let mut tick = 0u64;
    let verbose = std::env::var_os("MULTIAUDIO_VERBOSE").is_some();
    #[cfg(target_os = "macos")]
    let mut keys_hint = false;
    while !STOP.load(Relaxed) {
        if restart {
            restart = false;
            if eng.take().is_some() {
                std::thread::sleep(Duration::from_millis(500));
            }
            devices = backend::list_devices();
            plan::fill_auto_delays(&mut cfg, &devices);
            // MultiAudio's own output (macOS: the system output)
            let base = plan::make_plan(&cfg, &devices, false);
            if !volume_set {
                volume::set(starting_volume(&cfg), false);
                volume_set = true;
            }
            let want = !keep_output && backend::vout_supported() && (backend::VOUT_IS_SYSTEM || cfg.surround) && base.main.is_some();
            let enabled: Vec<String> = cfg.outputs.iter().filter(|o| o.enabled).map(|o| o.id.clone()).collect();
            let key = if want { backend::vout_members(&devices, &enabled, base.main.as_deref(), cfg.surround && cfg.bt_surround && !bt_clock_tripped) } else { Vec::new() };
            if key.is_empty() {
                vout = None;
                vout_key.clear();
            } else if vout.is_none() || key != vout_key {
                vout = None;
                let lat = plan::vout_latency_ms(&cfg, &devices, base.main.as_deref(), safety_seen);
                match backend::create_vout(&devices, &key, lat, base.main.as_deref()) {
                    Ok(h) => {
                        vout = Some(h);
                        vout_key = key;
                        devices = backend::list_devices();
                    }
                    Err(e) => eprintln!("! uscita MultiAudio: {e}"),
                }
            }
            p = plan::make_plan(&cfg, &devices, vout.is_some());
            if let Some(problem) = &p.problem {
                eprintln!("! {problem}");
            }
            if let Some((declared, _)) = vout.as_ref().and_then(|v| v.declared_ms()) {
                p.engine_cfg.anchor_ms = Some(plan::anchor_ms(&cfg, &devices, p.main.as_deref(), declared));
            }
            if let (Some(sys), false) = (&p.system_output, keep_output) {
                let current = backend::default_output_id().map(|d| backend::physical_key(&d));
                if current != Some(backend::physical_key(sys)) {
                    if let Err(e) = backend::set_default_output(sys) {
                        eprintln!("! imposta «{}» come uscita di sistema ({e})", backend::display_name(sys));
                    }
                }
            }
            let e = engine::Engine::start(&p.engine_cfg, &devices, std::env::var_os("MULTIAUDIO_TEST_CLICK").is_some());
            println!("Avviato:");
            print_status(&p, &e);
            // MULTIAUDIO_RECORD: a diagnosis of the first sound (see `record`)
            if std::env::var_os("MULTIAUDIO_RECORD").is_some() {
                let routes = p.engine_cfg.routes.clone();
                let tracks = record::tracks(&e, |i| (routes[i].name.clone(), String::new()));
                let inputs = e.inputs.clone();
                let about = format!("senza finestra, surround {}", cfg.surround);
                std::thread::spawn(move || {
                    let t0 = std::time::Instant::now();
                    while t0.elapsed() < Duration::from_secs(120) && !inputs.iter().any(|i| i.loud_within(300_000_000)) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    std::thread::sleep(Duration::from_secs(2));
                    match record::start(tracks, about).recv() {
                        Ok(Ok(dir)) => println!("Diagnosi registrata in {}", dir.display()),
                        Ok(Err(e)) => eprintln!("! diagnosi: {e}"),
                        Err(_) => eprintln!("! diagnosi: non è partita"),
                    }
                });
            }
            eng = Some(e);
        }
        for _ in 0..20 {
            if STOP.load(Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if STOP.load(Relaxed) {
            break;
        }
        tick += 1;
        let Some(e) = &eng else { continue };
        if let Ok(mut v) = e.errors.lock() {
            for m in v.drain(..) {
                eprintln!("! {m}");
            }
        }
        if let (Some(sys), false) = (&p.system_output, keep_output) {
            if let Some(was) = backend::guard_system_output(sys) {
                println!("L'uscita di sistema era «{was}»: rimessa «{}».", backend::display_name(sys));
            }
        }
        #[cfg(target_os = "macos")]
        if !mackeys::running() {
            if mackeys::trusted() {
                mackeys::start();
            } else if !keys_hint {
                keys_hint = true;
                println!("Per i tasti del volume: consenti questa app in Impostazioni di Sistema → Privacy e sicurezza → Accessibilità.");
            }
        }
        restart = e.failed.load(Relaxed);
        // the latency MultiAudio's output declares must still fit
        safety_seen = safety_seen.max(e.safety.effective_ms());
        if let Some((declared, floor)) = vout.as_ref().and_then(|v| v.declared_ms()) {
            if plan::vout_needs_rebuild(&cfg, &devices, p.main.as_deref(), e.safety.effective_ms(), declared, floor) {
                rebuild_ticks += 1;
                if rebuild_ticks >= 2 {
                    println!("La latenza da dichiarare è cambiata: rifaccio l'uscita MultiAudio.");
                    vout_key.clear();
                    rebuild_ticks = 0;
                    restart = true;
                }
            } else {
                rebuild_ticks = 0;
            }
        }
        if e.needs_devices() {
            let fresh = backend::list_devices();
            if fresh.all_ids() != devices.all_ids() {
                restart = true;
            }
            devices = fresh;
        }
        if restart {
            println!("Riavvio…");
        } else if verbose || tick.is_multiple_of(5) {
            print_status(&p, e);
        }
    }
    println!("Chiudo.");
    drop(eng);
    drop(vout); // gives the system output back
    Ok(())
}
