use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED};

/// RAII COM init for a worker thread (RPC handlers run on fresh threads).
/// MTA everywhere: the UIA client bindings initialize the apartment as MTA
/// and a later STA request on the same thread would fail with
/// RPC_E_CHANGED_MODE.
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
