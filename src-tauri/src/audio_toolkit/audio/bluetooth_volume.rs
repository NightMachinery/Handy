//! macOS may select a louder output volume when a Bluetooth mic opens.
//! Guard only that headset's transition into two-way audio.
use core_foundation::{base::TCFType, string::CFString};
use coreaudio::audio_unit::macos_helpers::{get_default_device_id, get_device_id_from_name};
use objc2_audio_toolbox::kAudioHardwareServiceDeviceProperty_VirtualMainVolume;
use objc2_core_audio::{
    kAudioDevicePropertyDeviceIsRunningSomewhere, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyTransportType,
    kAudioDeviceTransportTypeBluetooth, kAudioDeviceTransportTypeBluetoothLE,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeOutput, AudioObjectGetPropertyData, AudioObjectPropertyAddress,
    AudioObjectSetPropertyData,
};
use std::{
    mem::{size_of, MaybeUninit},
    ptr::{null, NonNull},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

// SAFETY: T must be the HAL property's documented return type.
unsafe fn read<T>(device: u32, property: AudioObjectPropertyAddress) -> Option<T> {
    let mut size = size_of::<T>() as u32;
    let mut value = MaybeUninit::<T>::uninit();
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&property),
            0,
            null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    if status == 0 && size as usize == size_of::<T>() {
        Some(unsafe { value.assume_init() })
    } else {
        None
    }
}

fn uid(device: u32) -> Option<String> {
    let property = address(
        kAudioDevicePropertyDeviceUID,
        kAudioObjectPropertyScopeGlobal,
    );
    // The HAL returns a retained CFStringRef for DeviceUID.
    let value = NonNull::new(unsafe { read(device, property)? })?;
    Some(unsafe { CFString::wrap_under_create_rule(value.as_ptr()) }.to_string())
}

fn paired_uids(input: &str, output: &str) -> bool {
    matches!((input.strip_suffix(":input"), output.strip_suffix(":output")),
        (Some(a), Some(b)) if !a.is_empty() && a == b)
}

fn is_bluetooth(device: u32) -> bool {
    let property = address(
        kAudioDevicePropertyTransportType,
        kAudioObjectPropertyScopeGlobal,
    );
    unsafe { read::<u32>(device, property) }.is_some_and(|transport| {
        transport == kAudioDeviceTransportTypeBluetooth
            || transport == kAudioDeviceTransportTypeBluetoothLE
    })
}

fn volume_property() -> AudioObjectPropertyAddress {
    address(
        kAudioHardwareServiceDeviceProperty_VirtualMainVolume,
        kAudioObjectPropertyScopeOutput,
    )
}

fn volume(device: u32) -> Option<f32> {
    unsafe { read::<f32>(device, volume_property()) }.filter(|v| (0.0..=1.0).contains(v))
}

fn running(device: u32) -> Option<bool> {
    let property = address(
        kAudioDevicePropertyDeviceIsRunningSomewhere,
        kAudioObjectPropertyScopeGlobal,
    );
    unsafe { read::<u32>(device, property) }.map(|flag| flag != 0)
}

fn sample_rate(device: u32) -> Option<f64> {
    let property = address(
        kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeGlobal,
    );
    unsafe { read::<f64>(device, property) }
}

#[derive(Debug, PartialEq)]
enum Action {
    Wait,
    Lower,
    Finish,
}

// Volume and route properties need not arrive in the same HAL notification.
// Allow a short settling interval once the microphone takes over the route.
#[derive(Default)]
struct Transition {
    switched_at: Option<Instant>,
}

impl Transition {
    fn observe(&mut self, now: Instant, switched: bool, baseline: f32, current: f32) -> Action {
        if current < baseline {
            return Action::Finish;
        }
        if switched {
            self.switched_at.get_or_insert(now);
        }
        match self.switched_at {
            None => Action::Wait,
            Some(t) if !switched || now.duration_since(t) > Duration::from_millis(500) => {
                Action::Finish
            }
            Some(_) if current > baseline => Action::Lower,
            Some(_) => Action::Wait,
        }
    }
}

struct Headset {
    input: u32,
    output: u32,
    baseline: f32,
    initial_rate: f64,
    was_running: bool,
}

impl Headset {
    fn before_open(input_name: &str) -> Option<Self> {
        let output = get_default_device_id(false)?;
        if !is_bluetooth(output) {
            return None;
        }
        let input = get_device_id_from_name(input_name, true)?;
        if input != output && !(is_bluetooth(input) && paired_uids(&uid(input)?, &uid(output)?)) {
            return None;
        }
        Some(Self {
            input,
            output,
            baseline: volume(output)?,
            initial_rate: sample_rate(output)?,
            was_running: running(input)?,
        })
    }

    fn correct_transition(&self, cancel: mpsc::Receiver<()>) {
        // The profile switch can happen inside build_input_stream or after play().
        // Watch only the open transition. Never pin the volume during dictation.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut transition = Transition::default();
        while Instant::now() < deadline {
            if !matches!(
                cancel.recv_timeout(Duration::from_millis(10)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                return;
            }
            if get_default_device_id(false) != Some(self.output) {
                return;
            }
            let Some(current) = volume(self.output) else {
                return;
            };
            // A split headset exposes a dedicated input endpoint. A combined
            // device can already be running for playback; the shift to the
            // lower voice sample rate identifies its profile change.
            let switched = (!self.was_running && running(self.input) == Some(true))
                || sample_rate(self.output).is_some_and(|rate| rate < self.initial_rate);
            match transition.observe(Instant::now(), switched, self.baseline, current) {
                Action::Wait => continue,
                Action::Finish => return,
                Action::Lower => {}
            }
            let status = unsafe {
                AudioObjectSetPropertyData(
                    self.output,
                    NonNull::from(&volume_property()),
                    0,
                    null(),
                    size_of::<f32>() as u32,
                    NonNull::from(&self.baseline).cast(),
                )
            };
            if status == 0 {
                log::debug!("Restored Bluetooth output volume after microphone open ({current:.3} to {:.3})", self.baseline);
            } else {
                log::warn!(
                    "Could not restore Bluetooth output volume after microphone open: {status}"
                );
            }
            return;
        }
    }
}

/// Dropping the guard cancels and joins the short-lived monitor before the
/// stream is destroyed or another capture worker can reuse the device.
pub(super) struct BluetoothVolumeGuard {
    cancel: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}

impl BluetoothVolumeGuard {
    pub(super) fn arm(input_name: &str) -> Option<Self> {
        let headset = Headset::before_open(input_name)?;
        let (cancel, rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("bluetooth-volume".into())
            .spawn(move || headset.correct_transition(rx))
            .ok()?;
        Some(Self {
            cancel,
            worker: Some(worker),
        })
    }
}

impl Drop for BluetoothVolumeGuard {
    fn drop(&mut self) {
        let _ = self.cancel.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn louder_profile_is_lowered_regardless_of_notification_order() {
        let now = Instant::now();
        for volume_first in [false, true] {
            let mut transition = Transition::default();
            let first = if volume_first { 0.67 } else { 0.56 };
            assert_eq!(
                transition.observe(now, !volume_first, 0.56, first),
                Action::Wait
            );
            assert_eq!(
                transition.observe(now + Duration::from_millis(30), true, 0.56, 0.67),
                Action::Lower
            );
        }
    }

    #[test]
    fn ignore_quieter_profiles_and_later_user_adjustments() {
        let now = Instant::now();
        let mut transition = Transition::default();
        assert_eq!(transition.observe(now, true, 0.56, 0.50), Action::Finish);
        assert_eq!(transition.observe(now, true, 0.56, 0.56), Action::Wait);
        assert_eq!(
            transition.observe(now + Duration::from_secs(1), true, 0.56, 0.75),
            Action::Finish
        );
    }

    #[test]
    fn split_endpoints_must_belong_to_the_same_headset() {
        assert!(paired_uids("headset-a:input", "headset-a:output"));
        assert!(!paired_uids("headset-a:input", "headset-b:output"));
        assert!(!paired_uids("headset", "headset"));
        assert!(!paired_uids(":input", ":output"));
    }
}
