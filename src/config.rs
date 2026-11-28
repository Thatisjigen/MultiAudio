use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct RouteConfig {
    pub name: String,
    pub enabled: bool,
    /// Device id (see backend::DeviceInfo::id)
    pub input: String,
    /// First channel of the input pair (0-based)
    pub input_ch: u16,
    pub output: String,
    pub output_ch: u16,
    pub delay_ms: f32,
    pub gain_db: f32,
    /// Volume of this output (0..1), see OutputConfig::volume.
    pub volume: f32,
    pub muted: bool,
    /// Plays the surround ambience instead of plain stereo.
    pub rear: bool,
    /// Below this frequency (Hz) its speakers play nothing: that sound goes
    /// to the outputs that play all the bass. 0: it plays everything.
    pub bass_hz: f32,
    /// The centre speaker.
    pub centre: bool,
    /// How loud its speakers sounded to the calibration microphone, and the
    /// user's own correction: see OutputConfig::level_db, surround_db.
    pub level_db: Option<f32>,
    pub surround_db: f32,
}

impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            name: "Uscita".into(),
            enabled: true,
            input: String::new(),
            input_ch: 0,
            output: String::new(),
            output_ch: 0,
            delay_ms: 0.0,
            gain_db: 0.0,
            volume: 1.0,
            muted: false,
            rear: false,
            bass_hz: 0.0,
            centre: false,
            level_db: None,
            surround_db: 0.0,
        }
    }
}

/// How much bass an output's speakers play. What they do not play goes to
/// the outputs that play all of it (see `engine::Places`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Bass {
    /// All of it: large speakers, or a subwoofer behind them.
    Full,
    /// From about 100 Hz up: speakers of medium size.
    Less,
    /// From about 200 Hz up: small ones (a laptop, a smart speaker).
    Off,
}

impl Bass {
    /// Below this the speakers play nothing (Hz; 0: they play everything).
    pub fn hz(self) -> f32 {
        match self {
            Bass::Full => 0.0,
            Bass::Less => 100.0,
            Bass::Off => 200.0,
        }
    }
}

/// With surround on and a stereo source (most of what plays: YouTube,
/// films in a browser), what the rear outputs play. Surround is for
/// sources that have surround channels; of a stereo one there is nothing
/// that belongs behind, only what can be made out of left and right.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum StereoRear {
    /// The stereo as it is, together with the front outputs: what they
    /// play with surround off. Whatever the source and however loud these
    /// speakers are against the front ones, it sounds as it does without
    /// surround.
    #[default]
    All,
    /// The surround matrix (`dsp::upmix`), `engine::REAR_DELAY_MS` after the
    /// front: only what differs between left and right, so nothing of the
    /// voices, which are at the centre. An effect, at a price: what
    /// differs in streamed sound is its ambience and what the codec left
    /// there, and from rear speakers louder than the front ones that is
    /// most of what is heard. (On 2026-11-27 the user's "fruscio" and
    /// "bzzt" with any browser video and surround on were laid at its
    /// door, and `All` became the default. They were the capture, which
    /// lost part of every block the browser played: see `mactap::Tuning`.
    /// With the capture whole he chose this, and with the voices still
    /// 11 dB down in it: "si sente bene ma dietro si sente ancora la
    /// voce". They are out of it since.)
    Ambience,
}

/// One output device the user wants to hear (what the app shows).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct OutputConfig {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub delay_ms: f32,
    /// Delay estimated from the declared latencies until the user (or the
    /// calibration) sets it.
    pub auto_delay: bool,
    /// Its own volume (0..1, slider position): set on the device itself
    /// when it has a volume control, in software otherwise. The volume keys
    /// raise and lower all outputs together, each keeping its own.
    pub volume: f32,
    pub muted: bool,
    /// With surround on: this output is behind the listener.
    pub rear: bool,
    /// Behind the listener and its speakers can play bass (older settings:
    /// `bass` says it now).
    pub rear_bass: bool,
    /// How much bass its speakers play; not set: see `bass_as`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bass: Option<Bass>,
    /// With surround on: this output is the centre speaker (voices). It
    /// gets the centre channel of a 5.1 source; of a stereo source the
    /// voices, unless `Config::stereo_centre` is off.
    pub centre: bool,
    /// Latency the device declared when it was last connected, ms: the
    /// automatic delays of the others stay right while it is disconnected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f32>,
    /// How loud its speakers sounded where the calibration microphone was,
    /// dB (the same test signal on every output, so only the differences
    /// between outputs mean something; with the volumes as they were then).
    /// Not set: never measured. See `Config::match_levels`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level_db: Option<f32>,
    /// Behind or at the centre, with surround on: how much louder (+) or
    /// quieter (−) than that the user wants it, dB, set by ear on its card.
    /// It is never louder than its own volume gives (the sum with what the
    /// calibration found stays at 0 dB or under).
    pub surround_db: f32,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: false,
            delay_ms: 0.0,
            auto_delay: true,
            volume: 1.0,
            muted: false,
            rear: false,
            rear_bass: false,
            bass: None,
            centre: false,
            latency_ms: None,
            level_db: None,
            surround_db: 0.0,
        }
    }
}

impl OutputConfig {
    /// How much bass it plays where it is now (`rear`, `centre`: its place
    /// as it counts, i.e. with surround on). Until the user chooses: all of
    /// it in front, a little behind, none as the centre speaker (it is
    /// there for the voices, and is often a small one: one that got the
    /// bass of the music sounded boomy and distorted).
    pub fn bass_as(&self, rear: bool, centre: bool) -> Bass {
        self.bass.unwrap_or(if centre {
            Bass::Off
        } else if rear && !self.rear_bass {
            Bass::Less
        } else {
            Bass::Full
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Outputs the user switched on (and their settings).
    pub outputs: Vec<OutputConfig>,
    /// Output the video is synchronised to; empty = automatic (the slowest).
    pub video_output: String,
    /// Surround effect: outputs marked "rear" play the ambience of the stereo.
    pub surround: bool,
    /// macOS, surround on and a Bluetooth video output: MultiAudio's own
    /// 6-channel output is made all the same, with the Bluetooth output as
    /// its clock (players read the clock's latency, so they delay the
    /// picture as they do for the Bluetooth output itself): 5.1 sources
    /// keep their channels. Off: the Bluetooth output stays the system
    /// output and the surround is made from the stereo.
    pub bt_surround: bool,
    /// With a stereo source (no centre channel) the centre speaker plays
    /// what left and right have in common, the voices; the front outputs
    /// play them either way (`dsp::upmix`). Off: it plays only the centre
    /// channel of 5.1 sources. (Until 2026-11-27 this was `stereo_voices`,
    /// off unless asked for: the user, "centro non suona quasi mai".)
    pub stereo_centre: bool,
    /// What the rear outputs play of a stereo source.
    pub stereo_rear: StereoRear,
    /// With surround on, a rear or centre output that sounded louder than
    /// the front ones to the calibration microphone is turned down to
    /// their level (`OutputConfig::level_db`, `engine::level_trims`).
    /// Off: only what the user set by ear (`OutputConfig::surround_db`)
    /// applies.
    pub match_levels: bool,
    /// Routes for the engine: derived from `outputs` by `plan`, not saved.
    #[serde(skip_serializing)]
    pub routes: Vec<RouteConfig>,
    /// Start routing as soon as the app opens.
    pub autostart: bool,
    /// Extra buffer common to every route, absorbs callback jitter.
    pub safety_ms: f32,
    /// Microphone used by auto-calibration.
    pub calib_mic: String,
    /// MultiAudio's own volume (macOS, while its output is the system
    /// output), 0..1. Used when the previous output has no volume control.
    pub volume: Option<f32>,
    /// Output played directly by the OS (derived by `plan`, not saved).
    /// Calibration aligns the routes to it and the test click plays on it.
    #[serde(skip_serializing)]
    pub reference: String,
    /// Common buffer that matches the latency MultiAudio's own output
    /// declares (derived, for the engine; see `Safety::anchor_ms`).
    #[serde(skip)]
    pub anchor_ms: Option<f32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            outputs: Vec::new(),
            video_output: String::new(),
            surround: false,
            bt_surround: true,
            stereo_centre: true,
            stereo_rear: StereoRear::All,
            match_levels: true,
            routes: Vec::new(),
            autostart: false,
            safety_ms: 30.0,
            calib_mic: String::new(),
            volume: None,
            reference: String::new(),
            anchor_ms: None,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("MULTIAUDIO_CONFIG") {
            return PathBuf::from(p);
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("MultiAudio")
            .join("config.json")
    }

    pub fn load() -> Self {
        let mut cfg: Config = std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        // Older versions saved routes: keep their outputs.
        if cfg.outputs.is_empty() {
            for r in &cfg.routes {
                if !r.output.is_empty() && !cfg.outputs.iter().any(|o| o.id == r.output) {
                    cfg.outputs.push(OutputConfig {
                        id: r.output.clone(),
                        name: r.name.clone(),
                        enabled: r.enabled,
                        delay_ms: r.delay_ms,
                        auto_delay: false,
                        volume: 10f32.powf(r.gain_db / 20.0).cbrt().clamp(0.0, 1.0),
                        muted: r.muted,
                        rear: false,
                        rear_bass: false,
                        bass: None,
                        centre: false,
                        latency_ms: None,
                        level_db: None,
                        surround_db: 0.0,
                    });
                }
            }
        }
        cfg.routes.clear();
        cfg.reference.clear();
        cfg
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let p = Self::path();
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }
}
