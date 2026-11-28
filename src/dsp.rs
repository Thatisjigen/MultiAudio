//! Real-time building blocks: lock-free ring buffer, windowed-sinc fractional
//! interpolator (used for delay + clock-drift correction + sample-rate
//! conversion), test signals and cross-correlation for calibration.

use rustfft::{num_complex::Complex, FftPlanner};
use std::f64::consts::PI;
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering::*};
use std::sync::OnceLock;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Small atomics helpers
// ---------------------------------------------------------------------------

/// f32 stored in an AtomicU32 (lock-free, Relaxed: used for parameters/stats).
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    #[inline]
    pub fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Relaxed))
    }
    #[inline]
    pub fn store(&self, v: f32) {
        self.0.store(v.to_bits(), Relaxed)
    }
    #[inline]
    pub fn swap(&self, v: f32) -> f32 {
        f32::from_bits(self.0.swap(v.to_bits(), Relaxed))
    }
    #[inline]
    pub fn fetch_max(&self, v: f32) {
        let mut cur = self.0.load(Relaxed);
        while f32::from_bits(cur) < v {
            match self.0.compare_exchange_weak(cur, v.to_bits(), Relaxed, Relaxed) {
                Ok(_) => break,
                Err(x) => cur = x,
            }
        }
    }
}

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Monotonic nanoseconds since program start (shared by all audio threads).
#[inline]
pub fn now_ns() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

// ---------------------------------------------------------------------------
// Ring buffer: single writer (input callback), many readers (output callbacks)
// ---------------------------------------------------------------------------

/// Multi-channel circular history of an input device. The writer publishes
/// (frames written, time of the write, chunk size) through a seqlock so the
/// readers can extrapolate the writer position between callbacks; this removes
/// the sawtooth jitter that chunked writes would otherwise inject into the
/// drift controller.
pub struct Ring {
    data: Box<[AtomicU32]>,
    channels: usize,
    mask: u64,
    seq: AtomicU64,
    pos: AtomicU64,
    time_ns: AtomicU64,
    chunk: AtomicU64,
}

impl Ring {
    pub fn new(min_frames: usize, channels: usize) -> Self {
        let frames = min_frames.max(1024).next_power_of_two();
        let channels = channels.max(1);
        let data = (0..frames * channels).map(|_| AtomicU32::new(0)).collect::<Vec<_>>();
        Self {
            data: data.into_boxed_slice(),
            channels,
            mask: frames as u64 - 1,
            seq: AtomicU64::new(0),
            pos: AtomicU64::new(0),
            time_ns: AtomicU64::new(0),
            chunk: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> u64 {
        self.mask + 1
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    #[inline]
    pub fn put(&self, frame: u64, ch: usize, v: f32) {
        let i = (frame & self.mask) as usize * self.channels + ch;
        self.data[i].store(v.to_bits(), Relaxed);
    }

    #[inline]
    pub fn get(&self, frame: u64, ch: usize) -> f32 {
        let i = (frame & self.mask) as usize * self.channels + ch;
        f32::from_bits(self.data[i].load(Relaxed))
    }

    /// Frames written so far (for the writer).
    #[inline]
    pub fn write_pos(&self) -> u64 {
        self.pos.load(Acquire)
    }

    /// Writer: make frames `< new_pos` visible to readers.
    pub fn publish(&self, new_pos: u64, time_ns: u64, chunk: u64) {
        let s = self.seq.load(Relaxed);
        self.seq.store(s.wrapping_add(1), Relaxed);
        fence(Release);
        self.time_ns.store(time_ns, Relaxed);
        self.chunk.store(chunk, Relaxed);
        self.pos.store(new_pos, Release);
        self.seq.store(s.wrapping_add(2), Release);
    }

    /// Reader: consistent (frames written, write time, last chunk size).
    pub fn snapshot(&self) -> (u64, u64, u64) {
        for _ in 0..16 {
            let s1 = self.seq.load(Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let p = self.pos.load(Relaxed);
            let t = self.time_ns.load(Relaxed);
            let c = self.chunk.load(Relaxed);
            fence(Acquire);
            if self.seq.load(Relaxed) == s1 {
                return (p, t, c);
            }
        }
        let p = self.pos.load(Acquire);
        (p, self.time_ns.load(Relaxed), self.chunk.load(Relaxed))
    }
}

// ---------------------------------------------------------------------------
// Windowed-sinc fractional interpolator
// ---------------------------------------------------------------------------

/// Taps on each side of the interpolation point.
pub const HALF: usize = 24;
pub const TAPS: usize = HALF * 2;
const PHASES: usize = 512;

/// Polyphase Kaiser-windowed sinc table. Reads a ring at any fractional
/// position with ~-80 dB image rejection; quality is constant whatever the
/// fractional phase, so slow drift correction never "breathes".
pub struct SincTable {
    coefs: Box<[f32]>,
}

impl SincTable {
    /// `cutoff` is relative to the input Nyquist frequency (0..1].
    pub fn new(cutoff: f64) -> Self {
        let beta = 8.0;
        let i0b = bessel_i0(beta);
        let mut coefs = vec![0f32; (PHASES + 1) * TAPS];
        let mut row = [0f64; TAPS];
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let mut sum = 0.0;
            for (k, c) in row.iter_mut().enumerate() {
                let t = k as f64 - (HALF as f64 - 1.0) - frac;
                let x = t / HALF as f64;
                let w = if x.abs() >= 1.0 { 0.0 } else { bessel_i0(beta * (1.0 - x * x).sqrt()) / i0b };
                *c = cutoff * sinc(cutoff * t) * w;
                sum += *c;
            }
            for (k, c) in row.iter().enumerate() {
                coefs[p * TAPS + k] = (c / sum) as f32;
            }
        }
        Self { coefs: coefs.into_boxed_slice() }
    }

    /// Stereo sample at position `i + frac` (requires `i >= HALF - 1` and
    /// frames up to `i + HALF` already written).
    #[inline]
    pub fn interp(&self, ring: &Ring, i: u64, frac: f64, ch_l: usize, ch_r: usize) -> (f32, f32) {
        let pf = frac * PHASES as f64;
        let p = (pf as usize).min(PHASES - 1);
        let t = (pf - p as f64) as f32;
        let a = &self.coefs[p * TAPS..(p + 1) * TAPS];
        let b = &self.coefs[(p + 1) * TAPS..(p + 2) * TAPS];
        let start = i + 1 - HALF as u64;
        let (mut l, mut r) = (0f32, 0f32);
        for k in 0..TAPS {
            let c = a[k] + (b[k] - a[k]) * t;
            let f = start + k as u64;
            l += c * ring.get(f, ch_l);
            r += c * ring.get(f, ch_r);
        }
        (l, r)
    }
}

impl SincTable {
    /// Like `interp`, for any set of channels (one coefficient pass).
    #[inline]
    pub fn interp_n<const N: usize>(&self, ring: &Ring, i: u64, frac: f64, chans: &[usize; N]) -> [f32; N] {
        let pf = frac * PHASES as f64;
        let p = (pf as usize).min(PHASES - 1);
        let t = (pf - p as f64) as f32;
        let a = &self.coefs[p * TAPS..(p + 1) * TAPS];
        let b = &self.coefs[(p + 1) * TAPS..(p + 2) * TAPS];
        let start = i + 1 - HALF as u64;
        let mut acc = [0f32; N];
        for k in 0..TAPS {
            let c = a[k] + (b[k] - a[k]) * t;
            let f = start + k as u64;
            for (j, ch) in chans.iter().enumerate() {
                acc[j] += c * ring.get(f, *ch);
            }
        }
        acc
    }
}

fn bessel_i0(x: f64) -> f64 {
    let q = x * x / 4.0;
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..64 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < 1e-14 * sum {
            break;
        }
    }
    sum
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

// ---------------------------------------------------------------------------
// Biquad filter (RBJ cookbook), used by the surround upmix
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn from(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0: (b0 / a0) as f32,
            b1: (b1 / a0) as f32,
            b2: (b2 / a0) as f32,
            a1: (a1 / a0) as f32,
            a2: (a2 / a0) as f32,
            z1: 0.0,
            z2: 0.0,
        }
    }

    pub fn lowpass(fs: f64, fc: f64, q: f64) -> Self {
        let w = 2.0 * PI * fc / fs;
        let (c, alpha) = (w.cos(), w.sin() / (2.0 * q));
        Self::from((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }

    pub fn highpass(fs: f64, fc: f64, q: f64) -> Self {
        let w = 2.0 * PI * fc / fs;
        let (c, alpha) = (w.cos(), w.sin() / (2.0 * q));
        Self::from((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

/// Crossover filter (Linkwitz–Riley, 24 dB per octave: two Butterworth
/// sections in a row). The low-pass and the high-pass at one frequency add
/// up to the whole signal again (flat, only its phase turned), so a sound
/// can be shared between a speaker that plays the bass and one that does
/// not without a hole or a bump where they meet.
#[derive(Clone, Copy)]
pub struct Lr4([Biquad; 2]);

impl Lr4 {
    pub fn lowpass(fs: f64, fc: f64) -> Self {
        Self([Biquad::lowpass(fs, fc, std::f64::consts::FRAC_1_SQRT_2); 2])
    }

    pub fn highpass(fs: f64, fc: f64) -> Self {
        Self([Biquad::highpass(fs, fc, std::f64::consts::FRAC_1_SQRT_2); 2])
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.0[0].process(x);
        self.0[1].process(y)
    }

    pub fn reset(&mut self) {
        self.0[0].reset();
        self.0[1].reset();
    }
}

// ---------------------------------------------------------------------------
// Surround upmix: a fixed matrix
// ---------------------------------------------------------------------------

/// A place around a stereo pair that `upmix` makes the signal for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Upmix {
    /// The rear pair.
    Rear,
    /// The front pair, when there is a centre speaker.
    Front,
    /// What a centre speaker plays (the same on both outputs).
    Centre,
}

/// How much of the difference between left and right the rear plays (see
/// `upmix`): half, so that what it plays never exceeds full scale (the
/// output limiter never has to work on it).
pub const REAR_SIDE: f32 = 0.5;

/// What the speakers around a stereo pair play of a stereo signal: always
/// the same mix of left and right, sample by sample (a fixed matrix:
/// nothing in it follows the signal).
/// * `Upmix::Front`: left and right as they are. Whatever else plays, the
///   front pair by itself is the stereo.
/// * `Upmix::Centre`, for a centre speaker: what the two channels have in
///   common, M = (L + R) / 2, times √(2·`share`). 0: silent
///   (`Config::stereo_centre` off); 1: a source at the centre as loud as
///   the front pair plays it. In between only while the engine moves from
///   one to the other.
/// * `Upmix::Rear`: `REAR_SIDE`·(L − R), the same on the two channels (the
///   one surround channel of Dolby Surround). What left and right share (a
///   voice, anything at the centre) is not there at all; what is on one
///   side comes out 6 dB down; what is matrixed as surround into a film
///   soundtrack (anti-phase) at its level. The same on the two channels
///   and not one the other turned over: a speaker that adds its two
///   channels up would play nothing of that.
///
/// The voices and the rear, 2026-11-27. That afternoon the rear was made
/// 0.64·its own side − 0.36·the other (the passive surround of Pro Logic
/// II), which leaves what is at the centre 11 dB down, in phase on the two
/// channels: the user's "distorto", "le voci fanno bzzt", "come quando
/// camuffano le voci", with a film and with YouTube in a browser, had been
/// put down to the difference signal. They were the capture, which lost
/// part of every block the browser played (see `mactap::Tuning`). With
/// the capture whole, on that matrix, he: "si sente bene ma dietro si
/// sente ancora la voce". So the rear is the difference alone again.
///
/// What that costs, measured. L − R takes out what the two channels share,
/// and of a voice it leaves its reverb and what the codec of the source
/// put there: streamed sound is coded as sum and difference, and the
/// difference gets the bits that are left. With Apple's encoders (AAC at
/// 96 and 128 kbit/s, HE-AAC at 64, Opus at 96 and 128): under a voice
/// alone at the centre, where L − R should be silent, it is the codec's
/// own and nothing else, 42 to 56 dB under the voice; with ambience 25 dB
/// under the voice, the codec's part of L − R is as large as the ambience
/// (Opus) or 3 to 6 dB under it (AAC). The rear plays this only if the
/// user chooses it (`config::StereoRear`).
///
/// This used to be done band by band from the short-term statistics of the
/// signal (what is correlated between the channels here, what is not
/// there). That separates more: a voice on the centre speaker alone, an
/// instrument on one side kept out of the rear. But what each speaker plays
/// then changes from moment to moment with the signal, and that is heard: a
/// hiss that opens and closes with every syllable, a buzz on voices over any
/// background (measured on dialogue over room tone 20 dB below it: sound
/// that no fixed mix of left and right accounts for, 8 to 12 dB under the
/// level of each speaker; the user's "tutto fruscioso con bzzt" of
/// 2026-11-27, a film in a browser, was also the capture losing part of
/// every block, see `mactap::Tuning`).
/// Here there is none, whatever plays, and no latency.
#[inline]
pub fn upmix(place: Upmix, share: f32, l: f32, r: f32) -> (f32, f32) {
    match place {
        Upmix::Rear => {
            let side = REAR_SIDE * (l - r);
            (side, side)
        }
        Upmix::Front => (l, r),
        Upmix::Centre => {
            let c = (2.0 * share.clamp(0.0, 1.0)).sqrt() * 0.5 * (l + r);
            (c, c)
        }
    }
}

// ---------------------------------------------------------------------------
// Test signals
// ---------------------------------------------------------------------------

/// Short percussive click (3 ms, 2.5 kHz burst) used for listening tests.
pub fn make_click(rate: f64) -> Vec<f32> {
    let n = (0.003 * rate) as usize;
    (0..n)
        .map(|i| {
            let w = 0.5 - 0.5 * (2.0 * PI * i as f64 / n as f64).cos();
            (0.7 * w * (2.0 * PI * 2500.0 * i as f64 / rate).sin()) as f32
        })
        .collect()
}

/// Calibration probe: 250 ms linear chirp 400 Hz → 7 kHz (Tukey windowed).
/// Its autocorrelation is a sharp spike, so arrival times can be measured to
/// a fraction of a millisecond even in a reverberant room.
pub fn make_probe(rate: f64) -> Vec<f32> {
    let dur = 0.25;
    let (f0, f1) = (400.0, 7000.0);
    let n = (dur * rate) as usize;
    let taper = (0.1 * n as f64) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / rate;
            let ph = 2.0 * PI * (f0 * t + (f1 - f0) * t * t / (2.0 * dur));
            let w = if i < taper {
                0.5 - 0.5 * (PI * i as f64 / taper as f64).cos()
            } else if i >= n - taper {
                0.5 - 0.5 * (PI * (n - 1 - i) as f64 / taper as f64).cos()
            } else {
                1.0
            };
            (0.5 * w * ph.sin()) as f32
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Correlation / arrival detection
// ---------------------------------------------------------------------------

/// c[t] = Σ_j sig[t + j] · probe[j]  for t in 0..=sig.len()-probe.len()
pub fn xcorr(sig: &[f32], probe: &[f32]) -> Vec<f32> {
    if probe.is_empty() || sig.len() < probe.len() {
        return Vec::new();
    }
    let n = (sig.len() + probe.len()).next_power_of_two();
    let mut planner = FftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);
    let zero = Complex::new(0.0f32, 0.0);
    let mut a: Vec<Complex<f32>> = sig.iter().map(|&x| Complex::new(x, 0.0)).collect();
    a.resize(n, zero);
    let mut b: Vec<Complex<f32>> = probe.iter().map(|&x| Complex::new(x, 0.0)).collect();
    b.resize(n, zero);
    fwd.process(&mut a);
    fwd.process(&mut b);
    for (x, y) in a.iter_mut().zip(b.iter()) {
        *x *= y.conj();
    }
    inv.process(&mut a);
    let scale = 1.0 / n as f32;
    a[..sig.len() - probe.len() + 1].iter().map(|c| c.re * scale).collect()
}

/// Energy of the probe's own correlation in the stretch `arrival_level_db`
/// adds up: what a probe recorded as it is, at full scale, would give.
pub fn probe_level_ref(rate: f64) -> f64 {
    let probe = make_probe(rate);
    let (pre, post) = level_window(rate);
    let mut sig = vec![0.0; pre];
    sig.extend_from_slice(&probe);
    sig.resize(pre + probe.len() + post, 0.0);
    xcorr(&sig, &probe).iter().map(|x| (*x as f64).powi(2)).sum()
}

/// Samples before and after the arrival that count as one sound: the
/// direct one and the first reflections (50 ms, what the ear takes as its
/// loudness; the later reverberation does not make a voice clearer).
fn level_window(rate: f64) -> (usize, usize) {
    ((0.002 * rate) as usize, (0.05 * rate) as usize)
}

/// How loud the probe arrived at `arrival` in the correlation `c`: the
/// energy around it less the noise of the room (measured in the rest of
/// `c[from..to]`), in dB against `reference` (`probe_level_ref`). Only the
/// difference between two outputs measured with the same microphone means
/// something. `None`: the noise is as strong as what arrived.
pub fn arrival_level_db(c: &[f32], arrival: usize, from: usize, to: usize, rate: f64, reference: f64) -> Option<f32> {
    let to = to.min(c.len());
    if arrival < from || arrival >= to || reference <= 0.0 {
        return None;
    }
    let (pre, post) = level_window(rate);
    let (a, b) = (arrival.saturating_sub(pre).max(from), (arrival + post).min(to));
    // the room alone: away from the arrival and from its tail (median, so
    // that what is left of a tail does not count)
    let (q0, q1) = (arrival.saturating_sub((0.03 * rate) as usize), arrival + (0.4 * rate) as usize);
    let mut quiet: Vec<f64> = (from..to).filter(|i| *i < q0 || *i >= q1).map(|i| (c[i] as f64).powi(2)).collect();
    if quiet.len() < (0.05 * rate) as usize {
        return None;
    }
    let mid = quiet.len() / 2;
    let (_, med, _) = quiet.select_nth_unstable_by(mid, |x, y| x.total_cmp(y));
    // (median of the square of a Gaussian: 0.455 of its mean)
    let noise = *med / 0.455 * (b - a) as f64;
    let energy = c[a..b].iter().map(|x| (*x as f64).powi(2)).sum::<f64>() - noise;
    (energy > noise.max(1e-30)).then(|| (10.0 * (energy / reference).log10()) as f32)
}

/// Finds the direct-path arrival of the probe in `c[from..to]`.
/// Returns (index, peak/median ratio). Prefers the earliest strong peak so a
/// loud wall reflection does not win over a weaker direct sound.
pub fn find_arrival(c: &[f32], from: usize, to: usize, rate: f64) -> Option<(f64, f32)> {
    let to = to.min(c.len());
    if from + 32 >= to {
        return None;
    }
    let w = &c[from..to];
    let (mut m, mut v) = (0usize, 0f32);
    for (i, x) in w.iter().enumerate() {
        if x.abs() > v {
            v = x.abs();
            m = i;
        }
    }
    if v <= 0.0 {
        return None;
    }
    let mut mags: Vec<f32> = w.iter().map(|x| x.abs()).collect();
    let mid = mags.len() / 2;
    let (_, med, _) = mags.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
    let snr = v / (*med + 1e-12);

    let back = (0.025 * rate) as usize;
    let mut first = m;
    for (i, x) in w.iter().enumerate().take(m).skip(m.saturating_sub(back)) {
        if x.abs() >= 0.5 * v {
            first = i;
            break;
        }
    }
    let r = (0.0006 * rate) as usize + 1;
    let (a, b) = (first.saturating_sub(r), (first + r).min(w.len() - 1));
    let mut best = first;
    for i in a..=b {
        if w[i].abs() > w[best].abs() {
            best = i;
        }
    }
    Some(((from + best) as f64, snr))
}

#[cfg(all(test, local_tests))]
#[path = "local_tests/dsp.rs"]
pub(crate) mod tests;
