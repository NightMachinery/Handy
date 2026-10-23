use cpal::traits::{DeviceTrait, HostTrait};

pub struct CpalDeviceInfo {
    pub index: String,
    pub name: String,
    pub is_default: bool,
    pub device: cpal::Device,
}

pub fn list_input_devices() -> Result<Vec<CpalDeviceInfo>, Box<dyn std::error::Error>> {
    let host = crate::audio_toolkit::get_cpal_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());

    let mut out = Vec::<CpalDeviceInfo>::new();

    for (index, device) in host.input_devices()?.enumerate() {
        let name = device.name().unwrap_or_else(|_| "Unknown".into());

        let is_default = Some(name.clone()) == default_name;

        out.push(CpalDeviceInfo {
            index: index.to_string(),
            name,
            is_default,
            device,
        });
    }

    Ok(out)
}

/// Whether the named input device is the computer's own microphone.
///
/// Judged by CoreAudio's transport type rather than the name, which macOS
/// localizes ("MacBook Pro Microphone", "MacBook Pro-Mikrofon"). `name` is
/// cpal's device name, which cpal reads through the same coreaudio-rs helper
/// this lookup matches against. `None` when the device cannot be found or
/// queried.
#[cfg(target_os = "macos")]
pub fn is_builtin_input_device(name: &str) -> Option<bool> {
    use coreaudio::audio_unit::macos_helpers::get_device_id_from_name;
    use objc2_core_audio::{
        kAudioDevicePropertyTransportType, kAudioDeviceTransportTypeBuiltIn, kAudioHardwareNoError,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
        AudioObjectGetPropertyData, AudioObjectPropertyAddress,
    };
    use std::ptr::{null, NonNull};

    let device_id = get_device_id_from_name(name, true)?;
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyTransportType,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut transport_type: u32 = 0;
    let mut data_size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            NonNull::from(&address),
            0,
            null(),
            NonNull::from(&mut data_size),
            NonNull::from(&mut transport_type).cast(),
        )
    };
    (status == kAudioHardwareNoError).then_some(transport_type == kAudioDeviceTransportTypeBuiltIn)
}

/// Built-in microphone detection is only needed for the macOS clamshell check.
#[cfg(not(target_os = "macos"))]
pub fn is_builtin_input_device(_name: &str) -> Option<bool> {
    None
}

pub fn list_output_devices() -> Result<Vec<CpalDeviceInfo>, Box<dyn std::error::Error>> {
    let host = crate::audio_toolkit::get_cpal_host();
    let default_name = host.default_output_device().and_then(|d| d.name().ok());

    let mut out = Vec::<CpalDeviceInfo>::new();

    for (index, device) in host.output_devices()?.enumerate() {
        let name = device.name().unwrap_or_else(|_| "Unknown".into());

        let is_default = Some(name.clone()) == default_name;

        out.push(CpalDeviceInfo {
            index: index.to_string(),
            name,
            is_default,
            device,
        });
    }

    Ok(out)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{is_builtin_input_device, list_input_devices};

    #[test]
    fn every_listed_input_resolves_to_a_transport_type() {
        // The built-in check finds devices by cpal's name. If the two ever
        // disagree on naming, the check silently reports "not built-in" and
        // the lid-closed refusal stops firing.
        for device in list_input_devices().expect("list input devices") {
            assert!(
                is_builtin_input_device(&device.name).is_some(),
                "no CoreAudio input device named {:?}",
                device.name
            );
        }
    }

    #[test]
    fn unknown_device_is_not_reported_builtin() {
        assert_eq!(
            is_builtin_input_device("no such microphone \u{1F399}"),
            None
        );
    }
}
