//! MultiAudio's own volume.
//!
//! On macOS, while MultiAudio is on, the volume keys and the app's slider
//! set this volume, and the engine applies it to every output times the
//! output's own volume (moving the device's volume when it has one, in
//! software otherwise).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};

static LEVEL: AtomicU32 = AtomicU32::new(0x3F00_0000); // 0.5
static MUTED: AtomicBool = AtomicBool::new(false);
static GEN: AtomicU64 = AtomicU64::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// (slider 0..1, muted, generation: changes at every set)
pub fn get() -> (f32, bool, u64) {
    (f32::from_bits(LEVEL.load(Relaxed)), MUTED.load(Relaxed), GEN.load(Relaxed))
}

pub fn set(level: f32, muted: bool) {
    let level = if level.is_finite() { level.clamp(0.0, 1.0) } else { 0.5 };
    LEVEL.store(level.to_bits(), Relaxed);
    MUTED.store(muted, Relaxed);
    GEN.fetch_add(1, Relaxed);
}

pub fn set_level(level: f32) {
    set(level, MUTED.load(Relaxed));
}

/// MultiAudio is on (macOS): the volume keys are ours.
pub fn active() -> bool {
    ACTIVE.load(Relaxed)
}

pub fn set_active(on: bool) {
    ACTIVE.store(on, Relaxed);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // keys are read on macOS only
pub enum Key {
    Up,
    Down,
    Mute,
}

/// What a volume key does, like macOS: 16 steps over the whole range (64
/// with ⌥⇧), raising or lowering also unmutes.
pub fn step(level: f32, muted: bool, key: Key, fine: bool) -> (f32, bool) {
    let n = if fine { 64.0 } else { 16.0 };
    let at = (level * n).round();
    match key {
        Key::Up => (((at + 1.0) / n).min(1.0), false),
        Key::Down => (((at - 1.0) / n).max(0.0), false),
        Key::Mute => (level, !muted),
    }
}

/// A volume key was pressed while MultiAudio is on.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn on_key(key: Key, fine: bool) {
    let (l, m, _) = get();
    let (l, m) = step(l, m, key, fine);
    set(l, m);
    #[cfg(target_os = "macos")]
    crate::machud::show(l, m);
}

/// Linear gain for outputs without a volume control of their own (the
/// usual cubic loudness curve).
pub fn curve(level: f32) -> f32 {
    let l = level.clamp(0.0, 1.0);
    l * l * l
}

#[cfg(all(test, local_tests))]
#[path = "local_tests/volume.rs"]
mod tests;
