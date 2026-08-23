//! Give a GUI-subsystem binary somewhere to print.
//!
//! `main.rs` sets `windows_subsystem = "windows"` for release builds, so the
//! process starts with no console attached and every `println!` writes to an
//! invalid handle — the user sees nothing at all. Attaching to the parent
//! console and re-pointing the standard handles fixes that for client mode.
//!
//! Two residual warts, documented rather than fixed here: `cmd.exe` does not
//! wait on a GUI-subsystem executable, so the prompt returns immediately and
//! output interleaves (use `start /wait handy …`, or PowerShell); and a
//! console-subsystem `handy-cli` shim would be the real fix if piping becomes
//! the primary Windows workflow, at the cost of a second LTO link.

use windows::core::PCSTR;
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    CreateFileA, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    AttachConsole, SetConsoleOutputCP, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

const CP_UTF8: u32 = 65001;

/// Attach to the parent console so client-mode output is visible.
///
/// Failure is normal and ignored: there is no parent console when launched
/// from Explorer or as a login item.
pub fn attach_parent_console() {
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return;
        }
        // Without this, non-ASCII transcripts come out as mojibake.
        let _ = SetConsoleOutputCP(CP_UTF8);

        if let Some(handle) = open_console("CONOUT$", true) {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, handle);
            let _ = SetStdHandle(STD_ERROR_HANDLE, handle);
        }
        if let Some(handle) = open_console("CONIN$", false) {
            let _ = SetStdHandle(STD_INPUT_HANDLE, handle);
        }
    }
}

unsafe fn open_console(name: &str, write: bool) -> Option<HANDLE> {
    let name = format!("{name}\0");
    let access = if write {
        FILE_GENERIC_WRITE.0
    } else {
        FILE_GENERIC_READ.0
    };
    let handle = CreateFileA(
        PCSTR(name.as_ptr()),
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        None,
        OPEN_EXISTING,
        Default::default(),
        None,
    )
    .ok()?;
    (handle != INVALID_HANDLE_VALUE).then_some(handle)
}
