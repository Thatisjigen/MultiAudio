//! Linux: MultiAudio in the system tray (StatusNotifierItem, like udiskie or
//! nm-applet), with a menu to switch it on and off, pick the outputs, open
//! the window and start it at login. The window can be closed: MultiAudio
//! keeps playing until "Esci".
//!
//! A second launch (e.g. from the app menu while it runs at login) does not
//! start a second MultiAudio: it asks the running one to open its window.

use anyhow::{anyhow, Result};
use ksni::blocking::TrayMethods;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};

/// What the tray (or a second launch) asks the app to do.
pub enum Cmd {
    Open,
    Quit,
    SetRunning(bool),
    ToggleOutput(String),
    SetAutostart(bool),
}

/// What the tray shows; the app sends a new one when something changes.
#[derive(Clone, Default, PartialEq)]
pub struct Snapshot {
    /// The power switch.
    pub running: bool,
    pub outputs: Vec<OutputItem>,
    pub autostart: bool,
}

#[derive(Clone, PartialEq)]
pub struct OutputItem {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub connected: bool,
}

struct Sni {
    snap: Snapshot,
    tx: Sender<Cmd>,
    icon_on: Vec<ksni::Icon>,
    icon_off: Vec<ksni::Icon>,
}

impl Sni {
    fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }
}

impl ksni::Tray for Sni {
    fn id(&self) -> String {
        "multiaudio".into()
    }
    fn title(&self) -> String {
        "MultiAudio".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        if self.snap.running {
            self.icon_on.clone()
        } else {
            self.icon_off.clone()
        }
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        let on = self.snap.outputs.iter().filter(|o| o.enabled && o.connected).count();
        let description = if self.snap.running {
            format!("Acceso · {on} {}", if on == 1 { "uscita" } else { "uscite" })
        } else {
            "Spento".into()
        };
        ksni::ToolTip { title: "MultiAudio".into(), description, ..Default::default() }
    }
    /// Left click: the window.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Cmd::Open);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let running = self.snap.running;
        let mut items: Vec<ksni::MenuItem<Self>> = vec![
            CheckmarkItem {
                label: "MultiAudio acceso".into(),
                checked: running,
                activate: Box::new(move |t: &mut Self| t.send(Cmd::SetRunning(!running))),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem { label: "Uscite".into(), enabled: false, ..Default::default() }.into(),
        ];
        for o in &self.snap.outputs {
            let id = o.id.clone();
            let label = if o.connected { o.name.clone() } else { format!("{} (non collegata)", o.name) };
            items.push(
                CheckmarkItem {
                    label,
                    checked: o.enabled,
                    enabled: o.connected,
                    activate: Box::new(move |t: &mut Self| t.send(Cmd::ToggleOutput(id.clone()))),
                    ..Default::default()
                }
                .into(),
            );
        }
        let autostart = self.snap.autostart;
        items.extend([
            MenuItem::Separator,
            StandardItem {
                label: "Apri MultiAudio".into(),
                activate: Box::new(|t: &mut Self| t.send(Cmd::Open)),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Avvia all'accesso".into(),
                checked: autostart,
                activate: Box::new(move |t: &mut Self| t.send(Cmd::SetAutostart(!autostart))),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Esci".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| t.send(Cmd::Quit)),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }
}

pub struct Tray {
    handle: ksni::blocking::Handle<Sni>,
    rx: Receiver<Cmd>,
    last: Snapshot,
}

impl Tray {
    /// Puts MultiAudio in the tray. `None` when there is no tray to show it
    /// in (then closing the window quits, as before). `wait_for_host`: at
    /// login the bar may come up after MultiAudio, so wait for it.
    pub fn spawn(wait_for_host: bool) -> Option<Tray> {
        let (tx, rx) = channel();
        let (icon_on, icon_off) = icons();
        let sni = Sni { snap: Snapshot::default(), tx: tx.clone(), icon_on, icon_off };
        let handle = match sni.assume_sni_available(wait_for_host).spawn() {
            Ok(h) => h,
            Err(e) => {
                crate::diag::log(format!("barra di sistema non disponibile: {e}"));
                return None;
            }
        };
        listen(tx);
        crate::diag::log("icona nella barra di sistema");
        Some(Tray { handle, rx, last: Snapshot::default() })
    }

    pub fn try_recv(&self) -> Option<Cmd> {
        self.rx.try_recv().ok()
    }

    /// Quitting: a launch from now on must start a new MultiAudio, not ask
    /// this one (which takes a few seconds to give the system output back)
    /// to open its window. On 2026-11-28 a launch right after «Esci» did
    /// that and left nothing running.
    pub fn release(&self) {
        let _ = std::fs::remove_file(socket_path());
    }

    /// Shows `snap` (only when something changed).
    pub fn show(&mut self, snap: Snapshot) {
        if snap != self.last {
            self.last = snap.clone();
            self.handle.update(move |t| t.snap = snap);
        }
    }
}

// ---------------------------------------------------------------------------
// One MultiAudio at a time
// ---------------------------------------------------------------------------

fn socket_path() -> PathBuf {
    dirs::runtime_dir().unwrap_or_else(std::env::temp_dir).join("multiaudio.sock")
}

/// If MultiAudio is already running, asks it to open its window. Returns
/// whether it did (then this launch has nothing left to do).
pub fn open_running_instance() -> bool {
    match UnixStream::connect(socket_path()) {
        Ok(mut s) => s.write_all(b"open\n").is_ok(),
        Err(_) => false,
    }
}

/// Listens for later launches.
fn listen(tx: Sender<Cmd>) {
    let path = socket_path();
    let _ = std::fs::remove_file(&path); // left over by a crash
    let Ok(listener) = UnixListener::bind(&path) else { return };
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut msg = String::new();
            let _ = (&stream).take(64).read_to_string(&mut msg);
            if msg.trim() == "open" && tx.send(Cmd::Open).is_err() {
                break;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Start at login (XDG autostart: niri, GNOME, KDE, …)
// ---------------------------------------------------------------------------

fn autostart_file() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("autostart").join("multiaudio.desktop"))
}

pub fn autostart_enabled() -> bool {
    autostart_file().map(|p| p.exists()).unwrap_or(false)
}

pub fn set_autostart(on: bool) -> Result<()> {
    let path = autostart_file().ok_or_else(|| anyhow!("cartella di configurazione sconosciuta"))?;
    if on {
        let exe = std::env::current_exe()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(
            &path,
            format!(
                "[Desktop Entry]\nType=Application\nName=MultiAudio\nComment=Audio del computer su più uscite\nExec=\"{}\" --tray\nIcon=multiaudio\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
                exe.display()
            ),
        )?;
        crate::diag::log("avvio all'accesso: acceso");
    } else if path.exists() {
        std::fs::remove_file(&path)?;
        crate::diag::log("avvio all'accesso: spento");
    }
    Ok(())
}

/// A desktop notification (best effort).
pub fn notify(title: &str, body: &str) {
    let _ = std::process::Command::new("notify-send").args(["-a", "MultiAudio", title, body]).spawn();
}

// ---------------------------------------------------------------------------
// Icon
// ---------------------------------------------------------------------------

/// The app icon at 64 and 32 px, in colour (on) and grey (off).
fn icons() -> (Vec<ksni::Icon>, Vec<ksni::Icon>) {
    let Ok(img) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")) else {
        return (Vec::new(), Vec::new());
    };
    let (w, h) = (img.width as usize, img.height as usize);
    let mut on = Vec::new();
    let mut off = Vec::new();
    for size in [64usize, 32] {
        if w < size || h < size || w % size != 0 || h % size != 0 {
            continue;
        }
        let (fx, fy) = (w / size, h / size);
        let mut colour = Vec::with_capacity(size * size * 4);
        let mut grey = Vec::with_capacity(size * size * 4);
        for y in 0..size {
            for x in 0..size {
                // average of the source block, weighted by alpha
                let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
                for sy in 0..fy {
                    for sx in 0..fx {
                        let i = ((y * fy + sy) * w + x * fx + sx) * 4;
                        let pa = img.rgba[i + 3] as u32;
                        r += img.rgba[i] as u32 * pa;
                        g += img.rgba[i + 1] as u32 * pa;
                        b += img.rgba[i + 2] as u32 * pa;
                        a += pa;
                    }
                }
                let n = (fx * fy) as u32;
                let (r, g, b) = if a > 0 { (r / a, g / a, b / a) } else { (0, 0, 0) };
                let a = a / n;
                colour.extend([a as u8, r as u8, g as u8, b as u8]); // ARGB, network byte order
                // off: grey, still easy to see on a dark bar
                let l = ((r * 30 + g * 59 + b * 11) / 100).max(96);
                grey.extend([a as u8, l as u8, l as u8, l as u8]);
            }
        }
        on.push(ksni::Icon { width: size as i32, height: size as i32, data: colour });
        off.push(ksni::Icon { width: size as i32, height: size as i32, data: grey });
    }
    (on, off)
}
