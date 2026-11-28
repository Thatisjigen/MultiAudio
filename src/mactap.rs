//! macOS: capture system audio with Core Audio process taps (macOS 14.2+).
//! No virtual driver (BlackHole & co.) is needed: the tap and the private
//! aggregate device that exposes it are created by the app and disappear with
//! it. Requires the "System Audio Recording" permission
//! (NSAudioCaptureUsageDescription in Info.plist).

use anyhow::{anyhow, bail, Result};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AllocAnyThread, Message};
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyTranslatePIDToProcessObject,
    kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    kAudioObjectSystemObject, kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice, AudioHardwareDestroyProcessTap,
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress, AudioObjectSetPropertyData, CATapDescription,
    CATapMuteBehavior,
};
use objc2_core_foundation::{CFDictionary, CFString};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSProcessInfo, NSString};
use std::ffi::{c_void, CStr};
use std::ptr::NonNull;

/// Prefix of the private aggregate devices we create (hidden from lists).
pub const AGG_PREFIX: &str = "app.multiaudio.tap.";

pub enum TapKind {
    /// Every app except MultiAudio, muted on its normal output: MultiAudio
    /// becomes the only way the sound reaches the speakers.
    System,
    /// Whatever is being played on this output device (by any app,
    /// MultiAudio included), left playing there.
    Device(String),
    /// Every output stream of this device, in order, muted there: the
    /// multichannel capture of the "MultiAudio" output. MultiAudio's own
    /// sound is excluded (it plays to the real devices, never to this one).
    Streams(String),
}

pub struct SystemTap {
    taps: Vec<AudioObjectID>,
    agg: AudioObjectID,
    pub agg_uid: String,
    /// The first channels of the capture come before the others: (how
    /// many channels, by how many frames). See `Taps::Members`.
    pub lead: Option<(usize, usize)>,
    /// The taps are on the real devices of our output (`Taps::Members`).
    pub members: bool,
}

impl SystemTap {
    /// Channels of the taps, together.
    fn channels(&self) -> usize {
        self.formats().iter().map(|f| f.0).sum()
    }

    /// (channels, rate) of each tap; (0, 0.0) for one that does not say.
    fn formats(&self) -> Vec<(usize, f64)> {
        use objc2_core_audio::kAudioTapPropertyFormat;
        use objc2_core_audio_types::AudioStreamBasicDescription;
        self.taps
            .iter()
            .map(|t| unsafe {
                let mut f: AudioStreamBasicDescription = std::mem::zeroed();
                let mut addr = address(kAudioTapPropertyFormat);
                let mut size = std::mem::size_of_val(&f) as u32;
                let st = AudioObjectGetPropertyData(*t, NonNull::from(&mut addr), 0, std::ptr::null(), NonNull::from(&mut size), NonNull::from(&mut f).cast());
                if st == 0 {
                    (f.mChannelsPerFrame as usize, f.mSampleRate)
                } else {
                    (0, 0.0)
                }
            })
            .collect()
    }

    /// What the system says of the capture device and of the taps, for the
    /// log: frames in a block (and the range allowed), latency and safety
    /// offset of the input, channels and rate of each tap.
    pub fn facts(&self) -> String {
        use objc2_core_audio::{kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyLatency, kAudioDevicePropertySafetyOffset, kAudioObjectPropertyScopeInput};
        let input = kAudioObjectPropertyScopeInput;
        let (block, latency, safety): (u32, u32, u32) = unsafe {
            (
                get_prop(self.agg, kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal).unwrap_or(0),
                get_prop(self.agg, kAudioDevicePropertyLatency, input).unwrap_or(0),
                get_prop(self.agg, kAudioDevicePropertySafetyOffset, input).unwrap_or(0),
            )
        };
        let (lo, hi) = block_range(self.agg).unwrap_or((0, 0));
        let taps: Vec<String> = self.formats().iter().map(|(c, r)| format!("{c} canali a {r:.0} Hz")).collect();
        format!("dispositivo di cattura: blocchi da {block} ({lo}–{hi}), latenza {latency} + sicurezza {safety}; tap: {}", taps.join(", "))
    }
}

impl Drop for SystemTap {
    fn drop(&mut self) {
        unsafe {
            if self.agg != 0 {
                AudioHardwareDestroyAggregateDevice(self.agg);
            }
            for t in &self.taps {
                AudioHardwareDestroyProcessTap(*t);
            }
        }
    }
}

/// The smallest and the largest block (frames) a device can be read or
/// written in.
fn block_range(dev: AudioObjectID) -> Option<(u32, u32)> {
    use objc2_core_audio::kAudioDevicePropertyBufferFrameSizeRange;
    use objc2_core_audio_types::AudioValueRange;
    let mut range = AudioValueRange { mMinimum: 0.0, mMaximum: 0.0 };
    let mut addr = address(kAudioDevicePropertyBufferFrameSizeRange);
    let mut size = std::mem::size_of::<AudioValueRange>() as u32;
    let st = unsafe { AudioObjectGetPropertyData(dev, NonNull::from(&mut addr), 0, std::ptr::null(), NonNull::from(&mut size), NonNull::from(&mut range).cast()) };
    (st == 0 && range.mMaximum >= 1.0).then_some((range.mMinimum as u32, range.mMaximum as u32))
}

fn output_stream_ids(dev: AudioObjectID) -> Vec<AudioObjectID> {
    use objc2_core_audio::{kAudioDevicePropertyStreams, kAudioObjectPropertyScopeOutput, AudioObjectGetPropertyDataSize};
    let mut addr = AudioObjectPropertyAddress { mSelector: kAudioDevicePropertyStreams, mScope: kAudioObjectPropertyScopeOutput, mElement: kAudioObjectPropertyElementMain };
    let mut size = 0u32;
    unsafe {
        if AudioObjectGetPropertyDataSize(dev, NonNull::from(&mut addr), 0, std::ptr::null(), NonNull::from(&mut size)) != 0 || size < 4 {
            return Vec::new();
        }
        let mut ids = vec![0 as AudioObjectID; size as usize / 4];
        let Some(ptr) = NonNull::new(ids.as_mut_ptr()) else { return Vec::new() };
        if AudioObjectGetPropertyData(dev, NonNull::from(&mut addr), 0, std::ptr::null(), NonNull::from(&mut size), ptr.cast()) != 0 {
            return Vec::new();
        }
        ids
    }
}

pub fn supported() -> bool {
    let v = NSProcessInfo::processInfo().operatingSystemVersion();
    (v.majorVersion, v.minorVersion) >= (14, 2)
}

fn any<T: Message>(x: &T) -> &AnyObject {
    // SAFETY: every Objective-C object can be viewed as AnyObject.
    unsafe { &*(x as *const T as *const AnyObject) }
}

fn to_any<T: Message>(r: Retained<T>) -> Retained<AnyObject> {
    // SAFETY: every Objective-C object is an AnyObject.
    unsafe { Retained::cast_unchecked(r) }
}

fn key(k: &CStr) -> Retained<NSString> {
    NSString::from_str(k.to_str().unwrap_or_default())
}

fn address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// The HAL process object of this app (to exclude ourselves from the tap).
fn own_process_object() -> Option<AudioObjectID> {
    let pid = std::process::id() as i32;
    let mut addr = address(kAudioHardwarePropertyTranslatePIDToProcessObject);
    let mut out: AudioObjectID = 0;
    let mut size = std::mem::size_of::<AudioObjectID>() as u32;
    let st = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&mut addr),
            std::mem::size_of::<i32>() as u32,
            &pid as *const i32 as *const c_void,
            NonNull::from(&mut size),
            NonNull::from(&mut out).cast(),
        )
    };
    (st == 0 && out != 0).then_some(out)
}

/// Where the taps that capture "MultiAudio" (our output) are put.
///
/// A program that plays to an output writes its sound a little ahead of
/// time, and a tap hands over only what the system still holds of it: the
/// last frames written, as many as the clock device of the output keeps
/// between two of its time references (`kAudioDevicePropertyZeroTimeStampPeriod`:
/// 2732 for Bluetooth speakers, 49152 for the wired outputs of the Mac this
/// was measured on). Measured on 2026-11-27 with a player that marks its
/// blocks, "MultiAudio" having Bluetooth speakers as its clock and 40 ms of
/// extra latency: a block of N frames ends 2N + 932 frames ahead of the end
/// of the block we read (B frames), so a tap on a stream of the output
/// hands it over whole only if 2N + B + 932 ≤ 2732. Blocks of 512 and 640
/// came whole. Of blocks of 1024, which browsers use for video, a third to
/// three quarters of each was missing, exact zeros in its place, 43 times a
/// second; and one such program playing was enough to tear the sound of all
/// the others. The user heard it for a day as "bzzt", "voci camuffate",
/// "fruscio", "distorto", on every output and whatever the surround did.
/// The size of our blocks, a clock of our own for the capture device,
/// reading each stream through a device of its own: none of it moves the
/// limit.
///
/// Taken from the streams of the real devices the output is made of, the
/// same sound comes whole: every block from 256 to 1024 frames, ours from
/// 256 to 1024, two channels and six. What goes to the devices that are not
/// the clock has been through the output's drift compensation and comes
/// `DRIFT_LAG` frames after the clock's (`SystemTap::lead`). Such taps also
/// take what a program plays straight to one of those devices.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Taps {
    /// On the streams of the output itself: all its channels as programs
    /// write them.
    Output,
    /// On the streams of the real devices it is made of.
    Members,
}

/// Frames the drift compensation of an aggregate device takes (measured at
/// 44.1 kHz, the same on every start: 96, with a fraction that wanders).
const DRIFT_LAG: usize = 96;

/// What a file `cattura` next to the diagnostic recordings (`record::dir`)
/// says of the capture of "MultiAudio", in words: `tap=output` or
/// `tap=members`, `buffer=<frames>`. To try the other way on a machine
/// where the capture misbehaves without a new build: the window starts the
/// engine again when the file changes.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Tuning {
    /// Where the taps go. None: where the clock of the output allows
    /// (`output_taps_fit`).
    pub taps: Option<Taps>,
    /// Frames in a block of the capture. None: what the system gives.
    pub buffer: Option<u32>,
}

impl Tuning {
    /// What the file says, on top of what comes with the app.
    pub fn parse(text: &str) -> Tuning {
        let mut t = Tuning::default();
        for word in text.split_whitespace() {
            match word.split_once('=') {
                Some(("buffer", v)) => t.buffer = v.parse().ok().filter(|n| *n > 0),
                Some(("tap", "output")) => t.taps = Some(Taps::Output),
                Some(("tap", "members")) => t.taps = Some(Taps::Members),
                _ => {}
            }
        }
        t
    }

    pub fn current() -> Tuning {
        match std::fs::read_to_string(crate::record::dir().join("cattura")) {
            Ok(text) => Tuning::parse(&text),
            Err(_) => Tuning::default(),
        }
    }
}

impl std::fmt::Display for Tuning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let taps = match self.taps {
            Some(Taps::Output) => "dai flussi dell'uscita MultiAudio",
            Some(Taps::Members) => "dai flussi dei dispositivi reali",
            None => "da dove l'orologio dell'uscita lo consente",
        };
        let buffer = self.buffer.map(|b| format!("blocchi da {b}")).unwrap_or_else(|| "blocchi del sistema".into());
        write!(f, "cattura: {taps}, {buffer}")
    }
}

/// Do taps on the streams of `out` hand everything over? What programs
/// write ahead must fit in what its clock device keeps (see `Taps`):
/// (yes or no, frames kept, frames needed). Reckoned with the largest
/// blocks a program may play in and we may read in, and with the whole
/// safety offset of the output (832 frames more than was measured to be
/// needed). A clock that does not say how much it keeps: yes, as it always
/// was.
fn output_taps_fit(out: AudioObjectID, clock_uid: &str) -> (bool, u32, u32) {
    use objc2_core_audio::{kAudioDevicePropertySafetyOffset, kAudioDevicePropertyZeroTimeStampPeriod, kAudioObjectPropertyScopeOutput};
    let kept: u32 = device_id_from_uid(clock_uid)
        .and_then(|d| unsafe { get_prop(d, kAudioDevicePropertyZeroTimeStampPeriod, kAudioObjectPropertyScopeGlobal) })
        .unwrap_or(0);
    let safety: u32 = unsafe { get_prop(out, kAudioDevicePropertySafetyOffset, kAudioObjectPropertyScopeOutput) }.unwrap_or(0);
    let largest = block_range(out).map(|r| r.1).unwrap_or(4096);
    let needed = frames_ahead(largest, largest, safety);
    (kept == 0 || kept >= needed, kept, needed)
}

/// Frames a program's block of `block` frames reaches ahead of where a
/// capture read in blocks of `ours` begins, on an output with this safety
/// offset (see `Taps`; on the safe side by 832 frames).
fn frames_ahead(block: u32, ours: u32, safety: u32) -> u32 {
    2 * block + ours + safety
}

/// The member taps did not work: the output's own, until the output is
/// made again.
static MEMBERS_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The capture from the real devices could not be made or opened: the next
/// one is made on the output's own streams (sound in pieces from programs
/// that play in large blocks, but sound).
pub fn members_failed(problem: &str) {
    MEMBERS_OFF.store(true, std::sync::atomic::Ordering::Relaxed);
    crate::diag::log(format!("cattura dai dispositivi reali non riuscita ({problem}): torno ai flussi dell'uscita MultiAudio"));
}

pub fn create(kind: &TapKind) -> Result<SystemTap> {
    if !supported() {
        bail!("la cattura dell'audio di sistema richiede macOS 14.2 o successivo");
    }
    unsafe {
        let exclude_me = || -> Retained<NSArray<NSNumber>> {
            match own_process_object() {
                Some(me) => NSArray::from_retained_slice(&[NSNumber::new_u32(me)]),
                None => NSArray::new(),
            }
        };
        let stream_tap = |exclude: &NSArray<NSNumber>, uid: &str, stream: usize, mute: CATapMuteBehavior| {
            let d = CATapDescription::initExcludingProcesses_andDeviceUID_withStream(CATapDescription::alloc(), exclude, &NSString::from_str(uid), stream as isize);
            d.setMuteBehavior(mute);
            d
        };
        match kind {
            TapKind::System => {
                let me = own_process_object().ok_or_else(|| anyhow!("processo audio di MultiAudio non trovato"))?;
                let exclude = NSArray::from_retained_slice(&[NSNumber::new_u32(me)]);
                // With the "MultiAudio" output present, capture only what
                // goes to the system output: what is sent to MultiAudio is
                // captured there, in 6 channels, and must not come twice.
                let system_uid = device_id_from_uid(VOUT_UID).and(default_output_device().and_then(device_uid));
                let d = match system_uid {
                    Some(uid) => {
                        crate::diag::log(format!(
                            "cattura audio di sistema: solo l'uscita «{}» (c'è l'uscita MultiAudio)",
                            crate::backend::display_name(&format!("cpal:coreaudio:{uid}"))
                        ));
                        stream_tap(&exclude, &uid, 0, CATapMuteBehavior::MutedWhenTapped)
                    }
                    None => {
                        let d = CATapDescription::initStereoGlobalTapButExcludeProcesses(CATapDescription::alloc(), &exclude);
                        d.setMuteBehavior(CATapMuteBehavior::MutedWhenTapped);
                        d
                    }
                };
                capture(&[d], None)
            }
            TapKind::Device(uid) => {
                let none: Retained<NSArray<NSNumber>> = NSArray::new();
                capture(&[stream_tap(&none, uid, 0, CATapMuteBehavior::Unmuted)], None)
            }
            TapKind::Streams(uid) => {
                let dev = device_id_from_uid(uid).ok_or_else(|| anyhow!("dispositivo non trovato"))?;
                let n = output_stream_ids(dev).len();
                if n == 0 {
                    bail!("il dispositivo non ha uscite");
                }
                // Never hear ourselves: these streams belong to devices
                // MultiAudio also plays to directly.
                let exclude = exclude_me();
                let tuning = Tuning::current();
                let members = MEMBERS.lock().map(|m| m.clone()).unwrap_or_default();
                let wanted = match (tuning.taps, members.first()) {
                    (_, None) => Taps::Output,
                    (Some(t), _) => t,
                    (None, Some(clock)) => {
                        let (fit, kept, needed) = output_taps_fit(dev, clock);
                        if !fit {
                            crate::diag::log(format!(
                                "cattura dai dispositivi reali: «{}», l'orologio dell'uscita, tiene {kept} campioni e i programmi ne scrivono fino a {needed} in anticipo",
                                crate::backend::display_name(&format!("cpal:coreaudio:{clock}"))
                            ));
                        }
                        if fit {
                            Taps::Output
                        } else {
                            Taps::Members
                        }
                    }
                };
                if wanted == Taps::Members && !MEMBERS_OFF.load(std::sync::atomic::Ordering::Relaxed) {
                    let streams = |m: &String| device_id_from_uid(m).map(|d| output_stream_ids(d).len()).unwrap_or(0);
                    let descs: Vec<Retained<CATapDescription>> =
                        members.iter().flat_map(|m| (0..streams(m)).map(|s| stream_tap(&exclude, m, s, CATapMuteBehavior::MutedWhenTapped)).collect::<Vec<_>>()).collect();
                    let clock_streams = streams(&members[0]);
                    let made = capture(&descs, None);
                    // (a tap may need a moment before it says its format)
                    for _ in 0..8 {
                        if made.as_ref().map(|t| t.channels() == output_channels(dev)).unwrap_or(true) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    let problem = match made {
                        Ok(mut tap) if tap.channels() == output_channels(dev) => {
                            // (the clock's channels come first, and before the others)
                            let first: usize = tap.formats().iter().take(clock_streams).map(|f| f.0).sum();
                            tap.lead = (members.len() > 1 && first > 0).then_some((first, DRIFT_LAG));
                            tap.members = true;
                            // (measured only with every device at the output's rate)
                            let rates: Vec<f64> = tap.formats().iter().map(|f| f.1).collect();
                            if rates.iter().any(|r| (r - rates[0]).abs() > 0.5) {
                                crate::diag::log(format!("cattura dai dispositivi reali: non vanno tutti alla stessa frequenza ({rates:?} Hz), non provato"));
                            }
                            return Ok(tap);
                        }
                        Ok(tap) => format!("danno {} canali, l'uscita ne ha {}", tap.channels(), output_channels(dev)),
                        Err(e) => e.to_string(),
                    };
                    // (what was made is gone with `tap`)
                    members_failed(&problem);
                }
                let descs: Vec<Retained<CATapDescription>> = (0..n).map(|s| stream_tap(&exclude, uid, s, CATapMuteBehavior::MutedWhenTapped)).collect();
                capture(&descs, None)
            }
        }
    }
}

/// Makes the taps and the private aggregate device they are read through
/// (all their channels, in this order).
unsafe fn capture(descs: &[Retained<CATapDescription>], lead: Option<(usize, usize)>) -> Result<SystemTap> {
    if descs.is_empty() {
        bail!("niente da catturare");
    }
    // (whatever fails from here on, dropping `tap` takes down what was made)
    let mut tap = SystemTap { taps: Vec::new(), agg: 0, agg_uid: String::new(), lead, members: false };
    let mut sub_taps = Vec::new();
    for desc in descs {
        desc.setName(&NSString::from_str("MultiAudio"));
        desc.setPrivate(true);
        let mut id: AudioObjectID = 0;
        let st = AudioHardwareCreateProcessTap(Some(desc), &mut id);
        if st != 0 || id == 0 {
            bail!("impossibile creare il tap audio (errore {st})");
        }
        tap.taps.push(id);
        let tap_uuid = desc.UUID().UUIDString().to_string();
        sub_taps.push(NSDictionary::<NSString, AnyObject>::from_slices(
            &[&*key(kAudioSubTapUIDKey), &*key(kAudioSubTapDriftCompensationKey)],
            &[any(&*NSString::from_str(&tap_uuid)), any(&*NSNumber::new_bool(true))],
        ));
    }
    let sub_refs: Vec<&AnyObject> = sub_taps.iter().map(|d| any(&**d)).collect();
    let tap_list = NSArray::<AnyObject>::from_slice(&sub_refs);
    let agg_uid = format!("{AGG_PREFIX}{}", descs[0].UUID().UUIDString());
    let name = NSString::from_str("MultiAudio (cattura)");
    let uid = NSString::from_str(&agg_uid);
    let yes = NSNumber::new_bool(true);
    let no = NSNumber::new_bool(false);
    let keys = [
        key(kAudioAggregateDeviceNameKey),
        key(kAudioAggregateDeviceUIDKey),
        key(kAudioAggregateDeviceIsPrivateKey),
        key(kAudioAggregateDeviceIsStackedKey),
        key(kAudioAggregateDeviceTapAutoStartKey),
        key(kAudioAggregateDeviceTapListKey),
    ];
    let vals: [Retained<AnyObject>; 6] = [to_any(name), to_any(uid), to_any(yes.clone()), to_any(no), to_any(yes), to_any(tap_list)];
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let val_refs: Vec<&AnyObject> = vals.iter().map(|v| &**v).collect();
    let dict = NSDictionary::<NSString, AnyObject>::from_slices(&key_refs, &val_refs);
    // NSDictionary is toll-free bridged to CFDictionary.
    let cf: &CFDictionary = &*(Retained::as_ptr(&dict) as *const CFDictionary);
    let mut agg: AudioObjectID = 0;
    let st = AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut agg));
    if st != 0 || agg == 0 {
        bail!("impossibile creare il dispositivo di cattura (errore {st})");
    }
    tap.agg = agg;
    tap.agg_uid = agg_uid;
    Ok(tap)
}

/// Pids of the processes that are playing sound right now (ours left
/// out). It is all macOS tells about the sources: who plays, not what (how
/// many channels, which file). None when it does not tell even that.
pub fn playing_pids() -> Option<Vec<i32>> {
    use objc2_core_audio::{kAudioHardwarePropertyProcessObjectList, kAudioProcessPropertyIsRunningOutput, kAudioProcessPropertyPID, AudioObjectGetPropertyDataSize};
    let system = kAudioObjectSystemObject as AudioObjectID;
    let mut addr = address(kAudioHardwarePropertyProcessObjectList);
    let mut size = 0u32;
    unsafe {
        if AudioObjectGetPropertyDataSize(system, NonNull::from(&mut addr), 0, std::ptr::null(), NonNull::from(&mut size)) != 0 {
            return None;
        }
        let mut ids = vec![0 as AudioObjectID; size as usize / 4 + 8];
        size = (ids.len() * 4) as u32;
        if AudioObjectGetPropertyData(system, NonNull::from(&mut addr), 0, std::ptr::null(), NonNull::from(&mut size), NonNull::new(ids.as_mut_ptr()).unwrap().cast()) != 0 {
            return None;
        }
        ids.truncate(size as usize / 4);
        let me = std::process::id() as i32;
        let global = kAudioObjectPropertyScopeGlobal;
        Some(
            ids.iter()
                .filter(|&&p| get_prop::<u32>(p, kAudioProcessPropertyIsRunningOutput, global) == Some(1))
                .filter_map(|&p| get_prop::<i32>(p, kAudioProcessPropertyPID, global))
                .filter(|&pid| pid != me)
                .collect(),
        )
    }
}

fn device_id_from_uid(uid: &str) -> Option<AudioObjectID> {
    let cf = CFString::from_str(uid);
    let cf_ref: *const CFString = &*cf;
    let mut addr = address(kAudioHardwarePropertyTranslateUIDToDevice);
    let mut out: AudioObjectID = 0;
    let mut size = std::mem::size_of::<AudioObjectID>() as u32;
    let st = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&mut addr),
            std::mem::size_of::<*const CFString>() as u32,
            &cf_ref as *const *const CFString as *const c_void,
            NonNull::from(&mut size),
            NonNull::from(&mut out).cast(),
        )
    };
    (st == 0 && out != 0).then_some(out)
}

/// Makes `uid` the system output device (what every app plays to).
pub fn set_default_output(uid: &str) -> Result<()> {
    set_hardware_device(kAudioHardwarePropertyDefaultOutputDevice, uid)
}

fn set_hardware_device(selector: u32, uid: &str) -> Result<()> {
    let mut dev = device_id_from_uid(uid).ok_or_else(|| anyhow!("dispositivo non trovato"))?;
    let mut addr = address(selector);
    let st = unsafe {
        AudioObjectSetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            std::mem::size_of::<AudioObjectID>() as u32,
            NonNull::from(&mut dev).cast(),
        )
    };
    if st != 0 {
        bail!("errore {st}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Latency reported by a device (what players use to delay the video)
// ---------------------------------------------------------------------------

unsafe fn get_prop<T: Copy + Default>(obj: AudioObjectID, selector: u32, scope: u32) -> Option<T> {
    let mut addr = AudioObjectPropertyAddress { mSelector: selector, mScope: scope, mElement: kAudioObjectPropertyElementMain };
    let mut v = T::default();
    let mut size = std::mem::size_of::<T>() as u32;
    let st = AudioObjectGetPropertyData(obj, NonNull::from(&mut addr), 0, std::ptr::null(), NonNull::from(&mut size), NonNull::from(&mut v).cast());
    (st == 0).then_some(v)
}

/// Output latency the device declares (device + safety offset + stream +
/// IO buffer), in milliseconds.
pub fn reported_latency_ms(uid: &str) -> Option<f64> {
    use objc2_core_audio::{
        kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyLatency, kAudioDevicePropertyNominalSampleRate,
        kAudioDevicePropertySafetyOffset, kAudioDevicePropertyStreams, kAudioObjectPropertyScopeOutput,
        AudioObjectGetPropertyDataSize,
    };
    let dev = device_id_from_uid(uid)?;
    let out = kAudioObjectPropertyScopeOutput;
    unsafe {
        let rate: f64 = get_prop(dev, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal)?;
        if rate <= 0.0 {
            return None;
        }
        let lat: u32 = get_prop(dev, kAudioDevicePropertyLatency, out).unwrap_or(0);
        let safety: u32 = get_prop(dev, kAudioDevicePropertySafetyOffset, out).unwrap_or(0);
        let buf: u32 = get_prop(dev, kAudioDevicePropertyBufferFrameSize, out).unwrap_or(0);
        let mut addr = AudioObjectPropertyAddress { mSelector: kAudioDevicePropertyStreams, mScope: out, mElement: kAudioObjectPropertyElementMain };
        let mut size = 0u32;
        let mut stream_lat = 0u32;
        if AudioObjectGetPropertyDataSize(dev, NonNull::from(&mut addr), 0, std::ptr::null(), NonNull::from(&mut size)) == 0 && size >= 4 {
            let mut ids = vec![0 as AudioObjectID; size as usize / 4];
            if AudioObjectGetPropertyData(dev, NonNull::from(&mut addr), 0, std::ptr::null(), NonNull::from(&mut size), NonNull::new(ids.as_mut_ptr()).unwrap().cast()) == 0 {
                stream_lat = get_prop(ids[0], kAudioDevicePropertyLatency, kAudioObjectPropertyScopeGlobal).unwrap_or(0);
            }
        }
        Some((lat + safety + buf + stream_lat) as f64 / rate * 1000.0)
    }
}

/// Current sample rate of a device.
pub fn nominal_rate(uid: &str) -> Option<f64> {
    use objc2_core_audio::kAudioDevicePropertyNominalSampleRate;
    let dev = device_id_from_uid(uid)?;
    unsafe { get_prop::<f64>(dev, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal) }.filter(|r| *r > 0.0)
}

/// The device can run at `rate` (unknown → assume yes).
pub fn supports_rate(uid: &str, rate: f64) -> bool {
    use objc2_core_audio::{kAudioDevicePropertyAvailableNominalSampleRates, AudioObjectGetPropertyDataSize};
    use objc2_core_audio_types::AudioValueRange;
    let Some(dev) = device_id_from_uid(uid) else { return false };
    let mut addr = address(kAudioDevicePropertyAvailableNominalSampleRates);
    let mut size = 0u32;
    unsafe {
        let item = std::mem::size_of::<AudioValueRange>();
        if AudioObjectGetPropertyDataSize(dev, NonNull::from(&mut addr), 0, std::ptr::null(), NonNull::from(&mut size)) != 0 || (size as usize) < item {
            return true;
        }
        let mut ranges = vec![AudioValueRange { mMinimum: 0.0, mMaximum: 0.0 }; size as usize / item];
        let Some(ptr) = NonNull::new(ranges.as_mut_ptr()) else { return true };
        if AudioObjectGetPropertyData(dev, NonNull::from(&mut addr), 0, std::ptr::null(), NonNull::from(&mut size), ptr.cast()) != 0 {
            return true;
        }
        ranges.truncate(size as usize / item);
        ranges.iter().any(|r| rate >= r.mMinimum - 0.5 && rate <= r.mMaximum + 0.5)
    }
}

/// Parts of the declared output latency, in frames: (rate, device latency,
/// safety offset, buffer, latency of each stream). For diagnostics.
pub fn latency_parts(uid: &str) -> Option<(f64, u32, u32, u32, Vec<u32>)> {
    use objc2_core_audio::{
        kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyLatency, kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertySafetyOffset,
        kAudioObjectPropertyScopeOutput, kAudioStreamPropertyLatency,
    };
    let dev = device_id_from_uid(uid)?;
    let out = kAudioObjectPropertyScopeOutput;
    unsafe {
        let rate: f64 = get_prop(dev, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal)?;
        let lat: u32 = get_prop(dev, kAudioDevicePropertyLatency, out).unwrap_or(0);
        let safety: u32 = get_prop(dev, kAudioDevicePropertySafetyOffset, out).unwrap_or(0);
        let buf: u32 = get_prop(dev, kAudioDevicePropertyBufferFrameSize, out).unwrap_or(0);
        let streams = output_stream_ids(dev).iter().map(|s| get_prop(*s, kAudioStreamPropertyLatency, kAudioObjectPropertyScopeGlobal).unwrap_or(0)).collect();
        Some((rate, lat, safety, buf, streams))
    }
}

// ---------------------------------------------------------------------------
// "System Audio Recording" permission
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Permission {
    Granted,
    Denied,
    NotAsked,
    Unknown,
}

extern "C" {
    fn dlopen(path: *const std::ffi::c_char, mode: std::ffi::c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
}

/// Asks TCC (the macOS privacy database) whether we may capture system audio.
/// Uses the same private check as Apple's own sample code; if it cannot be
/// loaded the answer is `Unknown`.
fn tcc_symbol(name: &std::ffi::CStr) -> Option<*mut c_void> {
    struct Handle(*mut c_void);
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}
    static TCC: std::sync::OnceLock<Handle> = std::sync::OnceLock::new();
    let h = TCC.get_or_init(|| Handle(unsafe { dlopen(c"/System/Library/PrivateFrameworks/TCC.framework/Versions/A/TCC".as_ptr(), 2) }));
    if h.0.is_null() {
        return None;
    }
    let f = unsafe { dlsym(h.0, name.as_ptr()) };
    (!f.is_null()).then_some(f)
}

pub fn capture_permission() -> Permission {
    type Preflight = unsafe extern "C" fn(*const CFString, *const c_void) -> i32;
    let Some(f) = tcc_symbol(c"TCCAccessPreflight") else { return Permission::Unknown };
    let preflight: Preflight = unsafe { std::mem::transmute(f) };
    let service = CFString::from_str("kTCCServiceAudioCapture");
    match unsafe { preflight(&*service, std::ptr::null()) } {
        0 => Permission::Granted,
        1 => Permission::Denied,
        2 => Permission::NotAsked,
        _ => Permission::Unknown,
    }
}

/// Shows the macOS "record system audio" prompt without blocking; `done`
/// is called with the answer. Returns false if the request is not possible.
///
/// Without this, opening the capture device blocks the calling thread until
/// the user answers the prompt (the window would freeze).
pub fn request_capture_permission(done: impl Fn(bool) + 'static) -> bool {
    type Request = unsafe extern "C" fn(*const CFString, *const c_void, *mut block2::Block<dyn Fn(u8)>);
    let Some(f) = tcc_symbol(c"TCCAccessRequest") else { return false };
    let request: Request = unsafe { std::mem::transmute(f) };
    let service = CFString::from_str("kTCCServiceAudioCapture");
    let block = block2::RcBlock::new(move |granted: u8| done(granted != 0));
    unsafe { request(&*service, std::ptr::null(), block2::RcBlock::as_ptr(&block)) };
    true
}

pub fn open_permission_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AudioCapture")
        .spawn();
}

// ---------------------------------------------------------------------------
// Device type and system volume
// ---------------------------------------------------------------------------

pub fn transport_kind(uid: &str) -> Option<crate::backend::DevKind> {
    use crate::backend::DevKind;
    use objc2_core_audio::*;
    let dev = device_id_from_uid(uid)?;
    let t: u32 = unsafe { get_prop(dev, kAudioDevicePropertyTransportType, kAudioObjectPropertyScopeGlobal)? };
    Some(match t {
        x if x == kAudioDeviceTransportTypeBluetooth || x == kAudioDeviceTransportTypeBluetoothLE => DevKind::Bluetooth,
        x if x == kAudioDeviceTransportTypeAirPlay => DevKind::AirPlay,
        x if x == kAudioDeviceTransportTypeHDMI || x == kAudioDeviceTransportTypeDisplayPort => DevKind::Screen,
        x if x == kAudioDeviceTransportTypeBuiltIn => DevKind::BuiltIn,
        x if x == kAudioDeviceTransportTypeUSB => DevKind::Usb,
        x if x == kAudioDeviceTransportTypeVirtual || x == kAudioDeviceTransportTypeAggregate => DevKind::Virtual,
        _ => DevKind::Other,
    })
}

fn default_output_device() -> Option<AudioObjectID> {
    let d: AudioObjectID =
        unsafe { get_prop(kAudioObjectSystemObject as AudioObjectID, kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyScopeGlobal)? };
    (d != 0).then_some(d)
}

fn device_uid(dev: AudioObjectID) -> Option<String> {
    use objc2_core_audio::kAudioDevicePropertyDeviceUID;
    let mut addr = address(kAudioDevicePropertyDeviceUID);
    let mut out: *const CFString = std::ptr::null();
    let mut size = std::mem::size_of::<*const CFString>() as u32;
    let st = unsafe {
        AudioObjectGetPropertyData(dev, NonNull::from(&mut addr), 0, std::ptr::null(), NonNull::from(&mut size), NonNull::from(&mut out).cast())
    };
    if st != 0 || out.is_null() {
        return None;
    }
    // SAFETY: the HAL returns a +1 retained CFString.
    let s = unsafe { objc2_core_foundation::CFRetained::from_raw(NonNull::new(out as *mut CFString)?) };
    Some(s.to_string())
}

/// Volume of an output device.
#[derive(Clone, Copy, Debug)]
pub struct DevVolume {
    /// Slider position 0..1, when the device has a volume control.
    pub scalar: Option<f32>,
    /// Linear gain (on the system's loudness curve; 1 without control).
    pub gain: f32,
    pub muted: bool,
}

fn volume_address(sel: u32, el: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress { mSelector: sel, mScope: objc2_core_audio::kAudioObjectPropertyScopeOutput, mElement: el }
}

fn has_property(dev: AudioObjectID, sel: u32, el: u32) -> bool {
    let mut a = volume_address(sel, el);
    unsafe { objc2_core_audio::AudioObjectHasProperty(dev, NonNull::from(&mut a)) }
}

fn settable(dev: AudioObjectID, sel: u32, el: u32) -> bool {
    if !has_property(dev, sel, el) {
        return false;
    }
    let mut a = volume_address(sel, el);
    let mut ok: u8 = 0;
    let st = unsafe { objc2_core_audio::AudioObjectIsPropertySettable(dev, NonNull::from(&mut a), NonNull::from(&mut ok)) };
    st == 0 && ok != 0
}

fn get_f32(dev: AudioObjectID, sel: u32, el: u32) -> Option<f32> {
    translate_f32(dev, sel, el, 0.0)
}

/// For translation properties (value in, value out).
fn translate_f32(dev: AudioObjectID, sel: u32, el: u32, input: f32) -> Option<f32> {
    let mut a = volume_address(sel, el);
    let mut v = input;
    let mut size = 4u32;
    let st = unsafe { AudioObjectGetPropertyData(dev, NonNull::from(&mut a), 0, std::ptr::null(), NonNull::from(&mut size), NonNull::from(&mut v).cast()) };
    (st == 0).then_some(v)
}

/// Loudness of a volume slider position. The driver's own curve comes
/// first: Bluetooth speakers with absolute volume turn themselves down and
/// report 0 dB as the current level whatever the slider says. Values that
/// make no sense (seen: 2e24 dB) are skipped; the last resort is the usual
/// cubic curve.
fn volume_gain(scalar: f32, curve_db: Option<f32>, current_db: Option<f32>) -> f32 {
    if scalar <= 0.001 {
        return 0.0;
    }
    let plausible = |d: &f32| d.is_finite() && (-120.0..=12.0).contains(d);
    match curve_db.filter(plausible).or(current_db.filter(plausible)) {
        Some(d) if d <= -95.0 => 0.0,
        Some(d) => 10f32.powf(d / 20.0),
        None => scalar * scalar * scalar,
    }
}

fn device_volume(dev: AudioObjectID) -> DevVolume {
    use objc2_core_audio::{
        kAudioDevicePropertyMute, kAudioDevicePropertyVolumeDecibels, kAudioDevicePropertyVolumeScalar, kAudioDevicePropertyVolumeScalarToDecibels,
    };
    // Master element first, otherwise the first channel.
    let el = [0, 1].into_iter().find(|&e| has_property(dev, kAudioDevicePropertyVolumeScalar, e));
    let scalar = el.and_then(|e| get_f32(dev, kAudioDevicePropertyVolumeScalar, e)).filter(|v| v.is_finite()).map(|v| v.clamp(0.0, 1.0));
    let gain = match (el, scalar) {
        (Some(e), Some(v)) => volume_gain(
            v,
            translate_f32(dev, kAudioDevicePropertyVolumeScalarToDecibels, e, v),
            get_f32(dev, kAudioDevicePropertyVolumeDecibels, e),
        ),
        _ => 1.0, // device without volume control (e.g. HDMI)
    };
    let muted = [0, 1]
        .into_iter()
        .find(|&e| has_property(dev, kAudioDevicePropertyMute, e))
        .and_then(|e| {
            let mut a = volume_address(kAudioDevicePropertyMute, e);
            let mut v = 0u32;
            let mut size = 4u32;
            let st = unsafe { AudioObjectGetPropertyData(dev, NonNull::from(&mut a), 0, std::ptr::null(), NonNull::from(&mut size), NonNull::from(&mut v).cast()) };
            (st == 0).then_some(v != 0)
        })
        .unwrap_or(false);
    DevVolume { scalar, gain, muted }
}

pub fn volume_of(uid: &str) -> Option<DevVolume> {
    Some(device_volume(device_id_from_uid(uid)?))
}

/// (UID, linear gain, muted) of the system output device.
pub fn default_output_volume() -> Option<(String, f32, bool)> {
    let dev = default_output_device()?;
    let uid = device_uid(dev)?;
    let v = device_volume(dev);
    Some((uid, v.gain, v.muted))
}

pub fn default_output_uid() -> Option<String> {
    default_output_device().and_then(device_uid)
}

/// Moves the volume slider of an output device (all its channels); false
/// if the device has no volume control.
pub fn set_volume(uid: &str, scalar: f32) -> bool {
    use objc2_core_audio::kAudioDevicePropertyVolumeScalar;
    let Some(dev) = device_id_from_uid(uid) else { return false };
    let sel = kAudioDevicePropertyVolumeScalar;
    let elements: Vec<u32> = if settable(dev, sel, 0) {
        vec![0]
    } else {
        (1..=output_channels(dev).clamp(2, 16) as u32).filter(|&e| settable(dev, sel, e)).collect()
    };
    let mut v = scalar.clamp(0.0, 1.0);
    let mut ok = false;
    for e in elements {
        let mut a = volume_address(sel, e);
        let st = unsafe { AudioObjectSetPropertyData(dev, NonNull::from(&mut a), 0, std::ptr::null(), 4, NonNull::from(&mut v).cast()) };
        ok |= st == 0;
    }
    ok
}

// ---------------------------------------------------------------------------
// "MultiAudio": the output every app plays to while MultiAudio is on
// ---------------------------------------------------------------------------

pub const VOUT_UID: &str = "app.multiaudio.out";
/// How long "MultiAudio" is left alone after it appears, and again after it
/// becomes the system output, before a tap is made on it.
///
/// macOS' audio daemon (coreaudiod) froze twice, for every program, while
/// this output was being made (2026-11-23, 2026-11-26). A sample of the
/// frozen daemon (2026-11-27) shows two of its threads waiting for each
/// other for good: one making a tap on this output (`HALS_System::CreateTap`:
/// it holds the system's lock and wants the output's, in
/// `HALS_Device::CopyStreamList`), the other answering a program that reads
/// how the output is made (`HALS_MetaDevice::GetPropertyData`: it holds the
/// output's lock and wants the system's, in `HALS_System::CopyDeviceByUID`).
/// Every other request then queues behind the system's lock. It is a fault
/// of the daemon's, and it takes the two requests at the same instant: a
/// new output, or a new system output, is when every program on the Mac
/// reads it (about twenty of them within 20 ms in the daemon's log), and
/// that is exactly when the taps used to be made. So they wait until that
/// is over. (Both times the output had a Bluetooth device as its clock;
/// with wired devices only it never happened.)
pub const VOUT_QUIET: std::time::Duration = std::time::Duration::from_millis(700);
/// Our output, also under the name an earlier version used: removed when
/// found at start (left over after a crash).
const STALE_UIDS: [&str; 2] = [VOUT_UID, "app.multiaudio.out51"];

/// A public aggregate device named "MultiAudio" with a 5.1 speaker layout.
/// It is made of real outputs only to have channels and a clock: what apps
/// play to it is captured (and muted there) and sent to the real speakers
/// with the right delays. It declares the latency of the whole chain, so
/// players delay the picture by it. While it exists it is the system output;
/// when dropped it gives the previous one back.
pub struct VirtualOut {
    agg: AudioObjectID,
    pub channels: usize,
    /// Latency it declares (what players read), ms.
    pub declared_ms: f64,
    /// The lowest it could declare: that of its slowest device (macOS adds
    /// the extra latency on top of it).
    pub floor_ms: f64,
}

/// What to put back when our output goes away (also after a panic).
struct Restore {
    agg: AudioObjectID,
    /// Taps that keep its sound off its real devices.
    silencers: Vec<AudioObjectID>,
    default: Option<String>,
    system: Option<String>,
}

static LIVE: std::sync::Mutex<Option<Restore>> = std::sync::Mutex::new(None);
/// UIDs of the real devices our output is made of, clock first (empty
/// when it is not there).
static MEMBERS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn is_ours(uid: &str) -> bool {
    uid.starts_with("app.multiaudio.")
}

fn system_device(selector: u32) -> Option<AudioObjectID> {
    let d: AudioObjectID = unsafe { get_prop(kAudioObjectSystemObject as AudioObjectID, selector, kAudioObjectPropertyScopeGlobal)? };
    (d != 0).then_some(d)
}

fn restore(r: Restore) {
    use objc2_core_audio::kAudioHardwarePropertyDefaultSystemOutputDevice as SYS;
    if let Ok(mut m) = MEMBERS.try_lock() {
        m.clear();
    }
    for (sel, uid) in [(kAudioHardwarePropertyDefaultOutputDevice, &r.default), (SYS, &r.system)] {
        if system_device(sel) == Some(r.agg) {
            if let Some(u) = uid.as_deref().filter(|u| device_id_from_uid(u).is_some()) {
                let _ = set_hardware_device(sel, u);
            }
        }
    }
    unsafe {
        for t in &r.silencers {
            AudioHardwareDestroyProcessTap(*t);
        }
        AudioHardwareDestroyAggregateDevice(r.agg);
    }
}

impl VirtualOut {
    /// Takes the output away now and gives the previous system output back
    /// (dropping it does the same).
    pub fn remove(&mut self) {
        if self.agg == 0 {
            return;
        }
        let live = LIVE.lock().ok().and_then(|mut g| if g.as_ref().map(|r| r.agg) == Some(self.agg) { g.take() } else { None });
        restore(live.unwrap_or(Restore { agg: self.agg, silencers: Vec::new(), default: None, system: None }));
        self.agg = 0;
        // It takes a moment to go: until then it is still found, and the
        // capture of the system's audio would be set up as if it were there
        // (seen on 2026-11-27: two seconds of the wrong capture).
        for _ in 0..40 {
            if device_id_from_uid(VOUT_UID).is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

impl Drop for VirtualOut {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Taps that keep what apps play to `uid` off its hardware, always (not
/// only while MultiAudio reads it): nothing leaks to the real devices it is
/// made of while the engine restarts or waits.
fn silence(uid: &str) -> Vec<AudioObjectID> {
    let Some(dev) = device_id_from_uid(uid) else { return Vec::new() };
    let exclude: Retained<NSArray<NSNumber>> = match own_process_object() {
        Some(me) => NSArray::from_retained_slice(&[NSNumber::new_u32(me)]),
        None => NSArray::new(),
    };
    (0..output_stream_ids(dev).len())
        .filter_map(|s| unsafe {
            let d = CATapDescription::initExcludingProcesses_andDeviceUID_withStream(CATapDescription::alloc(), &exclude, &NSString::from_str(uid), s as isize);
            d.setMuteBehavior(CATapMuteBehavior::Muted);
            d.setName(&NSString::from_str("MultiAudio (silenzio)"));
            d.setPrivate(true);
            let mut tap: AudioObjectID = 0;
            (AudioHardwareCreateProcessTap(Some(&d), &mut tap) == 0 && tap != 0).then_some(tap)
        })
        .collect()
}

/// Gives the system output back and removes our output (panic hook, signal).
pub fn emergency_restore() {
    if let Some(r) = LIVE.try_lock().ok().and_then(|mut g| g.take()) {
        restore(r);
    }
}

fn output_channels(dev: AudioObjectID) -> usize {
    use objc2_core_audio::kAudioStreamPropertyVirtualFormat;
    output_stream_ids(dev)
        .iter()
        .map(|s| unsafe {
            let mut asbd: objc2_core_audio_types::AudioStreamBasicDescription = std::mem::zeroed();
            let mut a = address(kAudioStreamPropertyVirtualFormat);
            let mut size = std::mem::size_of_val(&asbd) as u32;
            let st = AudioObjectGetPropertyData(*s, NonNull::from(&mut a), 0, std::ptr::null(), NonNull::from(&mut size), NonNull::from(&mut asbd).cast());
            if st == 0 {
                asbd.mChannelsPerFrame as usize
            } else {
                0
            }
        })
        .sum()
}

/// Channels of our output (0 if it does not exist).
pub fn vout_channels() -> usize {
    device_id_from_uid(VOUT_UID).map(output_channels).unwrap_or(0)
}

/// Removes our output left over by a previous run (crash).
pub fn destroy_stale() {
    for uid in STALE_UIDS {
        if let Some(d) = device_id_from_uid(uid) {
            unsafe {
                AudioHardwareDestroyAggregateDevice(d);
            }
            crate::diag::log(format!("rimossa una vecchia uscita «{uid}»"));
        }
    }
}

pub fn is_aggregate(uid: &str) -> bool {
    use objc2_core_audio::{kAudioDevicePropertyTransportType, kAudioDeviceTransportTypeAggregate};
    device_id_from_uid(uid)
        .and_then(|d| unsafe { get_prop::<u32>(d, kAudioDevicePropertyTransportType, kAudioObjectPropertyScopeGlobal) })
        .map(|t| t == kAudioDeviceTransportTypeAggregate)
        .unwrap_or(false)
}

/// Creates "MultiAudio" and makes it the system output.
/// `members`: (UID, declared latency in ms, channels) of the real outputs it
/// is made of, clock first. `latency_ms`: the latency it must declare.
/// `fallback`: output to give back when the current one is not usable.
pub fn create_vout(members: &[(String, f64, usize)], latency_ms: f64, fallback: Option<&str>) -> Result<VirtualOut> {
    use objc2_core_audio::{
        kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceSubDeviceListKey, kAudioDevicePropertyNominalSampleRate,
        kAudioDevicePropertyPreferredChannelLayout, kAudioHardwarePropertyDefaultSystemOutputDevice, kAudioObjectPropertyScopeOutput,
        kAudioSubDeviceDriftCompensationKey, kAudioSubDeviceExtraOutputLatencyKey, kAudioSubDeviceUIDKey,
    };
    let first_uid = members.first().map(|m| m.0.clone()).ok_or_else(|| anyhow!("nessuna uscita su cui costruirla"))?;
    // An aggregate declares as *device* latency that of its clock device
    // (the first): many players (Chrome, Firefox…) read only that, so the
    // clock must be the video output. The rest (slower devices, the extra
    // latency) goes into its safety offset, which other players add.
    let slowest = members.iter().map(|m| m.1).fold(0.0f64, f64::max);
    destroy_stale();
    let prev_default = default_output_uid().filter(|u| !is_ours(u)).or(fallback.map(str::to_string));
    let prev_system = system_device(kAudioHardwarePropertyDefaultSystemOutputDevice).and_then(device_uid).filter(|u| !is_ours(u)).or(prev_default.clone());
    unsafe {
        let rate: f64 = device_id_from_uid(&first_uid)
            .and_then(|d| get_prop(d, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal))
            .unwrap_or(48000.0);
        let extra_frames = ((latency_ms - slowest).max(0.0) / 1000.0 * rate).round() as u32;
        let extra_ms = extra_frames as f64 / rate * 1000.0;
        let mut subs: Vec<Retained<NSDictionary<NSString, AnyObject>>> = Vec::new();
        for (i, (u, _, _)) in members.iter().enumerate() {
            let mut k = vec![key(kAudioSubDeviceUIDKey), key(kAudioSubDeviceDriftCompensationKey)];
            let mut v = vec![to_any(NSString::from_str(u)), to_any(NSNumber::new_bool(i > 0))];
            if i == 0 && extra_frames > 0 {
                k.push(key(kAudioSubDeviceExtraOutputLatencyKey));
                v.push(to_any(NSNumber::new_u32(extra_frames)));
            }
            let kr: Vec<&NSString> = k.iter().map(|x| &**x).collect();
            let vr: Vec<&AnyObject> = v.iter().map(|x| &**x).collect();
            subs.push(NSDictionary::<NSString, AnyObject>::from_slices(&kr, &vr));
        }
        let sub_refs: Vec<&AnyObject> = subs.iter().map(|d| any(&**d)).collect();
        let list = NSArray::<AnyObject>::from_slice(&sub_refs);
        let keys = [
            key(kAudioAggregateDeviceNameKey),
            key(kAudioAggregateDeviceUIDKey),
            key(kAudioAggregateDeviceIsPrivateKey),
            key(kAudioAggregateDeviceIsStackedKey),
            key(kAudioAggregateDeviceMainSubDeviceKey),
            key(kAudioAggregateDeviceSubDeviceListKey),
        ];
        let vals: [Retained<AnyObject>; 6] = [
            to_any(NSString::from_str("MultiAudio")),
            to_any(NSString::from_str(VOUT_UID)),
            to_any(NSNumber::new_bool(false)),
            to_any(NSNumber::new_bool(false)),
            to_any(NSString::from_str(&first_uid)),
            to_any(list),
        ];
        let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
        let val_refs: Vec<&AnyObject> = vals.iter().map(|v| &**v).collect();
        let dict = NSDictionary::<NSString, AnyObject>::from_slices(&key_refs, &val_refs);
        let cf: &CFDictionary = &*(Retained::as_ptr(&dict) as *const CFDictionary);
        let mut agg: AudioObjectID = 0;
        let st = AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut agg));
        if st != 0 || agg == 0 {
            bail!("impossibile creare l'uscita MultiAudio (errore {st})");
        }
        // the device needs a moment before its streams are there
        let expected: usize = members.iter().map(|m| m.2).sum();
        let mut channels = 0;
        for _ in 0..40 {
            channels = output_channels(agg);
            if channels >= expected.max(2) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        // Speaker layout: L R C LFE Ls Rs (L R Ls Rs with 4 or 5 channels).
        let labels: &[u32] = if channels >= 6 {
            &[1, 2, 3, 4, 5, 6]
        } else if channels >= 4 {
            &[1, 2, 5, 6]
        } else {
            &[1, 2]
        };
        let mut layout: Vec<u32> = vec![0, 0, channels as u32]; // UseChannelDescriptions, bitmap, count
        for ch in 0..channels {
            layout.extend_from_slice(&[labels.get(ch).copied().unwrap_or(0), 0, 0, 0, 0]);
        }
        let mut a = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyPreferredChannelLayout,
            mScope: kAudioObjectPropertyScopeOutput,
            mElement: kAudioObjectPropertyElementMain,
        };
        let layout_ok = AudioObjectSetPropertyData(
            agg,
            NonNull::from(&mut a),
            0,
            std::ptr::null(),
            (layout.len() * 4) as u32,
            NonNull::new(layout.as_mut_ptr()).unwrap().cast(),
        ) == 0;
        std::thread::sleep(VOUT_QUIET); // (the other programs are reading it now)
        let silencers = silence(VOUT_UID);
        if silencers.is_empty() {
            crate::diag::log("uscita MultiAudio: non riesco a silenziarla sui dispositivi reali");
        }
        if let Ok(mut g) = LIVE.lock() {
            *g = Some(Restore { agg, silencers, default: prev_default.clone(), system: prev_system });
        }
        if let Ok(mut m) = MEMBERS.lock() {
            *m = members.iter().map(|x| x.0.clone()).collect();
        }
        // (another output, another try at capturing it from its devices)
        MEMBERS_OFF.store(false, std::sync::atomic::Ordering::Relaxed);
        let mut declared = None;
        for _ in 0..20 {
            declared = reported_latency_ms(VOUT_UID);
            if declared.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let declared_ms = declared.unwrap_or(slowest + extra_ms);
        let device_ms = latency_parts(VOUT_UID)
            .map(|(r, lat, _, _, streams)| (lat + streams.first().copied().unwrap_or(0)) as f64 / r * 1000.0)
            .unwrap_or(0.0);
        let out = VirtualOut { agg, channels, declared_ms, floor_ms: declared_ms - extra_ms };
        if let Err(e) = set_hardware_device(kAudioHardwarePropertyDefaultOutputDevice, VOUT_UID) {
            crate::diag::log(format!("uscita MultiAudio: non riesco a renderla l'uscita di sistema ({e})"));
        }
        // alerts and sound effects too (macOS may refuse it for aggregates)
        let _ = set_hardware_device(kAudioHardwarePropertyDefaultSystemOutputDevice, VOUT_UID);
        // (and now again: the engine makes its taps on it when this returns)
        std::thread::sleep(VOUT_QUIET);
        let names: Vec<String> = members.iter().map(|m| crate::backend::display_name(&format!("cpal:coreaudio:{}", m.0))).collect();
        crate::diag::log(format!(
            "uscita «MultiAudio» creata: {channels} canali ({}{}) da {}, latenza dichiarata {declared_ms:.0} ms (voluta {latency_ms:.0}; come dispositivo {device_ms:.0} ms), prima era «{}»",
            match channels {
                c if c >= 6 => "5.1",
                4 | 5 => "quadrifonico",
                _ => "stereo",
            },
            if layout_ok { "" } else { ", schema non accettato" },
            names.join(", "),
            prev_default.as_deref().map(|u| crate::backend::display_name(&format!("cpal:coreaudio:{u}"))).unwrap_or_default()
        ));
        Ok(out)
    }
}


#[cfg(all(test, local_tests))]
#[path = "local_tests/mactap.rs"]
mod tests;
