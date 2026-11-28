//! Diagnosis: a few seconds of what MultiAudio receives and of what it sends
//! to each output, as sound files.
//!
//! What a listener calls distorted cannot be told from the numbers the
//! engine keeps (buffers, drift, interruptions: all of them fine while the
//! sound was not). These files are the sound itself, at the two points
//! MultiAudio can vouch for: as the system hands it over, before anything is
//! done to it, and as each output's device is given it, after everything.
//! If the first is already wrong the fault is before MultiAudio (the
//! capture); if only the second, in MultiAudio; if neither, after it (the
//! device, the speakers) or in what each output is meant to play.
//!
//! Nothing is recorded unless asked: the button in the advanced settings,
//! the file `ARMED` put next to the recordings (the next sound is recorded,
//! once), or `MULTIAUDIO_RECORD` in headless mode. Each recording replaces
//! the one before.

use crate::engine::{Engine, InputShared, RouteCtl, Tape};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Seconds recorded.
pub const SECS: f64 = 10.0;
/// A file of this name in `dir()` asks for one recording of the next sound.
pub const ARMED: &str = "registra-al-prossimo-suono";

/// Where the recording goes (next to the log).
pub fn dir() -> PathBuf {
    crate::diag::path().with_file_name("diagnosi")
}

/// Is a recording of the next sound asked for (see `ARMED`)?
pub fn armed() -> bool {
    dir().join(ARMED).exists()
}

pub fn disarm() {
    let _ = std::fs::remove_file(dir().join(ARMED));
}

/// One output as it is recorded: its name and what the window knows of it.
pub struct Track {
    pub name: String,
    pub notes: String,
    pub ctl: Arc<RouteCtl>,
    pub input: Arc<InputShared>,
}

/// The outputs of a running engine (`name`, `notes`: per route).
pub fn tracks(e: &Engine, describe: impl Fn(usize) -> (String, String)) -> Vec<Track> {
    (0..e.route_ctl.len())
        .filter_map(|i| {
            let (name, notes) = describe(i);
            Some(Track { name, notes, ctl: e.route_ctl.get(i).cloned().flatten()?, input: e.route_input.get(i).cloned().flatten()? })
        })
        .collect()
}

fn wav(path: &Path, rate: u32, channels: usize, samples: &[f32]) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let bytes = (samples.len() * 4) as u32;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + bytes).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&3u16.to_le_bytes())?; // IEEE float
    f.write_all(&(channels as u16).to_le_bytes())?;
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&(rate * channels as u32 * 4).to_le_bytes())?;
    f.write_all(&((channels * 4) as u16).to_le_bytes())?;
    f.write_all(&32u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&bytes.to_le_bytes())?;
    for s in samples {
        f.write_all(&s.to_le_bytes())?;
    }
    f.flush()
}

fn file_name(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

/// Records `SECS` seconds from now, in the background. `about`: whatever
/// else is worth writing down with it (the settings in force).
pub fn start(tracks: Vec<Track>, about: String) -> Receiver<Result<PathBuf, String>> {
    let (tx, rx) = channel();
    let spawned = std::thread::Builder::new().name("multiaudio-diagnosis".into()).spawn(move || {
        let _ = tx.send(run(&tracks, &about, SECS, &dir()).map_err(|e| e.to_string()));
    });
    if let Err(e) = spawned {
        crate::diag::log(format!("diagnosi: non parte ({e})"));
    }
    rx
}

pub(crate) fn run(tracks: &[Track], about: &str, secs: f64, dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    for old in std::fs::read_dir(dir)?.flatten() {
        let p = old.path();
        if p.extension().is_some_and(|x| x == "wav" || x == "txt") {
            let _ = std::fs::remove_file(p);
        }
    }
    // the inputs, once each
    let mut inputs: Vec<Arc<InputShared>> = Vec::new();
    for t in tracks {
        if !inputs.iter().any(|i| Arc::ptr_eq(i, &t.input)) {
            inputs.push(t.input.clone());
        }
    }
    let mut got: Vec<(u64, u64, Vec<f32>)> = inputs.iter().map(|i| (i.ring.snapshot().0, i.ring.snapshot().0, Vec::new())).collect();
    for t in tracks {
        let rate = t.ctl.out_rate.load(Relaxed).max(8000) as f64;
        t.ctl.tape.get_or_init(|| Tape::new(((SECS.max(secs) + 2.0) * rate) as usize)).start();
    }
    let copy = |got: &mut Vec<(u64, u64, Vec<f32>)>| {
        for (inp, g) in inputs.iter().zip(got.iter_mut()) {
            let (wpos, _, _) = inp.ring.snapshot();
            // (never further back than the ring still holds)
            let from = g.1.max(wpos.saturating_sub(inp.ring.capacity() / 2));
            for f in from..wpos {
                for c in 0..inp.ring.channels() {
                    g.2.push(inp.ring.get(f, c));
                }
            }
            g.1 = wpos;
        }
    };
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < secs {
        std::thread::sleep(Duration::from_millis(50));
        copy(&mut got);
    }
    let taken = t0.elapsed().as_secs_f64();
    let tapes: Vec<Vec<f32>> = tracks.iter().map(|t| t.ctl.tape.get().map(|x| x.stop()).unwrap_or_default()).collect();
    copy(&mut got);

    let mut info = String::new();
    let line = |info: &mut String, s: String| {
        info.push_str(&s);
        info.push('\n');
    };
    line(&mut info, format!("MultiAudio {}: {taken:.2} s registrati", env!("CARGO_PKG_VERSION")));
    line(&mut info, about.to_string());
    for (k, (inp, g)) in inputs.iter().zip(&got).enumerate() {
        let ch = inp.ring.channels();
        let name = if k == 0 { "ingresso.wav".to_string() } else { format!("ingresso-{}.wav", k + 1) };
        wav(&dir.join(&name), inp.rate.round() as u32, ch, &g.2)?;
        let frames = g.2.len() / ch.max(1);
        line(
            &mut info,
            format!(
                "{name}: «{}», {:.0} Hz, {ch} canali, {frames} campioni ({:+.0} ppm sul nominale in {taken:.2} s) in blocchi da {}, arrivi in ritardo {}, buchi {}, pezzi mancanti {} in {} blocchi ({})",
                crate::backend::display_name(&inp.id),
                inp.rate,
                (frames as f64 / (taken * inp.rate) - 1.0) * 1e6,
                inp.ring.snapshot().2,
                inp.late.count.load(Relaxed),
                inp.holes.count.load(Relaxed),
                inp.torn.load(Relaxed),
                inp.blocks.load(Relaxed),
                crate::backend::capture_tuning(),
            ),
        );
        // when the sound got here: the last block that came in, as a place in the file and a time of day
        // (to measure, with a player that says when it played what, how long the system takes to hand it over)
        let (wpos, at_ns, chunk) = inp.ring.snapshot();
        let since_ns = crate::dsp::now_ns().saturating_sub(at_ns);
        if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            line(
                &mut info,
                format!(
                    "  il blocco che comincia al campione {} di {name} è arrivato a {} µs dall'epoca",
                    wpos as i64 - chunk as i64 - g.0 as i64,
                    now.as_micros() as i128 - since_ns as i128 / 1000
                ),
            );
        }
    }
    for (k, (t, tape)) in tracks.iter().zip(&tapes).enumerate() {
        let rate = t.ctl.out_rate.load(Relaxed);
        let name = format!("uscita-{}-{}.wav", k + 1, file_name(&t.name));
        wav(&dir.join(&name), rate, 2, tape)?;
        line(
            &mut info,
            format!(
                "{name}: «{}», {rate} Hz, {} campioni, ritardo {:.1} ms, dietro {}, centro {}, taglio bassi {:.0} Hz, livello {:.2}, volume {:.2}×{:.2}, buffer {:.1} ms, deriva {:+.0} ppm, interruzioni {}, riagganci {}. {}",
                t.name,
                tape.len() / 2,
                t.ctl.delay_ms.load(),
                t.ctl.rear.load(Relaxed),
                t.ctl.centre.load(Relaxed),
                t.ctl.xo_hz.load(),
                t.ctl.trim.load(),
                t.ctl.gain.load(),
                t.ctl.sys_gain.load(),
                t.ctl.st_buffer_ms.load(),
                t.ctl.st_ppm.load(),
                t.ctl.st_underruns.load(Relaxed),
                t.ctl.st_jumps.load(Relaxed),
                t.notes,
            ),
        );
    }
    std::fs::write(dir.join("leggimi.txt"), &info)?;
    crate::diag::log(format!("diagnosi: registrati {taken:.1} s in {}", dir.display()));
    Ok(dir.to_path_buf())
}

#[cfg(all(test, local_tests))]
#[path = "local_tests/record.rs"]
mod tests;
