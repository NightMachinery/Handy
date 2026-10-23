#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "macos")]
mod iokit {
    use core_foundation_sys::base::{CFAllocatorRef, CFTypeRef};
    use core_foundation_sys::dictionary::CFMutableDictionaryRef;
    use core_foundation_sys::string::CFStringRef;
    use std::os::raw::c_char;

    /// `io_object_t` and its `io_service_t` alias are mach ports.
    pub type IoObject = u32;
    /// `kIOMainPortDefault` (`MACH_PORT_NULL`): let IOKit pick the main port.
    pub const MAIN_PORT_DEFAULT: u32 = 0;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        pub fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
        /// Consumes one reference to `matching`.
        pub fn IOServiceGetMatchingService(
            main_port: u32,
            matching: CFMutableDictionaryRef,
        ) -> IoObject;
        pub fn IORegistryEntryCreateCFProperty(
            entry: IoObject,
            key: CFStringRef,
            allocator: CFAllocatorRef,
            options: u32,
        ) -> CFTypeRef;
        pub fn IOObjectRelease(object: IoObject) -> i32;
    }
}

/// Checks if the MacBook is in clamshell mode (lid closed with external display)
///
/// Reads the AppleClamshellState property of the IOPMrootDomain service
/// straight from the IORegistry. That costs microseconds, where spawning
/// `ioreg` for the same answer cost ~10ms, and the check sits on the
/// keypress-to-capture path of every recording start.
/// Returns true if the lid is closed, false if open.
#[cfg(target_os = "macos")]
pub fn is_clamshell() -> Result<bool, String> {
    use core_foundation_sys::base::{kCFAllocatorDefault, CFGetTypeID, CFRelease};
    use core_foundation_sys::number::{CFBooleanGetTypeID, CFBooleanGetValue, CFBooleanRef};
    use core_foundation_sys::string::{kCFStringEncodingUTF8, CFStringCreateWithCString};

    unsafe {
        let matching = iokit::IOServiceMatching(c"IOPMrootDomain".as_ptr());
        if matching.is_null() {
            return Err("IOServiceMatching(IOPMrootDomain) returned null".to_string());
        }
        let root_domain = iokit::IOServiceGetMatchingService(iokit::MAIN_PORT_DEFAULT, matching);
        if root_domain == 0 {
            return Err("IOPMrootDomain service not found".to_string());
        }

        let key = CFStringCreateWithCString(
            kCFAllocatorDefault,
            c"AppleClamshellState".as_ptr(),
            kCFStringEncodingUTF8,
        );
        let value = if key.is_null() {
            std::ptr::null()
        } else {
            let value =
                iokit::IORegistryEntryCreateCFProperty(root_domain, key, kCFAllocatorDefault, 0);
            CFRelease(key.cast());
            value
        };
        iokit::IOObjectRelease(root_domain);

        if value.is_null() {
            // Desktops have no lid, so the property is simply absent.
            return Ok(false);
        }
        let closed =
            CFGetTypeID(value) == CFBooleanGetTypeID() && CFBooleanGetValue(value as CFBooleanRef);
        CFRelease(value);
        Ok(closed)
    }
}

/// Checks if the Mac is a laptop by detecting battery presence
///
/// This uses pmset to check for battery information.
/// Returns true if a battery is detected (laptop), false otherwise (desktop)
#[cfg(target_os = "macos")]
#[tauri::command]
#[specta::specta]
pub fn is_laptop() -> Result<bool, String> {
    let output = Command::new("pmset")
        .arg("-g")
        .arg("batt")
        .output()
        .map_err(|e| e.to_string())?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Check if InternalBattery is present (laptops have batteries, desktops typically don't)
    Ok(stdout.contains("InternalBattery"))
}

/// Stub implementation for non-macOS platforms
/// Always returns false since clamshell mode is macOS-specific
#[cfg(not(target_os = "macos"))]
pub fn is_clamshell() -> Result<bool, String> {
    Ok(false)
}

/// Stub implementation for non-macOS platforms
/// Always returns false since laptop detection is macOS-specific
#[cfg(not(target_os = "macos"))]
#[tauri::command]
#[specta::specta]
pub fn is_laptop() -> Result<bool, String> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn test_clamshell_check() {
        // This will run on macOS and should not panic
        let result = is_clamshell();
        assert!(result.is_ok());
        let _ = result.unwrap();
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn clamshell_check_agrees_with_ioreg() {
        // The IOKit read replaced `ioreg -r -k AppleClamshellState`; both must
        // see the same lid, whichever position it is in while the test runs.
        let output = Command::new("ioreg")
            .args(["-r", "-k", "AppleClamshellState", "-d", "4"])
            .output()
            .expect("ioreg runs");
        let ioreg_closed =
            String::from_utf8_lossy(&output.stdout).contains("\"AppleClamshellState\" = Yes");
        assert_eq!(is_clamshell(), Ok(ioreg_closed));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_is_laptop() {
        let result = is_laptop();
        assert!(result.is_ok());
        if let Ok(is_laptop) = result {
            println!("Is laptop: {}", is_laptop);
        }
    }
}
