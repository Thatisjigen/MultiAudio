//! Windows: volume and mute of the default output (what the volume keys
//! change). WASAPI loopback captures audio before this volume is applied.

use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

/// (linear gain, muted) of the default render endpoint.
pub fn default_output_volume() -> Option<(f32, bool)> {
    unsafe {
        // Harmless if COM is already initialised on this thread.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
        let db = volume.GetMasterVolumeLevel().ok()?;
        let muted = volume.GetMute().ok()?.as_bool();
        Some((10f32.powf(db / 20.0), muted))
    }
}
