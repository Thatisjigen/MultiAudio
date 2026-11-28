//! Automatic alignment with a microphone.
//!
//! Every route plays a short chirp in turn. The chirp is time-stamped on the
//! input timeline (the position the route was reading when it started), and
//! the microphone tells when it actually came out of the speaker. For route k:
//!
//!     D_k = arrival_k(mic seconds) − position_k(input seconds)
//!
//! D_k is "how late route k plays the input", up to a constant shared by all
//! routes (mic latency etc.). Aligning means making every D_k equal:
//! new_delay_k = delay_k + (D_target − D_k).
//!
//! With a reference output (played by the OS, e.g. Bluetooth) the chirp is
//! also played on it: the mic gives its arrival, and the same chirp is found
//! inside the input ring (because the input is a loopback of what the OS plays)
//! which gives its position on the input timeline → D_ref. The routes are then
//! aligned to the reference instead of to each other.

use crate::backend;
use crate::dsp::{arrival_level_db, find_arrival, make_probe, probe_level_ref, xcorr};
use crate::engine::{InputShared, RefCtl, RouteCtl, MAX_DELAY_MS};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::*};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SHOT_S: f64 = 1.3;
const MIN_SNR: f32 = 8.0;

pub struct CalRoute {
    pub index: usize,
    pub name: String,
    pub ctl: Arc<RouteCtl>,
    pub input: Arc<InputShared>,
    pub delay_ms: f32,
    /// Extra delay the engine adds on top of `delay_ms` (surround rear):
    /// kept, so calibration does not undo it.
    pub extra_ms: f32,
}

pub struct CalRequest {
    pub mic: String,
    pub routes: Vec<CalRoute>,
    pub reference: Option<(String, Arc<RefCtl>)>,
    /// Without a reference: the delay the least delayed route gets (0,
    /// unless the routes wait for an output that is not playing now and
    /// must keep their place against it).
    pub base_ms: f32,
}

pub enum CalMsg {
    Log(String),
    /// The new delays (route, ms) and how loud each output sounded to the
    /// microphone (route, dB: only the differences between them count).
    Done { delays: Vec<(usize, f32)>, levels: Vec<(usize, f32)> },
    Failed(String),
}

struct MicRec {
    buf: Vec<AtomicU32>,
    len: AtomicUsize,
}

/// Unmutes the routes whatever happens.
struct MuteGuard<'a>(&'a [CalRoute]);
impl Drop for MuteGuard<'_> {
    fn drop(&mut self) {
        for r in self.0 {
            r.ctl.cal_mute.store(false, Relaxed);
        }
    }
}

pub fn spawn(req: CalRequest, tx: Sender<CalMsg>) {
    std::thread::Builder::new()
        .name("multiaudio-calibration".into())
        .spawn(move || {
            let msg = match run(&req, &tx) {
                Ok((delays, levels)) => CalMsg::Done { delays, levels },
                Err(e) => CalMsg::Failed(e),
            };
            let _ = tx.send(msg);
        })
        .expect("spawn calibration thread");
}

fn sleep(s: f64) {
    std::thread::sleep(Duration::from_secs_f64(s));
}

type Measured = (Vec<(usize, f32)>, Vec<(usize, f32)>);

fn run(req: &CalRequest, tx: &Sender<CalMsg>) -> Result<Measured, String> {
    let log = |s: String| {
        let _ = tx.send(CalMsg::Log(s));
    };
    if req.routes.is_empty() {
        return Err("nessuna uscita attiva da calibrare".into());
    }
    if req.mic.is_empty() {
        return Err("scegli il microfono".into());
    }

    // ---- microphone ----
    let fmt = backend::input_format(&req.mic).map_err(|e| format!("microfono: {e}"))?;
    let mic_rate = fmt.rate as f64;
    let total_s = 1.0 + if req.reference.is_some() { SHOT_S + 0.3 } else { 0.0 } + req.routes.len() as f64 * SHOT_S + 1.5;
    let rec = Arc::new(MicRec {
        buf: (0..(total_s * mic_rate) as usize).map(|_| AtomicU32::new(0)).collect(),
        len: AtomicUsize::new(0),
    });
    let r2 = rec.clone();
    let dch = fmt.channels.max(1);
    let cb: backend::InputCallback = Box::new(move |d: &[f32]| {
        let mut w = r2.len.load(Relaxed);
        for frame in d.chunks_exact(dch) {
            if w >= r2.buf.len() {
                break;
            }
            r2.buf[w].store(frame[0].to_bits(), Relaxed);
            w += 1;
        }
        r2.len.store(w, Release);
    });
    let mic_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let me = mic_err.clone();
    let stream = backend::open_input(&req.mic, &fmt, cb, Arc::new(move |m| *me.lock().unwrap() = Some(m)))
        .map_err(|e| format!("microfono: {e}"))?;
    sleep(0.7);

    for r in &req.routes {
        r.ctl.cal_mute.store(true, Relaxed);
    }
    let _guard = MuteGuard(&req.routes);
    sleep(0.1);

    // ---- reference shot ----
    let mut ref_shot = None;
    if let Some((label, rc)) = &req.reference {
        log(format!("Segnale di prova sul riferimento «{label}»…"));
        let m0 = rec.len.load(Acquire);
        rc.probe_done.store(false, Relaxed);
        rc.probe_req.store(true, Relaxed);
        sleep(SHOT_S + 0.3);
        if !rc.probe_done.load(Acquire) {
            log("  ⚠ il riferimento non ha suonato il segnale".into());
        }
        // Grab the recent input history to find the chirp there too.
        let mut snaps = Vec::new();
        let mut seen: Vec<*const InputShared> = Vec::new();
        for r in &req.routes {
            let p = Arc::as_ptr(&r.input);
            if seen.contains(&p) {
                continue;
            }
            seen.push(p);
            let ring = &r.input.ring;
            let (wpos, _, _) = ring.snapshot();
            let len = ((SHOT_S + 0.6) * r.input.rate) as u64;
            let len = len.min(wpos).min(ring.capacity() / 2);
            let start = wpos - len;
            let two = ring.channels() > 1;
            let data: Vec<f32> = (start..wpos)
                .map(|f| if two { 0.5 * (ring.get(f, 0) + ring.get(f, 1)) } else { ring.get(f, 0) })
                .collect();
            snaps.push((r.input.clone(), start, data));
        }
        ref_shot = Some((m0, snaps));
    }

    // ---- one shot per route ----
    let mut shots = Vec::new();
    for r in &req.routes {
        log(format!("Segnale di prova su «{}»…", r.name));
        let m0 = rec.len.load(Acquire);
        r.ctl.probe_done.store(false, Relaxed);
        r.ctl.probe_req.store(true, Relaxed);
        sleep(SHOT_S);
        let pos = if r.ctl.probe_done.load(Acquire) { f64::from_bits(r.ctl.probe_pos.load(Relaxed)) } else { f64::NAN };
        shots.push((m0, pos));
    }
    sleep(0.2);
    drop(_guard);
    drop(stream);
    if let Some(e) = mic_err.lock().unwrap().take() {
        return Err(format!("microfono: {e}"));
    }

    // ---- analysis ----
    let n = rec.len.load(Acquire);
    let mic: Vec<f32> = rec.buf[..n].iter().map(|a| f32::from_bits(a.load(Relaxed))).collect();
    let peak = mic.iter().fold(0f32, |m, v| m.max(v.abs()));
    if peak < 1e-4 {
        return Err("il microfono non registra nulla (controlla il permesso Microfono nelle impostazioni di sistema)".into());
    }
    log("Analisi…".into());
    let probe = make_probe(mic_rate);
    let probe_len = probe.len();
    let c = xcorr(&mic, &probe);
    let win = (SHOT_S * mic_rate) as usize;
    let level_ref = probe_level_ref(mic_rate);

    let mut d: Vec<Option<f64>> = vec![None; req.routes.len()];
    // how loud each output is here (the same probe on all of them)
    let mut levels: Vec<(usize, f32)> = Vec::new();
    for (k, ((m0, pos), r)) in shots.iter().zip(&req.routes).enumerate() {
        if pos.is_nan() {
            log(format!("  ⚠ «{}» non stava suonando (nessun audio in ingresso?)", r.name));
            continue;
        }
        match find_arrival(&c, *m0, m0 + win, mic_rate) {
            Some((idx, snr)) if snr >= MIN_SNR => {
                d[k] = Some(idx / mic_rate - pos / r.input.rate - r.extra_ms as f64 / 1000.0);
                // (a microphone driven into clipping reads a loud output
                // lower than it is: no level from it)
                let (a, b) = (idx as usize, (idx as usize + probe_len).min(mic.len()));
                let clipped = mic[a.min(b)..b].iter().any(|v| v.abs() >= 0.98);
                if clipped {
                    log(format!("  ⚠ «{}» è troppo forte per il microfono (satura): il suo volume non viene misurato. Abbassa il guadagno del microfono o allontanalo, e ricalibra", r.name));
                } else if let Some(db) = arrival_level_db(&c, idx as usize, *m0, m0 + win, mic_rate, level_ref) {
                    levels.push((k, db));
                }
                log(format!("  «{}»: sentito (qualità {snr:.0})", r.name));
            }
            other => {
                let snr = other.map(|o| o.1).unwrap_or(0.0);
                log(format!("  ⚠ «{}»: non sentito dal microfono (qualità {snr:.1}) — alza il volume o avvicina il microfono", r.name));
            }
        }
    }

    // D of the reference, per input timeline
    let mut ref_d: Vec<(*const InputShared, f64)> = Vec::new();
    if let Some((m0, snaps)) = ref_shot {
        match find_arrival(&c, m0, m0 + ((SHOT_S + 0.3) * mic_rate) as usize, mic_rate) {
            Some((idx, snr)) if snr >= MIN_SNR => {
                log(format!("  riferimento: sentito (qualità {snr:.0})"));
                let m_ref = idx / mic_rate;
                for (input, start, data) in snaps {
                    let ci = xcorr(&data, &make_probe(input.rate));
                    match find_arrival(&ci, 0, ci.len(), input.rate) {
                        Some((ii, s2)) if s2 >= MIN_SNR => {
                            ref_d.push((Arc::as_ptr(&input), m_ref - (start as f64 + ii) / input.rate));
                        }
                        _ => log(format!(
                            "  ⚠ l'ingresso «{}» non riceve l'audio del riferimento: allineo le uscite solo tra loro",
                            backend::display_name(&input.id)
                        )),
                    }
                }
            }
            _ => log("  ⚠ riferimento non sentito dal microfono: allineo le uscite solo tra loro".into()),
        }
    }

    // ---- new delays, per input group ----
    let mut result = Vec::new();
    let mut done = vec![false; req.routes.len()];
    for k in 0..req.routes.len() {
        if done[k] {
            continue;
        }
        let p = Arc::as_ptr(&req.routes[k].input);
        let group: Vec<usize> = (0..req.routes.len()).filter(|&j| Arc::as_ptr(&req.routes[j].input) == p).collect();
        for &j in &group {
            done[j] = true;
        }
        let measured: Vec<(usize, f64)> = group.iter().filter_map(|&j| d[j].map(|v| (j, v))).collect();
        if measured.is_empty() {
            continue;
        }
        let reference = ref_d.iter().find(|(q, _)| *q == p).map(|x| x.1);
        let target = reference.unwrap_or_else(|| measured.iter().map(|x| x.1).fold(f64::MIN, f64::max));
        let mut new: Vec<(usize, f64)> =
            measured.iter().map(|&(j, dj)| (j, req.routes[j].delay_ms as f64 + (target - dj) * 1000.0)).collect();
        if reference.is_none() {
            // Nothing to wait for: the slowest route gets 0 ms (or keeps
            // its place against an output that is not playing now).
            let min = new.iter().map(|x| x.1).fold(f64::MAX, f64::min);
            for x in new.iter_mut() {
                x.1 += req.base_ms as f64 - min;
            }
        }
        if measured.len() < group.len() && reference.is_none() {
            log("  ⚠ non tutte le uscite sono state sentite: risultato parziale".into());
        }
        for (j, v) in new {
            let r = &req.routes[j];
            let mut v = v;
            if v < 0.0 {
                log(format!(
                    "  ⚠ «{}» è già {:.0} ms più lenta del riferimento: riduci il buffer di sicurezza",
                    r.name, -v
                ));
                v = 0.0;
            }
            let v = v.min(MAX_DELAY_MS as f64) as f32;
            log(format!("  «{}»: {:.1} ms → {:.1} ms", r.name, r.delay_ms, v));
            result.push((r.index, (v * 10.0).round() / 10.0));
        }
    }
    if result.is_empty() {
        return Err("nessuna misura valida".into());
    }
    if levels.len() > 1 {
        let top = levels.iter().map(|x| x.1).fold(f32::MIN, f32::max);
        let parts: Vec<String> = levels
            .iter()
            .map(|(k, db)| {
                let name = &req.routes[*k].name;
                if top - db < 0.5 {
                    format!("«{name}» la più forte")
                } else {
                    format!("«{name}» {:.0} dB in meno", top - db)
                }
            })
            .collect();
        log(format!("  volume al microfono: {}", parts.join(", ")));
    }
    Ok((result, levels.into_iter().map(|(k, db)| (req.routes[k].index, db)).collect()))
}
