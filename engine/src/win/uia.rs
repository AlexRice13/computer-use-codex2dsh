// UI Automation tree construction for get_window_state. The original helper
// renders the control-view tree as an indented text document:
//
//   Window: "{title}", App: {exe}.
//   \t{idx} {LocalizedControlType} [(disabled)] [(settable, string)] [{name}]
//       [ Description: …] [ Value: …] [ ID: …] [ Secondary Actions: Raise]
//
// with a global element index feeding element_index actions, the focused
// node echoed as its own tree line, and document_text from the last
// Document node's ValuePattern.

use uiautomation::core::{UIAutomation, UIElement};
use uiautomation::patterns::{UILegacyIAccessiblePattern, UIValuePattern};
use windows::Win32::Foundation::HWND;

/// One row of the flattened snapshot tree, kept for element_index actions.
pub struct ElementRecord {
    pub element: UIElement,
    #[allow(dead_code)] // descriptive fields kept for diagnostics/future filters
    pub name: String,
    #[allow(dead_code)] // ditto
    pub control_type: String,
    /// Screen-space rectangle (left, top, width, height).
    pub rect: (i32, i32, i32, i32),
    #[allow(dead_code)] // ditto
    pub offscreen: bool,
    #[allow(dead_code)] // ditto
    pub enabled: bool,
}

pub struct BuiltTree {
    /// Rendered tree (header line + one tab-indented line per node).
    pub tree: String,
    /// Elements in index order (index == position in this vec == tree idx).
    pub elements: Vec<ElementRecord>,
    /// Focused element rendered as its tree line (index matches the tree).
    pub focused_element: Option<String>,
    /// Text of the last Document node's value (browser page text).
    pub document_text: Option<String>,
}

/// Cap on walked elements. 600 used to cut real pages off: the commit list of
/// a busy repository measured 868 nodes through the reference client, and the
/// walk stopped at exactly index 599. 2000 is above the largest tree observed
/// anywhere, so the limit no longer decides what the model gets to see;
/// TREE_BUDGET_MS remains the guard against pathological pages.
const MAX_ELEMENTS: usize = 2000;
const MAX_DEPTH: usize = 64;
const TREE_BUDGET_MS: u128 = 6500;

/// Elements at or below this count mean the provider has not answered yet:
/// measured stub trees are 2-4 nodes (Paint 2, Chrome 3) against real trees of
/// 181 and 1438. Anything above this is a window that walked fine and must
/// never enter the wake path.
const STUB_MAX: usize = 12;

// `SECURITY_MANDATORY_LOW_RID` / `..._MEDIUM_RID` from Win32_System_SystemServices,
// inlined so this diagnostic costs no extra `windows` crate feature.
const MANDATORY_LOW_RID: u32 = 0x1000;
const MANDATORY_MEDIUM_RID: u32 = 0x2000;

/// Mandatory integrity level of this process, as the last SID sub-authority of
/// its `TokenIntegrityLevel` label: 0x1000 Low, 0x2000 Medium, 0x3000 High,
/// 0x4000 System. `-1` means "not queried yet", `-2` means "could not be
/// queried" (in which case we behave as if the level were fine and let the
/// wake path run its normal bounded course).
///
/// WHY THIS IS HERE. A client process below Medium mandatory level can never
/// be handed a real UIA tree: the UIA core will not open an out-of-process
/// provider connection across the Medium boundary, so every window answers
/// with the frame the core can synthesize in-proc — the window plus its title
/// bar, i.e. exactly the 2-4 node "stub" this file has been chasing for three
/// agents. It is a property of the *token*, not of the request, so no advise,
/// event handler, apartment, message pump, settle loop or hosted-CLR walk can
/// change it; measured all of those, all inert, and the same `sky-engine.exe`
/// bytes walk Paint to 230 nodes when launched from a directory without a Low
/// mandatory label and to 2 from one with it. Detecting it here turns a
/// 2-4 node mystery into one trace line, and stops us paying for a wake that
/// provably cannot land.
static INTEGRITY_RID: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);

fn integrity_rid() -> i64 {
    let cached = INTEGRITY_RID.load(std::sync::atomic::Ordering::Relaxed);
    if cached != -1 {
        return cached;
    }
    let rid = query_integrity_rid().map_or(-2, |r| r as i64);
    INTEGRITY_RID.store(rid, std::sync::atomic::Ordering::Relaxed);
    rid
}

fn query_integrity_rid() -> Option<u32> {
    unsafe {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::Security::{
            GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, PSID,
            TokenIntegrityLevel, TOKEN_QUERY,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        let mut token = windows::Win32::Foundation::HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        // TOKEN_MANDATORY_LABEL is { SID_AND_ATTRIBUTES { PSID Sid; DWORD Attributes } },
        // so the label SID pointer is the first machine word of the buffer.
        let mut buf = [0u8; 64];
        let mut len = 0u32;
        let r = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            buf.len() as u32,
            &mut len,
        );
        let _ = CloseHandle(token);
        r.ok()?;
        let sid = PSID(*(buf.as_ptr() as *const *mut core::ffi::c_void));
        if sid.0.is_null() {
            return None;
        }
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(sid, count as u32 - 1))
    }
}

/// Below Medium mandatory level the stub is structural, not temporal: waiting,
/// advising or hosting a CLR cannot make the UIA core cross the boundary.
fn below_medium_il() -> bool {
    matches!(integrity_rid(), r if (0..(MANDATORY_MEDIUM_RID as i64)).contains(&r))
}

/// Once per process, say it out loud too. With the wake path gated off, a
/// Low-IL engine would otherwise answer *every* window with a 2-4 node stub and
/// the only clue would be an opt-in trace file; stderr is the one channel the
/// engine has that the operator actually sees, and it is not the JSON-RPC
/// stdout channel, so the output contract is untouched.
fn warn_low_il_once() {
    use std::sync::atomic::AtomicBool;
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!(
            "sky-engine: UIA returned a stub tree and this process runs at {} integrity \
             ({:#x}): the UIA core will not open an out-of-process provider connection for a \
             client below Medium, so every window will come back as 2-4 nodes. This is not a \
             provider-timing problem and no wake/retry can fix it. Launch the engine from a \
             directory with no Low mandatory label — e.g. \
             `icacls <dir> /setintegritylevel Medium`, or run the installed copy under \
             %LOCALAPPDATA% — see SKY_TRACE=1 for the per-window trace.",
            integrity_name(integrity_rid()),
            integrity_rid()
        );
    }
}

/// Human name for the token's mandatory level, for the trace line.
fn integrity_name(rid: i64) -> &'static str {
    match rid as u32 {
        MANDATORY_LOW_RID => "Low",
        MANDATORY_MEDIUM_RID => "Medium",
        0x3000 => "High",
        0x4000 => "System",
        _ => "Unknown",
    }
}

/// The wake path is behind an opt-in gate (`SKY_WAKE=1`) and off by default.
///
/// It has never been measured to land a tree on this machine (0/5), and the
/// reason is now known: every one of those measurements — plus the "the managed
/// stack only gets 4 nodes too" conclusion that retired the CLR-hosting work —
/// was taken from a client process running below Medium mandatory level, where
/// no client-side trick can ever work. Meanwhile the cost is real: on a stubbed
/// window it put a thread, a cross-process advise and up to `WAKE_WINDOW_MS` of
/// waiting in front of the first request (measured q1 1194 ms with the wake
/// path live versus ~80 ms with it gated), which is the latency regression that
/// was previously blamed on `methods.rs`. A window that walks normally still
/// pays nothing either way, because the stub test gates entry.
fn wake_enabled() -> bool {
    std::env::var("SKY_WAKE").map(|v| v == "1").unwrap_or(false)
}

/// Hard ceiling on the wake attempt. The reference helper's cold first request
/// takes ~400-660 ms, so this covers a legitimate late provider activation
/// without letting an unwakeable provider eat the request. TREE_BUDGET_MS
/// remains the absolute guard on the whole build.
const WAKE_WINDOW_MS: u64 = 1200;
const RETRY_INTERVAL_MS: u64 = 90;

/// The wake path is single-flight for the life of the process. It has never
/// been observed to help on this machine, so paying it once is already
/// generous; paying it per window would be pure latency.
static WAKE_ATTEMPTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn dbg_log(msg: &str) {
    if std::env::var("SKY_TRACE").is_err() {
        return;
    }
    let path = crate::win::taskbar::tmp_dir().join("dsh-sky-uia-debug.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = writeln!(f, "{msg}");
    }
}

fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

// Wake a potentially suspended UIA provider: the original helper is proven
// (ETW: IRawElementProviderAdviseEvents::AdviseEventAdded(20005) x360 during
// its first request) to hold live focus-changed + structure-changed event
// advices, which make the UIA core negotiate event-enabled connections with
// every provider it connects to. We subscribe the same pair (process-wide
// AutomationFocusChanged 20005 + subtree StructureChanged) and then let the
// tree walk run against a growth-stable retry loop so the Chromium provider
// has its settle window inside the same request budget.
#[allow(non_camel_case_types)]
mod uia_bindings {
    use windows::core::{implement, Ref as WinRef, Result as WinResult};
    use windows::Win32::System::Com::SAFEARRAY;
    use windows::Win32::UI::Accessibility::{
        IUIAutomationElement, IUIAutomationFocusChangedEventHandler,
        IUIAutomationFocusChangedEventHandler_Impl, IUIAutomationStructureChangedEventHandler,
        IUIAutomationStructureChangedEventHandler_Impl, StructureChangeType,
    };

    #[derive(Default)]
    #[implement(IUIAutomationStructureChangedEventHandler)]
    pub struct NopStructureHandler;

    impl IUIAutomationStructureChangedEventHandler_Impl for NopStructureHandler_Impl {
        fn HandleStructureChangedEvent(
            &self,
            _sender: WinRef<'_, IUIAutomationElement>,
            _changetype: StructureChangeType,
            _runtimeid: *const SAFEARRAY,
        ) -> WinResult<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    #[implement(IUIAutomationFocusChangedEventHandler)]
    pub struct NopFocusHandler;

    impl IUIAutomationFocusChangedEventHandler_Impl for NopFocusHandler_Impl {
        fn HandleFocusChangedEvent(
            &self,
            _sender: WinRef<'_, IUIAutomationElement>,
        ) -> WinResult<()> {
            Ok(())
        }
    }

    pub use windows::Win32::UI::Accessibility::IUIAutomationStructureChangedEventHandler as HandlerIfc;

    /// The UIA core keeps calling into the handler from its own threads;
    /// we only ever park strong references, which is Send-safe across the
    /// MTA process.
    pub struct Keepalive(#[allow(dead_code)] pub HandlerIfc);
    // SAFETY: interface pointer to an apartment-agnostic COM object.
    unsafe impl Send for Keepalive {}

    pub struct FocusKeepalive(#[allow(dead_code)] pub IUIAutomationFocusChangedEventHandler);
    // SAFETY: interface pointer to an apartment-agnostic COM object.
    unsafe impl Send for FocusKeepalive {}
}

/// Run the process's single wake attempt for a stubbed window.
///
/// Safety properties, in priority order:
///   1. Never hang the engine. The body runs on a dedicated thread and the
///      caller only ever waits on `recv_timeout`. On timeout we drop the
///      receiver, return the tree we already have, and let the abandoned
///      thread die on its own — a hung engine is a far worse outcome than a
///      stub tree, so this timeout is the load-bearing guarantee.
///   2. Never crash the engine. Every fallible step returns `false`; nothing
///      in the wake path can surface an error to the caller.
///   3. At most once per process, and reachable only from the stub branch, so
///      a window that walked normally pays nothing.
fn run_wake_attempt(hwnd: HWND, started: u64) -> bool {
    if WAKE_ATTEMPTED
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Relaxed,
        )
        .is_err()
    {
        return false; // this process already spent its one attempt
    }

    let elapsed = now_ms() - started;
    let headroom = (TREE_BUDGET_MS as u64)
        .saturating_sub(elapsed)
        .saturating_sub(250);
    let budget = WAKE_WINDOW_MS.min(headroom);
    if budget == 0 {
        dbg_log(&format!("[wake-skip] hwnd={hwnd:?} no budget left (elapsed={elapsed}ms)"));
        return false;
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let hwnd_val = hwnd.0 as isize;
    let spawned = std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(move || {
            // A fresh thread needs its own apartment init; MTA like everywhere
            // else in this process (see win::com).
            let _com = crate::win::com::ComGuard::new();
            let _ = tx.send(wake_body(hwnd_val));
        });
    if spawned.is_err() {
        dbg_log("[wake-skip] thread spawn failed");
        return false;
    }

    match rx.recv_timeout(std::time::Duration::from_millis(budget)) {
        Ok(true) => true,
        Ok(false) => {
            dbg_log(&format!("[wake-fail] hwnd={hwnd:?} body reported failure"));
            false
        }
        Err(_) => {
            dbg_log(&format!(
                "[wake-timeout] hwnd={hwnd:?} budget={budget}ms — abandoning wake thread"
            ));
            false
        }
    }
}

/// The wake body, executed on its own thread. Every step is permitted to fail;
/// a failure only ever means "no wake happened".
fn wake_body(hwnd: isize) -> bool {
    let mut did = wake_provider(hwnd);
    // Opt-in hosted-CLR managed wake. MEASURED INERT — see managed_wake.
    if std::env::var("SKY_WAKE_MANAGED").map(|v| v == "1").unwrap_or(false) {
        if let Ok(dll) = std::env::var("SKY_WAKE_DLL") {
            did |= managed_wake::run(&dll, hwnd);
        }
    }
    did
}

/// Host the .NET Framework CLR in-process and run a `System.Windows.Automation`
/// walk from the default AppDomain (opt-in: `SKY_WAKE_MANAGED=1` plus
/// `SKY_WAKE_DLL=<path to ManagedWake.dll>`).
///
/// MEASURED INERT — kept only so a future waker has somewhere to live without
/// anyone re-deriving the hosting. `_probe\clr-wake` proves, with the CLR chain
/// returning S_OK at every step (`CLRCreateInstance`, `GetRuntime`,
/// `GetVersionString = "v4.0.30319"`, `GetInterface`, `Start`,
/// `ExecuteInDefaultAppDomain`), that the managed stack itself returns the
/// stub: it walks Paint to 4 nodes and Chrome to 4 nodes against the reference
/// helper's 181 and 1438 measured in the same minute. A standalone real .NET
/// process gets the same 4 nodes (MTA, STA, a 4.3 s settle loop, RawView *and*
/// ControlView walkers, `FindAll(Subtree)` and root `FindAll(Descendants)`),
/// so the 2-4 node stub is not a hosting artefact and no amount of CLR hosting
/// — here or in a helper process — can turn it into a tree. The premise that
/// "the managed stack gets full trees from a fresh process" does not reproduce.
///
/// GUIDs and vtable slots are the Microsoft MIDL values, verified working by
/// that probe run. `GetVersionString` doubles as a layout self-check, so a
/// wrong vtable can never send us into an arbitrary slot.
mod managed_wake {
    use std::ffi::c_void;
    use windows::core::GUID;

    const CLSID_CLR_META_HOST: GUID = GUID::from_u128(0x9280188d_0e8e_4867_b30c_7fa83884e8de);
    const IID_ICLR_META_HOST: GUID = GUID::from_u128(0xd332db9e_b9b3_4125_8207_a14884f53216);
    const IID_ICLR_RUNTIME_INFO: GUID = GUID::from_u128(0xbd39d1d2_ba2f_486a_89b0_b4b0cb466891);
    const CLSID_CLR_RUNTIME_HOST: GUID = GUID::from_u128(0x90f1a06e_7712_4762_86b5_7a5eba6bdb02);
    const IID_ICLR_RUNTIME_HOST: GUID = GUID::from_u128(0x90f1a06c_7712_4762_86b5_7a5eba6bdb02);

    type FnCreate = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> i32;
    type FnGetRuntime =
        unsafe extern "system" fn(*mut c_void, *const u16, *const GUID, *mut *mut c_void) -> i32;
    type FnGetVersionString = unsafe extern "system" fn(*mut c_void, *mut u16, *mut u32) -> i32;
    type FnGetInterface =
        unsafe extern "system" fn(*mut c_void, *const GUID, *const GUID, *mut *mut c_void) -> i32;
    type FnStart = unsafe extern "system" fn(*mut c_void) -> i32;
    type FnExec = unsafe extern "system" fn(
        *mut c_void,
        *const u16,
        *const u16,
        *const u16,
        *const u16,
        *mut u32,
    ) -> i32;

    fn vtable(obj: *mut c_void, slot: usize) -> *const c_void {
        unsafe {
            let vtbl = *(obj as *const *const *const c_void);
            *vtbl.add(slot)
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Returns true only if the managed walk completed and saw more than the
    /// stub. It merely gates a re-walk; it is never an error to the caller.
    pub fn run(dll: &str, hwnd: isize) -> bool {
        unsafe {
            let name = wide("mscoree.dll");
            let Ok(hmod) = windows::Win32::System::LibraryLoader::LoadLibraryW(
                windows::core::PCWSTR(name.as_ptr()),
            ) else {
                return false;
            };
            let Some(proc) = windows::Win32::System::LibraryLoader::GetProcAddress(
                hmod,
                windows::core::s!("CLRCreateInstance"),
            ) else {
                return false;
            };
            let create =
                std::mem::transmute::<unsafe extern "system" fn() -> isize, FnCreate>(proc);

            let mut meta: *mut c_void = std::ptr::null_mut();
            if create(&CLSID_CLR_META_HOST, &IID_ICLR_META_HOST, &mut meta) != 0 || meta.is_null() {
                return false;
            }

            // ICLRMetaHost::GetRuntime — slot 3.
            let get_runtime: FnGetRuntime = std::mem::transmute(vtable(meta, 3));
            let ver = wide("v4.0.30319");
            let mut info: *mut c_void = std::ptr::null_mut();
            if get_runtime(meta, ver.as_ptr(), &IID_ICLR_RUNTIME_INFO, &mut info) != 0
                || info.is_null()
            {
                return false;
            }

            // Layout self-check: ICLRRuntimeInfo::GetVersionString is slot 3.
            // If this is not the MIDL layout we bail rather than call
            // GetInterface through a wrong slot.
            let get_version: FnGetVersionString = std::mem::transmute(vtable(info, 3));
            let mut buf = [0u16; 64];
            let mut cch = 64u32;
            if get_version(info, buf.as_mut_ptr(), &mut cch) != 0 {
                return false;
            }
            let verstr = String::from_utf16_lossy(&buf[..(cch.saturating_sub(1)).min(63) as usize]);
            if !verstr.starts_with("v4") {
                return false;
            }

            // ICLRRuntimeInfo::GetInterface — slot 9 (NOT 4).
            let get_iface: FnGetInterface = std::mem::transmute(vtable(info, 9));
            let mut host: *mut c_void = std::ptr::null_mut();
            if get_iface(info, &CLSID_CLR_RUNTIME_HOST, &IID_ICLR_RUNTIME_HOST, &mut host) != 0
                || host.is_null()
            {
                return false;
            }

            // ICLRRuntimeHost::Start — slot 3 (NOT 4).
            let start: FnStart = std::mem::transmute(vtable(host, 3));
            if start(host) != 0 {
                return false;
            }

            // ICLRRuntimeHost::ExecuteInDefaultAppDomain — slot 11 (slot 10 is
            // ExecuteApplication).
            let exec: FnExec = std::mem::transmute(vtable(host, 11));
            let asm = wide(dll);
            let cls = wide("ManagedWake");
            let meth = wide("Wake");
            let arg = wide(&format!("{hwnd}|2000|64|"));
            let mut result: u32 = 0xFFFF_FFFF;
            exec(host, asm.as_ptr(), cls.as_ptr(), meth.as_ptr(), arg.as_ptr(), &mut result) == 0
                && (result as i32) > 12
        }
    }
}

/// Subscribe the per-hwnd structure-changed event advice. Returns whether the
/// advice was installed. Measured: this alone does NOT wake the provider
/// (zero AdviseEventAdded on the wire), but it is the cheapest thing to keep
/// trying and it is the shape the reference helper's connections have.
fn wake_provider(hwnd_isize: isize) -> bool {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    use uia_bindings::{FocusKeepalive, Keepalive, NopFocusHandler, NopStructureHandler};
    static KEEP: OnceLock<Mutex<(HashSet<isize>, Vec<Keepalive>)>> = OnceLock::new();
    static FOCUS_KEEP: OnceLock<Mutex<Option<FocusKeepalive>>> = OnceLock::new();
    let keep = KEEP.get_or_init(|| Mutex::new((HashSet::new(), Vec::new())));
    let already_waked = keep
        .lock()
        .map(|g| g.0.contains(&hwnd_isize))
        .unwrap_or(false);
    if already_waked {
        return true; // already subscribed for this window
    }
    let hwnd = HWND(hwnd_isize as *mut std::ffi::c_void);
    unsafe {
        use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
        use windows::Win32::UI::Accessibility::{
            CUIAutomation, IUIAutomation, IUIAutomationFocusChangedEventHandler,
            IUIAutomationStructureChangedEventHandler, TreeScope_Subtree,
        };
        let Ok(uia) = CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER) else {
            return false;
        };
        let Ok(root) = uia.ElementFromHandle(hwnd) else {
            return false;
        };
        // AutomationFocusChangedEvent (20005) advice — the event the
        // original's providers are demonstrably advised with (ETW:
        // AdviseEventAdded(20005) x360). Process-wide focus advice is the
        // only shape the COM client API offers; it is opt-in via
        // SKY_WAKE_FOCUS=1 while we verify it cannot regress other
        // Chromium windows' cached trees on this connection.
        if std::env::var("SKY_WAKE_FOCUS").is_ok() {
            if let Ok(mut g) = FOCUS_KEEP.get_or_init(|| Mutex::new(None)).lock() {
                if g.is_none() {
                    let fh: IUIAutomationFocusChangedEventHandler = NopFocusHandler.into();
                    if uia.AddFocusChangedEventHandler(None, &fh).is_ok() {
                        *g = Some(FocusKeepalive(fh));
                    }
                }
            }
        }
        let handler: IUIAutomationStructureChangedEventHandler = NopStructureHandler.into();
        match uia.AddStructureChangedEventHandler(&root, TreeScope_Subtree, None, &handler) {
            Ok(()) => {
                if let Ok(mut g) = keep.lock() {
                    g.0.insert(hwnd_isize);
                    g.1.push(Keepalive(handler));
                }
                // Let the event negotiation run: pump this thread's messages
                // briefly (the original's event thread always pumps).
                pump_messages(60);
                true
            }
            Err(e) => {
                dbg_log(&format!("[wake-fail] hwnd={hwnd:?} {e}"));
                false
            }
        }
    }
}

/// Dispatch and drop this thread's pending messages for up to `ms`
/// milliseconds. UIA core completes parts of the client event registration
/// through the subscribing thread's message queue; without pumping, provider
/// advises stall (measured: AdviseEventAdded never fires).
fn pump_messages(ms: u64) {
    let t0 = now_ms();
    while (now_ms() - t0) < ms {
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{
                DispatchMessageW, PeekMessageW, MSG, PM_REMOVE,
            };
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// The wake trigger is purely "the first walk came back looking like the stub"
/// (see `STUB_MAX`). It is deliberately NOT gated on the window class: the
/// stub is not a Chromium peculiarity — Paint is UWP (`MSPaintApp`) and walks
/// 2 nodes exactly like Chrome's `Chrome_WidgetWin_1`, so a class-based gate
/// silently leaves every non-Chromium stubbed window unwaaked while paying the
/// cost on healthy Chromium windows that never needed it.
struct Snapshot {
    elements: Vec<ElementRecord>,
    lines: Vec<String>,
    last_document: Option<String>,
    title: String,
}

/// Build the accessibility tree for `hwnd`. `app_name` renders the header
/// line (`Window: "{title}", App: {app_name}.`).
///
/// Deliberately uses the non-cached walker/property calls: UIA's
/// BuildCache variants ask the provider to answer live and yield an empty
/// subtree for hung/suspended providers, while the plain calls let the UIA
/// core replay its last known snapshot — which is what the original does
/// (it renders full trees even for suspended Chromium processes).
pub fn build_tree(hwnd: HWND, app_name: &str) -> Result<BuiltTree, String> {
    let uia = UIAutomation::new().map_err(|e| {
        dbg_log(&format!("[init-fail] {e}"));
        format!("uia init: {e}")
    })?;
    let root = uia
        .element_from_handle(uiautomation::types::Handle::from(hwnd.0 as isize))
        .map_err(|e| {
            dbg_log(&format!("[from-handle-fail] hwnd={hwnd:?} {e}"));
            format!("element from handle: {e}")
        })?;
    let walker = uia
        .get_raw_view_walker()
        .map_err(|e| format!("walker: {e}"))?;

    let started = now_ms();
    let mut snapshot = walk_snapshot(&walker, &root, started);

    // Wake path: entered ONLY when the first walk came back looking like the
    // synthesized stub. A window that walked normally returns from build_tree
    // without touching any of this — no advise, no pump, no thread, no added
    // latency. (Previously wake_provider ran unconditionally *before* the first
    // walk, so every fresh window paid a 60 ms message pump.)
    //
    // Order matters: the integrity check is a token query we can answer in
    // microseconds and it explains the stub outright, so it runs first and
    // short-circuits a wake attempt that provably cannot succeed. The wake
    // itself is opt-in (`SKY_WAKE=1`) — see `wake_enabled`.
    if snapshot.elements.len() <= STUB_MAX {
        if below_medium_il() {
            warn_low_il_once();
            dbg_log(&format!(
                "[stub-below-medium-il] hwnd={hwnd:?} elements={} integrity={}({:#x}) — \
                 this process is below Medium mandatory level, so the UIA core will not open \
                 an out-of-process provider connection for it and every window answers with the \
                 2-4 node frame it can synthesize in-proc. This is a property of the token, not \
                 of the request: launch the engine from a directory with no Low mandatory label \
                 (icacls <dir> /setintegritylevel Medium) — no wake, advise, apartment, message \
                 pump, settle loop or hosted CLR can change it.",
                snapshot.elements.len(),
                integrity_name(integrity_rid()),
                integrity_rid()
            ));
        }
        // An explicit SKY_WAKE=1 still runs the wake at Low IL: the diagnosis
        // is ours to report, not to hide an experiment behind.
        if wake_enabled() && run_wake_attempt(hwnd, started) {
            let next = walk_snapshot(&walker, &root, started);
            // Growth-stable settle: after a wake that actually landed, keep
            // re-walking until the tree is past the stub and stops growing.
            // OPT-IN (SKY_WAKE_RETRY=1), still bounded by TREE_BUDGET_MS.
            //
            // It stays opt-in because no wake on this machine has been measured to
            // land: with the retry loop Chrome's first request was 4 lines/3 nodes
            // in 1653 ms, without it the same 4 lines/3 nodes in 89 ms, while the
            // reference helper served 1438 nodes in ~530 ms in the same run. The
            // loop is the right shape for the day a wake does land; it must not
            // cost 1.5 s per request while unproven.
            if next.elements.len() > snapshot.elements.len()
                && std::env::var("SKY_WAKE_RETRY").map(|v| v == "1").unwrap_or(false)
            {
                let deadline = (started + WAKE_WINDOW_MS).min(started + TREE_BUDGET_MS as u64 - 250);
                snapshot = next;
                let mut last = 0usize;
                let mut rounds = 0u32;
                while now_ms() < deadline {
                    pump_messages(RETRY_INTERVAL_MS);
                    rounds += 1;
                    let grown = walk_snapshot(&walker, &root, started);
                    let n = grown.elements.len();
                    snapshot = grown;
                    if n > STUB_MAX && n == last {
                        break; // grown and stable
                    }
                    if n > last {
                        last = n;
                    }
                }
                dbg_log(&format!(
                    "[wake-retry] hwnd={hwnd:?} rounds={rounds} elements={}",
                    snapshot.elements.len()
                ));
            } else {
                snapshot = next;
            }
        }
    }

    let mut out_lines = vec![format!("Window: \"{}\", App: {app_name}.", snapshot.title)];
    let mut lines = snapshot.lines;
    out_lines.append(&mut lines);
    let tree = out_lines.join("\n");

    // Focused element: rendered as its tree line when inside this subtree.
    let focused_element = uia.get_focused_element().ok().and_then(|f| {
        let fid = f.get_runtime_id().ok()?;
        snapshot
            .elements
            .iter()
            .position(|r| {
                r.element
                    .get_runtime_id()
                    .map(|id| id == fid)
                    .unwrap_or(false)
            })
    }).and_then(|i| out_lines.get(i + 1).cloned()); // +1: skip header line

    let built = BuiltTree {
        tree,
        elements: snapshot.elements,
        focused_element,
        document_text: snapshot.last_document,
    };
    dbg_log(&format!(
        "[ok] hwnd={hwnd:?} elements={} tree_len={} ms={}",
        built.elements.len(),
        built.tree.len(),
        now_ms() - started
    ));
    Ok(built)
}

fn walk_snapshot(
    walker: &uiautomation::core::UITreeWalker,
    root: &UIElement,
    started: u64,
) -> Snapshot {
    let mut elements: Vec<ElementRecord> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut last_document: Option<String> = None;
    walk(
        walker,
        root,
        0,
        started,
        &mut elements,
        &mut lines,
        &mut last_document,
    );
    let title = one_line(&root.get_name().unwrap_or_default());
    Snapshot {
        elements,
        lines,
        last_document,
        title,
    }
}

#[allow(clippy::too_many_arguments)]
fn walk(
    walker: &uiautomation::core::UITreeWalker,
    node: &UIElement,
    depth: usize,
    started: u64,
    elements: &mut Vec<ElementRecord>,
    lines: &mut Vec<String>,
    last_document: &mut Option<String>,
) {
    if elements.len() >= MAX_ELEMENTS || depth > MAX_DEPTH {
        return;
    }
    if (now_ms() - started) as u128 > TREE_BUDGET_MS {
        return;
    }

    let name = one_line(&node.get_name().unwrap_or_default());
    let control_type = node
        .get_control_type()
        .map(|c| format!("{c:?}"))
        .unwrap_or_else(|_| "Unknown".into());
    let localized = node
        .get_localized_control_type()
        .unwrap_or_else(|_| control_type.clone());
    let rect = node
        .get_bounding_rectangle()
        .map(|r| (r.get_left(), r.get_top(), r.get_width(), r.get_height()))
        .unwrap_or((0, 0, 0, 0));
    let offscreen = node.is_offscreen().unwrap_or(false);
    let enabled = node.is_enabled().unwrap_or(true);

    // ValuePattern shape: value string + settable marker.
    let value = node
        .get_pattern::<UIValuePattern>()
        .ok()
        .and_then(|p| p.get_value().ok())
        .map(|v| one_line(&v))
        .filter(|v| !v.is_empty());
    let settable = node
        .get_pattern::<UIValuePattern>()
        .ok()
        .and_then(|p| p.is_readonly().ok())
        .map(|ro| !ro)
        .unwrap_or(false);
    let description = node
        .get_pattern::<UILegacyIAccessiblePattern>()
        .ok()
        .and_then(|p| p.get_help().ok())
        .map(|v| one_line(&v))
        .filter(|v| !v.is_empty());
    let automation_id = node
        .get_automation_id()
        .map(|v| one_line(&v))
        .unwrap_or_default();

    if control_type == "Document" {
        if let Some(v) = &value {
            *last_document = Some(v.clone());
        }
    }

    let index = elements.len();
    elements.push(ElementRecord {
        element: node.clone(),
        name: name.clone(),
        control_type: control_type.clone(),
        rect,
        offscreen,
        enabled,
    });

    // Render per the original's field order.
    let mut label = format!("{}{index} {localized}", "\t".repeat(depth + 1));
    if !enabled {
        label.push_str(" (disabled)");
    }
    if settable {
        label.push_str(" (settable, string)");
    }
    if !name.is_empty() {
        label.push_str(&format!(" {name}"));
    }
    if let Some(d) = &description {
        label.push_str(&format!(" Description: {d}"));
    }
    if let Some(v) = &value {
        label.push_str(&format!(" Value: {v}"));
    }
    if !automation_id.is_empty() {
        label.push_str(&format!(" ID: {automation_id}"));
    }
    if control_type == "Window" {
        label.push_str(" Secondary Actions: Raise");
    }
    lines.push(label);

    // Recurse through children (disabled/offscreen subtrees included: the
    // original renders them). No children / provider gaps surface as Err
    // (null + S_OK) and end the sibling walk, as before.
    let mut child = match walker.get_first_child(node) {
        Ok(c) => Some(c),
        Err(e) => {
            dbg_log(&format!("[first-child-err] depth={depth} ct={control_type} {e}"));
            None
        }
    };
    while let Some(c) = child {
        walk(walker, &c, depth + 1, started, elements, lines, last_document);
        if elements.len() >= MAX_ELEMENTS || (now_ms() - started) as u128 > TREE_BUDGET_MS {
            break;
        }
        child = match walker.get_next_sibling(&c) {
            Ok(s) => Some(s),
            Err(e) => {
                dbg_log(&format!("[next-sib-err] depth={depth} {e}"));
                None
            }
        };
    }
}
