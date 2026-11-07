use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE,
};

/// 父进程死了就退的看门狗：宿主传 `--parent-pid <node pid>`，那个进程一死
/// 我们就得跟着结束，否则留个孤儿进程继续操作桌面。每 500 ms 查一次父进程
/// 句柄。
pub fn start_watchdog(parent_pid: u32) {
    thread::spawn(move || loop {
        unsafe {
            match OpenProcess(PROCESS_SYNCHRONIZE, false, parent_pid) {
                Ok(h) if !h.is_invalid() => {
                    let wait = windows::Win32::System::Threading::WaitForSingleObject(h, 0);
                    let _ = CloseHandle(h);
                    if wait == WAIT_OBJECT_0 {
                        // 父进程没了——悄悄退（不给 130，这不是物理 Esc
                        // 打断那条路）。
                        std::process::exit(0);
                    }
                }
                Ok(_) => std::process::exit(0),
                // 父进程已经没了（或者早退了，句柄都打不开）：直接结束。
                Err(_) => std::process::exit(0),
            }
        }
        thread::sleep(Duration::from_millis(500));
    });
}
