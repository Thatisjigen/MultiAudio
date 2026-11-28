//! Device enumeration and stream opening.
//!
//! Device ids:
//! * `cpal:<device id>`     – CoreAudio (macOS), WASAPI (Windows), ALSA (Linux);
//!   the id is the system's stable device id (survives renames / language changes)
//! * `loopback:<device id>` – Windows only: records what an output device is playing
//! * `tap:system`           – macOS only: all system audio (every app except
//!   MultiAudio), muted on its normal output so it only plays through MultiAudio
//! * `tap:dev:<uid>`        – macOS only: what is being played on that output
//! * `tap:out`              – macOS only: what apps play to the "MultiAudio"
//!   output (6 channels), muted there
//! * `pulse:<name>`     – Linux only: PipeWire / PulseAudio sink or source
//!   (sources include "Monitor of …" = what a sink is playing)
//!
//! No virtual audio driver is ever needed: system audio is captured with
//! Core Audio taps (macOS), WASAPI loopback (Windows) or sink monitors (Linux).

use anyhow::{anyhow, bail, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DevKind {
    Bluetooth,
    AirPlay,
    Screen,
    BuiltIn,
    Usb,
    Virtual,
    #[default]
    Other,
}

impl DevKind {
    pub fn label(self) -> &'static str {
        match self {
            DevKind::Bluetooth => "Bluetooth",
            DevKind::AirPlay => "AirPlay",
            DevKind::Screen => "Schermo",
            DevKind::BuiltIn => "Integrato",
            DevKind::Usb => "USB",
            DevKind::Virtual => "Virtuale",
            DevKind::Other => "",
        }
    }
    pub fn icon(self) -> &'static str {
        match self {
            DevKind::Bluetooth | DevKind::AirPlay => "🎧",
            DevKind::Screen => "📺",
            DevKind::Virtual => "⚙",
            _ => "🔊",
        }
    }
    /// Guess from the name where the system does not tell.
    pub fn guess(name: &str) -> DevKind {
        let n = name.to_lowercase();
        if n.contains("bluez") || n.contains("bluetooth") || n.contains("airpods") || n.contains("headset") {
            DevKind::Bluetooth
        } else if n.contains("airplay") {
            DevKind::AirPlay
        } else if n.contains("built-in") || n.contains("internal") || n.contains("integrat") || n.contains("analog stereo") {
            DevKind::BuiltIn
        } else if n.contains("hdmi") || n.contains("displayport") {
            DevKind::Screen
        } else if n.contains("usb") {
            DevKind::Usb
        } else if n.contains("blackhole") || n.contains("cable") || n.contains("virtual") || n.contains("null") {
            DevKind::Virtual
        } else {
            DevKind::Other
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DeviceInfo {
    pub id: String,
    pub label: String,
    pub channels: usize,
    /// Output latency the device declares to the system (players use it to
    /// delay the video), when known.
    pub latency_ms: Option<f32>,
    pub kind: DevKind,
}

#[derive(Clone, Debug, Default)]
pub struct DeviceList {
    pub inputs: Vec<DeviceInfo>,
    pub outputs: Vec<DeviceInfo>,
}

impl DeviceList {
    pub fn input(&self, id: &str) -> Option<&DeviceInfo> {
        self.inputs.iter().find(|d| d.id == id)
    }
    pub fn output(&self, id: &str) -> Option<&DeviceInfo> {
        self.outputs.iter().find(|d| d.id == id)
    }
    pub fn all_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .inputs
            .iter()
            .map(|d| format!("i:{}", d.id))
            .chain(self.outputs.iter().map(|d| format!("o:{}", d.id)))
            .collect();
        v.sort();
        v
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StreamFormat {
    pub channels: usize,
    pub rate: u32,
}

pub type InputCallback = Box<dyn FnMut(&[f32]) + Send + 'static>;
pub type OutputCallback = Box<dyn FnMut(&mut [f32]) + Send + 'static>;
pub type ErrorCallback = Arc<dyn Fn(String) + Send + Sync + 'static>;

#[allow(dead_code)] // the handles only need to stay alive
pub enum StreamHandle {
    Cpal(cpal::Stream),
    /// Stream first: fields drop in order, the tap must outlive the stream.
    #[cfg(target_os = "macos")]
    Tap(cpal::Stream, crate::mactap::SystemTap),
    #[cfg(target_os = "linux")]
    Pulse(pulse::PulseStream),
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

/// Names of devices seen during this run, so a disconnected device can still
/// be shown by name instead of by its system id.
static NAMES: std::sync::Mutex<Option<std::collections::HashMap<String, String>>> = std::sync::Mutex::new(None);

pub fn remember_name(id: &str, label: &str) {
    if let Ok(mut g) = NAMES.lock() {
        g.get_or_insert_with(Default::default).insert(id.to_string(), label.to_string());
    }
}

/// The name a device calls itself, when it is known without the device
/// being connected: seen earlier in this run, or (macOS) a paired Bluetooth
/// device, whose address is in our id. The settings keep the name of each
/// output; an older version saved its own ("Bluetooth", "Filo"), and a line
/// saying that «Bluetooth» is not connected reads as if it were about
/// Bluetooth itself, with another Bluetooth speaker playing.
pub fn known_name(id: &str) -> Option<String> {
    if let Ok(g) = NAMES.lock() {
        if let Some(n) = g.as_ref().and_then(|m| m.get(id)) {
            return Some(n.clone());
        }
    }
    #[cfg(target_os = "macos")]
    return paired_bluetooth_name(id);
    #[allow(unreachable_code)]
    None
}

/// The Bluetooth address in the id of a macOS output
/// ("cpal:coreaudio:AA-BB-CC-11-22-33:output"), as AA:BB:CC:11:22:33.
#[cfg(any(target_os = "macos", all(test, local_tests)))]
fn bluetooth_address(id: &str) -> Option<String> {
    id.split(':')
        .find(|p| {
            let bytes: Vec<&str> = p.split('-').collect();
            bytes.len() == 6 && bytes.iter().all(|b| b.len() == 2 && b.chars().all(|c| c.is_ascii_hexdigit()))
        })
        .map(|p| p.to_ascii_uppercase().replace('-', ":"))
}

/// Names of the paired Bluetooth devices by address, from what
/// `system_profiler SPBluetoothDataType -json` prints.
#[cfg(any(target_os = "macos", all(test, local_tests)))]
fn paired_from_profiler(json: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return map };
    for section in v["SPBluetoothDataType"].as_array().into_iter().flatten() {
        for list in ["device_connected", "device_not_connected"] {
            for item in section[list].as_array().into_iter().flatten() {
                for (name, info) in item.as_object().into_iter().flatten() {
                    if let Some(addr) = info["device_address"].as_str() {
                        map.insert(addr.to_ascii_uppercase(), name.clone());
                    }
                }
            }
        }
    }
    map
}

/// Asked of the system once, in the background (it takes a moment): until
/// the answer is there no name is known.
#[cfg(target_os = "macos")]
fn paired_bluetooth_name(id: &str) -> Option<String> {
    static PAIRED: std::sync::Mutex<Option<std::collections::HashMap<String, String>>> = std::sync::Mutex::new(None);
    static ASKED: std::sync::Once = std::sync::Once::new();
    let addr = bluetooth_address(id)?;
    ASKED.call_once(|| {
        std::thread::spawn(|| {
            let out = std::process::Command::new("/usr/sbin/system_profiler").args(["SPBluetoothDataType", "-json"]).output();
            let map = out.map(|o| paired_from_profiler(&String::from_utf8_lossy(&o.stdout))).unwrap_or_default();
            if let Ok(mut g) = PAIRED.lock() {
                *g = Some(map);
            }
        });
    });
    PAIRED.lock().ok()?.as_ref()?.get(&addr).cloned()
}

/// Best human-readable name for an id (connected or not).
pub fn display_name(id: &str) -> String {
    if id.contains("multiaudio_51") {
        return "MultiAudio 5.1".into();
    }
    if id.contains("app.multiaudio.out") {
        return "MultiAudio".into();
    }
    if let Ok(g) = NAMES.lock() {
        if let Some(n) = g.as_ref().and_then(|m| m.get(id)) {
            return n.clone();
        }
    }
    let raw = id.split_once(':').map(|(_, n)| n).unwrap_or(id);
    raw.rsplit(':').next().unwrap_or(raw).to_string()
}

pub fn list_devices() -> DeviceList {
    let mut list = DeviceList::default();

    #[cfg(target_os = "linux")]
    {
        let (i, o) = pulse::list();
        list.inputs.extend(i);
        list.outputs.extend(o);
    }

    let host = cpal::default_host();
    let prefix = if cfg!(target_os = "linux") { "ALSA · " } else { "" };
    if let Ok(devs) = host.devices() {
        for d in devs {
            let Ok(id) = d.id() else { continue };
            if id.to_string().contains("app.multiaudio.") {
                continue; // our own capture devices and output
            }
            let name = d.description().map(|x| x.name().to_string()).unwrap_or_else(|_| id.to_string());
            if let Ok(c) = d.default_input_config() {
                list.inputs.push(DeviceInfo { id: format!("cpal:{id}"), label: format!("{prefix}{name}"), channels: c.channels() as usize, kind: DevKind::guess(&name), ..Default::default() });
            }
            if let Ok(c) = d.default_output_config() {
                let channels = c.channels() as usize;
                let full = format!("cpal:{id}");
                list.outputs.push(DeviceInfo {
                    latency_ms: reported_latency_ms(&full),
                    kind: device_kind(&full, &name),
                    id: full,
                    label: format!("{prefix}{name}"),
                    channels,
                });
                if cfg!(target_os = "windows") {
                    list.inputs.push(DeviceInfo { id: format!("loopback:{id}"), label: format!("⟲ Audio inviato a {name}"), channels, ..Default::default() });
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    if crate::mactap::supported() {
        let mut taps = vec![DeviceInfo {
            id: "tap:system".into(),
            label: "⟲ Audio di sistema (lo suona solo MultiAudio)".into(),
            channels: 2,
            ..Default::default()
        }];
        for o in &list.outputs {
            if let Some(uid) = o.id.strip_prefix("cpal:coreaudio:") {
                taps.push(DeviceInfo { id: format!("tap:dev:{uid}"), label: format!("⟲ Audio inviato a {}", o.label), channels: 2, ..Default::default() });
            }
        }
        if host_has_uid(crate::mactap::VOUT_UID) {
            let channels = crate::mactap::vout_channels().max(2);
            taps.push(DeviceInfo { id: VOUT_INPUT.into(), label: "⟲ MultiAudio".into(), channels, ..Default::default() });
        }
        list.inputs.splice(0..0, taps);
    }
    dedup(&mut list.inputs);
    dedup(&mut list.outputs);
    for d in list.inputs.iter().chain(list.outputs.iter()) {
        remember_name(&d.id, &d.label);
    }
    list
}

#[cfg(target_os = "macos")]
fn host_has_uid(uid: &str) -> bool {
    let want = format!("coreaudio:{uid}");
    cpal::default_host().devices().map(|mut it| it.any(|d| d.id().map(|i| i.to_string() == want).unwrap_or(false))).unwrap_or(false)
}

fn dedup(v: &mut Vec<DeviceInfo>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|d| seen.insert(d.id.clone()));
}

/// The physical device behind an id, whatever its role (output, loopback,
/// tap, monitor): used to warn about feedback loops.
pub fn physical_key(id: &str) -> String {
    if let Some(u) = id.strip_prefix("tap:dev:") {
        return format!("coreaudio:{u}");
    }
    if let Some(r) = id.strip_prefix("cpal:").or_else(|| id.strip_prefix("loopback:")) {
        return r.to_string();
    }
    if let Some(r) = id.strip_prefix("pulse:") {
        return format!("pulse:{}", r.trim_end_matches(".monitor"));
    }
    id.to_string()
}

/// Input that captures "what is being played on this output", if the
/// platform can do it natively.
pub fn mirror_input_for(output_id: &str) -> Option<String> {
    if let Some(uid) = output_id.strip_prefix("cpal:coreaudio:") {
        #[cfg(target_os = "macos")]
        if crate::mactap::supported() {
            return Some(format!("tap:dev:{uid}"));
        }
        let _ = uid;
        return None;
    }
    if let Some(r) = output_id.strip_prefix("cpal:wasapi:") {
        return Some(format!("loopback:wasapi:{r}"));
    }
    if let Some(r) = output_id.strip_prefix("pulse:") {
        return Some(format!("pulse:{r}.monitor"));
    }
    None
}

/// Makes an output the system default output, where the platform allows it.
pub fn set_default_output(output_id: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    if let Some(uid) = output_id.strip_prefix("cpal:coreaudio:") {
        return crate::mactap::set_default_output(uid);
    }
    #[cfg(target_os = "linux")]
    if let Some(name) = output_id.strip_prefix("pulse:") {
        let ok = std::process::Command::new("pactl").args(["set-default-sink", name]).status().map(|s| s.success()).unwrap_or(false);
        return if ok { Ok(()) } else { Err(anyhow!("pactl set-default-sink non riuscito")) };
    }
    let _ = output_id;
    bail!("su questo sistema scegli l'uscita predefinita dalle impostazioni audio")
}

fn device_kind(id: &str, name: &str) -> DevKind {
    #[cfg(target_os = "macos")]
    if let Some(uid) = id.strip_prefix("cpal:coreaudio:") {
        if let Some(k) = crate::mactap::transport_kind(uid) {
            return k;
        }
    }
    let _ = id;
    DevKind::guess(name)
}

/// Volume of the system output: what the volume keys control.
#[derive(Clone, Debug)]
pub struct SystemVolume {
    /// The output device the volume belongs to.
    pub device: String,
    /// Linear gain (already on the system's loudness curve).
    pub gain: f32,
    pub muted: bool,
}

/// Captured audio (Core Audio tap, WASAPI loopback, PipeWire monitor) is
/// taken *before* the system volume, so outputs fed by MultiAudio must apply
/// it themselves. The gain is always finite, between 0 and +12 dB.
pub fn system_volume() -> Option<SystemVolume> {
    system_volume_raw().map(|mut v| {
        v.gain = if v.gain.is_finite() { v.gain.clamp(0.0, 4.0) } else { 1.0 };
        v
    })
}

fn system_volume_raw() -> Option<SystemVolume> {
    #[cfg(target_os = "macos")]
    {
        let (uid, gain, muted) = crate::mactap::default_output_volume()?;
        return Some(SystemVolume { device: format!("cpal:coreaudio:{uid}"), gain, muted });
    }
    #[cfg(target_os = "windows")]
    {
        let device = default_output_id()?;
        let (gain, muted) = crate::winvol::default_output_volume()?;
        return Some(SystemVolume { device, gain, muted });
    }
    #[cfg(target_os = "linux")]
    {
        let device = default_output_id()?;
        let run = |args: &[&str]| -> Option<String> {
            let o = std::process::Command::new("pactl").args(args).env("LC_ALL", "C").output().ok()?;
            o.status.success().then(|| String::from_utf8_lossy(&o.stdout).to_string())
        };
        let vol = run(&["get-sink-volume", "@DEFAULT_SINK@"])?;
        // "Volume: front-left: 32768 /  50% / -18.06 dB,   front-right: ..."
        let pct: f32 = vol.split('/').nth(1).and_then(|p| p.trim().trim_end_matches('%').parse().ok())?;
        let muted = run(&["get-sink-mute", "@DEFAULT_SINK@"]).map(|m| m.contains("yes")).unwrap_or(false);
        // PulseAudio/PipeWire volume curve is cubic.
        let v = (pct / 100.0).max(0.0);
        return Some(SystemVolume { device, gain: v * v * v, muted });
    }
    #[allow(unreachable_code)]
    None
}

/// Opens a file with the system's default app.
pub fn reveal(path: &std::path::Path) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd).arg(path).spawn();
}

// ---------------------------------------------------------------------------
// MultiAudio's own output
//
// macOS: "MultiAudio", the system output while MultiAudio is on: every app
//   plays to it (5.1 included) and MultiAudio sends it to the real speakers.
// Linux: "MultiAudio 5.1", an extra 6-channel output for players that send
//   real 5.1 (surround on); the system plays to the video output as usual.
// ---------------------------------------------------------------------------

/// Input that captures MultiAudio's own output.
#[cfg(target_os = "linux")]
pub const VOUT_INPUT: &str = "pulse:multiaudio_51.monitor";
#[cfg(not(target_os = "linux"))]
pub const VOUT_INPUT: &str = "tap:out";

/// MultiAudio's own output is the system output (macOS).
pub const VOUT_IS_SYSTEM: bool = cfg!(target_os = "macos");

/// Output id of MultiAudio's own output.
pub fn vout_output_id() -> String {
    #[cfg(target_os = "macos")]
    return format!("cpal:coreaudio:{}", crate::mactap::VOUT_UID);
    #[cfg(target_os = "linux")]
    return "pulse:multiaudio_51".into();
    #[allow(unreachable_code)]
    String::new()
}

/// Keeps MultiAudio's output alive; it disappears when dropped (macOS: the
/// previous system output comes back).
pub enum VirtualOut {
    /// (the output, whether its clock is a Bluetooth device)
    #[cfg(target_os = "macos")]
    Mac(crate::mactap::VirtualOut, bool),
    #[cfg(target_os = "linux")]
    Linux(u32),
}

impl VirtualOut {
    pub fn channels(&self) -> usize {
        #[cfg(target_os = "macos")]
        #[allow(irrefutable_let_patterns)] // the only variant on macOS
        if let VirtualOut::Mac(v, _) = self {
            return v.channels;
        }
        6
    }

    /// Latency it declares (macOS) and the lowest it could declare, ms.
    pub fn declared_ms(&self) -> Option<(f32, f32)> {
        #[cfg(target_os = "macos")]
        #[allow(irrefutable_let_patterns)] // the only variant on macOS
        if let VirtualOut::Mac(v, _) = self {
            return Some((v.declared_ms as f32, v.floor_ms as f32));
        }
        None
    }
}

impl Drop for VirtualOut {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        #[allow(irrefutable_let_patterns)] // the only variant on Linux
        if let VirtualOut::Linux(module) = self {
            let _ = std::process::Command::new("pactl").args(["unload-module", &module.to_string()]).status();
        }
        #[cfg(target_os = "macos")]
        #[allow(irrefutable_let_patterns)] // the only variant on macOS
        if let VirtualOut::Mac(v, bt_clock) = self {
            let asked = std::time::Instant::now();
            v.remove();
            // (taken away in the normal way: its start did get through)
            if *bt_clock && !daemon_froze(asked) {
                bt_clock_reset();
            }
        }
        crate::diag::log("uscita MultiAudio rimossa");
    }
}

pub fn vout_supported() -> bool {
    #[cfg(target_os = "macos")]
    {
        return crate::mactap::supported();
    }
    #[cfg(target_os = "linux")]
    {
        return std::process::Command::new("pactl").arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
    }
    #[allow(unreachable_code)]
    false
}

/// The order in which real outputs are borrowed for "MultiAudio" after its
/// clock: those running at the clock's rate before the others, then by
/// kind (`rank`: wired before Bluetooth, which may go away), then by id.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn borrow_order(off_rate: bool, rank: u8, id: &str) -> (bool, u8, String) {
    (off_rate, rank, id.to_string())
}

/// macOS: the real outputs "MultiAudio" is built from (it only borrows
/// their channels and clock; they play nothing of it). The clock comes
/// first: it is the video output `main`, since the latency many players
/// read is the clock's (measured: with a Bluetooth clock Apple's player
/// delays the picture as for the Bluetooth output itself, with a wired one
/// half a second less). Then wired devices, as few as needed for 6
/// channels; Bluetooth ones only if they are among the `enabled` outputs
/// (they may disconnect). AirPlay cannot be part of it: with such a video
/// output the list is empty, no "MultiAudio" output, and the video output
/// stays the system output. The same with a Bluetooth video output unless
/// `bt_clock` (see `bt_clock_tripped`), or when there would be nothing to
/// gain (its two channels only).
pub fn vout_members(devs: &DeviceList, enabled: &[String], main: Option<&str>, bt_clock: bool) -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        let rank = |k: DevKind| -> u8 {
            match k {
                DevKind::BuiltIn => 0,
                DevKind::Screen => 1,
                DevKind::Usb => 2,
                DevKind::Other => 3,
                DevKind::Virtual => 4,
                _ => 5, // Bluetooth: may come and go
            }
        };
        let usable = |d: &DeviceInfo| {
            d.channels > 0
                && d.kind != DevKind::AirPlay
                && !is_own_device(&d.id)
                && d.id.strip_prefix("cpal:coreaudio:").map(|u| d.kind != DevKind::Virtual || !crate::mactap::is_aggregate(u)).unwrap_or(false)
        };
        let uid = |d: &DeviceInfo| d.id.strip_prefix("cpal:coreaudio:").unwrap_or_default().to_string();
        // The list with `clock` first, and how many channels it has.
        let with_clock = |clock: &DeviceInfo| {
            // Only devices that can run at the clock's rate: one at another
            // rate would be driven at two rates at once (by "MultiAudio" and
            // by the engine), a likely source of crackles.
            let rate = crate::mactap::nominal_rate(&uid(clock));
            let mut c: Vec<&DeviceInfo> = devs
                .outputs
                .iter()
                .filter(|d| d.id != clock.id && usable(d))
                .filter(|d| d.kind != DevKind::Bluetooth || enabled.contains(&d.id))
                .filter(|d| rate.map(|r| crate::mactap::supports_rate(&uid(d), r)).unwrap_or(true))
                .collect();
            // Those already running at the clock's rate first: the capture of
            // "MultiAudio" may be taken from these devices' own streams
            // (`mactap::Taps::Members`), measured only with all of them at one
            // rate. (A screen at 48 kHz stays at 48 kHz inside a 44.1 kHz
            // "MultiAudio": the system converts.)
            let off_rate = |d: &DeviceInfo| match (rate, crate::mactap::nominal_rate(&uid(d))) {
                (Some(r), Some(own)) => (r - own).abs() > 0.5,
                _ => false,
            };
            c.sort_by_key(|d| borrow_order(off_rate(d), rank(d.kind), &d.id));
            let mut total = clock.channels;
            let mut out = vec![clock.id.clone()];
            for d in c {
                if total >= 6 {
                    break;
                }
                total += d.channels;
                out.push(d.id.clone());
            }
            (total, out)
        };
        let Some(main) = main.and_then(|m| devs.output(m)).filter(|d| usable(d)) else { return Vec::new() };
        let bluetooth = main.kind == DevKind::Bluetooth;
        if bluetooth && !bt_clock {
            return Vec::new();
        }
        let (total, list) = with_clock(main);
        // (a Bluetooth clock only for something to gain: more than its own two channels)
        if bluetooth && total <= 2 {
            return Vec::new();
        }
        return list;
    }
    #[allow(unreachable_code)]
    {
        let _ = (devs, enabled, main, bt_clock);
        vec![vout_output_id()]
    }
}

/// File left while a "MultiAudio" output with a Bluetooth clock starts.
fn bt_clock_mark() -> std::path::PathBuf {
    crate::config::Config::path().with_file_name("clock-bluetooth.avvio")
}

/// A "MultiAudio" output with a Bluetooth clock has frozen macOS' audio
/// daemon while it was being made (2026-11-23 and 2026-11-26; how, and what
/// is done about it, in `mactap::VOUT_QUIET`). Until the daemon is restarted
/// no program on the Mac has sound. What is done makes it unlikely, not
/// impossible, so a mark is left on disk while one starts and removed once
/// it has been there for some seconds with the daemon still answering
/// quickly: a run that finds the mark knows the last start never got
/// through, and does without the Bluetooth clock instead of freezing the
/// audio at every start.
pub fn bt_clock_tripped() -> bool {
    bt_clock_mark().exists()
}

/// Forgets a start that did not get through (the user wants to try again).
pub fn bt_clock_reset() {
    let _ = std::fs::remove_file(bt_clock_mark());
}

/// macOS' audio daemon answers in milliseconds. Frozen (2026-11-26), what
/// was asked of it came back after a minute, as errors, or never: longer
/// than this over one step counts as frozen.
#[cfg(target_os = "macos")]
const DAEMON_SLOW: std::time::Duration = std::time::Duration::from_secs(4);

/// Whether the audio daemon froze in this run: now (the step begun at
/// `asked` took too long) or earlier. The first time it is said in the log
/// and to the user, in a dialog of the system's: the window cannot, it waits
/// for the daemon too.
#[cfg(target_os = "macos")]
fn daemon_froze(asked: std::time::Instant) -> bool {
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    static FROZE: AtomicBool = AtomicBool::new(false);
    let took = asked.elapsed();
    if took > DAEMON_SLOW && !FROZE.swap(true, Relaxed) {
        crate::diag::log(format!(
            "il servizio audio del Mac (coreaudiod) ha risposto dopo {:.0} s: bloccato con l'uscita MultiAudio sul clock Bluetooth. Va fatto ripartire («sudo killall -9 coreaudiod» nel Terminale, o riavvio del Mac). Dal prossimo avvio niente 5.1 vero col Bluetooth",
            took.as_secs_f32()
        ));
        std::thread::spawn(|| {
            let _ = std::process::Command::new("osascript").args(["-e", DAEMON_ALERT]).status();
        });
    }
    FROZE.load(Relaxed)
}

#[cfg(target_os = "macos")]
const DAEMON_ALERT: &str = "display alert \"MultiAudio: l'audio del Mac si è bloccato\" message \"Il servizio audio di macOS ha smesso di rispondere con l'uscita «MultiAudio» a 6 canali sulle casse Bluetooth. Finché non riparte nessun programma ha l'audio.\n\nPer farlo ripartire apri il Terminale e scrivi:\n\nsudo killall -9 coreaudiod\n\n(oppure riavvia il Mac). Al prossimo avvio MultiAudio farà a meno del 5.1 vero col Bluetooth: si riaccende dalle impostazioni avanzate.\" as critical";

/// Creates MultiAudio's output. macOS: from `members` (see `vout_members`),
/// declaring `latency_ms`; it becomes the system output, and `main` is
/// where the system output goes back if the current one is not usable.
pub fn create_vout(devs: &DeviceList, members: &[String], latency_ms: f32, main: Option<&str>) -> Result<VirtualOut> {
    #[cfg(target_os = "macos")]
    {
        let uid = |id: &str| id.strip_prefix("cpal:coreaudio:").map(str::to_string);
        let list: Vec<(String, f64, usize)> = members
            .iter()
            .filter_map(|id| {
                let d = devs.output(id)?;
                Some((uid(id)?, d.latency_ms.unwrap_or(0.0) as f64, d.channels))
            })
            .collect();
        let fallback = main.and_then(uid);
        let bt_clock = members.first().and_then(|id| devs.output(id)).is_some_and(|d| d.kind == DevKind::Bluetooth);
        if !bt_clock {
            return Ok(VirtualOut::Mac(crate::mactap::create_vout(&list, latency_ms as f64, fallback.as_deref())?, false));
        }
        // (see `bt_clock_tripped`)
        use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
        static STARTS: AtomicU32 = AtomicU32::new(0);
        let mine = STARTS.fetch_add(1, Relaxed) + 1;
        let _ = std::fs::write(bt_clock_mark(), b"");
        // (the two pauses it takes on purpose are not the daemon's time)
        let asked = std::time::Instant::now() + 2 * crate::mactap::VOUT_QUIET;
        let made = crate::mactap::create_vout(&list, latency_ms as f64, fallback.as_deref());
        let froze = daemon_froze(asked);
        return match made {
            Ok(v) => {
                if !froze {
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(10));
                        let asked = std::time::Instant::now();
                        let _ = default_output_id();
                        if !daemon_froze(asked) && STARTS.load(Relaxed) == mine {
                            bt_clock_reset();
                        }
                    });
                }
                Ok(VirtualOut::Mac(v, true))
            }
            Err(e) => {
                if !froze {
                    bt_clock_reset(); // refused, not frozen
                }
                Err(e)
            }
        };
    }
    #[cfg(target_os = "linux")]
    {
        let _ = (devs, members, latency_ms, main);
        let out = std::process::Command::new("pactl")
            .args([
                "load-module",
                "module-null-sink",
                "sink_name=multiaudio_51",
                "channels=6",
                "channel_map=front-left,front-right,front-center,lfe,rear-left,rear-right",
                "sink_properties=device.description=MultiAudio_5.1",
            ])
            .output()?;
        let module: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().map_err(|_| anyhow!("pactl load-module non riuscito"))?;
        crate::diag::log("uscita «MultiAudio 5.1» creata (sink PipeWire a 6 canali)");
        return Ok(VirtualOut::Linux(module));
    }
    #[allow(unreachable_code)]
    {
        let _ = (devs, members, latency_ms, main);
        bail!("su questo sistema serve un driver audio virtuale")
    }
}

/// Removes leftovers of a previous run (crash).
pub fn cleanup_vout() {
    #[cfg(target_os = "macos")]
    crate::mactap::destroy_stale();
    #[cfg(target_os = "linux")]
    {
        if let Ok(o) = std::process::Command::new("pactl").args(["list", "short", "modules"]).output() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                if line.contains("sink_name=multiaudio_51") {
                    if let Some(idx) = line.split_whitespace().next() {
                        let _ = std::process::Command::new("pactl").args(["unload-module", idx]).status();
                    }
                }
            }
        }
    }
}

/// Puts the system output back and removes our output: for a panic or a
/// termination signal, when the normal clean-up will not run.
pub fn emergency_restore() {
    #[cfg(target_os = "macos")]
    crate::mactap::emergency_restore();
}

/// Pids of the processes that are playing sound now, when the system tells
/// (macOS 14.2 and later). It is the nearest thing to "which source is
/// this": no system says how many channels a player is sending.
pub fn playing_processes() -> Option<Vec<i32>> {
    #[cfg(target_os = "macos")]
    return crate::mactap::playing_pids();
    #[allow(unreachable_code)]
    None
}

/// The system output is MultiAudio's own output (macOS): the volume keys
/// and the volume of the outputs are then MultiAudio's job.
pub fn system_output_is_vout() -> bool {
    #[cfg(target_os = "macos")]
    return crate::mactap::default_output_uid().as_deref() == Some(crate::mactap::VOUT_UID);
    #[allow(unreachable_code)]
    false
}

/// Volume slider of an output (0..1) and its linear gain, if it has one.
pub fn output_volume(id: &str) -> Option<(f32, f32)> {
    #[cfg(target_os = "macos")]
    if let Some(uid) = id.strip_prefix("cpal:coreaudio:") {
        let v = crate::mactap::volume_of(uid)?;
        return v.scalar.map(|s| (s, if v.gain.is_finite() { v.gain.clamp(0.0, 1.0) } else { s * s * s }));
    }
    let _ = id;
    None
}

/// Moves the volume slider of an output; false if it has none.
pub fn set_output_volume(id: &str, scalar: f32) -> bool {
    #[cfg(target_os = "macos")]
    if let Some(uid) = id.strip_prefix("cpal:coreaudio:") {
        return crate::mactap::set_volume(uid, scalar);
    }
    let _ = (id, scalar);
    false
}

/// Slider position of the current system output, unless it is ours or
/// has no volume control.
pub fn system_volume_slider() -> Option<f32> {
    let def = default_output_id()?;
    if is_own_device(&def) {
        return None;
    }
    output_volume(&def).map(|v| v.0)
}

/// Test tool: plays a different tone on each channel of an output (channel
/// k at 300 + 100·k Hz, −30 dBFS) for `secs` seconds.
pub fn play_test_tones(id: &str, secs: f64) -> Result<()> {
    let dev_id = id.strip_prefix("cpal:").ok_or_else(|| anyhow!("id dispositivo non valido: {id}"))?;
    let dev = find_cpal(dev_id)?;
    let cfg = widest(dev.supported_output_configs()?.collect(), dev.default_output_config()?);
    let rate = cfg.sample_rate() as f64;
    let ch = cfg.channels() as usize;
    let mut n: u64 = 0;
    let cb: OutputCallback = Box::new(move |d: &mut [f32]| {
        for frame in d.chunks_exact_mut(ch) {
            for (k, v) in frame.iter_mut().enumerate() {
                let hz = 300.0 + 100.0 * k as f64;
                *v = (0.03 * (2.0 * std::f64::consts::PI * hz * n as f64 / rate).sin()) as f32;
            }
            n += 1;
        }
    });
    let config = cpal::StreamConfig { channels: cfg.channels(), sample_rate: cfg.sample_rate(), buffer_size: cpal::BufferSize::Default };
    let mut cb = cb;
    let _s = dev.build_output_stream(config, move |d: &mut [f32], _: &cpal::OutputCallbackInfo| cb(d), |e| eprintln!("! {e}"), None)?;
    _s.play()?;
    println!("toni su {} canali di {}", ch, display_name(id));
    std::thread::sleep(std::time::Duration::from_secs_f64(secs));
    Ok(())
}

/// One of MultiAudio's own devices (its output, capture devices).
pub fn is_own_device(id: &str) -> bool {
    id.contains("app.multiaudio.") || id.contains("multiaudio_51")
}

/// If the system output is one of MultiAudio's own devices but should not be
/// (picked by hand, or by macOS right after it appeared), puts `wanted` back.
/// Returns the name of the device that was replaced.
pub fn guard_system_output(wanted: &str) -> Option<String> {
    let current = default_output_id()?;
    if !is_own_device(&current) || is_own_device(wanted) {
        return None;
    }
    set_default_output(wanted).ok()?;
    crate::diag::log(format!(
        "l'uscita di sistema era «{}»: rimessa «{}»",
        display_name(&current),
        display_name(wanted)
    ));
    Some(display_name(&current))
}

/// Id of the output the operating system currently plays to.
pub fn default_output_id() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(o) = std::process::Command::new("pactl").arg("get-default-sink").output() {
            let name = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if o.status.success() && !name.is_empty() {
                return Some(format!("pulse:{name}"));
            }
        }
    }
    let d = cpal::default_host().default_output_device()?;
    Some(format!("cpal:{}", d.id().ok()?))
}

/// Latency an output declares to the system (macOS only), in ms.
pub fn reported_latency_ms(output_id: &str) -> Option<f32> {
    #[cfg(target_os = "macos")]
    if let Some(uid) = output_id.strip_prefix("cpal:coreaudio:") {
        return crate::mactap::reported_latency_ms(uid).map(|v| v as f32);
    }
    let _ = output_id;
    None
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // only macOS has a permission
pub enum CapturePermission {
    Granted,
    Denied,
    /// macOS will ask the first time audio is captured.
    NotAsked,
    /// No permission involved (Windows, Linux) or it cannot be checked.
    NotApplicable,
}

/// Whether the OS lets us capture system audio.
pub fn capture_permission() -> CapturePermission {
    #[cfg(target_os = "macos")]
    {
        return match crate::mactap::capture_permission() {
            crate::mactap::Permission::Granted => CapturePermission::Granted,
            crate::mactap::Permission::Denied => CapturePermission::Denied,
            crate::mactap::Permission::NotAsked => CapturePermission::NotAsked,
            crate::mactap::Permission::Unknown => CapturePermission::NotApplicable,
        };
    }
    #[allow(unreachable_code)]
    CapturePermission::NotApplicable
}

/// Asks the OS for the capture permission without blocking (macOS);
/// `done(granted)` runs when the user answers. False if nothing was asked.
pub fn request_capture_permission(done: impl Fn(bool) + Send + 'static) -> bool {
    #[cfg(target_os = "macos")]
    {
        return crate::mactap::request_capture_permission(done);
    }
    #[allow(unreachable_code)]
    {
        let _ = done;
        false
    }
}

pub fn open_permission_settings() {
    #[cfg(target_os = "macos")]
    crate::mactap::open_permission_settings();
}

// ---- macOS taps: created when the format is queried, handed to the stream ----

#[cfg(target_os = "macos")]
static PENDING_TAPS: std::sync::Mutex<Vec<(String, crate::mactap::SystemTap)>> = std::sync::Mutex::new(Vec::new());

#[cfg(target_os = "macos")]
fn tap_kind(id: &str) -> Option<crate::mactap::TapKind> {
    if id == "tap:system" {
        Some(crate::mactap::TapKind::System)
    } else if id == VOUT_INPUT {
        Some(crate::mactap::TapKind::Streams(crate::mactap::VOUT_UID.to_string()))
    } else {
        id.strip_prefix("tap:dev:").map(|u| crate::mactap::TapKind::Device(u.to_string()))
    }
}

/// Creates (or reuses) the tap for `id` and returns it with its capture device.
#[cfg(target_os = "macos")]
fn tap_device(id: &str, take: bool) -> Result<(cpal::Device, cpal::SupportedStreamConfig, Option<crate::mactap::SystemTap>)> {
    let kind = tap_kind(id).ok_or_else(|| anyhow!("id non valido: {id}"))?;
    let mut pending = PENDING_TAPS.lock().map_err(|_| anyhow!("lock"))?;
    let pos = pending.iter().position(|(k, _)| k == id);
    let fresh = pos.is_none();
    let tap = match pos {
        Some(i) => pending.remove(i).1,
        None => crate::mactap::create(&kind)?,
    };
    let device = || -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
        let agg_id = format!("coreaudio:{}", tap.agg_uid);
        // The private device can take a moment to show up.
        let mut found = None;
        for _ in 0..20 {
            if let Ok(d) = find_cpal(&agg_id) {
                found = Some(d);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let dev = found.ok_or_else(|| anyhow!("dispositivo di cattura non trovato"))?;
        // The capture runs at its clock device's rate (e.g. 44.1 kHz for
        // Bluetooth), which may differ from the "default" format reported.
        // A capture with several taps has several streams: the "default" format
        // only describes the first one, so take the config with all channels.
        let def = dev.default_input_config()?;
        if std::env::var_os("MULTIAUDIO_DEBUG").is_some() {
            crate::diag::log(format!("debug cattura {id}: predefinito {}ch {} Hz", def.channels(), def.sample_rate()));
            for r in dev.supported_input_configs()? {
                crate::diag::log(format!("debug cattura {id}: supporta {}ch {}–{} Hz", r.channels(), r.min_sample_rate(), r.max_sample_rate()));
            }
        }
        let cfg = widest(dev.supported_input_configs()?.collect(), def);
        Ok((dev, cfg))
    };
    let (dev, cfg) = match device() {
        Ok(found) => found,
        Err(e) => {
            // (a capture from the real devices that cannot be read: the next try is the other way)
            if tap.members {
                crate::mactap::members_failed(&e.to_string());
            }
            return Err(e);
        }
    };
    if fresh {
        crate::diag::log(tap.facts());
    }
    if take {
        Ok((dev, cfg, Some(tap)))
    } else {
        pending.push((id.to_string(), tap));
        Ok((dev, cfg, None))
    }
}

fn find_cpal(id: &str) -> Result<cpal::Device> {
    let host = cpal::default_host();
    host.devices()?
        .find(|d| d.id().map(|x| x.to_string() == id).unwrap_or(false))
        .ok_or_else(|| anyhow!("dispositivo non trovato: {}", display_name(id)))
}

fn cpal_input(id: &str) -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    if let Some(dev_id) = id.strip_prefix("loopback:") {
        let d = find_cpal(dev_id)?;
        let c = d.default_output_config()?;
        Ok((d, c))
    } else if let Some(dev_id) = id.strip_prefix("cpal:") {
        let d = find_cpal(dev_id)?;
        let c = d.default_input_config()?;
        Ok((d, c))
    } else {
        bail!("id dispositivo non valido: {id}")
    }
}

fn cpal_output(id: &str) -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    let dev_id = id.strip_prefix("cpal:").ok_or_else(|| anyhow!("id dispositivo non valido: {id}"))?;
    let d = find_cpal(dev_id)?;
    let c = d.default_output_config()?;
    Ok((d, c))
}

/// The supported config with the most channels, at the device's current
/// rate (devices made of several streams report only the first stream as
/// "default", and list each rate as a separate range).
fn widest(ranges: Vec<cpal::SupportedStreamConfigRange>, def: cpal::SupportedStreamConfig) -> cpal::SupportedStreamConfig {
    let rate = def.sample_rate();
    let Some(max_ch) = ranges.iter().map(|r| r.channels()).max() else { return def };
    if max_ch <= def.channels() {
        return def;
    }
    let wide: Vec<cpal::SupportedStreamConfigRange> = ranges.into_iter().filter(|r| r.channels() == max_ch).collect();
    if let Some(c) = wide.iter().find_map(|r| r.try_with_sample_rate(rate)) {
        return c;
    }
    // otherwise the closest rate
    wide.into_iter()
        .min_by_key(|r| (r.min_sample_rate() as i64 - rate as i64).abs().min((r.max_sample_rate() as i64 - rate as i64).abs()))
        .map(|r| {
            let rr = rate.clamp(r.min_sample_rate(), r.max_sample_rate());
            r.with_sample_rate(rr)
        })
        .unwrap_or(def)
}

pub fn input_format(id: &str) -> Result<StreamFormat> {
    #[cfg(target_os = "macos")]
    if id.starts_with("tap:") {
        let (_, c, _) = tap_device(id, false)?;
        return Ok(StreamFormat { channels: c.channels() as usize, rate: c.sample_rate() });
    }
    #[cfg(target_os = "linux")]
    if let Some(name) = id.strip_prefix("pulse:") {
        return pulse::format(name, false);
    }
    let (_, c) = cpal_input(id)?;
    Ok(StreamFormat { channels: c.channels() as usize, rate: c.sample_rate() })
}

pub fn output_format(id: &str) -> Result<StreamFormat> {
    #[cfg(target_os = "linux")]
    if let Some(name) = id.strip_prefix("pulse:") {
        return pulse::format(name, true);
    }
    let (_, c) = cpal_output(id)?;
    Ok(StreamFormat { channels: c.channels() as usize, rate: c.sample_rate() })
}

// ---------------------------------------------------------------------------
// Opening streams
// ---------------------------------------------------------------------------

fn errfn(e: ErrorCallback) -> impl FnMut(cpal::Error) + Send + 'static {
    move |err| e(err.to_string())
}

fn stream_config(sup: &cpal::SupportedStreamConfig, fmt: &StreamFormat) -> Result<cpal::StreamConfig> {
    stream_config_with(sup, fmt, None)
}

/// `block`: frames in a block, if it matters (kept within what the device
/// takes); None: what the system gives.
fn stream_config_with(sup: &cpal::SupportedStreamConfig, fmt: &StreamFormat, block: Option<u32>) -> Result<cpal::StreamConfig> {
    if sup.channels() as usize != fmt.channels || sup.sample_rate() != fmt.rate {
        bail!("il formato del dispositivo è cambiato, riprovo");
    }
    let buffer_size = match (block, sup.buffer_size()) {
        (Some(n), cpal::SupportedBufferSize::Range { min, max }) => cpal::BufferSize::Fixed(n.clamp(*min, *max)),
        (Some(n), cpal::SupportedBufferSize::Unknown) => cpal::BufferSize::Fixed(n),
        (None, _) => cpal::BufferSize::Default,
    };
    Ok(cpal::StreamConfig { channels: sup.channels(), sample_rate: sup.sample_rate(), buffer_size })
}

/// How the capture of the system's sound is set up now, in words (macOS:
/// `mactap::Tuning`; elsewhere there is nothing to choose).
pub fn capture_tuning() -> String {
    #[cfg(target_os = "macos")]
    return crate::mactap::Tuning::current().to_string();
    #[cfg(not(target_os = "macos"))]
    String::new()
}

/// Opens an input stream. `fmt` must come from `input_format(id)`.
pub fn open_input(id: &str, fmt: &StreamFormat, cb: InputCallback, err: ErrorCallback) -> Result<StreamHandle> {
    #[cfg(target_os = "linux")]
    if let Some(name) = id.strip_prefix("pulse:") {
        return Ok(StreamHandle::Pulse(pulse::open_input(name.to_string(), *fmt, cb, err)?));
    }
    #[cfg(target_os = "macos")]
    if id.starts_with("tap:") {
        let (dev, sup, tap) = tap_device(id, true)?;
        let tap = tap.expect("tap");
        let cb = match tap.lead {
            Some((channels, frames)) => held_back(channels, frames, fmt.channels, cb),
            None => cb,
        };
        return match open_cpal_input(&dev, &sup, fmt, crate::mactap::Tuning::current().buffer, cb, err) {
            Ok(stream) => Ok(StreamHandle::Tap(stream, tap)),
            Err(e) => {
                if tap.members {
                    crate::mactap::members_failed(&e.to_string());
                }
                Err(e)
            }
        };
    }
    let (dev, sup) = cpal_input(id)?;
    Ok(StreamHandle::Cpal(open_cpal_input(&dev, &sup, fmt, None, cb, err)?))
}

/// `cb` with the first `channels` channels of every frame (of `total`)
/// held back by `frames` frames: for a capture whose first channels come
/// before the others (macOS, `mactap::SystemTap::lead`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn held_back(channels: usize, frames: usize, total: usize, mut cb: InputCallback) -> InputCallback {
    let channels = channels.min(total);
    if channels == 0 || frames == 0 {
        return cb;
    }
    let mut line = vec![0f32; channels * frames];
    let mut at = 0usize;
    // (room for the largest blocks: nothing is allocated while it runs)
    let mut out: Vec<f32> = Vec::with_capacity(8192 * total);
    Box::new(move |data: &[f32]| {
        out.clear();
        out.extend_from_slice(data);
        for frame in out.chunks_exact_mut(total) {
            for (c, v) in frame[..channels].iter_mut().enumerate() {
                std::mem::swap(v, &mut line[at + c]);
            }
            at = (at + channels) % line.len();
        }
        cb(&out);
    })
}

fn open_cpal_input(
    dev: &cpal::Device,
    sup: &cpal::SupportedStreamConfig,
    fmt: &StreamFormat,
    block: Option<u32>,
    cb: InputCallback,
    err: ErrorCallback,
) -> Result<cpal::Stream> {
    let dev = dev.clone();
    let config = stream_config_with(sup, fmt, block)?;
    use cpal::SampleFormat as F;
    let stream = match sup.sample_format() {
        F::F32 => {
            let mut cb = cb;
            dev.build_input_stream(config, move |d: &[f32], _: &cpal::InputCallbackInfo| cb(d), errfn(err), None)?
        }
        F::I16 => build_in_conv::<i16>(&dev, config, cb, err)?,
        F::I32 => build_in_conv::<i32>(&dev, config, cb, err)?,
        F::U16 => build_in_conv::<u16>(&dev, config, cb, err)?,
        F::I8 => build_in_conv::<i8>(&dev, config, cb, err)?,
        F::U8 => build_in_conv::<u8>(&dev, config, cb, err)?,
        F::F64 => build_in_conv::<f64>(&dev, config, cb, err)?,
        other => bail!("formato campioni non supportato: {other:?}"),
    };
    stream.play()?;
    Ok(stream)
}

/// Opens an output stream. `fmt` must come from `output_format(id)`.
pub fn open_output(id: &str, fmt: &StreamFormat, cb: OutputCallback, err: ErrorCallback) -> Result<StreamHandle> {
    #[cfg(target_os = "linux")]
    if let Some(name) = id.strip_prefix("pulse:") {
        return Ok(StreamHandle::Pulse(pulse::open_output(name.to_string(), *fmt, cb, err)?));
    }
    let (dev, sup) = cpal_output(id)?;
    let config = stream_config(&sup, fmt)?;
    use cpal::SampleFormat as F;
    let stream = match sup.sample_format() {
        F::F32 => {
            let mut cb = cb;
            dev.build_output_stream(config, move |d: &mut [f32], _: &cpal::OutputCallbackInfo| cb(d), errfn(err), None)?
        }
        F::I16 => build_out_conv::<i16>(&dev, config, cb, err)?,
        F::I32 => build_out_conv::<i32>(&dev, config, cb, err)?,
        F::U16 => build_out_conv::<u16>(&dev, config, cb, err)?,
        F::I8 => build_out_conv::<i8>(&dev, config, cb, err)?,
        F::U8 => build_out_conv::<u8>(&dev, config, cb, err)?,
        F::F64 => build_out_conv::<f64>(&dev, config, cb, err)?,
        other => bail!("formato campioni non supportato: {other:?}"),
    };
    stream.play()?;
    Ok(StreamHandle::Cpal(stream))
}

fn build_in_conv<T>(dev: &cpal::Device, config: cpal::StreamConfig, mut cb: InputCallback, err: ErrorCallback) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let mut buf: Vec<f32> = Vec::with_capacity(1 << 16);
    Ok(dev.build_input_stream(
        config,
        move |d: &[T], _: &cpal::InputCallbackInfo| {
            buf.clear();
            buf.extend(d.iter().map(|s| <f32 as cpal::Sample>::from_sample(*s)));
            cb(&buf);
        },
        errfn(err),
        None,
    )?)
}

fn build_out_conv<T>(dev: &cpal::Device, config: cpal::StreamConfig, mut cb: OutputCallback, err: ErrorCallback) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let mut buf: Vec<f32> = Vec::with_capacity(1 << 16);
    Ok(dev.build_output_stream(
        config,
        move |d: &mut [T], _: &cpal::OutputCallbackInfo| {
            buf.resize(d.len(), 0.0);
            cb(&mut buf);
            for (o, s) in d.iter_mut().zip(buf.iter()) {
                *o = <T as cpal::Sample>::from_sample(*s);
            }
        },
        errfn(err),
        None,
    )?)
}

// ---------------------------------------------------------------------------
// Linux: PipeWire / PulseAudio (Bluetooth sinks are not ALSA devices)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub mod pulse {
    use super::*;
    use libpulse_binding::def::BufferAttr;
    use libpulse_binding::error::PAErr;
    use libpulse_binding::sample::{Format, Spec};
    use libpulse_binding::stream::Direction;
    use libpulse_simple_binding::Simple;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct PaDev {
        name: String,
        desc: String,
        channels: usize,
        rate: u32,
    }

    fn pactl(kind: &str) -> Vec<PaDev> {
        let Ok(out) = std::process::Command::new("pactl").args(["list", kind]).env("LC_ALL", "C").output() else {
            return Vec::new();
        };
        if !out.status.success() {
            return Vec::new();
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut res = Vec::new();
        let mut cur: Option<PaDev> = None;
        for line in text.lines() {
            if line.starts_with("Sink #") || line.starts_with("Source #") {
                if let Some(d) = cur.take() {
                    res.push(d);
                }
                cur = Some(PaDev { name: String::new(), desc: String::new(), channels: 2, rate: 48000 });
                continue;
            }
            let Some(d) = cur.as_mut() else { continue };
            let t = line.trim();
            if let Some(v) = t.strip_prefix("Name: ") {
                d.name = v.to_string();
            } else if let Some(v) = t.strip_prefix("Description: ") {
                d.desc = v.to_string();
            } else if let Some(v) = t.strip_prefix("Sample Specification: ") {
                for part in v.split_whitespace() {
                    if let Some(c) = part.strip_suffix("ch").and_then(|c| c.parse().ok()) {
                        d.channels = c;
                    }
                    if let Some(r) = part.strip_suffix("Hz").and_then(|r| r.parse().ok()) {
                        d.rate = r;
                    }
                }
            }
        }
        if let Some(d) = cur.take() {
            res.push(d);
        }
        res.retain(|d| !d.name.is_empty());
        res
    }

    pub fn list() -> (Vec<DeviceInfo>, Vec<DeviceInfo>) {
        let inputs = pactl("sources")
            .into_iter()
            .map(|d| {
                let monitor = d.name.ends_with(".monitor");
                let label = if monitor { format!("⟲ {}", d.desc) } else { d.desc.clone() };
                DeviceInfo { id: format!("pulse:{}", d.name), label: format!("PipeWire · {label}"), channels: d.channels.min(8), ..Default::default() }
            })
            .collect();
        let outputs = pactl("sinks")
            .into_iter()
            .map(|d| DeviceInfo { id: format!("pulse:{}", d.name), label: format!("PipeWire · {}", d.desc), channels: d.channels.min(8), kind: DevKind::guess(&d.name), ..Default::default() })
            .collect();
        (inputs, outputs)
    }

    pub fn format(name: &str, output: bool) -> Result<StreamFormat> {
        let devs = pactl(if output { "sinks" } else { "sources" });
        let d = devs.iter().find(|d| d.name == name).ok_or_else(|| anyhow!("dispositivo non trovato: {name}"))?;
        Ok(StreamFormat { channels: d.channels.clamp(1, 8), rate: d.rate })
    }

    pub struct PulseStream {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for PulseStream {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn pa_err(e: PAErr) -> String {
        e.to_string().unwrap_or_else(|| format!("errore PulseAudio {}", e.0))
    }

    fn as_bytes(v: &[f32]) -> &[u8] {
        // SAFETY: f32 has no padding/invalid bit patterns; length is exact.
        unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
    }

    fn as_bytes_mut(v: &mut [f32]) -> &mut [u8] {
        // SAFETY: any byte pattern is a valid f32.
        unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, std::mem::size_of_val(v)) }
    }

    type Body = Box<dyn FnMut(&Simple, &mut [f32]) -> Result<(), String> + Send>;

    fn spawn(
        name: String,
        fmt: StreamFormat,
        dir: Direction,
        mut body: Body,
        err: ErrorCallback,
    ) -> Result<PulseStream> {
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let thread = std::thread::Builder::new().name("multiaudio-pulse".into()).spawn(move || {
            let spec = Spec { format: Format::F32le, channels: fmt.channels as u8, rate: fmt.rate };
            let chunk = (fmt.rate as usize / 200).max(64); // 5 ms
            let bytes = (chunk * fmt.channels * 4) as u32;
            let attr = BufferAttr {
                maxlength: u32::MAX,
                tlength: bytes * 6,
                prebuf: u32::MAX,
                minreq: bytes,
                fragsize: bytes,
            };
            let s = match Simple::new(None, "MultiAudio", dir, Some(&name), "MultiAudio", &spec, None, Some(&attr)) {
                Ok(s) => {
                    let _ = tx.send(Ok(()));
                    s
                }
                Err(e) => {
                    let _ = tx.send(Err(pa_err(e)));
                    return;
                }
            };
            let mut buf = vec![0f32; chunk * fmt.channels];
            while !st.load(Ordering::Relaxed) {
                if let Err(e) = body(&s, &mut buf) {
                    err(e);
                    break;
                }
            }
        })?;
        match rx.recv() {
            Ok(Ok(())) => Ok(PulseStream { stop, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                bail!("PipeWire/Pulse: {e}")
            }
            Err(_) => bail!("PipeWire/Pulse: thread terminato"),
        }
    }

    pub fn open_input(name: String, fmt: StreamFormat, mut cb: InputCallback, err: ErrorCallback) -> Result<PulseStream> {
        spawn(
            name,
            fmt,
            Direction::Record,
            Box::new(move |s, buf| {
                s.read(as_bytes_mut(buf)).map_err(pa_err)?;
                cb(buf);
                Ok(())
            }),
            err,
        )
    }

    pub fn open_output(name: String, fmt: StreamFormat, mut cb: OutputCallback, err: ErrorCallback) -> Result<PulseStream> {
        spawn(
            name,
            fmt,
            Direction::Playback,
            Box::new(move |s, buf| {
                cb(buf);
                s.write(as_bytes(buf)).map_err(pa_err)
            }),
            err,
        )
    }
}

#[cfg(all(test, local_tests))]
#[path = "local_tests/backend.rs"]
mod tests;
