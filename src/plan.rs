//! Turns "these outputs, please" into an engine setup, choosing by itself how
//! to capture the system audio and which output the video follows.
//!
//! * macOS: apps play to MultiAudio's own output «MultiAudio» (the system
//!   output while it runs), which MultiAudio captures and sends to every
//!   output. It declares the latency of the slowest output: players read it
//!   and delay the video by it. Without it (older macOS): a system-audio tap,
//!   and the slowest output as system output.
//! * Windows / Linux: the slowest output is played by the system itself
//!   (players delay the video for it), MultiAudio captures what is sent to it
//!   (loopback / monitor) and feeds the other outputs, delayed to match.

use crate::backend::{self, DeviceList};
use crate::config::{Config, RouteConfig};

pub struct Plan {
    /// Config with `routes` and `reference` filled in, for the engine.
    pub engine_cfg: Config,
    /// Engine route index → index in `cfg.outputs`.
    pub route_output: Vec<usize>,
    /// Output the video is synchronised to.
    pub main: Option<String>,
    /// What the system output should be (MultiAudio's own output on macOS,
    /// otherwise `main`).
    pub system_output: Option<String>,
    /// The main output is played by the system, not by MultiAudio.
    pub main_by_system: bool,
    /// How much of every delay is only waiting for an output that is not
    /// connected, and is left out (see `idle_wait_ms`).
    pub idle_wait_ms: f32,
    pub problem: Option<String>,
}

/// Estimated latency of MultiAudio's own path (capture + buffering), ms.
const OWN_PATH_MS: f32 = 10.0;

/// Where an output is as it counts now (behind and centre only with
/// surround on) and how much bass it plays there.
pub fn placement(cfg: &Config, o: &crate::config::OutputConfig) -> crate::engine::Placement {
    let centre = cfg.surround && o.centre;
    let rear = cfg.surround && o.rear && !o.centre;
    crate::engine::Placement { rear, centre, bass_hz: o.bass_as(rear, centre).hz() }
}

/// What the outputs that play do together (see `engine::Places`): those
/// switched on and connected; one that is muted or at volume zero cannot be
/// heard.
pub fn places(cfg: &Config, devs: &DeviceList) -> crate::engine::Places {
    let playing = cfg.outputs.iter().filter(|o| o.enabled && devs.output(&o.id).is_some());
    crate::engine::Places::of(playing.map(|o| (placement(cfg, o), !o.muted && o.volume > 0.0))).with_stereo(cfg)
}

/// How the outputs that play compare in loudness, as the calibration
/// microphone heard them, and what is done about it with surround on (see
/// `engine::level_trims`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Levels {
    /// How much each output is turned down, dB (id, 0 or less).
    pub trims: Vec<(String, f32)>,
    /// A rear or centre output plays as loud as it happens to be: it or
    /// the front was never measured, and the user set nothing by ear.
    pub unmeasured: bool,
    /// The output turned down the most, when by 10 dB or more: the front
    /// is much weaker than it (its name, the loudest front output's, dB).
    pub weak_front: Option<(String, String, f32)>,
}

/// See `Levels`; of the outputs switched on and connected.
pub fn levels(cfg: &Config, devs: &DeviceList) -> Levels {
    let playing: Vec<&crate::config::OutputConfig> = cfg.outputs.iter().filter(|o| o.enabled && devs.output(&o.id).is_some()).collect();
    let heard: Vec<crate::engine::Heard> =
        playing.iter().map(|o| crate::engine::Heard { place: placement(cfg, o), level_db: o.level_db, by_ear_db: o.surround_db }).collect();
    let front = |k: usize| !heard[k].place.rear && !heard[k].place.centre;
    if !(0..heard.len()).any(front) || (0..heard.len()).all(front) {
        return Levels::default();
    }
    let db = crate::engine::level_trims(&heard, cfg.match_levels);
    let known = |k: usize| heard[k].level_db.filter(|_| cfg.match_levels);
    let loudest_front = (0..heard.len()).filter(|&k| front(k) && known(k).is_some()).max_by(|&a, &b| known(a).unwrap_or(0.0).total_cmp(&known(b).unwrap_or(0.0)));
    let unmeasured = (0..heard.len()).any(|k| !front(k) && heard[k].by_ear_db == 0.0 && (known(k).is_none() || loudest_front.is_none()));
    // (only what the measure did: what the user chose by ear needs no warning)
    let by_measure = crate::engine::level_trims(&heard.iter().map(|h| crate::engine::Heard { by_ear_db: 0.0, ..*h }).collect::<Vec<_>>(), cfg.match_levels);
    let most = (0..heard.len()).min_by(|&a, &b| by_measure[a].total_cmp(&by_measure[b])).filter(|&k| by_measure[k] <= -10.0);
    Levels {
        trims: playing.iter().zip(&db).map(|(o, d)| (o.id.clone(), *d)).collect(),
        unmeasured,
        weak_front: most.zip(loudest_front).map(|(k, f)| (playing[k].name.clone(), playing[f].name.clone(), -by_measure[k])),
    }
}

/// The levels of the outputs after a calibration that measured some of
/// them (`new`: index, dB). A level only means something against the
/// others of the same measurement (the microphone's own gain and place are
/// in it): the new ones are shifted so that the outputs measured both
/// times keep, on average, the level they had, and the ones not measured
/// now keep theirs. With none in common the old ones say nothing about
/// the new: they are forgotten.
pub fn merge_levels(old: &[Option<f32>], new: &[(usize, f32)]) -> Vec<Option<f32>> {
    let new: Vec<(usize, f32)> = new.iter().copied().filter(|(i, l)| *i < old.len() && l.is_finite()).collect();
    if new.is_empty() {
        return old.to_vec();
    }
    let common: Vec<f32> = new.iter().filter_map(|(i, l)| old[*i].map(|o| o - l)).collect();
    let shift = if common.is_empty() { 0.0 } else { common.iter().sum::<f32>() / common.len() as f32 };
    let mut out: Vec<Option<f32>> = if common.is_empty() { vec![None; old.len()] } else { old.to_vec() };
    for (i, l) in new {
        out[i] = Some(l + shift);
    }
    out
}

/// Latency an output declares: the device's when connected, else what it
/// declared the last time it was.
fn latency_of(cfg: &Config, devs: &DeviceList, id: &str) -> Option<f32> {
    devs.output(id).and_then(|d| d.latency_ms).or_else(|| cfg.outputs.iter().find(|o| o.id == id).and_then(|o| o.latency_ms))
}

/// The delays are set against the slowest of the outputs in the list.
/// While that one does not play (switched off, or not connected) the others
/// have nothing to wait for: this is the part of their delays that is only
/// waiting (0 when they all play, or when the ones that do not are not the
/// ones waited for).
pub fn idle_wait_ms(cfg: &Config, devs: &DeviceList) -> f32 {
    let least = |playing_only: bool| {
        cfg.outputs
            .iter()
            .filter(|o| !playing_only || (o.enabled && devs.output(&o.id).is_some()))
            .map(|o| o.delay_ms)
            .fold(f32::INFINITY, f32::min)
    };
    let (all, here) = (least(false), least(true));
    if all.is_finite() && here.is_finite() {
        (here - all).max(0.0)
    } else {
        0.0
    }
}

pub fn main_output(cfg: &Config, devs: &DeviceList) -> Option<String> {
    let enabled: Vec<&str> = cfg.outputs.iter().filter(|o| o.enabled).map(|o| o.id.as_str()).collect();
    if enabled.is_empty() {
        return None;
    }
    // the one chosen by the user, if it is there
    if !cfg.video_output.is_empty() && enabled.contains(&cfg.video_output.as_str()) && devs.output(&cfg.video_output).is_some() {
        return Some(cfg.video_output.clone());
    }
    // Slowest connected output, from the latency it declares.
    let slowest = enabled
        .iter()
        .filter_map(|id| devs.output(id).and_then(|d| d.latency_ms).map(|l| (*id, l)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|x| x.0.to_string());
    if slowest.is_some() {
        return slowest;
    }
    // Unknown latencies: prefer what the system plays now, else a Bluetooth one.
    if let Some(def) = backend::default_output_id() {
        if enabled.contains(&def.as_str()) {
            return Some(def);
        }
    }
    enabled
        .iter()
        .find(|id| devs.output(id).map(|d| d.kind == backend::DevKind::Bluetooth).unwrap_or(false))
        .or(enabled.first())
        .map(|s| s.to_string())
}

/// `vout`: MultiAudio's own output exists (macOS: "MultiAudio", the system
/// output; Linux: "MultiAudio 5.1", for players that send real 5.1).
pub fn make_plan(cfg: &Config, devs: &DeviceList, vout: bool) -> Plan {
    let mut engine_cfg = cfg.clone();
    engine_cfg.routes.clear();
    engine_cfg.reference.clear();
    let mut route_output = Vec::new();
    let main = main_output(cfg, devs);
    let own_output = vout && backend::VOUT_IS_SYSTEM && devs.input(backend::VOUT_INPUT).is_some();
    let all_via_app = devs.input("tap:system").is_some();
    let mut problem = None;
    let mut main_by_system = false;
    // MultiAudio plays every output: none waits for one that is not there
    let idle_wait_ms = if own_output || all_via_app { idle_wait_ms(cfg, devs) } else { 0.0 };

    let route = |o: &crate::config::OutputConfig, input: &str| {
        let place = placement(cfg, o);
        RouteConfig {
            name: o.name.clone(),
            enabled: true,
            input: input.to_string(),
            input_ch: 0,
            output: o.id.clone(),
            output_ch: 0,
            delay_ms: (o.delay_ms - idle_wait_ms).max(0.0),
            gain_db: 0.0,
            volume: o.volume,
            muted: o.muted,
            rear: place.rear,
            bass_hz: place.bass_hz,
            centre: place.centre,
            level_db: o.level_db,
            surround_db: o.surround_db,
        }
    };

    if own_output {
        for (i, o) in cfg.outputs.iter().enumerate() {
            if o.enabled {
                engine_cfg.routes.push(route(o, backend::VOUT_INPUT));
                route_output.push(i);
            }
        }
    } else if all_via_app {
        for (i, o) in cfg.outputs.iter().enumerate() {
            if o.enabled {
                engine_cfg.routes.push(route(o, "tap:system"));
                route_output.push(i);
            }
        }
    } else if let Some(m) = &main {
        match backend::mirror_input_for(m) {
            Some(input) => {
                main_by_system = true;
                engine_cfg.reference = m.clone();
                for (i, o) in cfg.outputs.iter().enumerate() {
                    if o.enabled && o.id != *m {
                        engine_cfg.routes.push(route(o, &input));
                        route_output.push(i);
                    }
                }
            }
            None => problem = Some(format!("non riesco a catturare l'audio inviato a «{}»", backend::display_name(m))),
        }
    }
    // Linux: real 5.1 from players using the "MultiAudio 5.1" output: every
    // enabled output also gets its share of those six channels.
    if !backend::VOUT_IS_SYSTEM && cfg.surround && vout && problem.is_none() {
        for (i, o) in cfg.outputs.iter().enumerate() {
            if o.enabled {
                let mut r = route(o, backend::VOUT_INPUT);
                r.name = format!("{} · 5.1", o.name);
                engine_cfg.routes.push(r);
                route_output.push(i);
            }
        }
    }
    let system_output = if own_output && main.is_some() { Some(backend::vout_output_id()) } else { main.clone() };
    Plan { engine_cfg, route_output, main, system_output, main_by_system, idle_wait_ms, problem }
}


/// The video output's own latency plus its delay: what the sound takes
/// after MultiAudio's buffer.
fn after_buffer_ms(cfg: &Config, devs: &DeviceList, main: Option<&str>) -> f32 {
    let Some(main) = main else { return OWN_PATH_MS };
    let lat = devs.output(main).and_then(|d| d.latency_ms).unwrap_or(0.0);
    let delay = cfg.outputs.iter().find(|o| o.id == main).map(|o| o.delay_ms).unwrap_or(0.0);
    lat + (delay - idle_wait_ms(cfg, devs)).max(0.0) + OWN_PATH_MS
}

/// Latency MultiAudio's own output should declare (players delay the video
/// by it): when the sound comes out of the outputs, all aligned on the
/// video output. (Players that only read the device latency see that of
/// the video output itself, as if it were the system output.)
pub fn vout_latency_ms(cfg: &Config, devs: &DeviceList, main: Option<&str>, safety_ms: f32) -> f32 {
    after_buffer_ms(cfg, devs, main) + safety_ms
}

/// Common buffer that makes the sound come out exactly when players show
/// the picture, given the latency MultiAudio's output declares.
pub fn anchor_ms(cfg: &Config, devs: &DeviceList, main: Option<&str>, declared_ms: f32) -> f32 {
    declared_ms - after_buffer_ms(cfg, devs, main)
}

/// The declared latency no longer fits and MultiAudio's output should be
/// made again (it cannot change while it exists): the sound would come out
/// clearly after the picture, or everything is clearly late for nothing.
pub fn vout_needs_rebuild(cfg: &Config, devs: &DeviceList, main: Option<&str>, safety_ms: f32, declared_ms: f32, floor_ms: f32) -> bool {
    let anchor = anchor_ms(cfg, devs, main, declared_ms);
    let wanted = vout_latency_ms(cfg, devs, main, safety_ms).max(floor_ms);
    anchor < safety_ms - 25.0 || declared_ms - wanted > 60.0
}

/// Delay each output needs, estimated from declared latencies (None when a
/// latency is unknown).
pub fn estimated_delay(cfg: &Config, devs: &DeviceList, plan: &Plan, output_id: &str) -> Option<f32> {
    // (of an output that is not connected: the latency it declared last)
    let lat = |id: &str| latency_of(cfg, devs, id);
    let mine = lat(output_id)?;
    if plan.main_by_system {
        let main = plan.main.as_deref()?;
        if main == output_id {
            return Some(0.0);
        }
        let safety = cfg.safety_ms.max(25.0);
        Some((lat(main)? - mine - safety - OWN_PATH_MS).max(0.0).round())
    } else {
        // Against the slowest output in the list, playing or not (the ones
        // that do not play are not waited for: see `idle_wait_ms`).
        let known = cfg.outputs.iter().filter_map(|o| lat(&o.id)).fold(0.0f32, f32::max);
        // An output that is switched on but away and never told its latency
        // (settings saved by an older version): the automatic delays that
        // are there still tell it, delay + latency being the same for all.
        let unknown = cfg.outputs.iter().any(|o| o.enabled && devs.output(&o.id).is_none() && o.latency_ms.is_none());
        let implied = if unknown {
            cfg.outputs.iter().filter(|o| o.auto_delay).filter_map(|o| lat(&o.id).map(|l| l + o.delay_ms)).fold(0.0f32, f32::max)
        } else {
            0.0
        };
        Some((known.max(implied) - mine).max(0.0).round())
    }
}

/// Sets the delay of every output still in "auto" from the declared
/// latencies, switched on or not (and remembers the latency of the outputs
/// that are connected).
pub fn fill_auto_delays(cfg: &mut Config, devs: &DeviceList) {
    for o in cfg.outputs.iter_mut() {
        if let Some(l) = devs.output(&o.id).and_then(|d| d.latency_ms) {
            if o.latency_ms.map(|was| (was - l).abs() > 0.5).unwrap_or(true) {
                o.latency_ms = Some(l);
            }
        }
    }
    let plan = make_plan(cfg, devs, false);
    let updates: Vec<(usize, f32)> = cfg
        .outputs
        .iter()
        .enumerate()
        .filter(|(_, o)| o.auto_delay)
        .filter_map(|(i, o)| estimated_delay(cfg, devs, &plan, &o.id).map(|d| (i, d)))
        .collect();
    for (i, d) in updates {
        if (cfg.outputs[i].delay_ms - d).abs() > 0.05 {
            cfg.outputs[i].delay_ms = d;
        }
    }
}

#[cfg(all(test, local_tests))]
#[path = "local_tests/plan.rs"]
mod tests;
