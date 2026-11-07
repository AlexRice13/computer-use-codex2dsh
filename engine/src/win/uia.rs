// get_window_state 的 UIA 树就在这儿拼。原版把 control view 的树渲染成一份
// 带缩进的文本：
//
//   Window: "{title}", App: {exe}.
//   \t{idx} {LocalizedControlType} [(disabled)] [(settable, string)] [{name}]
//       [ Description: …] [ Value: …] [ ID: …] [ Secondary Actions: Raise]
//
// 索引是全局的，element_index 操作靠它找元素。焦点节点单独占一行回显。
// document_text 取自最后一个 Document 节点的 ValuePattern。

use uiautomation::core::{UIAutomation, UIElement};
use uiautomation::patterns::{UILegacyIAccessiblePattern, UIValuePattern};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::HiDpi::GetDpiForWindow;

/// 拍平后的树里的一行，留着给 element_index 操作用。
pub struct ElementRecord {
    pub element: UIElement,
    #[allow(dead_code)] // 这几个描述字段留着查问题，以后也可能拿来做过滤
    pub name: String,
    #[allow(dead_code)] // 同上
    pub control_type: String,
    /// 屏幕坐标的矩形（left, top, width, height）。
    pub rect: (i32, i32, i32, i32),
    #[allow(dead_code)] // 同上
    pub offscreen: bool,
    #[allow(dead_code)] // 同上
    pub enabled: bool,
}

pub struct BuiltTree {
    /// 渲染好的树：header 一行，之后每个节点一行，按层级加 tab。
    pub tree: String,
    /// 按下标排的元素。下标 == 在这个 vec 里的位置 == 树里的 idx。
    pub elements: Vec<ElementRecord>,
    /// 焦点元素在树里的那一行（下标跟树对得上）。
    pub focused_element: Option<String>,
    /// 最后一个 Document 节点的文本，也就是网页正文。
    pub document_text: Option<String>,
}

/// 走树时的节点数上限。以前是 600，真实页面会被砍断：一个忙仓库的 commit 列表
/// 用原版客户端实测 868 个节点，我们正好停在 index 599 就没了。现在放到 2000，
/// 比目前见过的任何一棵树都大，所以这条限制不再决定模型能看到多少；防病态页面
/// 的仍是 TREE_BUDGET_MS。
const MAX_ELEMENTS: usize = 2000;
const MAX_DEPTH: usize = 64;
const TREE_BUDGET_MS: u128 = 6500;

/// 节点数小于等于这个值，说明 provider 还没应答。实测桩树就是 2-4 个节点
/// （Paint 2 个、Chrome 3 个），而真树是 181 和 1438。超过这个数的窗口就是
/// 走通了，绝不能放它进 wake 路径。
const STUB_MAX: usize = 12;

// `SECURITY_MANDATORY_LOW_RID` / `..._MEDIUM_RID` 本来在 Win32_System_SystemServices
// 里，这里直接把值抄过来，省得为一个诊断多开一个 `windows` crate feature。
const MANDATORY_LOW_RID: u32 = 0x1000;
const MANDATORY_MEDIUM_RID: u32 = 0x2000;

/// 本进程的完整性级别，取 `TokenIntegrityLevel` 标签 SID 最后一个子权威值：
/// 0x1000 Low、0x2000 Medium、0x3000 High、0x4000 System。`-1` 表示还没查过，
/// `-2` 表示查不到（这时就当级别没问题，让 wake 路径按原来的限时流程走一遍）。
///
/// 为什么要查：低于 Medium 的客户端进程永远拿不到真正的 UIA 树。UIA 核心不会
/// 跨 Medium 这条边界去开进程外 provider 连接，每个窗口只能用核心在进程内拼出来
/// 的框架应付——窗口加标题栏，正好就是 2-4 个节点，也就是这个文件折腾了几轮
/// 才弄明白的那个“桩”。这是 token 的性质，不是请求的问题：advise、事件处理、
/// apartment、消息泵、settle 循环、托管 CLR 全试过，全没用。同一个
/// `sky-engine.exe`，从不带 Low 标签的目录启动，画图能走到 230 个节点；从带 Low
/// 标签的目录启动只有 2 个。在这里认出来，2-4 个节点的怪事就变成一行 trace，
/// 也省得为一个明知不会成的 wake 白费功夫。
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
        // TOKEN_MANDATORY_LABEL 是 { SID_AND_ATTRIBUTES { PSID Sid; DWORD Attributes } }，
        // 所以缓冲区开头第一个机器字就是标签的 SID 指针。
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

/// 低于 Medium 时桩是结构性的，不是等一会儿就会好：等待、advise、托管 CLR
/// 都没法让 UIA 核心跨过这条边界。
fn below_medium_il() -> bool {
    matches!(integrity_rid(), r if (0..(MANDATORY_MEDIUM_RID as i64)).contains(&r))
}

/// 每个进程也往 stderr 喊一次。wake 路径默认关着，不然一个 Low IL 的引擎会对
/// *每个* 窗口都回 2-4 个节点的桩，唯一线索只有一份要手动打开的 trace 文件。
/// stderr 是引擎手里运维真能看到的那条通道，而且不是 JSON-RPC 的 stdout，
/// 所以输出约定不受影响。
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

/// 完整性级别的人话名字，trace 里用。
fn integrity_name(rid: i64) -> &'static str {
    match rid as u32 {
        MANDATORY_LOW_RID => "Low",
        MANDATORY_MEDIUM_RID => "Medium",
        0x3000 => "High",
        0x4000 => "System",
        _ => "Unknown",
    }
}

/// wake 路径要显式开关（`SKY_WAKE=1`），默认关。
///
/// 在这台机器上从来没测到它真能拿到树（0/5）。原因现在知道了：那 5 次测量，
/// 加上“托管栈也只有 4 个节点”这个让 CLR 托管那摊活收工的结论，全是在低于
/// Medium 的客户端进程里做的，那种环境下客户端怎么折腾都没用。代价倒是实打实：
/// 碰到桩窗口时，它会在第一个请求前头塞一个线程、一次跨进程 advise、最多
/// `WAKE_WINDOW_MS` 的等待（实测 q1 开着 wake 是 1194 ms，关掉约 80 ms），之前
/// 这段延迟被记到了 `methods.rs` 头上。走得通的窗口两头都不花时间，因为入口被
/// 桩判断挡住了。
fn wake_enabled() -> bool {
    std::env::var("SKY_WAKE").map(|v| v == "1").unwrap_or(false)
}

/// wake 尝试的硬上限。原版客户端冷启动第一个请求要 400-660 ms 左右，这个值够
/// 覆盖 provider 正常晚到，又不至于让一个根本唤不醒的 provider 把请求吃掉。
/// 整个建树过程的绝对兜底还是 TREE_BUDGET_MS。
const WAKE_WINDOW_MS: u64 = 1200;
const RETRY_INTERVAL_MS: u64 = 90;

/// wake 路径整个进程只跑一次。在这台机器上没见过它有用，付一次已经算大方；
/// 每个窗口都付就是纯白等。
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

// 目的是唤醒可能挂起的 UIA provider。原版第一个请求期间用 ETW 抓过
// （IRawElementProviderAdviseEvents::AdviseEventAdded(20005) 出现 360 次），
// 证明它一直挂着 focus-changed 和 structure-changed 两种事件 advise。UIA 核心
// 因此会跟每个连上的 provider 协商“开事件”的连接。我们照抄这一对（进程级
// AutomationFocusChanged 20005 + 子树 StructureChanged），然后让走树跑在一个
// “长到不再变”的重试循环里，这样 Chromium provider 在同一个请求预算内就有自己
// 的 settle 窗口。
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

    /// UIA 核心会从它自己的线程回调 handler。我们只是把强引用存着不放，
    /// 跨 MTA 进程算 Send 安全。
    pub struct Keepalive(#[allow(dead_code)] pub HandlerIfc);
    // SAFETY: 这是个跟 apartment 无关的 COM 对象的接口指针。
    unsafe impl Send for Keepalive {}

    pub struct FocusKeepalive(#[allow(dead_code)] pub IUIAutomationFocusChangedEventHandler);
    // SAFETY: 这是个跟 apartment 无关的 COM 对象的接口指针。
    unsafe impl Send for FocusKeepalive {}
}

/// 对一个桩窗口跑本进程唯一的一次 wake 尝试。
///
/// 按重要性排的三条底线：
///   1. 不能把引擎卡死。主体在单独线程里跑，调用方只等 `recv_timeout`。超时就
///      丢掉接收端，返回已经拿到的树，让那个被扔下的线程自己慢慢死。引擎卡死
///      比拿到桩树糟得多，所以这个超时是底线。
///   2. 不能把引擎搞崩。每一步失败都返回 `false`，wake 路径里的错不会冒到调用方。
///   3. 一个进程最多一次，而且只有走桩那条分支能进来，正常走通的窗口不用陪跑。
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
        return false; // 这个进程唯一一次机会已经花掉了
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
            // 新线程得自己初始化 apartment，跟本进程别处一样用 MTA
            // （见 win::com）。
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

/// wake 的主体，跑在自己的线程上。每一步都允许失败，失败只意味着“没唤醒成功”。
fn wake_body(hwnd: isize) -> bool {
    let mut did = wake_provider(hwnd);
    // 可选的托管 CLR wake。实测没用——见 managed_wake。
    if std::env::var("SKY_WAKE_MANAGED").map(|v| v == "1").unwrap_or(false) {
        if let Ok(dll) = std::env::var("SKY_WAKE_DLL") {
            did |= managed_wake::run(&dll, hwnd);
        }
    }
    did
}

/// 在进程内托管 .NET Framework CLR，从默认 AppDomain 里跑一遍
/// `System.Windows.Automation` 走树（要手动开：`SKY_WAKE_MANAGED=1` 加
/// `SKY_WAKE_DLL=<ManagedWake.dll 的路径>`）。
///
/// 实测没用。留着只是给以后的 waker 一个落脚点，省得再从头推一遍托管流程。
/// `_probe\clr-wake` 里 CLR 那条链每一步都返回 S_OK（`CLRCreateInstance`、
/// `GetRuntime`、`GetVersionString = "v4.0.30319"`、`GetInterface`、`Start`、
/// `ExecuteInDefaultAppDomain`），可托管栈拿回来的还是桩：画图 4 个节点、Chrome
/// 4 个节点，而同一分钟里原版客户端是 181 和 1438。单独跑一个真 .NET 进程也是
/// 4 个（MTA、STA、4.3 s 的 settle 循环、RawView 和 ControlView 两种 walker、
/// `FindAll(Subtree)` 和从 root `FindAll(Descendants)` 都试过），所以 2-4 个节点
/// 的桩不是托管方式造成的，托管 CLR 再怎么折腾——在这里还是在单独的 helper 进程
/// 里——都变不出树。“托管栈从新进程能拿到整棵树”这个前提复现不出来。
///
/// GUID 和 vtable 槽位用的是微软 MIDL 里的值，那次 probe 跑通了验证。
/// `GetVersionString` 顺便当布局自检，槽位不对就不往下走，不会失手调到别的槽里。
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

    /// 只有托管走树跑完、而且看到的比桩多才返回 true。它只决定要不要再走一遍，
    /// 对调用方永远不算错。
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

            // ICLRMetaHost::GetRuntime，槽位 3。
            let get_runtime: FnGetRuntime = std::mem::transmute(vtable(meta, 3));
            let ver = wide("v4.0.30319");
            let mut info: *mut c_void = std::ptr::null_mut();
            if get_runtime(meta, ver.as_ptr(), &IID_ICLR_RUNTIME_INFO, &mut info) != 0
                || info.is_null()
            {
                return false;
            }

            // 布局自检：ICLRRuntimeInfo::GetVersionString 在槽位 3。
            // 版本串不对就说明不是 MIDL 那套布局，直接退出，别拿错槽位去调
            // GetInterface。
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

            // ICLRRuntimeInfo::GetInterface，槽位 9，不是 4。
            let get_iface: FnGetInterface = std::mem::transmute(vtable(info, 9));
            let mut host: *mut c_void = std::ptr::null_mut();
            if get_iface(info, &CLSID_CLR_RUNTIME_HOST, &IID_ICLR_RUNTIME_HOST, &mut host) != 0
                || host.is_null()
            {
                return false;
            }

            // ICLRRuntimeHost::Start，槽位 3，不是 4。
            let start: FnStart = std::mem::transmute(vtable(host, 3));
            if start(host) != 0 {
                return false;
            }

            // ICLRRuntimeHost::ExecuteInDefaultAppDomain，槽位 11（槽位 10 是
            // ExecuteApplication）。
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

/// 给这个 hwnd 挂上 structure-changed 事件 advise，返回有没有挂上。实测：光挂
/// 这个唤不醒 provider（实际通信里 AdviseEventAdded 一次都没出现），但它是开销最小的
/// 尝试，而且原版客户端的连接就是这个形状。
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
        return true; // 这个窗口已经挂过了
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
        // 挂 AutomationFocusChangedEvent（20005）的 advise。原版那边的 provider
        // 就是挂着这个（ETW 里 AdviseEventAdded(20005) 出现 360 次）。COM 客户端
        // API 只给进程级的焦点 advise 这一种写法；先用 SKY_WAKE_FOCUS=1 手动开，
        // 同时验证它不会把这条连接上其他 Chromium 窗口的缓存树搞坏。
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
                // 让事件协商跑起来：先泵一会儿这个线程的消息（原版的事件线程
                // 一直在泵）。
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

/// 把本线程待处理的消息派发掉，最多泵 `ms` 毫秒。UIA 核心有一部分客户端事件
/// 注册要靠订阅线程的消息队列才走得完；不泵，provider 那边的 advise 就卡住
/// （实测：AdviseEventAdded 一次都不触发）。
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

/// 触发 wake 只看一条：第一次走树的结果像桩（见 `STUB_MAX`）。故意不按窗口类
/// 判断。桩不是 Chromium 独有的——画图是 UWP（`MSPaintApp`），照样只走 2 个
/// 节点，跟 Chrome 的 `Chrome_WidgetWin_1` 一样。按类名筛的话，所有非 Chromium
/// 的桩窗口会被悄悄漏掉不唤醒，却在那些本来没事的 Chromium 窗口上白费时间。
struct Snapshot {
    elements: Vec<ElementRecord>,
    lines: Vec<String>,
    last_document: Option<String>,
    title: String,
}

/// 给 `hwnd` 建无障碍树。`app_name` 只用来填 header 那行
/// （`Window: "{title}", App: {app_name}.`）。
///
/// 故意用不带缓存的 walker 和属性调用。UIA 的 BuildCache 系列会要求 provider
/// 当场应答，碰到挂起或卡住的 provider 只给一棵空子树；普通调用则让 UIA 核心
/// 重放它最后一次知道的快照——原版就是这么干的（连挂起的 Chromium 进程它也能
/// 渲染出整棵树）。
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
    // 窗口在屏幕上的位置（屏幕 px）加 DPI 系数。元素 rect 是屏幕物理像素，
    // 但树是给一个按逻辑像素量窗口截图的模型看的，所以每行带的是窗口相对的
    // 逻辑坐标。
    let (win_origin, win_scale) = {
        let origin = root
            .get_bounding_rectangle()
            .map(|r| (r.get_left(), r.get_top()))
            .unwrap_or((0, 0));
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let scale = if dpi > 0 { dpi as f64 / 96.0 } else { 1.0 };
        (origin, if scale.is_finite() && scale > 0.0 { scale } else { 1.0 })
    };
    let mut snapshot = walk_snapshot(&walker, &root, started, win_origin, win_scale);

    // wake 路径只在第一次走树的结果像“合成桩”时才进。正常走通的窗口直接从
    // build_tree 返回，这里一点都不碰：没有 advise、不泵消息、不起线程、不多等
    // 一分。以前 wake_provider 是不管不顾在第一次走树*之前*跑的，结果每个新窗口
    // 都白付 60 ms 的消息泵。
    //
    // 顺序有讲究：完整性检查就是一次 token 查询，几微秒就能答，而且它直接把桩
    // 解释清楚了，所以放最前面，先把明知不会成的 wake 挡掉。wake 本身还要手动开
    // （`SKY_WAKE=1`），见 `wake_enabled`。
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
        // Low IL 下显式给了 SKY_WAKE=1 照样跑：诊断归诊断，不能拿它当借口把
        // 实验藏起来。
        if wake_enabled() && run_wake_attempt(hwnd, started) {
            let next = walk_snapshot(&walker, &root, started, win_origin, win_scale);
            // 长到不再变为止：wake 真成功之后，反复重走，直到树过了桩、并且
            // 不再变大。要手动开（SKY_WAKE_RETRY=1），仍然受 TREE_BUDGET_MS 约束。
            //
            // 之所以留着手动开：这台机器上没测到过 wake 成功。开着重试循环，
            // Chrome 第一个请求是 4 行/3 个节点、耗时 1653 ms；关掉还是 4 行/3 个
            // 节点，只要 89 ms。而同一次运行里原版客户端是 1438 个节点、约 530 ms。
            // 等哪天 wake 真成了，这个循环形状是对的；没验证之前不能每个请求都
            // 花掉 1.5 s。
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
                    let grown = walk_snapshot(&walker, &root, started, win_origin, win_scale);
                    let n = grown.elements.len();
                    snapshot = grown;
                    if n > STUB_MAX && n == last {
                        break; // 长起来了而且稳住了
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

    // 焦点元素：在这棵子树里，就把它在树里的那一行回显出去。
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
    }).and_then(|i| out_lines.get(i + 1).cloned()); // +1 是跳过 header 行

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
    win_origin: (i32, i32),
    win_scale: f64,
) -> Snapshot {
    let mut elements: Vec<ElementRecord> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut last_document: Option<String> = None;
    walk(
        walker,
        root,
        0,
        started,
        win_origin,
        win_scale,
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
    win_origin: (i32, i32),
    win_scale: f64,
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

    // ValuePattern 的两样东西：值字符串和能不能改的标记。
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

    // 按原版的字段顺序拼。
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
    // 窗口相对的逻辑坐标。模型在窗口截图上量坐标、想直接点而不瞎猜，就靠这个。
    if rect.2 > 0 && rect.3 > 0 {
        let s = win_scale;
        let lx = ((rect.0 - win_origin.0) as f64 / s).round() as i32;
        let ly = ((rect.1 - win_origin.1) as f64 / s).round() as i32;
        let lw = (rect.2 as f64 / s).round() as i32;
        let lh = (rect.3 as f64 / s).round() as i32;
        label.push_str(&format!(" @{lx},{ly} {lw}x{lh}"));
    }
    if control_type == "Window" {
        label.push_str(" Secondary Actions: Raise");
    }
    lines.push(label);

    // 递归走子节点，disabled/offscreen 的子树也走（原版会渲染它们）。没子节点
    // 或者 provider 有断档时表现为 Err（null + S_OK），跟以前一样，这一层兄弟
    // 节点的遍历就到此为止。
    let mut child = match walker.get_first_child(node) {
        Ok(c) => Some(c),
        Err(e) => {
            dbg_log(&format!("[first-child-err] depth={depth} ct={control_type} {e}"));
            None
        }
    };
    while let Some(c) = child {
        walk(walker, &c, depth + 1, started, win_origin, win_scale, elements, lines, last_document);
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
