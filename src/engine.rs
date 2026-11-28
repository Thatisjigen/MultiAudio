//! Audio engine.
//!
//! One input stream per distinct input device writes into a ring buffer.
//! Every route owns a `Reader` that lives inside the output stream of its
//! output device. The reader keeps its read position exactly `delay + safety`
//! behind the writer: a PI controller slightly speeds up / slows down the read
//! rate (±0.4 % max, normally a few ppm) to absorb the clock drift between
//! devices, and the windowed-sinc interpolator makes that inaudible. Changing
//! the delay is click-free (short fade out → jump → fade in).

use crate::backend::{self, DeviceList, ErrorCallback, StreamFormat, StreamHandle};
use crate::config::Config;
use crate::dsp::{make_click, make_probe, now_ns, upmix, AtomicF32, Ring, SincTable, Upmix, HALF};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering::*};
use std::sync::{Arc, Mutex, OnceLock};

pub const MAX_DELAY_MS: f32 = 3000.0;
/// Rear outputs that play the surround matrix play it a little later than
/// the front (precedence effect: the sound image stays in front, the rear
/// only adds space). Those that play the stereo as it is play it with the
/// front (`config::StereoRear`, `RouteCtl::extra_delay_ms`).
pub const REAR_DELAY_MS: f32 = 15.0;
/// The bass of the front that a rear output which plays everything gets
/// too: up to here (Hz).
const BASS_HZ: f64 = 100.0;
/// The centre speaker shares with the front pair only what is above this
/// (Hz), also when it plays everything: the bass in the middle of a stereo
/// source stays whole on the front speakers, which are the ones that play it.
pub const CENTRE_MIN_HZ: f32 = 100.0;
/// How long the voices of a stereo source take to move between the centre
/// speaker and the front outputs when `Voices` is changed, s.
const VOICES_MOVE_S: f64 = 0.05;
/// LFE channel level in each front channel (5.1 plays it louder than the
/// others: +6 dB against the plain −6 dB downmix).
const LFE_GAIN: f32 = 1.0;
/// "MultiAudio" always has its six channels and nothing says whether what
/// is playing is 5.1 or stereo: a source is taken as 5.1 as soon as its
/// centre, LFE or surround channels carry sound. It stays so until the
/// front pair has played alone for this long (time without any sound does
/// not count: a paused film is still that film). Films have such stretches
/// (music only), and the rear and centre speakers must not change what
/// they play in the middle of one.
const DISCRETE_HOLD_S: f64 = 60.0;
/// That long only for a source that has shown to be one: the front pair
/// alone is waited for this many times as long as those channels have
/// carried sound, DISCRETE_HOLD_S at most (a film gets there in its first
/// quarter of a minute) and never less than DISCRETE_HOLD_MIN_S. Seen on
/// 2026-11-27, twice: a browser that starts a stream on the six-channel
/// output plays its first second on all of them and then on the front pair
/// only, and for the next minute of a stereo film the centre and the rear
/// speakers were silent.
const DISCRETE_TRUST: f64 = 4.0;
const DISCRETE_HOLD_MIN_S: f64 = 2.0;
/// A silence this long may be the end of a source: when sound comes back,
/// who is playing tells (see `new_source`).
const DISCRETE_RESET_S: f64 = 3.0;
/// The centre, LFE or surround channels must carry sound for this long, in
/// one stretch, before the source is taken as 5.1: one stray sample or a
/// click there (seen from capture devices when a player starts or stops)
/// would otherwise leave the rear and centre speakers playing those empty
/// channels, silent, for the next DISCRETE_HOLD_S.
const DISCRETE_AFTER_S: f64 = 0.05;

/// After a silence, is what plays now another source than the 5.1 one of
/// before? `owners`: the processes that were playing while the 5.1
/// channels carried sound; `now`: those playing now. Another program
/// playing means a new source, which starts as stereo (upmix) until its
/// other channels carry sound; the same ones mean the film went on. When
/// the system does not tell who plays, a silence is taken as the end.
fn new_source(owners: Option<&[i32]>, now: Option<&[i32]>) -> bool {
    match (owners, now) {
        (Some(owners), Some(now)) => now.iter().any(|p| !owners.contains(p)),
        _ => true,
    }
}
/// Output limiter threshold (5.1 fold-downs can exceed full scale).
const LIMIT: f32 = 0.98;
pub const MAX_SAFETY_MS: f32 = 300.0;
const MAX_CORR: f64 = 0.004;
/// PI gains, normalised by the input rate (ζ≈1, ω≈0.2 rad/s).
const KP: f64 = 0.4;
const KI: f64 = 0.04;
const ERR_SMOOTH_S: f64 = 0.5;
const FADE_S: f64 = 0.006;
/// Delay changes larger than this jump (with a fade); smaller ones glide.
const JUMP_ON_CHANGE_MS: f32 = 2.0;
const MAX_RING_CHANNELS: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ReaderState {
    Waiting = 0,
    Running = 1,
    FadedOut = 2,
}

/// A stretch of what an output is sent, kept for a diagnosis (see
/// `record`): the audio thread writes it a frame at a time, with no lock
/// and no allocation; made and read by other threads.
pub struct Tape {
    buf: Box<[AtomicU32]>,
    len: AtomicUsize,
    on: AtomicBool,
}

impl Tape {
    /// Room for `frames` stereo frames.
    pub fn new(frames: usize) -> Self {
        Self { buf: (0..frames * 2).map(|_| AtomicU32::new(0)).collect(), len: AtomicUsize::new(0), on: AtomicBool::new(false) }
    }

    pub fn start(&self) {
        self.on.store(false, Release);
        self.len.store(0, Release);
        self.on.store(true, Release);
    }

    /// Stops it and gives what was written (left, right, left, ...).
    pub fn stop(&self) -> Vec<f32> {
        self.on.store(false, Release);
        let n = self.len.load(Acquire).min(self.buf.len());
        self.buf[..n].iter().map(|a| f32::from_bits(a.load(Relaxed))).collect()
    }

    #[inline]
    fn push(&self, l: f32, r: f32) {
        let i = self.len.load(Relaxed);
        if i + 2 <= self.buf.len() {
            self.buf[i].store(l.to_bits(), Relaxed);
            self.buf[i + 1].store(r.to_bits(), Relaxed);
            self.len.store(i + 2, Release);
        }
    }
}

/// Shared between the GUI and one route's real-time reader.
pub struct RouteCtl {
    pub delay_ms: AtomicF32,
    pub gain: AtomicF32,
    /// System volume (volume keys) applied to this output in software, see
    /// OwnVolume.
    pub sys_gain: AtomicF32,
    /// This output's own volume (0..1, slider position), see OwnVolume.
    pub level: AtomicF32,
    /// Turned down by this factor so that it is not louder than the front
    /// outputs (`level_trims`); 1: as it is.
    pub trim: AtomicF32,
    /// Surround: play the rear channels (discrete 5.1 or upmix).
    pub rear: AtomicBool,
    /// Some output plays the rear: front outputs then leave the surround
    /// channels of a 5.1 source to it instead of folding them in.
    pub has_rear: AtomicBool,
    /// Surround: this output is the centre speaker (the real centre channel
    /// of a 5.1 source, or what a stereo one has at the centre).
    pub centre: AtomicBool,
    /// Some output is the centre speaker: the others leave the centre
    /// channel of a 5.1 source to it.
    pub has_centre: AtomicBool,
    /// Stereo source: the centre speaker plays what left and right have in
    /// common as well; the front outputs play it either way (see
    /// `Config::stereo_centre`).
    pub voices_centre: AtomicBool,
    /// Stereo source: a rear output plays it as it is, together with the
    /// front (see `config::StereoRear`), not the surround matrix.
    pub rear_all: AtomicBool,
    /// Below this frequency (Hz) this output plays nothing; 0: everything.
    /// (A rear output that plays everything also gets the front's bass,
    /// aligned with the front, without the rear's extra delay, so it adds
    /// to the sub instead of cancelling.)
    pub xo_hz: AtomicF32,
    /// This output plays the bass the others leave: the LFE channel, and
    /// what the centre and rear speakers do not play of their channels.
    pub sub: AtomicBool,
    /// Below these frequencies (Hz) the centre and the rear speakers leave
    /// their channels to the outputs with `sub`; 0: nothing is left.
    pub centre_xo: AtomicF32,
    pub rear_xo: AtomicF32,
    /// What this output is sent, while a diagnosis is being recorded.
    pub tape: OnceLock<Tape>,
    /// Muted by the calibration while it measures.
    pub cal_mute: AtomicBool,
    pub probe_req: AtomicBool,
    pub probe_done: AtomicBool,
    /// Input-timeline position (f64 bits) where the last probe started.
    pub probe_pos: AtomicU64,
    // ---- stats (written by the audio thread) ----
    pub st_state: AtomicU8,
    pub st_buffer_ms: AtomicF32,
    pub st_ppm: AtomicF32,
    pub st_underruns: AtomicU32,
    pub st_peak: AtomicF32,
    /// Times the reader (re)locked its position: start, after an underrun,
    /// after a big delay change.
    pub st_jumps: AtomicU32,
    /// Playing discrete 5.1 surround channels (not the upmix).
    pub st_discrete: AtomicBool,
    /// Output frames rendered (to detect sample-rate changes).
    pub st_frames: AtomicU64,
    /// Smoothed buffer the callbacks actually need, in ms.
    pub st_need_ms: AtomicF32,
    /// Output callbacks that came late (the device probably played a gap).
    pub st_late: Glitches,
    pub out_rate: AtomicU32,
}

impl RouteCtl {
    /// How long after the front this output plays, on top of its delay
    /// (see `REAR_DELAY_MS`).
    pub fn extra_delay_ms(&self) -> f32 {
        if self.rear.load(Relaxed) && !self.centre.load(Relaxed) && !self.rear_all.load(Relaxed) {
            REAR_DELAY_MS
        } else {
            0.0
        }
    }

    pub(crate) fn new(delay_ms: f32, gain: f32) -> Self {
        Self {
            delay_ms: AtomicF32::new(delay_ms),
            gain: AtomicF32::new(gain),
            sys_gain: AtomicF32::new(1.0),
            level: AtomicF32::new(1.0),
            trim: AtomicF32::new(1.0),
            rear: AtomicBool::new(false),
            has_rear: AtomicBool::new(false),
            centre: AtomicBool::new(false),
            has_centre: AtomicBool::new(false),
            voices_centre: AtomicBool::new(false),
            rear_all: AtomicBool::new(false),
            xo_hz: AtomicF32::new(0.0),
            sub: AtomicBool::new(true),
            centre_xo: AtomicF32::new(0.0),
            rear_xo: AtomicF32::new(0.0),
            tape: OnceLock::new(),
            cal_mute: AtomicBool::new(false),
            probe_req: AtomicBool::new(false),
            probe_done: AtomicBool::new(false),
            probe_pos: AtomicU64::new(f64::NAN.to_bits()),
            st_state: AtomicU8::new(ReaderState::Waiting as u8),
            st_buffer_ms: AtomicF32::new(0.0),
            st_ppm: AtomicF32::new(0.0),
            st_underruns: AtomicU32::new(0),
            st_peak: AtomicF32::new(0.0),
            st_jumps: AtomicU32::new(0),
            st_discrete: AtomicBool::new(false),
            st_frames: AtomicU64::new(0),
            st_need_ms: AtomicF32::new(0.0),
            st_late: Glitches::new(),
            out_rate: AtomicU32::new(0),
        }
    }

    pub fn state(&self) -> ReaderState {
        match self.st_state.load(Relaxed) {
            1 => ReaderState::Running,
            2 => ReaderState::FadedOut,
            _ => ReaderState::Waiting,
        }
    }
}

/// Hiccups of the audio flow (diagnostics: each one is a probable crackle).
pub struct Glitches {
    pub count: AtomicU32,
    /// Worst one, ms.
    pub max_ms: AtomicF32,
}

impl Glitches {
    fn new() -> Self {
        Self { count: AtomicU32::new(0), max_ms: AtomicF32::new(0.0) }
    }
    #[inline]
    fn note(&self, ms: f32) {
        self.count.fetch_add(1, Relaxed);
        self.max_ms.fetch_max(ms);
    }
}

/// A callback came clearly later than the previous one implied: the
/// device (or the system) missed a beat. Returns how late, in ms.
#[inline]
fn late_ms(last_ns: u64, now: u64, expected_s: f64) -> Option<f32> {
    if last_ns == 0 || expected_s <= 0.0 {
        return None;
    }
    let gap = now.saturating_sub(last_ns) as f64 * 1e-9;
    (gap > 2.0 * expected_s + 0.010).then_some(((gap - expected_s) * 1000.0) as f32)
}

pub struct InputShared {
    pub id: String,
    pub ring: Ring,
    pub rate: f64,
    pub dev_channels: usize,
    pub peak: AtomicF32,
    /// Peak per channel (diagnostics).
    pub ch_peak: Vec<AtomicF32>,
    /// Channels that carried sound since the monitor last looked (a bit each).
    ch_heard: AtomicU32,
    /// Only one writer at a time: the device callback, or the silence
    /// generator while the device sends nothing.
    writer: AtomicBool,
    /// Time of the last callback from the real device.
    last_real_ns: AtomicU64,
    /// Silence generator active (the source is not sending anything).
    pub filling: AtomicBool,
    /// The test click is on: every output must play it (see `Reader::render`).
    pub clicking: AtomicBool,
    /// Last time the source carried sound (above −60 dBFS).
    last_loud_ns: AtomicU64,
    /// Goes up when what plays is another source than before (see
    /// `new_source`): what was known about it (5.1 or stereo) is dropped.
    source_epoch: AtomicU32,
    /// Capture callbacks that came late.
    pub late: Glitches,
    /// The capture stopped in the middle of the music and silence was put
    /// in its place (ms of silence).
    pub holes: Glitches,
    /// Pieces missing from the capture (`Holes`: short stretches of exact
    /// zeros between sound and sound), how many frames they add up to, and
    /// the blocks received. The system handed over only part of what a
    /// program played (see `mactap::Tuning`).
    pub torn: AtomicU32,
    pub torn_frames: AtomicU64,
    pub blocks: AtomicU64,
    click: Vec<f32>,
}

impl InputShared {
    /// Did the source carry sound in the last `ns` nanoseconds?
    pub fn loud_within(&self, ns: u64) -> bool {
        let loud = self.last_loud_ns.load(Relaxed);
        loud > 0 && now_ns().saturating_sub(loud) < ns
    }

    pub fn new(id: String, frames: usize, ring_channels: usize, rate: f64, dev_channels: usize) -> Self {
        Self {
            id,
            ring: Ring::new(frames, ring_channels),
            rate,
            dev_channels,
            peak: AtomicF32::new(0.0),
            ch_peak: (0..ring_channels.max(1)).map(|_| AtomicF32::new(0.0)).collect(),
            ch_heard: AtomicU32::new(0),
            writer: AtomicBool::new(false),
            last_real_ns: AtomicU64::new(0),
            filling: AtomicBool::new(false),
            clicking: AtomicBool::new(false),
            last_loud_ns: AtomicU64::new(0),
            source_epoch: AtomicU32::new(0),
            late: Glitches::new(),
            holes: Glitches::new(),
            torn: AtomicU32::new(0),
            torn_frames: AtomicU64::new(0),
            blocks: AtomicU64::new(0),
            click: make_click(rate),
        }
    }

    #[inline]
    fn lock(&self) {
        while self.writer.compare_exchange_weak(false, true, Acquire, Relaxed).is_err() {
            std::hint::spin_loop();
        }
    }

    #[inline]
    fn try_lock(&self) -> bool {
        self.writer.compare_exchange(false, true, Acquire, Relaxed).is_ok()
    }

    #[inline]
    fn unlock(&self) {
        self.writer.store(false, Release);
    }

    #[inline]
    fn click_at(&self, frame: u64) -> f32 {
        let period = self.rate.round().max(1.0) as u64;
        self.click.get((frame % period) as usize).copied().unwrap_or(0.0)
    }
}

/// Keeps an input's timeline moving while its device sends nothing (macOS
/// system-audio capture and Windows loopback go quiet when nothing plays):
/// it writes silence at the nominal rate, so the outputs stay locked and the
/// test click and the calibration work without music.
struct Filler {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Filler {
    fn spawn(inp: Arc<InputShared>, test_click: Arc<AtomicBool>, click_at_input: Arc<AtomicBool>) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let thread = std::thread::Builder::new()
            .name("multiaudio-filler".into())
            .spawn(move || {
                let mut origin: Option<(u64, u64)> = None;
                // filling started right after sound (a hiccup, not a pause)
                let mut mid_music = false;
                while !st.load(Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(4));
                    let now = now_ns();
                    let last_real = inp.last_real_ns.load(Relaxed);
                    let (_, _, chunk) = inp.ring.snapshot();
                    let chunk_ns = (chunk as f64 / inp.rate * 1e9) as u64;
                    // While music plays, a source that stops for a moment is
                    // a hiccup: give it time before putting silence in (the
                    // outputs' buffer covers it); after quiet, fill quickly
                    // so the outputs stay locked (test click, calibration).
                    let playing = last_real.saturating_sub(inp.last_loud_ns.load(Relaxed)) < 300_000_000;
                    let wait_ns = if playing { 250_000_000u64 } else { 40_000_000 };
                    let quiet_after = wait_ns.max(3 * chunk_ns);
                    if now.saturating_sub(last_real) < quiet_after {
                        if let Some((o_ns, _)) = origin.take() {
                            inp.filling.store(false, Relaxed);
                            // a short fill right after sound: a hole in the music
                            let ms = (now.saturating_sub(o_ns) as f64 * 1e-6) as f32;
                            if mid_music && ms < 2000.0 {
                                inp.holes.note(ms);
                            }
                        }
                        continue;
                    }
                    if !inp.try_lock() {
                        continue;
                    }
                    let pos = inp.ring.write_pos();
                    // Continue the timeline from the last real data, as if the
                    // device had kept sending silence.
                    if origin.is_none() {
                        mid_music = playing && last_real > 0;
                    }
                    let (o_ns, o_pos) = *origin.get_or_insert_with(|| {
                        inp.filling.store(true, Relaxed);
                        (if last_real > 0 { last_real } else { now }, pos)
                    });
                    let due = o_pos + (now.saturating_sub(o_ns) as f64 * 1e-9 * inp.rate) as u64;
                    if due > pos {
                        let n = (due - pos).min(inp.rate as u64);
                        inp.clicking.store(test_click.load(Relaxed), Relaxed);
                        let click_on = test_click.load(Relaxed) && click_at_input.load(Relaxed);
                        let cs = inp.ring.channels();
                        let mut peak = 0f32;
                        for f in pos..pos + n {
                            let v = if click_on { inp.click_at(f) } else { 0.0 };
                            peak = peak.max(v.abs());
                            for c in 0..cs {
                                // the click: front pair only (every output plays it)
                                inp.ring.put(f, c, if c < 2 { v } else { 0.0 });
                            }
                        }
                        inp.ring.publish(pos + n, now, n);
                        inp.peak.fetch_max(peak);
                    }
                    inp.unlock();
                }
            })
            .ok()?;
        Some(Self { stop, thread: Some(thread) })
    }
}

impl Drop for Filler {
    fn drop(&mut self) {
        self.stop.store(true, SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Common buffer added to every route. `min_ms` is raised by the monitor
/// thread when the callbacks need more (from smoothed sizes, never from a
/// single odd callback) or when dropouts repeat; every route uses the same
/// effective value so their relative alignment is preserved.
pub struct Safety {
    pub user_ms: AtomicF32,
    pub min_ms: AtomicF32,
    /// MultiAudio's own output (macOS) declares a fixed latency to players:
    /// every output then plays this long after the capture (plus its own
    /// delay), so the sound comes out exactly when the picture is shown,
    /// whatever the safety buffer. NaN: none.
    pub anchor_ms: AtomicF32,
}

impl Safety {
    pub fn new(user_ms: f32) -> Self {
        Self { user_ms: AtomicF32::new(user_ms), min_ms: AtomicF32::new(0.0), anchor_ms: AtomicF32::new(f32::NAN) }
    }
    /// Buffer the devices need (user setting or measured minimum).
    #[inline]
    pub fn effective_ms(&self) -> f32 {
        self.user_ms.load().clamp(0.0, MAX_SAFETY_MS).max(self.min_ms.load())
    }
    /// Buffer common to every output: the anchor, unless the devices need more.
    #[inline]
    pub fn common_ms(&self) -> f32 {
        let s = self.effective_ms();
        let a = self.anchor_ms.load();
        if a.is_finite() {
            // (the ring holds MAX_DELAY_MS + MAX_SAFETY_MS + 1 s)
            a.clamp(0.0, MAX_SAFETY_MS + 500.0).max(s)
        } else {
            s
        }
    }
}

/// Control of the optional reference output (played by the OS).
pub struct RefCtl {
    pub probe_req: AtomicBool,
    pub probe_done: AtomicBool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RouteStatus {
    Disabled,
    Incomplete,
    Running,
    Missing(String),
    Error(String),
}

/// One route as seen by the monitor thread.
struct MonRoute {
    name: String,
    output: String,
    /// Bluetooth output: it may ignore its volume while nothing plays.
    bluetooth: bool,
    ctl: Arc<RouteCtl>,
}

/// The volume on macOS while MultiAudio is on: it is MultiAudio's (volume
/// keys, the app's slider). Each output plays at that volume times its own
/// (its slider in the app), set on the device itself when it has a volume
/// control (the device's volume in the Mac moves), in software otherwise.
/// A change made from outside on the system output (menu bar, its own
/// buttons, the keys without the Accessibility permission) moves
/// MultiAudio's volume, so the other outputs follow it.
///
/// A Bluetooth speaker ignores a volume given while nothing plays (it works
/// only with sound going), and the Mac does not send it again when sound
/// starts: so a Bluetooth output given its volume in silence (or when
/// MultiAudio starts) gets it once more after a second of sound. Meanwhile
/// what it reports is not taken as a change from outside.
#[derive(Default)]
struct OwnVolume {
    /// (volume generation, outputs' own volumes) last applied
    applied: Option<(u64, Vec<u32>)>,
    /// output → the volume MultiAudio gave its device, None: no volume
    /// control (software)
    hw: HashMap<String, Option<DevTarget>>,
    /// Bluetooth outputs to give their volume again once sound plays.
    pending: Vec<String>,
    /// Sound has been playing since then.
    sound_since: Option<std::time::Instant>,
    /// mute of the system output seen last
    sys_muted: Option<bool>,
    /// what each device reported last (for the log)
    reported: HashMap<String, f32>,
}

#[derive(Clone, Copy, Debug)]
struct DevTarget {
    target: f32,
    sent: std::time::Instant,
}

/// Was the volume of a device changed from outside? Judged only where it
/// is reliable (sound playing, for Bluetooth), not right after MultiAudio
/// set it, and not while it still has to get it again.
fn moved_from_outside(t: &DevTarget, reported: f32, reliable: bool, pending: bool, since_sent: std::time::Duration) -> bool {
    reliable && !pending && since_sent >= std::time::Duration::from_millis(1500) && (reported - t.target).abs() > 0.02
}

/// How long the slightly different volume of `resend_volume` stays before
/// the right one follows. A speaker with few volume steps (Android has 15)
/// takes 99 % as a whole step down, and the Echo Show 5 answered each value
/// about 100 ms late: with 30 ms between the two its answer to the first
/// sometimes came after the second, and it kept the lower step (2026-11-26).
const RESEND_GAP: std::time::Duration = std::time::Duration::from_millis(250);

/// Gives a device its volume again. If the Mac thinks it already has it,
/// it would not pass it on: a slightly different value goes first.
fn resend_volume(id: &str, target: f32) {
    if backend::output_volume(id).is_some_and(|(s, _)| (s - target).abs() < 0.005) {
        let nudge = if target >= 0.01 { target - 0.01 } else { target + 0.01 };
        backend::set_output_volume(id, nudge);
        std::thread::sleep(RESEND_GAP);
    }
    backend::set_output_volume(id, target);
}

/// Does a Bluetooth output have to get its volume again once sound plays?
/// When it was given in silence (or at start) and it was a change: only a
/// change can have been missed. The engine restarts often, always with the
/// same volume: giving it again each time was a step down and up on a
/// speaker with few steps, and its volume shown on its screen, for nothing.
/// `had`: the volume the Mac had for the device before.
fn resend_with_sound(bluetooth: bool, in_silence: bool, had: Option<f32>, target: f32) -> bool {
    bluetooth && in_silence && had.is_none_or(|h| (h - target).abs() > 0.005)
}

fn pct(v: f32) -> String {
    format!("{:.0} %", v * 100.0)
}

/// MultiAudio's volume after a device it follows was moved to `slider`
/// (`own`: that device's own volume).
fn follow_device(slider: f32, own: f32) -> f32 {
    if own < 0.05 {
        slider
    } else {
        (slider / own).clamp(0.0, 1.0)
    }
}

impl OwnVolume {
    /// `sound`: sound is coming out (the source is above −60 dBFS).
    fn tick(&mut self, routes: &[MonRoute], own_is_system: bool, sound: bool) {
        let now = std::time::Instant::now();
        if !sound {
            self.sound_since = None;
        } else if self.sound_since.is_none() {
            self.sound_since = Some(now);
        }
        // a second of sound: it has reached the speakers, which take the
        // volume now
        if self.sound_since.is_some_and(|t| now - t >= std::time::Duration::from_secs(1)) {
            for id in std::mem::take(&mut self.pending) {
                if let Some(Some(t)) = self.hw.get_mut(&id) {
                    let was = backend::output_volume(&id).map(|v| pct(v.0)).unwrap_or_default();
                    resend_volume(&id, t.target);
                    t.sent = std::time::Instant::now();
                    crate::diag::log(format!("volume: c'è suono, ridò {} a «{}» (il Mac diceva {was})", pct(t.target), backend::display_name(&id)));
                }
            }
        }

        // changes made from outside are followed on the system output (or,
        // when that is MultiAudio's output, on the first device with a
        // volume control)
        let sys = if own_is_system { None } else { backend::system_volume() };
        let lead = match &sys {
            Some(v) => {
                let key = backend::physical_key(&v.device);
                routes.iter().find(|r| backend::physical_key(&r.output) == key)
            }
            None => routes.iter().find(|r| matches!(self.hw.get(&r.output), Some(Some(_)))),
        };
        let mut moved = None;
        for r in routes {
            let Some(Some(t)) = self.hw.get_mut(&r.output) else { continue };
            let Some((s, _)) = backend::output_volume(&r.output) else { continue };
            let pending = self.pending.contains(&r.output);
            let last = self.reported.insert(r.output.clone(), s);
            if last.is_none_or(|l| (l - s).abs() > 0.005) && (s - t.target).abs() > 0.02 {
                crate::diag::log(format!(
                    "volume: «{}» dice {} (MultiAudio gli ha dato {}){}",
                    backend::display_name(&r.output),
                    pct(s),
                    pct(t.target),
                    if sound { "" } else { ", nessun suono" }
                ));
            }
            if moved_from_outside(t, s, sound || !r.bluetooth, pending, now - t.sent) {
                t.target = s;
                if lead.is_some_and(|l| l.output == r.output) {
                    moved = Some((s, r.ctl.level.load().clamp(0.0, 1.0)));
                }
            }
        }
        if let (Some((s, own)), Some(r)) = (moved, lead) {
            let v = follow_device(s, own);
            crate::diag::log(format!("volume: «{}» cambiato da fuori a {}: MultiAudio va a {}", backend::display_name(&r.output), pct(s), pct(v)));
            crate::volume::set_level(v);
        }
        if let Some(v) = &sys {
            if self.sys_muted.is_some_and(|m| m != v.muted) {
                let (level, _, _) = crate::volume::get();
                crate::volume::set(level, v.muted);
            }
            self.sys_muted = Some(v.muted);
        }

        let (level, muted, gen) = crate::volume::get();
        let owns: Vec<u32> = routes.iter().map(|r| r.ctl.level.load().to_bits()).collect();
        if self.applied.as_ref() != Some(&(gen, owns.clone())) {
            let first = self.applied.is_none();
            self.applied = Some((gen, owns));
            let mut done: HashMap<&str, bool> = HashMap::new();
            let mut parts = Vec::new();
            for r in routes {
                let target = level * r.ctl.level.load().clamp(0.0, 1.0);
                let hw = *done.entry(r.output.as_str()).or_insert_with(|| {
                    let had = backend::output_volume(&r.output).map(|v| v.0);
                    let ok = backend::set_output_volume(&r.output, target);
                    self.hw.insert(r.output.clone(), ok.then_some(DevTarget { target, sent: now }));
                    let again = ok && resend_with_sound(r.bluetooth, first || !sound, had, target);
                    self.pending.retain(|p| *p != r.output);
                    if again {
                        self.pending.push(r.output.clone());
                    }
                    parts.push(format!(
                        "«{}» {}{}",
                        backend::display_name(&r.output),
                        pct(target),
                        if !ok {
                            " (software)"
                        } else if again {
                            " (ridato quando c'è suono)"
                        } else {
                            ""
                        }
                    ));
                    ok
                });
                let g = if muted {
                    0.0
                } else if hw {
                    1.0
                } else {
                    crate::volume::curve(target)
                };
                r.ctl.sys_gain.store(g);
            }
            crate::diag::log(format!("volume {}{}: {}", pct(level), if muted { " (muto)" } else { "" }, parts.join(", ")));
        }
    }
}

/// Background thread (never in the audio path) that
/// * makes outputs fed by MultiAudio follow the system volume (captured audio
///   is taken before the system applies it; the output the system itself
///   plays to already gets it, so it is left alone), or MultiAudio's own
///   volume when its output is the system output;
/// * raises the common safety buffer when really needed;
/// * notices a device changing sample rate and asks for a restart;
/// * writes what happens to the diagnostic log.
struct Monitor {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct RateWatch {
    last: Option<(u64, f64)>,
    ratios: Vec<f64>,
}

impl RateWatch {
    /// Feeds (frames counter, seconds); returns Some(ratio) when 3 windows in a
    /// row agree on a rate more than 3 % away from the nominal one.
    fn feed(&mut self, frames: u64, t: f64, nominal: f64, valid: bool) -> Option<f64> {
        let prev = self.last.replace((frames, t));
        if !valid {
            self.ratios.clear();
            return None;
        }
        let (f0, t0) = prev?;
        let dt = t - t0;
        if dt < 1.0 || frames < f0 {
            return None;
        }
        let ratio = (frames - f0) as f64 / dt / nominal;
        self.ratios.push(ratio);
        if self.ratios.len() > 3 {
            self.ratios.remove(0);
        }
        let consistent = self.ratios.len() == 3
            && self.ratios.iter().all(|r| (r - 1.0).abs() > 0.03)
            && self.ratios.iter().all(|r| (r / self.ratios[0] - 1.0).abs() < 0.01);
        consistent.then_some(ratio)
    }
}

impl Monitor {
    fn spawn(
        routes: Vec<MonRoute>,
        inputs: Vec<Arc<InputShared>>,
        safety: Arc<Safety>,
        failed: Arc<AtomicBool>,
        errors: Arc<Mutex<Vec<String>>>,
    ) -> Option<Self> {
        if routes.is_empty() {
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let thread = std::thread::Builder::new()
            .name("multiaudio-monitor".into())
            .spawn(move || {
                let t0 = std::time::Instant::now();
                let vol_every = if cfg!(target_os = "linux") { 6 } else { 1 };
                let mut tick: u64 = 0;
                let mut in_watch: Vec<RateWatch> = inputs.iter().map(|_| RateWatch::default()).collect();
                let mut out_watch: Vec<RateWatch> = routes.iter().map(|_| RateWatch::default()).collect();
                let mut prev_state: Vec<u8> = vec![255; routes.len()];
                let mut prev_jumps: Vec<u32> = vec![0; routes.len()];
                let mut under_hist: Vec<Vec<u32>> = vec![Vec::new(); routes.len()];
                let mut reported = false;
                let mut was_filling: Vec<bool> = vec![false; inputs.len()];
                let mut heard: Vec<u32> = vec![0; inputs.len()];
                // per input: 5.1 sound was heard, who was playing then, and
                // a long silence came after
                let mut had_discrete: Vec<bool> = vec![false; inputs.len()];
                let mut owners: Vec<Option<Vec<i32>>> = vec![None; inputs.len()];
                let mut long_quiet: Vec<bool> = vec![false; inputs.len()];
                let mut own = OwnVolume::default();
                let mut seen_in: Vec<(u32, u32)> = vec![(0, 0); inputs.len()];
                let mut seen_torn: Vec<(u32, u64, u64)> = vec![(0, 0, 0); inputs.len()];
                let mut seen_out: Vec<u32> = vec![0; routes.len()];
                let mut was_discrete = false;
                while !st.load(Relaxed) {
                    if tick.is_multiple_of(vol_every) && cfg!(target_os = "macos") {
                        // the volume is MultiAudio's (keys included) while it is on
                        crate::volume::set_active(true);
                        let now = now_ns();
                        let sound = inputs.iter().any(|i| {
                            let loud = i.last_loud_ns.load(Relaxed);
                            loud > 0 && now.saturating_sub(loud) < 300_000_000
                        });
                        own.tick(&routes, backend::system_output_is_vout(), sound);
                    } else if tick.is_multiple_of(vol_every) {
                        if let Some(v) = backend::system_volume() {
                            let dev = backend::physical_key(&v.device);
                            for r in &routes {
                                let g = if v.muted {
                                    0.0
                                } else if backend::physical_key(&r.output) == dev {
                                    1.0
                                } else {
                                    v.gain
                                };
                                r.ctl.sys_gain.store(g * crate::volume::curve(r.ctl.level.load()));
                            }
                        }
                    }
                    // every 2 s
                    if tick % 20 == 10 && !reported {
                        let t = t0.elapsed().as_secs_f64();
                        let now = now_ns();
                        let mut alive_any = false;
                        // hiccups since the last look (each one: a probable crackle)
                        for (i, inp) in inputs.iter().enumerate() {
                            let (late, holes) = (inp.late.count.load(Relaxed), inp.holes.count.load(Relaxed));
                            if late != seen_in[i].0 {
                                crate::diag::log(format!(
                                    "intoppo: l'audio catturato da «{}» è arrivato in ritardo {} volte (peggiore {:.0} ms, totale {late})",
                                    backend::display_name(&inp.id),
                                    late - seen_in[i].0,
                                    inp.late.max_ms.load()
                                ));
                            }
                            if holes != seen_in[i].1 {
                                crate::diag::log(format!(
                                    "intoppo: la cattura di «{}» si è fermata durante la musica, inserito silenzio {} volte (peggiore {:.0} ms, totale {holes})",
                                    backend::display_name(&inp.id),
                                    holes - seen_in[i].1,
                                    inp.holes.max_ms.load()
                                ));
                            }
                            seen_in[i] = (late, holes);
                            // (told every 10 s at most: it is every block, or none)
                            let (torn, frames, blocks) = (inp.torn.load(Relaxed), inp.torn_frames.load(Relaxed), inp.blocks.load(Relaxed));
                            if tick % 100 == 10 {
                                let (k, z, m) = (torn - seen_torn[i].0, frames - seen_torn[i].1, blocks - seen_torn[i].2);
                                // (a tear is in every block of the program that plays, tens a second;
                                // a few in ten seconds are short silences of the programme)
                                if k >= 20 {
                                    let (_, _, chunk) = inp.ring.snapshot();
                                    crate::diag::log(format!(
                                        "⚠ «{}»: il suono arriva a pezzi: {k} pezzi mancanti in {m} blocchi da {chunk} (in media {} campioni l'uno). Un programma suona in blocchi troppo grandi per questa cattura ({}).",
                                        backend::display_name(&inp.id),
                                        z / k.max(1) as u64,
                                        backend::capture_tuning()
                                    ));
                                }
                                seen_torn[i] = (torn, frames, blocks);
                            }
                        }
                        for (i, r) in routes.iter().enumerate() {
                            let late = r.ctl.st_late.count.load(Relaxed);
                            if late != seen_out[i] {
                                crate::diag::log(format!(
                                    "intoppo: il sistema ha chiamato l'uscita «{}» in ritardo {} volte (peggiore {:.0} ms, totale {late})",
                                    r.name,
                                    late - seen_out[i],
                                    r.ctl.st_late.max_ms.load()
                                ));
                                seen_out[i] = late;
                            }
                        }
                        // 5.1 or stereo, as the outputs take the source now
                        let discrete = routes.iter().any(|r| r.ctl.st_discrete.load(Relaxed));
                        if discrete != was_discrete {
                            was_discrete = discrete;
                            crate::diag::log(String::from(if discrete {
                                "sorgente presa come 5.1: dietro e centro suonano i suoi canali"
                            } else {
                                "sorgente di nuovo stereo: dietro e centro suonano l'upmix"
                            }));
                        }
                        for (i, inp) in inputs.iter().enumerate() {
                            let filling = inp.filling.load(Relaxed);
                            if filling != was_filling[i] {
                                crate::diag::log(if filling {
                                    format!("«{}» in silenzio: genero silenzio per tenere agganciate le uscite", backend::display_name(&inp.id))
                                } else {
                                    format!("«{}»: audio ripreso", backend::display_name(&inp.id))
                                });
                                was_filling[i] = filling;
                            }
                            // which channels of a multichannel source carry sound
                            // (each one once after a silence): shows whether a
                            // player sends real 5.1 or only the front pair
                            let fresh = inp.ch_heard.swap(0, Relaxed);
                            if filling {
                                heard[i] = 0;
                            } else {
                                let all = heard[i] | fresh;
                                if all != heard[i] && inp.ch_peak.len() > 2 {
                                    crate::diag::log(format!("«{}»: suono sui canali {}", backend::display_name(&inp.id), channel_names(inp.ch_peak.len(), all)));
                                }
                                heard[i] = all;
                            }
                            // 5.1 or stereo: after a pause the same players
                            // mean the same source, another one a new source
                            if inp.ch_peak.len() > 2 {
                                if !filling && fresh & !0b11 != 0 {
                                    had_discrete[i] = true;
                                    owners[i] = backend::playing_processes();
                                }
                                let quiet_s = now.saturating_sub(inp.last_loud_ns.load(Relaxed)) as f64 * 1e-9;
                                if quiet_s >= DISCRETE_RESET_S {
                                    long_quiet[i] = true;
                                } else if long_quiet[i] {
                                    long_quiet[i] = false;
                                    if had_discrete[i] && new_source(owners[i].as_deref(), backend::playing_processes().as_deref()) {
                                        had_discrete[i] = false;
                                        owners[i] = None;
                                        inp.source_epoch.fetch_add(1, Relaxed);
                                        crate::diag::log(format!("«{}»: dopo la pausa suona altro: si riparte dallo stereo", backend::display_name(&inp.id)));
                                    }
                                }
                            }
                            let (pos, wtime, _) = inp.ring.snapshot();
                            let alive = pos > 0 && now.saturating_sub(wtime) < 200_000_000;
                            alive_any |= alive;
                            if let Some(r) = in_watch[i].feed(pos, wtime as f64 * 1e-9, inp.rate, alive) {
                                let msg = format!(
                                    "l'ingresso «{}» ora va a {:.0} Hz invece di {:.0} Hz: riavvio",
                                    backend::display_name(&inp.id),
                                    inp.rate * r,
                                    inp.rate
                                );
                                crate::diag::log(&msg);
                                if let Ok(mut e) = errors.lock() {
                                    e.push(msg);
                                }
                                failed.store(true, SeqCst);
                                reported = true;
                            }
                        }
                        for (i, r) in routes.iter().enumerate() {
                            let c = &r.ctl;
                            let rate = c.out_rate.load(Relaxed) as f64;
                            if rate > 0.0 {
                                if let Some(x) = out_watch[i].feed(c.st_frames.load(Relaxed), t, rate, true) {
                                    let msg = format!("l'uscita «{}» ora va a {:.0} Hz invece di {:.0} Hz: riavvio", r.name, rate * x, rate);
                                    crate::diag::log(&msg);
                                    if let Ok(mut e) = errors.lock() {
                                        e.push(msg);
                                    }
                                    failed.store(true, SeqCst);
                                    reported = true;
                                }
                            }
                            // state changes and re-locks
                            let stv = c.st_state.load(Relaxed);
                            let jumps = c.st_jumps.load(Relaxed);
                            if stv != prev_state[i] || jumps != prev_jumps[i] {
                                crate::diag::log(format!(
                                    "«{}»: {:?}, buffer {:.1} ms, deriva {:+.0} ppm, interruzioni {}, riagganci {}",
                                    r.name,
                                    c.state(),
                                    c.st_buffer_ms.load(),
                                    c.st_ppm.load(),
                                    c.st_underruns.load(Relaxed),
                                    jumps
                                ));
                                prev_state[i] = stv;
                                prev_jumps[i] = jumps;
                            }
                            // repeated dropouts → more safety buffer (all routes)
                            let h = &mut under_hist[i];
                            h.push(c.st_underruns.load(Relaxed));
                            if h.len() > 5 {
                                h.remove(0);
                            }
                            if alive_any && h.len() == 5 && h[4].saturating_sub(h[0]) >= 2 {
                                let new = (safety.effective_ms() + 10.0).min(MAX_SAFETY_MS);
                                if new > safety.min_ms.load() {
                                    safety.min_ms.store(new);
                                    crate::diag::log(format!("interruzioni ripetute su «{}»: buffer di sicurezza portato a {new:.0} ms", r.name));
                                }
                                h.clear();
                            }
                        }
                        // what the callbacks need (smoothed), for all routes
                        let need = routes.iter().map(|r| r.ctl.st_need_ms.load()).fold(0.0f32, f32::max);
                        let want = (need * 1.25 + 2.0).min(MAX_SAFETY_MS);
                        if alive_any && need > 0.0 && want > safety.min_ms.load() + 0.5 {
                            safety.min_ms.store(want);
                            crate::diag::log(format!("buffer minimo per questi dispositivi: {want:.0} ms (in uso {:.0} ms)", safety.effective_ms()));
                        }
                        // periodic summary
                        if tick % 300 == 10 {
                            for r in &routes {
                                let c = &r.ctl;
                                crate::diag::log(format!(
                                    "stato «{}»: {:?}, buffer {:.1} ms (ritardo {:.1} + sicurezza {:.0}), deriva {:+.0} ppm, interruzioni {}, riagganci {}, volume sistema {:.2}",
                                    r.name,
                                    c.state(),
                                    c.st_buffer_ms.load(),
                                    c.delay_ms.load(),
                                    safety.effective_ms(),
                                    c.st_ppm.load(),
                                    c.st_underruns.load(Relaxed),
                                    c.st_jumps.load(Relaxed),
                                    c.sys_gain.load()
                                ));
                            }
                        }
                    }
                    tick += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            })
            .ok()?;
        Some(Self { stop, thread: Some(thread) })
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        crate::volume::set_active(false);
    }
}

pub struct Engine {
    _monitor: Option<Monitor>,
    _fillers: Vec<Filler>,
    _streams: Vec<StreamHandle>,
    pub route_ctl: Vec<Option<Arc<RouteCtl>>>,
    pub route_input: Vec<Option<Arc<InputShared>>>,
    pub status: Vec<RouteStatus>,
    pub inputs: Vec<Arc<InputShared>>,
    pub reference: Option<Arc<RefCtl>>,
    pub reference_error: Option<String>,
    pub failed: Arc<AtomicBool>,
    pub errors: Arc<Mutex<Vec<String>>>,
    pub test_click: Arc<AtomicBool>,
    pub safety: Arc<Safety>,
}

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Where a route's output is, and whether it can be heard.
fn place_of(r: &crate::config::RouteConfig) -> (Placement, bool) {
    (Placement { rear: r.rear, centre: r.centre, bass_hz: r.bass_hz }, !r.muted && r.volume > 0.0)
}

pub fn route_gain(r: &crate::config::RouteConfig) -> f32 {
    if r.muted {
        0.0
    } else {
        db_to_gain(r.gain_db)
    }
}

/// An output is never turned down by more than this.
pub const MAX_TRIM_DB: f32 = 40.0;

/// What `level_trims` knows of an output that plays: where it is, how loud
/// it sounded to the calibration microphone (`OutputConfig::level_db`;
/// `None`: not measured) and what the user added by ear
/// (`OutputConfig::surround_db`).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Heard {
    pub place: Placement,
    pub level_db: Option<f32>,
    pub by_ear_db: f32,
}

/// How much each output that plays is turned down (dB, 0 or less) so that
/// no rear or centre output is louder than the front ones where the
/// listener sits.
///
/// With surround on each place plays something else: the front the
/// programme as it is, the rear the same with what is at the centre turned
/// down, 15 ms later. A rear output 20 dB louder than the front one (a big
/// pair of speakers behind, a small one in front) is then most of what is
/// heard, and the sound comes from behind. (This was written on 2026-11-27
/// for the rear of that morning, which played only what left and right do
/// not share, and was meant to explain the user's "le voci non sono
/// chiare", "come quando camuffano le voci". It did not: what he heard was
/// the capture losing part of every block the browser played, on every
/// output, see `mactap::Tuning`. The levels are still worth matching.)
///
/// The front is the measure: the loudest front output that was measured
/// (with no front output, what stands in for it, see `Places::of`). The
/// others are only ever turned down to it; a quieter one stays as it is,
/// and so does everything when the front was not measured (`measured`
/// false: the user does not want it, `Config::match_levels`). What the
/// user set by ear on a rear or centre output is added, and the sum is
/// never above 0 dB: an output is not made louder than its volume gives.
/// With surround off every output is a front one and nothing is touched.
pub fn level_trims(playing: &[Heard], measured: bool) -> Vec<f32> {
    let any = |f: fn(&Placement) -> bool| playing.iter().any(|h| f(&h.place));
    let is_front: fn(&Placement) -> bool = if any(|p| !p.rear && !p.centre) {
        |p| !p.rear && !p.centre
    } else if any(|p| p.centre) {
        |p| p.centre
    } else {
        |_| true
    };
    let level = |h: &Heard| h.level_db.filter(|l| measured && l.is_finite());
    let front = playing.iter().filter(|h| is_front(&h.place)).filter_map(level).fold(f32::NEG_INFINITY, f32::max);
    playing
        .iter()
        .map(|h| {
            if is_front(&h.place) {
                return 0.0;
            }
            let matched = match level(h) {
                Some(l) if front.is_finite() && l > front => front - l,
                _ => 0.0,
            };
            let by_ear = if h.by_ear_db.is_finite() { h.by_ear_db } else { 0.0 };
            (matched + by_ear).clamp(-MAX_TRIM_DB, 0.0)
        })
        .collect()
}

/// `level_trims` for the routes in `which`, as factors.
fn route_trims(cfg: &Config, which: &[usize]) -> Vec<f32> {
    let playing: Vec<Heard> =
        which.iter().map(|&i| Heard { place: place_of(&cfg.routes[i]).0, level_db: cfg.routes[i].level_db, by_ear_db: cfg.routes[i].surround_db }).collect();
    level_trims(&playing, cfg.match_levels).into_iter().map(db_to_gain).collect()
}

/// Names of the channels in `mask` of a source with `n` channels, in the
/// order MultiAudio's own output declares (L R C LFE Ls Rs, or L R Ls Rs).
fn channel_names(n: usize, mask: u32) -> String {
    const SIX: [&str; 6] = ["sinistra", "destra", "centro", "bassi", "dietro sx", "dietro dx"];
    const FOUR: [&str; 4] = ["sinistra", "destra", "dietro sx", "dietro dx"];
    let names: &[&str] = if n >= 6 { &SIX } else if n >= 4 { &FOUR } else { &[] };
    (0..n.min(32))
        .filter(|c| mask & (1 << c) != 0)
        .map(|c| names.get(c).map(|s| s.to_string()).unwrap_or_else(|| (c + 1).to_string()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Which speaker positions are there among the outputs that play, and who
/// plays the bass: it decides what each of them does (a front output plays
/// the surround channels itself when there is no rear, and keeps the voices
/// when there is no centre speaker).
///
/// Bass: every output plays its own sound down to its own limit
/// (`Placement::bass_hz`). What the centre and the rear speakers leave of
/// their channels, and the LFE channel, go to the front outputs that play
/// everything. The bass has to come out somewhere: if no front output plays
/// everything, they all do, whatever their setting.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Places {
    pub has_rear: bool,
    pub has_centre: bool,
    /// Below these frequencies (Hz) the rear and the centre speakers leave
    /// their channels to the front; 0: they play everything (or are not
    /// there).
    pub rear_xo: f32,
    pub centre_xo: f32,
    /// Some front output plays all the bass.
    pub front_full: bool,
    /// No front output plays: the centre speaker stands in for it and
    /// plays everything...
    pub centre_is_front: bool,
    /// ...or, with no centre speaker either, the rear ones do.
    pub rear_is_front: bool,
    /// Stereo sources: the centre speaker plays the voices too, and the
    /// rear outputs the stereo as it is (see `Config::stereo_centre` and
    /// `config::StereoRear`; set by who knows the settings, not by `of`).
    pub voices_centre: bool,
    pub rear_all: bool,
}

/// Where an output is and how much bass its speakers play.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Placement {
    pub rear: bool,
    pub centre: bool,
    /// Below this frequency (Hz) it plays nothing; 0: it plays everything.
    pub bass_hz: f32,
}

impl Places {
    /// From the place of every output that plays, and whether it can be
    /// heard (not muted, volume above zero). A rear or centre speaker that
    /// cannot be heard leaves its part to the front; a silent front is
    /// still the front (so the others can be listened to on their own).
    pub fn of(playing: impl Iterator<Item = (Placement, bool)>) -> Places {
        let (mut front, mut rear, mut centre) = (false, false, false);
        let (mut rear_heard, mut centre_heard) = (false, false);
        // lowest limit among the front outputs, the centre and the rear ones
        let mut xo = [f32::INFINITY; 3];
        for (p, heard) in playing {
            let (r, c) = (p.rear && !p.centre, p.centre);
            front |= !r && !c;
            centre |= c;
            rear |= r;
            centre_heard |= c && heard;
            rear_heard |= r && heard;
            let k = if c { 1 } else if r { 2 } else { 0 };
            if k == 0 || heard {
                xo[k] = xo[k].min(p.bass_hz.max(0.0));
            }
        }
        let centre_is_front = !front && centre;
        let rear_is_front = !front && !centre && rear;
        let has_rear = rear_heard && !rear_is_front;
        let has_centre = centre_heard && !centre_is_front;
        // (the speakers that stand in for the front are the front)
        let front_xo = if centre_is_front { xo[1] } else if rear_is_front { xo[2] } else { xo[0] };
        Places {
            has_rear,
            has_centre,
            rear_xo: if has_rear { xo[2] } else { 0.0 },
            centre_xo: if has_centre { xo[1] } else { 0.0 },
            front_full: front_xo == 0.0,
            centre_is_front,
            rear_is_front,
            voices_centre: false,
            rear_all: false,
        }
    }

    /// The same, with what centre and rear speakers play of a stereo
    /// source.
    pub fn with_stereo(self, cfg: &Config) -> Places {
        Places { voices_centre: cfg.stereo_centre, rear_all: cfg.stereo_rear == crate::config::StereoRear::All, ..self }
    }

    /// Tells an output what it does, given its own place.
    pub fn store(&self, ctl: &RouteCtl, p: Placement) {
        let centre_here = p.centre && !self.centre_is_front;
        let rear_here = p.rear && !p.centre && !self.rear_is_front;
        let front_here = !centre_here && !rear_here;
        let xo = if front_here && !self.front_full { 0.0 } else { p.bass_hz.max(0.0) };
        ctl.centre.store(centre_here, Relaxed);
        ctl.rear.store(rear_here, Relaxed);
        ctl.xo_hz.store(xo);
        ctl.sub.store(front_here && xo == 0.0, Relaxed);
        ctl.has_rear.store(self.has_rear, Relaxed);
        ctl.has_centre.store(self.has_centre, Relaxed);
        ctl.voices_centre.store(self.voices_centre, Relaxed);
        ctl.rear_all.store(self.rear_all, Relaxed);
        ctl.rear_xo.store(self.rear_xo);
        ctl.centre_xo.store(self.centre_xo);
    }
}

impl Engine {
    pub fn start(cfg: &Config, devs: &DeviceList, test_click: bool) -> Engine {
        let failed = Arc::new(AtomicBool::new(false));
        let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let make_err = |label: String| -> ErrorCallback {
            let f = failed.clone();
            let e = errors.clone();
            Arc::new(move |msg: String| {
                f.store(true, SeqCst);
                if let Ok(mut v) = e.lock() {
                    v.push(format!("{label}: {msg}"));
                }
            })
        };
        let test_click = Arc::new(AtomicBool::new(test_click));
        let safety = Arc::new(Safety::new(cfg.safety_ms));
        safety.anchor_ms.store(cfg.anchor_ms.unwrap_or(f32::NAN));
        let n = cfg.routes.len();
        let mut status = vec![RouteStatus::Disabled; n];
        let mut route_ctl: Vec<Option<Arc<RouteCtl>>> = vec![None; n];
        let mut route_input: Vec<Option<Arc<InputShared>>> = vec![None; n];
        let mut streams = Vec::new();

        // ---- which routes can run ----
        let mut active = Vec::new();
        for (i, r) in cfg.routes.iter().enumerate() {
            if !r.enabled {
                continue;
            }
            if r.input.is_empty() || r.output.is_empty() {
                status[i] = RouteStatus::Incomplete;
                continue;
            }
            if devs.input(&r.input).is_none() {
                status[i] = RouteStatus::Missing(backend::display_name(&r.input).to_string());
                continue;
            }
            if devs.output(&r.output).is_none() {
                status[i] = RouteStatus::Missing(backend::display_name(&r.output).to_string());
                continue;
            }
            active.push(i);
        }

        // ---- reference output (clicks + calibration probe) ----
        let mut reference = None;
        let mut reference_error = None;
        if !cfg.reference.is_empty() {
            if devs.output(&cfg.reference).is_none() {
                reference_error = Some(format!("{} non collegato", backend::display_name(&cfg.reference)));
            } else {
                let rc = Arc::new(RefCtl { probe_req: AtomicBool::new(false), probe_done: AtomicBool::new(false) });
                match backend::output_format(&cfg.reference).and_then(|fmt| {
                    let cb = reference_callback(fmt, rc.clone(), test_click.clone());
                    backend::open_output(&cfg.reference, &fmt, cb, make_err(backend::display_name(&cfg.reference).to_string()))
                }) {
                    Ok(s) => {
                        streams.push(s);
                        reference = Some(rc);
                    }
                    Err(e) => reference_error = Some(e.to_string()),
                }
            }
        }
        let click_at_input = Arc::new(AtomicBool::new(reference.is_none()));

        // ---- inputs ----
        let mut inputs: HashMap<String, Arc<InputShared>> = HashMap::new();
        let mut input_list = Vec::new();
        let mut tried: Vec<String> = Vec::new();
        for &i in &active {
            let id = cfg.routes[i].input.clone();
            if tried.contains(&id) {
                continue;
            }
            tried.push(id.clone());
            let res = backend::input_format(&id).and_then(|fmt| {
                let frames = (((MAX_DELAY_MS + MAX_SAFETY_MS) / 1000.0 + 1.0) as f64 * fmt.rate as f64) as usize;
                let shared = Arc::new(InputShared::new(id.clone(), frames, fmt.channels.clamp(1, MAX_RING_CHANNELS), fmt.rate as f64, fmt.channels));
                let cb = input_callback(shared.clone(), test_click.clone(), click_at_input.clone());
                let s = backend::open_input(&id, &fmt, cb, make_err(backend::display_name(&id).to_string()))?;
                Ok((shared, s))
            });
            match res {
                Ok((shared, s)) => {
                    streams.push(s);
                    input_list.push(shared.clone());
                    inputs.insert(id, shared);
                }
                Err(e) => {
                    for &j in &active {
                        if cfg.routes[j].input == id {
                            status[j] = RouteStatus::Error(format!("ingresso: {e}"));
                        }
                    }
                }
            }
        }

        // ---- outputs ----
        let mut outputs: Vec<String> = Vec::new();
        for &i in &active {
            let o = &cfg.routes[i].output;
            if inputs.contains_key(&cfg.routes[i].input) && !outputs.contains(o) {
                outputs.push(o.clone());
            }
        }
        for out_id in outputs {
            let fmt = match backend::output_format(&out_id) {
                Ok(f) => f,
                Err(e) => {
                    for &j in &active {
                        if cfg.routes[j].output == out_id {
                            status[j] = RouteStatus::Error(format!("uscita: {e}"));
                        }
                    }
                    continue;
                }
            };
            let mut readers = Vec::new();
            let mut members = Vec::new();
            for &j in &active {
                let r = &cfg.routes[j];
                if r.output != out_id {
                    continue;
                }
                let Some(input) = inputs.get(&r.input) else { continue };
                let ctl = Arc::new(RouteCtl::new(r.delay_ms, route_gain(r)));
                ctl.level.store(r.volume);
                if let Some(k) = active.iter().position(|&a| a == j) {
                    ctl.trim.store(route_trims(cfg, &active)[k]);
                }
                // (the speakers that really play: see below)
                Places::of(active.iter().map(|&k| place_of(&cfg.routes[k]))).with_stereo(cfg).store(&ctl, place_of(r).0);
                readers.push(Reader::new(ctl.clone(), input.clone(), safety.clone(), r.input_ch as usize, r.output_ch as usize, fmt));
                members.push((j, ctl, input.clone()));
            }
            let cb = output_callback(readers, fmt.channels);
            match backend::open_output(&out_id, &fmt, cb, make_err(backend::display_name(&out_id).to_string())) {
                Ok(s) => {
                    streams.push(s);
                    for (j, ctl, input) in members {
                        status[j] = RouteStatus::Running;
                        route_ctl[j] = Some(ctl);
                        route_input[j] = Some(input);
                    }
                }
                Err(e) => {
                    for (j, _, _) in members {
                        status[j] = RouteStatus::Error(format!("uscita: {e}"));
                    }
                }
            }
        }

        // A rear or centre speaker counts only if it plays: what a missing
        // one would play (surround channels, voices) stays with the front.
        let places = Places::of(cfg.routes.iter().zip(&route_ctl).filter(|(_, c)| c.is_some()).map(|(r, _)| place_of(r))).with_stereo(cfg);
        for (r, ctl) in cfg.routes.iter().zip(&route_ctl) {
            if let Some(ctl) = ctl {
                places.store(ctl, place_of(r).0);
            }
        }
        // ...and is the measure of how loud the others may be only if it
        // plays (`level_trims`).
        let running: Vec<usize> = (0..cfg.routes.len()).filter(|&i| route_ctl[i].is_some()).collect();
        let trims = route_trims(cfg, &running);
        for (&i, t) in running.iter().zip(&trims) {
            if let Some(ctl) = &route_ctl[i] {
                ctl.trim.store(*t);
            }
        }

        let mon_routes: Vec<MonRoute> = route_ctl
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                c.clone().map(|ctl| MonRoute {
                    name: cfg.routes[i].name.clone(),
                    output: cfg.routes[i].output.clone(),
                    bluetooth: devs.output(&cfg.routes[i].output).is_some_and(|d| d.kind == backend::DevKind::Bluetooth),
                    ctl,
                })
            })
            .collect();
        crate::diag::log(format!(
            "motore avviato: {} ingressi, {} uscite attive, riferimento «{}»",
            input_list.len(),
            mon_routes.len(),
            if cfg.reference.is_empty() { "-".to_string() } else { backend::display_name(&cfg.reference) }
        ));
        for inp in &input_list {
            crate::diag::log(format!("  ingresso «{}»: {:.0} Hz, {} canali", backend::display_name(&inp.id), inp.rate, inp.dev_channels));
        }
        for (i, r) in cfg.routes.iter().enumerate() {
            let rate = route_ctl[i].as_ref().map(|c| c.out_rate.load(Relaxed)).unwrap_or(0);
            let trim = route_ctl[i].as_ref().map(|c| 20.0 * c.trim.load().max(1e-6).log10()).filter(|db| *db < -0.5);
            let trim = trim.map(|db| format!(", abbassata di {:.0} dB (al microfono era più forte delle casse davanti)", -db)).unwrap_or_default();
            crate::diag::log(format!("  uscita «{}»: {:?}, {} Hz, ritardo {:.1} ms{trim}", r.name, status[i], rate, r.delay_ms));
        }
        if let Some(e) = &reference_error {
            crate::diag::log(format!("  riferimento: {e}"));
        }
        Engine {
            _monitor: Monitor::spawn(mon_routes, input_list.clone(), safety.clone(), failed.clone(), errors.clone()),
            _fillers: input_list.iter().filter_map(|i| Filler::spawn(i.clone(), test_click.clone(), click_at_input.clone())).collect(),
            _streams: streams,
            route_ctl,
            route_input,
            status,
            inputs: input_list,
            reference,
            reference_error,
            failed,
            errors,
            test_click,
            safety,
        }
    }

    /// True when a route is waiting for a device that is not available.
    pub fn needs_devices(&self) -> bool {
        self.reference_error.is_some()
            || self.status.iter().any(|s| matches!(s, RouteStatus::Missing(_) | RouteStatus::Error(_)))
    }
}

// ---------------------------------------------------------------------------
// Callbacks
// ---------------------------------------------------------------------------

/// Finds the pieces missing from a capture: stretches of exact zeros (front
/// pair) between sound and sound, too short to be a pause of the programme
/// (16 frames to `HOLE_MAX`). That is what the system hands over when it
/// has lost part of what a program played (see `mactap::Tuning`).
#[derive(Default)]
struct Holes {
    /// Frames of exact zeros up to now, since the last sound.
    run: usize,
    /// There was sound before them.
    heard: bool,
}

/// A silence longer than this (46 ms at 44.1 kHz) is the programme's.
const HOLE_MAX: usize = 2048;

impl Holes {
    /// (missing pieces that end in this block, frames they are long).
    fn feed(&mut self, data: &[f32], channels: usize) -> (u32, u64) {
        let channels = channels.max(1);
        let (mut count, mut frames) = (0u32, 0u64);
        for f in data.chunks_exact(channels) {
            if f[..channels.min(2)].iter().all(|v| *v == 0.0) {
                self.run += 1;
                if self.run > HOLE_MAX {
                    self.heard = false;
                }
            } else {
                if self.heard && self.run >= 16 {
                    count += 1;
                    frames += self.run as u64;
                }
                self.run = 0;
                self.heard = true;
            }
        }
        (count, frames)
    }
}

fn input_callback(shared: Arc<InputShared>, test_click: Arc<AtomicBool>, click_at_input: Arc<AtomicBool>) -> backend::InputCallback {
    let mut last: (u64, usize) = (0, 0);
    let mut holes = Holes::default();
    Box::new(move |data: &[f32]| {
        let dch = shared.dev_channels.max(1);
        let cs = shared.ring.channels();
        let n = data.len() / dch;
        if n == 0 {
            return;
        }
        shared.blocks.fetch_add(1, Relaxed);
        let (torn, missing) = holes.feed(data, dch);
        if torn > 0 {
            shared.torn.fetch_add(torn, Relaxed);
            shared.torn_frames.fetch_add(missing, Relaxed);
        }
        let now = now_ns();
        if let Some(ms) = late_ms(last.0, now, last.1 as f64 / shared.rate) {
            shared.late.note(ms);
        }
        last = (now, n);
        shared.clicking.store(test_click.load(Relaxed), Relaxed);
        let click_on = test_click.load(Relaxed) && click_at_input.load(Relaxed);
        let mut peak = 0f32;
        // The silence generator holds the lock only for a few microseconds.
        shared.lock();
        let pos = shared.ring.write_pos();
        let mut ch_peak = [0f32; 16];
        for f in 0..n {
            let fp = pos + f as u64;
            let ck = if click_on { shared.click_at(fp) } else { 0.0 };
            let base = f * dch;
            for c in 0..cs {
                // the click: front pair only (every output plays it)
                let v = data[base + c] + if c < 2 { ck } else { 0.0 };
                shared.ring.put(fp, c, v);
                peak = peak.max(v.abs());
                ch_peak[c] = ch_peak[c].max(v.abs());
            }
        }
        for (c, p) in shared.ch_peak.iter().enumerate() {
            p.fetch_max(ch_peak[c]);
        }
        let heard = ch_peak.iter().enumerate().filter(|(_, p)| **p > 1e-3).fold(0u32, |m, (c, _)| m | 1 << c);
        if heard != 0 {
            shared.ch_heard.fetch_or(heard, Relaxed);
        }
        shared.ring.publish(pos + n as u64, now, n as u64);
        shared.last_real_ns.store(now, Relaxed);
        if peak > 1e-3 {
            shared.last_loud_ns.store(now, Relaxed);
        }
        shared.unlock();
        shared.peak.fetch_max(peak);
    })
}

fn output_callback(mut readers: Vec<Reader>, ch_out: usize) -> backend::OutputCallback {
    let ch_out = ch_out.max(1);
    let mut last: (u64, usize) = (0, 0);
    Box::new(move |data: &mut [f32]| {
        data.fill(0.0);
        let n = data.len() / ch_out;
        let now = now_ns();
        if let Some(r) = readers.first() {
            if let Some(ms) = late_ms(last.0, now, last.1 as f64 / r.out_rate_f) {
                for r in readers.iter() {
                    r.ctl.st_late.note(ms);
                }
            }
        }
        last = (now, n);
        for r in readers.iter_mut() {
            r.render(data, n, ch_out, now);
        }
    })
}

fn reference_callback(fmt: StreamFormat, ctl: Arc<RefCtl>, test_click: Arc<AtomicBool>) -> backend::OutputCallback {
    let rate = fmt.rate as f64;
    let ch = fmt.channels.max(1);
    let click = make_click(rate);
    let probe = make_probe(rate);
    let period = rate.round().max(1.0) as u64;
    let mut pos = 0u64;
    let mut probe_idx = usize::MAX;
    Box::new(move |data: &mut [f32]| {
        let n = data.len() / ch;
        if ctl.probe_req.swap(false, Relaxed) {
            probe_idx = 0;
            ctl.probe_done.store(true, Release);
        }
        let click_on = test_click.load(Relaxed);
        for f in 0..n {
            let mut v = 0.0;
            if click_on {
                v += click.get((pos % period) as usize).copied().unwrap_or(0.0);
            }
            if let Some(p) = probe.get(probe_idx) {
                v += *p;
                probe_idx += 1;
            }
            data[f * ch..(f + 1) * ch].fill(v);
            pos += 1;
        }
    })
}

// ---------------------------------------------------------------------------
// Reader: delay line + drift correction + resampling for one route
// ---------------------------------------------------------------------------

pub struct Reader {
    ctl: Arc<RouteCtl>,
    input: Arc<InputShared>,
    safety: Arc<Safety>,
    sinc: SincTable,
    ch_l: usize,
    ch_r: usize,
    out_off: usize,
    in_rate: f64,
    out_rate_f: f64,
    step_nominal: f64,
    read_int: u64,
    read_frac: f64,
    state: ReaderState,
    wait_floor: f64,
    fade: f32,
    fade_dir: i8,
    fade_step: f32,
    gain_cur: f32,
    gain_coef: f32,
    err_ema: f64,
    integ: f64,
    corr: f64,
    last_delay: f32,
    probe: Vec<f32>,
    probe_idx: usize,
    need_ema: f64,
    /// What this output takes from the upmix, if anything.
    role: Option<Upmix>,
    /// How much of what left and right have in common the centre speaker
    /// plays now (0: nothing, 1: all of it; see `Config::stereo_centre`).
    share: f32,
    /// What a rear output is playing of a stereo source now: it as it is,
    /// or the surround matrix (`RouteCtl::rear_all`, taken up only while
    /// this output is silent).
    rear_as_is: bool,
    /// The band of the ambience the rear plays (upmix).
    surround_hp: [crate::dsp::Biquad; 2],
    surround_lp: [crate::dsp::Biquad; 2],
    /// Input carries 5.1 (L R C LFE Ls Rs) or quad (L R Ls Rs).
    five_one: bool,
    has_c_lfe: bool,
    chans6_fl: usize,
    chans6_fr: usize,
    chans6: [usize; 6],
    /// 0 = upmix from stereo, 1 = discrete surround channels (crossfaded).
    disc_w: f32,
    disc_target: f32,
    disc_quiet_s: f64,
    /// For how long the channels of a 5.1 source have carried sound, in one
    /// stretch (see `DISCRETE_AFTER_S`) and in all (see `DISCRETE_TRUST`).
    disc_seen_s: f64,
    disc_total_s: f64,
    /// `InputShared::source_epoch` as last seen.
    source_epoch: u32,
    /// Bass management (see `Places`). The front's bass for a rear output
    /// that plays everything.
    bass_lp: crate::dsp::Biquad,
    /// This output's own limit.
    own_hp: Split<2>,
    /// Left and right above the centre speaker's limit: what it plays of a
    /// stereo source comes from there.
    mid_hp: Split<2>,
    /// What the centre and the rear speakers leave of their channels.
    centre_low: Split<1>,
    rear_low: Split<2>,
    lim_gain: f32,
    lim_rel: f32,
}

/// A sample as it can go into a filter: not a number becomes silence, an
/// absurd value is cut down.
#[inline]
fn sane(x: f32) -> f32 {
    if x.is_finite() {
        x.clamp(-8.0, 8.0)
    } else {
        0.0
    }
}

/// A crossover filter on a few channels that follows the frequency asked
/// of it. At 0 a high-pass lets everything through and a low-pass nothing.
struct Split<const N: usize> {
    low: bool,
    fs: f64,
    hz: f32,
    f: [crate::dsp::Lr4; N],
}

impl<const N: usize> Split<N> {
    fn new(fs: f64, low: bool) -> Self {
        Self { low, fs, hz: 0.0, f: [crate::dsp::Lr4::lowpass(fs, 100.0); N] }
    }

    #[inline]
    fn tune(&mut self, hz: f32) {
        let hz = if hz.is_finite() { hz.max(0.0) } else { 0.0 };
        if hz != self.hz {
            self.hz = hz;
            let fc = (hz as f64).clamp(20.0, self.fs * 0.4);
            self.f = [if self.low { crate::dsp::Lr4::lowpass(self.fs, fc) } else { crate::dsp::Lr4::highpass(self.fs, fc) }; N];
        }
    }

    #[inline]
    fn run(&mut self, k: usize, x: f32) -> f32 {
        if self.hz > 0.0 {
            self.f[k].process(x)
        } else if self.low {
            0.0
        } else {
            x
        }
    }

    fn reset(&mut self) {
        for f in self.f.iter_mut() {
            f.reset();
        }
    }
}

impl Reader {
    pub fn new(ctl: Arc<RouteCtl>, input: Arc<InputShared>, safety: Arc<Safety>, in_ch: usize, out_ch: usize, out: StreamFormat) -> Self {
        let in_rate = input.rate;
        let out_rate = out.rate as f64;
        let cs = input.ring.channels();
        let ch_l = in_ch.min(cs - 1);
        let ch_r = (in_ch + 1).min(cs - 1);
        let out_off = if out.channels <= 1 { 0 } else { out_ch.min(out.channels - 2) };
        let cutoff = 0.92 * (out_rate / in_rate).min(1.0);
        let last_delay = ctl.delay_ms.load();
        ctl.out_rate.store(out.rate, Relaxed);
        let gain_cur = 0.0;
        let rear_as_is = ctl.rear_all.load(Relaxed);
        Self {
            ctl,
            input,
            safety,
            sinc: SincTable::new(cutoff),
            ch_l,
            ch_r,
            out_off,
            in_rate,
            out_rate_f: out_rate,
            step_nominal: in_rate / out_rate,
            read_int: 0,
            read_frac: 0.0,
            state: ReaderState::Waiting,
            wait_floor: 0.0,
            fade: 0.0,
            fade_dir: 0,
            fade_step: (1.0 / (FADE_S * out_rate)) as f32,
            gain_cur,
            gain_coef: (1.0 - (-1.0 / (0.005 * out_rate)).exp()) as f32,
            err_ema: 0.0,
            integ: 0.0,
            corr: 0.0,
            last_delay,
            probe: make_probe(out_rate),
            probe_idx: usize::MAX,
            need_ema: 0.0,
            role: None,
            share: f32::NAN,
            rear_as_is,
            surround_hp: [crate::dsp::Biquad::highpass(out_rate, 100.0, 0.707); 2],
            surround_lp: [crate::dsp::Biquad::lowpass(out_rate, 7000.0, 0.707); 2],
            // 6+ channels: L R C LFE Ls Rs; 4 channels: L R Ls Rs (quad)
            five_one: cs >= 4 && in_ch == 0,
            chans6: if cs >= 6 { [0, 1, 2, 3, 4, 5] } else { [0, 1, 0, 0, 2, 3] },
            has_c_lfe: cs >= 6,
            chans6_fl: if cs >= 4 && in_ch == 0 { 0 } else { ch_l },
            chans6_fr: if cs >= 4 && in_ch == 0 { 1 } else { ch_r },
            disc_w: 0.0,
            disc_target: 0.0,
            disc_quiet_s: 0.0,
            disc_seen_s: 0.0,
            disc_total_s: 0.0,
            source_epoch: 0,
            bass_lp: crate::dsp::Biquad::lowpass(out_rate, BASS_HZ, 0.707),
            own_hp: Split::new(out_rate, false),
            mid_hp: Split::new(out_rate, false),
            centre_low: Split::new(out_rate, true),
            rear_low: Split::new(out_rate, true),
            lim_gain: 1.0,
            lim_rel: (1.0 - (-1.0 / (0.15 * out_rate)).exp()) as f32,
        }
    }

    #[inline]
    fn rpos(&self) -> f64 {
        self.read_int as f64 + self.read_frac
    }

    fn jump_to(&mut self, pos: f64) {
        self.ctl.st_jumps.fetch_add(1, Relaxed);
        let pos = pos.max(HALF as f64);
        self.read_int = pos.floor() as u64;
        self.read_frac = pos - pos.floor();
        self.state = ReaderState::Running;
        self.fade = 0.0;
        self.fade_dir = 1;
        self.err_ema = 0.0;
    }

    #[inline]
    fn emit(out: &mut [f32], f: usize, ch_out: usize, off: usize, l: f32, r: f32) {
        if ch_out == 1 {
            out[f] += 0.5 * (l + r);
        } else {
            out[f * ch_out + off] += l;
            out[f * ch_out + off + 1] += r;
        }
    }

    /// Adds `n` frames of this route into `out` (interleaved, `ch_out` ch).
    pub fn render(&mut self, out: &mut [f32], n: usize, ch_out: usize, now: u64) {
        let (wpos, wtime, wchunk) = self.input.ring.snapshot();
        let in_rate = self.in_rate;
        let ahead = (now.saturating_sub(wtime) as f64 * 1e-9 * in_rate).min(wchunk as f64);
        let w_est = wpos as f64 + ahead;
        // The test click must come out of every output at the same moment:
        // while it is on each output is a plain front one. Nothing is moved
        // from a speaker to another and the rear is not held back (the
        // click is for lining the outputs up by ear).
        let clicking = self.input.clicking.load(Relaxed);
        let has_centre = self.ctl.centre.load(Relaxed) || self.ctl.has_centre.load(Relaxed);
        let centre = self.ctl.centre.load(Relaxed) && !clicking;
        let rear = self.ctl.rear.load(Relaxed) && !centre && !clicking;
        // With a centre speaker somewhere the front leaves it the centre
        // channel of a 5.1 source (of a stereo one nothing: `upmix`).
        let role = if centre {
            Some(Upmix::Centre)
        } else if rear {
            Some(Upmix::Rear)
        } else if has_centre {
            Some(Upmix::Front)
        } else {
            None
        };
        if role != self.role {
            self.role = role;
            for f in self.surround_hp.iter_mut().chain(self.surround_lp.iter_mut()) {
                f.reset();
            }
            self.bass_lp.reset();
            self.own_hp.reset();
            self.mid_hp.reset();
            self.centre_low.reset();
            self.rear_low.reset();
        }
        let has_rear = self.ctl.has_rear.load(Relaxed) && !clicking;
        // What a rear output plays of a stereo source (as it is, or the
        // surround matrix 15 ms later) changes only while it is silent:
        // if it is playing it fades out first, as for any jump in delay.
        let rear_all = self.ctl.rear_all.load(Relaxed);
        if self.rear_as_is != rear_all {
            let heard = self.state == ReaderState::Running && self.fade > 0.0 && role == Some(Upmix::Rear);
            if !heard {
                self.rear_as_is = rear_all;
                for f in self.surround_hp.iter_mut().chain(self.surround_lp.iter_mut()) {
                    f.reset();
                }
                self.bass_lp.reset();
            } else if self.fade_dir == 0 {
                self.fade_dir = -1;
            }
        }
        // Bass (see `Places`): what this output plays itself, and what it
        // plays for the others.
        let xo = self.ctl.xo_hz.load();
        let sub = self.ctl.sub.load(Relaxed);
        // (a rear output that plays the matrix and all the bass: see below)
        let rear_full = rear && xo == 0.0 && !self.rear_as_is;
        self.own_hp.tune(xo);
        let centre_xo = self.ctl.centre_xo.load();
        self.mid_hp.tune(if role == Some(Upmix::Centre) { xo.max(CENTRE_MIN_HZ) } else { 0.0 });
        self.centre_low.tune(if sub { centre_xo } else { 0.0 });
        self.rear_low.tune(if sub && has_rear { self.ctl.rear_xo.load() } else { 0.0 });
        // The rear plays the surround matrix REAR_DELAY_MS after the front
        // (the stereo as it is, with the front).
        let extra = if role == Some(Upmix::Rear) && !rear_all { REAR_DELAY_MS } else { 0.0 };
        let delay = (self.ctl.delay_ms.load() + extra).clamp(0.0, MAX_DELAY_MS);
        // Smallest buffer that survives one input chunk + one output chunk
        // + interpolation taps, with 25 % / 2 ms headroom for jitter.
        let need_frames = wchunk as f64 + n as f64 * self.step_nominal + (HALF + 2) as f64;
        // Smoothed: a single odd callback must not move everything (the
        // monitor turns this into the common minimum safety buffer).
        self.need_ema += (need_frames / in_rate * 1000.0 - self.need_ema) * 0.02;
        self.ctl.st_need_ms.store(self.need_ema as f32);
        self.ctl.st_frames.fetch_add(n as u64, Relaxed);
        let safety = self.safety.common_ms();
        let target = (delay + safety) as f64 * 1e-3 * in_rate + (HALF + 2) as f64;
        let dt = n as f64 / (in_rate / self.step_nominal);
        let input_alive = wpos > 0 && now.saturating_sub(wtime) < 500_000_000;

        // Calibration probe (generated at the output, timestamped on the
        // input timeline so different outputs can be compared).
        if self.ctl.probe_req.swap(false, Relaxed) {
            self.probe_idx = 0;
            let p = if self.state == ReaderState::Running { self.rpos() } else { f64::NAN };
            self.ctl.probe_pos.store(p.to_bits(), Relaxed);
            self.ctl.probe_done.store(true, Release);
        }

        // ---- state machine ----
        if self.state == ReaderState::FadedOut {
            if w_est - self.rpos() < target {
                // Need more delay: hold silence until the buffer has grown.
                self.state = ReaderState::Waiting;
                self.wait_floor = self.rpos();
            } else {
                self.jump_to(w_est - target);
            }
        }
        if self.state == ReaderState::Waiting {
            let start = w_est - target;
            if input_alive && start >= self.wait_floor.max(HALF as f64) {
                self.jump_to(start);
            }
        }
        if self.state == ReaderState::Running {
            let rpos = self.rpos();
            let fill = w_est - rpos;
            let lookahead = if rear_full { REAR_DELAY_MS as f64 * 1e-3 * in_rate } else { 0.0 };
            let need = rpos + lookahead + n as f64 * self.step_nominal * (1.0 + MAX_CORR) + HALF as f64 + 2.0;
            let cap = self.input.ring.capacity() as f64;
            let jump_thresh = (0.015 * in_rate).max(3.0 * wchunk as f64);
            if need > wpos as f64 {
                // Underrun: input stalled or delivered late.
                self.ctl.st_underruns.fetch_add(1, Relaxed);
                self.state = ReaderState::Waiting;
                self.wait_floor = rpos;
            } else if fill > cap - 4.0 * wchunk as f64 - 4096.0 {
                // Data about to be overwritten: resync immediately.
                self.jump_to(w_est - target);
            } else if self.fade_dir == 0
                && ((delay - self.last_delay).abs() >= JUMP_ON_CHANGE_MS || (fill - target).abs() > jump_thresh)
            {
                self.fade_dir = -1;
            } else if self.fade_dir == 0 {
                let err = fill - target;
                let a = 1.0 - (-dt / ERR_SMOOTH_S).exp();
                self.err_ema += (err - self.err_ema) * a;
                let imax = MAX_CORR * in_rate / KI;
                self.integ = (self.integ + self.err_ema * dt).clamp(-imax, imax);
                self.corr = ((KP * self.err_ema + KI * self.integ) / in_rate).clamp(-MAX_CORR, MAX_CORR);
            }
            self.ctl.st_buffer_ms.store(((fill - (HALF + 2) as f64) / in_rate * 1000.0) as f32);
        }
        if self.fade_dir != -1 {
            self.last_delay = delay;
        }

        // ---- render ----
        let gtarget = if self.ctl.cal_mute.load(Relaxed) {
            0.0
        } else {
            let g = self.ctl.gain.load() * self.ctl.sys_gain.load() * self.ctl.trim.load();
            // never let a bad value (inf/NaN from a driver) reach the audio
            if g.is_finite() {
                g.clamp(0.0, 8.0)
            } else {
                1.0
            }
        };
        let step = self.step_nominal * (1.0 + self.corr);
        let ring = &self.input.ring;
        let mut peak = 0f32;
        const H: f32 = std::f32::consts::FRAC_1_SQRT_2; // −3 dB
        let lfe_gain = if sub { LFE_GAIN } else { 0.0 };
        // The rear reads REAR_DELAY_MS behind the front; its bass is read
        // that much later, so it comes out together with the front's.
        let bass_ahead = REAR_DELAY_MS as f64 * 1e-3 * in_rate;
        let disc_step = (1.0 / (0.08 * self.out_rate_f)) as f32;
        let (mut disc_peak, mut front_peak) = (0f32, 0f32);
        // Whether the centre speaker plays the voices of a stereo source
        // (it starts so, and moves in VOICES_MOVE_S when the choice changes).
        let share_target = if self.ctl.voices_centre.load(Relaxed) { 1.0 } else { 0.0 };
        if !self.share.is_finite() {
            self.share = share_target;
        }
        let share_step = (1.0 / (VOICES_MOVE_S * self.out_rate_f)) as f32;
        let tape = self.ctl.tape.get().filter(|t| t.on.load(Relaxed));
        for f in 0..n {
            let (mut l, mut r) = (0f32, 0f32);
            let mut probe = 0f32;
            if let Some(p) = self.probe.get(self.probe_idx) {
                probe = *p;
                self.probe_idx += 1;
            }
            if self.state == ReaderState::Running {
                self.gain_cur += (gtarget - self.gain_cur) * self.gain_coef;
                // One sample that is not a number (or absurdly large) would
                // stay in the filters below for good and silence this
                // output, or pass for the sound of a 5.1 source: it becomes
                // silence here, once, for everything that follows.
                let (fl, fr, c, lfe, sl, sr) = if self.five_one {
                    let v = self.sinc.interp_n(ring, self.read_int, self.read_frac, &self.chans6).map(sane);
                    // (with 4 channels there is no centre or LFE: those two
                    // slots repeat the front left and must not count)
                    let c_lfe = if self.has_c_lfe { v[2].abs().max(v[3].abs()) } else { 0.0 };
                    disc_peak = disc_peak.max(c_lfe).max(v[4].abs()).max(v[5].abs());
                    front_peak = front_peak.max(v[0].abs()).max(v[1].abs());
                    if self.has_c_lfe {
                        (v[0], v[1], v[2], v[3], v[4], v[5])
                    } else {
                        (v[0], v[1], 0.0, 0.0, v[4], v[5])
                    }
                } else {
                    let (a, b) = self.sinc.interp(ring, self.read_int, self.read_frac, self.ch_l, self.ch_r);
                    (sane(a), sane(b), 0.0, 0.0, 0.0, 0.0)
                };
                // real 5.1 channels in use instead of the upmix (crossfaded)
                self.disc_w += (self.disc_target - self.disc_w).clamp(-disc_step, disc_step);
                let w = self.disc_w;
                self.share += (share_target - self.share).clamp(-share_step, share_step);
                let (a, b) = if role == Some(Upmix::Rear) && self.rear_as_is {
                    // Rear, as most sources come (stereo): what the front
                    // plays, at the same moment, down to these speakers'
                    // limit. The real surround channels when the source
                    // has them.
                    (self.own_hp.run(0, fl + (sl - fl) * w), self.own_hp.run(1, fr + (sr - fr) * w))
                } else if role == Some(Upmix::Rear) {
                    // Rear, the surround matrix: real surround channels when
                    // the source has them, otherwise what differs between
                    // left and right of the stereo, the same on the two
                    // channels (`upmix`): nothing of what is at the centre.
                    let (ul, ur) = upmix(Upmix::Rear, 0.0, fl, fr);
                    // Bass for the rear (if its speakers play everything):
                    // the same bass as the front, read on the front's timing
                    // (newer samples), so it comes out together with the
                    // sub instead of cancelling it.
                    let bass = if rear_full {
                        let pos = self.read_int as f64 + self.read_frac + bass_ahead;
                        let (bi, bf) = (pos.floor() as u64, pos - pos.floor());
                        let (bl, br) = self.sinc.interp(ring, bi, bf, self.chans6_fl, self.chans6_fr);
                        self.bass_lp.process(0.5 * (sane(bl) + sane(br)))
                    } else {
                        0.0
                    };
                    let ul = self.surround_lp[0].process(self.surround_hp[0].process(ul));
                    let ur = self.surround_lp[1].process(self.surround_hp[1].process(ur));
                    // (what these speakers do not play of the surround
                    // channels the front does: `rear_low` there)
                    let a = (ul + bass) + (sl - (ul + bass)) * w;
                    let b = (ur + bass) + (sr - (ur + bass)) * w;
                    (self.own_hp.run(0, a), self.own_hp.run(1, b))
                } else if role == Some(Upmix::Centre) {
                    // Centre speaker: the real centre channel when the source
                    // has one. Otherwise, if the user wants the voices here
                    // too (`Voices`), what left and right have in common
                    // (with a 4-channel source also while its surround
                    // channels play: it has no centre channel). Both down to
                    // this speaker's limit: below it the front has the
                    // stereo whole anyway, and plays the real centre
                    // (`centre_low` there).
                    let share = self.share * if self.has_c_lfe { 1.0 - w } else { 1.0 };
                    let (x, _) = upmix(Upmix::Centre, share, self.mid_hp.run(0, fl), self.mid_hp.run(1, fr));
                    let v = H * (x + w * self.own_hp.run(0, c));
                    (v, v)
                } else if role == Some(Upmix::Front) {
                    // Front with a centre speaker: left and right as they
                    // are, whatever that speaker plays of a stereo source
                    // (`upmix`: the front pair is never taken apart). Of a
                    // 5.1 source the centre channel is that speaker's, down
                    // to its limit: what is below comes out here, with the
                    // LFE and what the rear speakers leave.
                    // (Test click: the centre stays here, whole.)
                    let (xl, xr) = upmix(Upmix::Front, 0.0, fl, fr);
                    let c_bass = self.centre_low.run(0, c);
                    let low = lfe_gain * lfe + H * if clicking { c } else { c_bass };
                    let (el, er) = if !has_rear { (H * sl, H * sr) } else { (self.rear_low.run(0, sl), self.rear_low.run(1, sr)) };
                    (self.own_hp.run(0, xl + low + el), self.own_hp.run(1, xr + low + er))
                } else if has_rear {
                    // Front with a rear present: centre, LFE and (bass
                    // management) what the rear speakers do not play of the
                    // surround channels.
                    let m = H * c + lfe_gain * lfe;
                    let (bl, br) = (self.rear_low.run(0, sl), self.rear_low.run(1, sr));
                    (self.own_hp.run(0, fl + m + bl), self.own_hp.run(1, fr + m + br))
                } else {
                    // No rear: everything folded into stereo.
                    let m = lfe_gain * lfe;
                    (self.own_hp.run(0, fl + H * (c + sl) + m), self.own_hp.run(1, fr + H * (c + sr) + m))
                };
                if self.fade_dir != 0 {
                    self.fade += self.fade_step * self.fade_dir as f32;
                    if self.fade >= 1.0 {
                        self.fade = 1.0;
                        self.fade_dir = 0;
                    } else if self.fade <= 0.0 {
                        self.fade = 0.0;
                        self.fade_dir = 0;
                        self.state = ReaderState::FadedOut;
                    }
                }
                let g = self.gain_cur * self.fade;
                l = a * g;
                r = b * g;
                // Limiter: instant attack, 150 ms release, never above LIMIT.
                if !(l.is_finite() && r.is_finite()) {
                    l = 0.0;
                    r = 0.0;
                }
                let pk = l.abs().max(r.abs());
                let needed = if pk > LIMIT { LIMIT / pk } else { 1.0 };
                self.lim_gain = (self.lim_gain + (1.0 - self.lim_gain) * self.lim_rel).min(needed);
                l *= self.lim_gain;
                r *= self.lim_gain;
                if self.state == ReaderState::Running {
                    self.read_frac += step;
                    let ip = self.read_frac.floor();
                    self.read_int += ip as u64;
                    self.read_frac -= ip;
                }
            }
            l += probe;
            r += probe;
            peak = peak.max(l.abs()).max(r.abs());
            if let Some(t) = tape {
                t.push(l, r);
            }
            Self::emit(out, f, ch_out, self.out_off, l, r);
        }

        if self.five_one {
            let epoch = self.input.source_epoch.load(Relaxed);
            if epoch != self.source_epoch {
                // another source: stereo until its other channels play
                self.source_epoch = epoch;
                self.disc_target = 0.0;
                self.disc_quiet_s = 0.0;
                self.disc_seen_s = 0.0;
                self.disc_total_s = 0.0;
            }
            let dt = n as f64 / self.out_rate_f;
            if disc_peak > 1e-4 {
                // (already 5.1: any sound there keeps it so)
                self.disc_seen_s += dt;
                self.disc_total_s += dt;
                if self.disc_target > 0.5 || self.disc_seen_s >= DISCRETE_AFTER_S {
                    self.disc_target = 1.0;
                    self.disc_quiet_s = 0.0;
                }
            } else {
                self.disc_seen_s = 0.0;
                if front_peak > 1e-4 {
                    self.disc_quiet_s += dt;
                    let hold = (DISCRETE_TRUST * self.disc_total_s).clamp(DISCRETE_HOLD_MIN_S, DISCRETE_HOLD_S);
                    if self.disc_quiet_s > hold {
                        // (the next stretch of sound there starts from nothing)
                        self.disc_target = 0.0;
                        self.disc_total_s = 0.0;
                    }
                }
            }
            // (a centre speaker has a real channel to play only with six of them)
            self.ctl.st_discrete.store(self.disc_target > 0.5 && (role != Some(Upmix::Centre) || self.has_c_lfe), Relaxed);
        }
        self.ctl.st_state.store(self.state as u8, Relaxed);
        self.ctl.st_ppm.store((self.corr * 1e6) as f32);
        self.ctl.st_peak.fetch_max(peak);
    }
}

#[cfg(all(test, local_tests))]
#[path = "local_tests/engine.rs"]
mod tests;
