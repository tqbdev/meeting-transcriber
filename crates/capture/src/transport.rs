//! Device connection types, read from Core Audio because cpal reports
//! `InterfaceType::Unknown` for every device on macOS.

use std::{collections::HashSet, ffi::c_void, ptr::NonNull};

use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, kAudioDevicePropertyDeviceUID, kAudioDevicePropertyTransportType,
    kAudioDeviceTransportTypeBluetooth, kAudioDeviceTransportTypeBluetoothLE,
    kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
};
use objc2_core_foundation::{CFRetained, CFString};

/// Core Audio UIDs of the connected Bluetooth devices. cpal's device ids on
/// macOS are `coreaudio:<UID>`, so [`cpal::DeviceId::id`] matches these.
pub(crate) fn bluetooth_uids() -> HashSet<String> {
    devices()
        .into_iter()
        .filter(|&device| {
            read::<u32>(device, kAudioDevicePropertyTransportType).is_some_and(|transport| {
                transport == kAudioDeviceTransportTypeBluetooth
                    || transport == kAudioDeviceTransportTypeBluetoothLE
            })
        })
        .filter_map(uid)
        .collect()
}

fn address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn devices() -> Vec<AudioObjectID> {
    let system = kAudioObjectSystemObject as AudioObjectID;
    let address = address(kAudioHardwarePropertyDevices);
    let mut size = 0u32;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            system,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    let mut ids = vec![0 as AudioObjectID; size as usize / size_of::<AudioObjectID>()];
    let status = unsafe {
        AudioObjectGetPropertyData(
            system,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(ids.as_mut_ptr()).unwrap().cast(),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    ids.truncate(size as usize / size_of::<AudioObjectID>());
    ids
}

/// Reads a fixed-size property. Only for plain-data types like `u32`.
fn read<T: Copy + Default>(object: AudioObjectID, selector: u32) -> Option<T> {
    let address = address(selector);
    let mut value = T::default();
    let mut size = size_of::<T>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    (status == 0).then_some(value)
}

fn uid(device: AudioObjectID) -> Option<String> {
    // The property hands back a +1 retained CFString.
    let raw = read::<usize>(device, kAudioDevicePropertyDeviceUID)?;
    let string = NonNull::new(raw as *mut c_void)?.cast::<CFString>();
    let string = unsafe { CFRetained::from_raw(string) };
    Some(string.to_string())
}
