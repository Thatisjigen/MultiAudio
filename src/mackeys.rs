//! macOS: the volume keys.
//!
//! The "MultiAudio" output has no volume control of its own, so with it as
//! the system output macOS would only show a crossed-out speaker. While it
//! is the system output MultiAudio takes the volume keys (and only those)
//! and sets its own volume. Reading the keys needs the Accessibility
//! permission.

use objc2_app_kit::{NSEvent, NSEventModifierFlags};
use objc2_core_foundation::{kCFRunLoopCommonModes, CFBoolean, CFDictionary, CFMachPort, CFRetained, CFRunLoop, CFString, CFType};
use objc2_core_graphics::{CGEvent, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering::Relaxed};

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const CFDictionary) -> bool;
}

static RUNNING: AtomicBool = AtomicBool::new(false);
/// The event tap (macOS turns it off if a callback is ever too slow).
static PORT: AtomicPtr<CFMachPort> = AtomicPtr::new(std::ptr::null_mut());

/// MultiAudio may read the keyboard's special keys.
pub fn trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Shows the macOS prompt that leads to Privacy → Accessibility.
pub fn ask() {
    let key = CFString::from_str("AXTrustedCheckOptionPrompt");
    let yes = CFBoolean::new(true);
    let dict = CFDictionary::<CFString, CFType>::from_slices(&[&*key], &[yes.as_ref()]);
    unsafe {
        AXIsProcessTrustedWithOptions(CFRetained::as_ptr(&dict).as_ptr() as *const CFDictionary);
    }
}

pub fn open_settings() {
    let _ = std::process::Command::new("open").arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility").spawn();
}

pub fn running() -> bool {
    RUNNING.load(Relaxed)
}

// Codes of the special keys in NSSystemDefined events (IOKit's NX_KEYTYPE_*).
const NX_SUBTYPE_AUX_CONTROL_BUTTONS: i16 = 8;
const NX_KEYTYPE_SOUND_UP: isize = 0;
const NX_KEYTYPE_SOUND_DOWN: isize = 1;
const NX_KEYTYPE_MUTE: isize = 7;
const NS_SYSTEM_DEFINED: u32 = 14;

unsafe extern "C-unwind" fn callback(_proxy: CGEventTapProxy, ty: CGEventType, event: NonNull<CGEvent>, _user: *mut c_void) -> *mut CGEvent {
    let pass = event.as_ptr();
    if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
        let p = PORT.load(Relaxed);
        if !p.is_null() {
            CGEvent::tap_enable(&*p, true);
        }
        return pass;
    }
    if ty.0 != NS_SYSTEM_DEFINED || !crate::volume::active() {
        return pass;
    }
    let Some(ev) = NSEvent::eventWithCGEvent(event.as_ref()) else { return pass };
    if ev.subtype().0 != NX_SUBTYPE_AUX_CONTROL_BUTTONS {
        return pass;
    }
    let data1 = ev.data1();
    let key = match (data1 >> 16) & 0xFFFF {
        NX_KEYTYPE_SOUND_UP => crate::volume::Key::Up,
        NX_KEYTYPE_SOUND_DOWN => crate::volume::Key::Down,
        NX_KEYTYPE_MUTE => crate::volume::Key::Mute,
        _ => return pass,
    };
    let down = (data1 >> 8) & 0xFF == 0x0A;
    if down {
        let m = ev.modifierFlags();
        let fine = m.contains(NSEventModifierFlags::Option) && m.contains(NSEventModifierFlags::Shift);
        crate::volume::on_key(key, fine);
    }
    std::ptr::null_mut() // taken: macOS does not see it
}

/// Starts listening to the volume keys (once, for the whole run). False if
/// macOS does not allow it (yet).
pub fn start() -> bool {
    if RUNNING.load(Relaxed) {
        return true;
    }
    if !trusted() {
        return false;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("multiaudio-keys".into()).spawn(move || unsafe {
        let mask: u64 = 1 << NS_SYSTEM_DEFINED;
        let Some(port) = CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            mask,
            Some(callback),
            std::ptr::null_mut(),
        ) else {
            let _ = tx.send(false);
            return;
        };
        let (Some(src), Some(rl)) = (CFMachPort::new_run_loop_source(None, Some(&port), 0), CFRunLoop::current()) else {
            let _ = tx.send(false);
            return;
        };
        PORT.store(CFRetained::as_ptr(&port).as_ptr(), Relaxed);
        rl.add_source(Some(&src), kCFRunLoopCommonModes);
        CGEvent::tap_enable(&port, true);
        let _ = tx.send(true);
        CFRunLoop::run();
        drop(src);
        drop(port);
    });
    let ok = spawned.is_ok() && rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap_or(false);
    RUNNING.store(ok, Relaxed);
    crate::diag::log(if ok { "tasti volume: attivi" } else { "tasti volume: macOS non li concede" });
    ok
}
