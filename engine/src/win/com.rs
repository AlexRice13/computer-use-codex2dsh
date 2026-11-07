use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED};

/// 工作线程上的 RAII COM 初始化（RPC handler 都跑在新线程上）。
/// 一律用 MTA：UIA 客户端那套绑定会把 apartment 初始化成 MTA，同一个线程后
/// 面再要求 STA 就会以 RPC_E_CHANGED_MODE 失败。
pub struct ComGuard {
    owned: bool,
}

impl ComGuard {
    pub fn new() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE) };
        ComGuard { owned: hr.is_ok() }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.owned {
            unsafe { CoUninitialize() }
        }
    }
}
