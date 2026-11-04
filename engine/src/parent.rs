use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE,
};

/// Die-with-parent watchdog: the host passes `--parent-pid <node pid>`; when
/// that process dies the helper must terminate so no orphan keeps driving the
/// desktop. Polls the parent handle every 500 ms.
pub fn start_watchdog(parent_pid: u32) {
    thread::spawn(move || loop {
        unsafe {
            match OpenProcess(PROCESS_SYNCHRONIZE, false, parent_pid) {
                Ok(h) if !h.is_invalid() => {
                    let wait = windows::Win32::System::Threading::WaitForSingleObject(h, 0);
                    let _ = CloseHandle(h);
                    if wait == WAIT_OBJECT_0 {
                        // Parent gone — exit quietly (no 130: this is not the
                        // physical-Esc interrupt path).
                        std::process::exit(0);
                    }
                }
                Ok(_) => std::process::exit(0),
                // Parent vanished (or cannot be opened because it already
                // exited): terminate.
                Err(_) => std::process::exit(0),
            }
        }
        thread::sleep(Duration::from_millis(500));
    });
}
