// 用 Windows.Graphics.Capture（WGC）抓单个窗口，遮不遮挡都拍得到，跟原版
// helper 一样；JPEG 走 WIC 编码。
//
// 下面几条是拿原版 codex-computer-use.exe 实测出来的：
//   * 原版给每个窗口缓存一个 capture session（它二进制里有
//     `CachedGraphicsCaptureSession`、`Direct3D11CaptureFramePool.Recreate`
//     和一个 `FrameArrived` 处理），不是每次调用都新建 pool。为什么？调用
//     前几秒才建好的 pool，只有目标碰巧重绘时才收得到帧。所以刚拉起来的
//     engine 抓后台窗口返回 `screenshots: []`，而插件这个活得久的 engine 能
//     抓到一帧。这里也照这个方式缓存 session。
//   * 原版报的窗口矩形是*逻辑*px，JPEG 的像素尺寸跟它分毫不差（最大化
//     Chrome：3840x2160、192 DPI 的桌面上，原点 (0,0)，尺寸 1920x1032，
//     JPEG SOF 里也是 1920x1032）。WGC 给的是物理像素，所以编码前要按窗口
//     的 DPI 比例缩一下。
//   * 窗口 item 一帧都拿不到时，原版会换一个能抓的表面（它字符串里有
//     `no desktop window`、`another window` 和
//     `IGraphicsCaptureItemInterop.CreateForMonitor`）。这里拿桌面窗口当最后
//     的退路，按目标矩形裁出来。

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice as WinrtDevice;
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{
    Common::DXGI_FORMAT_B8G8R8A8_UNORM, IDXGIDevice,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, IWICImagingFactory, IWICStream,
};
use windows::Win32::System::Com::CLSCTX_INPROC_SERVER;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::RoGetActivationFactory;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetDesktopWindow, GetWindowRect, IsWindowVisible,
};

/// 全新的 session 等第一帧最多等这么久。原版冷启动连拍三张要 2.5-6.5 s，
/// 所以花几秒也在预算内。
const FIRST_FRAME_BUDGET: Duration = Duration::from_millis(1200);
/// kick 一下之后再看一眼窗口，然后才放弃这个窗口 item。
const KICK_BUDGET: Duration = Duration::from_millis(900);
/// 缓存的帧老到这个程度，就值得再短轮询一次，看能不能拿到更新的。
const STALE_AFTER: Duration = Duration::from_millis(250);
/// 帧新到这个程度，直接拿去用，不再等。
const FRESH_ENOUGH: Duration = Duration::from_millis(60);
/// 一张截图从头到尾花的时间不许超过这个数（调用方的
/// `x-oai-cua-request-budget-ms` 通常是 19 s）。
const TOTAL_BUDGET: Duration = Duration::from_millis(6500);
const MAX_CACHED_SESSIONS: usize = 24;

thread_local! {
    static DEVICE: RefCell<Option<(ID3D11Device, WinrtDevice)>> = const { RefCell::new(None) };
    /// hwnd -> 还活着的 capture session（对应原版的 CachedGraphicsCaptureSession）。
    static SESSIONS: RefCell<HashMap<isize, Cached>> = RefCell::new(HashMap::new());
}

/// 靠环境变量（`SKY_CAPTURE_TRACE=1`）才开的 stderr 诊断。默认关着，不然
/// 会弄脏 wire 协议和插件 stderr 的尾巴。
fn trace_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("SKY_CAPTURE_TRACE")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false)
    })
}

macro_rules! tr {
    ($($a:tt)*) => {
        if crate::win::capture::trace_on() {
            eprintln!("[capture] {}", format!($($a)*));
        }
    };
}

/// 一帧解码好的画面，已经从 GPU 拷到 CPU 内存，pool 没了它也还在。
#[derive(Clone)]
struct Captured {
    w: usize,
    h: usize,
    row_pitch: u32,
    px: Vec<u8>,
}

struct Cached {
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    size: (i32, i32),
    last: Option<Captured>,
    last_at: Instant,
}

/// 拿到共享的 D3D11 device 和它的 WinRT 包装，没有就建一个。
fn ensure_device() -> windows::core::Result<(ID3D11Device, WinrtDevice)> {
    if let Some(d) = DEVICE.with(|d| d.borrow().clone()) {
        return Ok(d);
    }
    let mut device: Option<ID3D11Device> = None;
    let mut ctx: Option<windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            None,
            windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
    }
    let device = device.ok_or_else(windows::core::Error::from_win32)?;
    let dxgi: IDXGIDevice = device.cast()?;
    let inspectable = unsafe {
        windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?
    };
    let winrt: WinrtDevice = inspectable.cast()?;
    let pair = (device.clone(), winrt);
    DEVICE.with(|d| *d.borrow_mut() = Some(pair.clone()));
    Ok(pair)
}

/// WinRT 初始化，加上原版会做的 `GraphicsCaptureAccess.RequestAccessAsync`
/// 握手（它二进制里有 `Windows.Graphics.Capture.GraphicsCaptureAccess`、
/// `GraphicsCaptureAccessKind`、`graphicsCaptureWithoutBorder`）。少了这一步，
/// 抓打包应用（AppContainer，比如记事本和画图）时
/// `IGraphicsCaptureItemInterop::CreateForWindow` 会报
/// `E_ACCESSDENIED (0x80070005)`，结果这些窗口一张截图都出不来。幂等，一个
/// 进程只跑一次。
fn ensure_capture_consent() {
    static DONE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    DONE.get_or_init(|| {
        unsafe {
            // WinRT 激活要求这个线程先 RoInitialize；单靠 COM MTA 对 capture
            // 那些工厂不一定够。
            let hr = windows::Win32::System::WinRT::RoInitialize(
                windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
            );
            tr!("RoInitialize -> {hr:?}");
            for kind in [
                windows::Graphics::Capture::GraphicsCaptureAccessKind::Borderless,
                windows::Graphics::Capture::GraphicsCaptureAccessKind::Programmatic,
            ] {
                match windows::Graphics::Capture::GraphicsCaptureAccess::RequestAccessAsync(kind) {
                    Ok(op) => match op.get() {
                        Ok(status) => tr!("RequestAccessAsync({kind:?}) -> {status:?}"),
                        Err(e) => tr!("RequestAccessAsync({kind:?}) get failed: {e}"),
                    },
                    Err(e) => tr!("RequestAccessAsync({kind:?}) failed: {e}"),
                }
            }
        }
    });
}

fn create_item(hwnd: HWND) -> windows::core::Result<GraphicsCaptureItem> {
    unsafe {
        let class: windows::core::HSTRING = "Windows.Graphics.Capture.GraphicsCaptureItem".into();
        let interop: IGraphicsCaptureItemInterop = match RoGetActivationFactory(&class) {
            Ok(f) => f,
            Err(e) => {
                tr!("RoGetActivationFactory(GraphicsCaptureItem) failed: {e:?}");
                return Err(e);
            }
        };
        interop.CreateForWindow(hwnd)
    }
}

fn unwrap_surface(
    surface: &windows::Graphics::DirectX::Direct3D11::IDirect3DSurface,
) -> windows::core::Result<ID3D11Texture2D> {
    unsafe {
        let access: windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess =
            surface.cast()?;
        access.GetInterface()
    }
}

/// 把一帧的 BGRA 数据拷进 CPU 内存。
fn copy_frame(device: &ID3D11Device, frame: &windows::Graphics::Capture::Direct3D11CaptureFrame)
    -> Result<Captured, String> {
    let surface = frame.Surface().map_err(|e| format!("frame surface: {e}"))?;
    let src = unwrap_surface(&surface).map_err(|e| format!("unwrap surface: {e}"))?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { src.GetDesc(&mut desc) };
    if desc.Width == 0 || desc.Height == 0 {
        return Err("frame has no size".into());
    }
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: desc.Width,
        Height: desc.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: desc.SampleDesc,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let ctx: windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext =
        unsafe { device.GetImmediateContext() }.map_err(|e| format!("ctx: {e}"))?;
    let staging: ID3D11Texture2D = unsafe {
        let mut t: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&staging_desc, None, Some(&mut t))
            .map_err(|e| format!("staging: {e}"))?;
        t.ok_or_else(|| "staging missing".to_string())?
    };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    let out = unsafe {
        ctx.CopyResource(&staging, &src);
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("map: {e}"))?;
        let px = std::slice::from_raw_parts(
            mapped.pData as *const u8,
            (desc.Height * mapped.RowPitch) as usize,
        )
        .to_vec();
        ctx.Unmap(&staging, 0);
        Captured {
            w: desc.Width as usize,
            h: desc.Height as usize,
            row_pitch: mapped.RowPitch,
            px,
        }
    };
    Ok(out)
}

/// 给 `item` 建 pool 和 session，然后开抓。
fn start_session(
    winrt_device: &WinrtDevice,
    item: &GraphicsCaptureItem,
    size: windows::Graphics::SizeInt32,
) -> Result<(Direct3D11CaptureFramePool, GraphicsCaptureSession), String> {
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )
    .map_err(|e| format!("frame pool: {e}"))?;
    let session: GraphicsCaptureSession = pool
        .CreateCaptureSession(item)
        .map_err(|e| format!("capture session: {e}"))?;
    session
        .StartCapture()
        .map_err(|e| format!("start capture: {e}"))?;
    Ok((pool, session))
}

/// 把 pool 里排着的帧全取出来，只留最新那张。
fn drain(device: &ID3D11Device, pool: &Direct3D11CaptureFramePool) -> Option<Captured> {
    let mut newest: Option<Captured> = None;
    for _ in 0..4 {
        match pool.TryGetNextFrame() {
            Ok(frame) => {
                if let Ok(c) = copy_frame(device, &frame) {
                    newest = Some(c);
                }
            }
            Err(_) => break,
        }
    }
    newest
}

/// 在 `budget` 这段时间里反复问 pool，返回看到的最新一帧。
fn poll(device: &ID3D11Device, pool: &Direct3D11CaptureFramePool, budget: Duration) -> Option<Captured> {
    let deadline = Instant::now() + budget;
    let mut best: Option<Captured> = None;
    loop {
        if let Some(c) = drain(device, pool) {
            best = Some(c);
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    best
}

/// 拿 `key` 的一帧：有缓存的 session 就复用，没有就新建。
/// `need_new` 为真时至少轮询一次，要的是这次调用自己产出的帧。
fn session_frame(
    key: isize,
    hwnd: HWND,
    budget: Duration,
    need_new: bool,
) -> Result<Captured, String> {
    let (device, winrt_device) = ensure_device().map_err(|e| format!("d3d device: {e}"))?;
    ensure_capture_consent();
    let deadline = Instant::now() + budget;

    let mut entry = SESSIONS.with(|m| m.borrow_mut().remove(&key));
    if entry.is_none() {
        let item = create_item(hwnd).map_err(|e| format!("capture item: {e}"))?;
        let size = item.Size().map_err(|e| format!("item size: {e}"))?;
        if size.Width <= 0 || size.Height <= 0 {
            return Err("window has no capturable size".into());
        }
        let (pool, session) = start_session(&winrt_device, &item, size)?;
        tr!("new session hwnd={key} item={}", format!("{}x{}", size.Width, size.Height));
        entry = Some(Cached {
            item,
            pool,
            session,
            size: (size.Width, size.Height),
            last: None,
            last_at: Instant::now(),
        });
    }
    let mut entry = entry.ok_or("no capture session")?;

    // item 的尺寸跟着窗口走；尺寸一变就重建 pool（原版为此调
    // Direct3D11CaptureFramePool.Recreate）。
    if let Ok(size) = entry.item.Size() {
        if size.Width > 0 && size.Height > 0 && (size.Width, size.Height) != entry.size {
            tr!("recreate pool hwnd={key} {:?} -> {:?}", entry.size, (size.Width, size.Height));
            match Direct3D11CaptureFramePool::CreateFreeThreaded(
                &winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                size,
            ) {
                Ok(pool) => {
                    let session = pool
                        .CreateCaptureSession(&entry.item)
                        .map_err(|e| format!("capture session: {e}"))?;
                    session.StartCapture().map_err(|e| format!("start capture: {e}"))?;
                    entry.pool = pool;
                    entry.session = session;
                    entry.size = (size.Width, size.Height);
                }
                Err(e) => tr!("recreate failed hwnd={key}: {e}"),
            }
        }
    }

    // 1. pool 里已经攒着帧了吗？
    let mut frame = drain(&device, &entry.pool);
    if frame.is_some() {
        entry.last = frame.clone();
        entry.last_at = Instant::now();
    }

    // 2. 帧比 FRESH_ENOUGH 还新，那就是答案。
    let fresh = frame.is_some() && entry.last_at.elapsed() < FRESH_ENOUGH;
    if frame.is_none() {
        frame = entry.last.clone();
    }
    if !fresh {
        let have = frame.is_some();
        // 从来没拿到过帧就多等一会；已经有帧了，就只短暂等一下新的。
        let wait = if have {
            STALE_AFTER.min(budget)
        } else {
            FIRST_FRAME_BUDGET.min(budget)
        };
        if let Some(f) = poll(&device, &entry.pool, wait) {
            frame = Some(f.clone());
            entry.last = Some(f);
            entry.last_at = Instant::now();
        }
        // 3. 还是一帧都没有：重建 session（目标闲着的时候建的 session，可能
        // 压根就没人给它发帧），再试一次，还不行才退到别的表面。
        if frame.is_none() && !have && Instant::now() < deadline {
            tr!("kick session hwnd={key}", );
            let _ = entry.session.Close();
            if let Ok(size) = entry.item.Size() {
                if let Ok((pool, session)) = start_session(&winrt_device, &entry.item, size) {
                    entry.pool = pool;
                    entry.session = session;
                    entry.size = (size.Width, size.Height);
                    let left = (deadline - Instant::now()).min(KICK_BUDGET);
                    if let Some(f) = poll(&device, &entry.pool, left) {
                        frame = Some(f.clone());
                        entry.last = Some(f);
                        entry.last_at = Instant::now();
                    }
                }
            }
        }
    }
    if frame.is_none() && need_new {
        SESSIONS.with(|m| m.borrow_mut().insert(key, entry));
        return Err("no frame from capture session".into());
    }
    let frame = frame.ok_or("no frame from capture session")?;
    SESSIONS.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= MAX_CACHED_SESSIONS {
            // 扔掉一个差不多最老的；session 重建开销很小。
            if let Some(k) = m.keys().next().copied() {
                if let Some(old) = m.remove(&k) {
                    let _ = old.session.Close();
                }
            }
        }
        m.insert(key, entry);
    });
    Ok(frame)
}

/// 把抓到的帧裁出一块（桌面兜底那条路会用到）。
fn crop(src: &Captured, x: usize, y: usize, w: usize, h: usize) -> Result<Captured, String> {
    let x = x.min(src.w.saturating_sub(1));
    let y = y.min(src.h.saturating_sub(1));
    let w = w.min(src.w - x).max(1);
    let h = h.min(src.h - y).max(1);
    let mut px = Vec::with_capacity(w * 4 * h);
    for row in 0..h {
        let from = (y + row) * src.row_pitch as usize + x * 4;
        let to = from + w * 4;
        if to > src.px.len() {
            return Err("crop out of range".into());
        }
        px.extend_from_slice(&src.px[from..to]);
    }
    Ok(Captured { w, h, row_pitch: (w * 4) as u32, px })
}

/// `hwnd` 在屏幕上的矩形，用原版那套*逻辑*px 表示。
///
/// 麻烦在两点。一是原版量的是 DWM 的扩展边框（它字符串里是
/// `DwmGetWindowAttribute`），不是裸的窗口矩形。二是 `GetWindowRect` 返回的
/// 到底是物理像素还是按 96 dpi 虚拟化过的像素，取决于进程的 DPI 感知设置，
/// 而这事从这里看不出来。所以比例只能拿矩形去比 WGC item 的尺寸反推：
/// `rect * scale == item` 就说明这个矩形本来就是逻辑像素。
fn logical_rect(hwnd: HWND, scale: f64, item: (usize, usize)) -> (i32, i32, usize, usize) {
    let mut rect = RECT::default();
    let mut got_extended = false;
    unsafe {
        use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
        if DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut rect as *mut RECT as *mut _,
            core::mem::size_of::<RECT>() as u32).is_ok()
            && rect.right > rect.left && rect.bottom > rect.top
        {
            got_extended = true;
        }
    }
    if !got_extended {
        unsafe {
            let _ = GetWindowRect(hwnd, &mut rect);
        }
    }
    let rw = (rect.right - rect.left).max(1);
    let rh = (rect.bottom - rect.top).max(1);
    // item 正好是矩形的 `scale` 倍，说明这个矩形已经是虚拟化过的逻辑像素。
    let logical_already = (rw as f64 * scale - item.0 as f64).abs()
        .min((rh as f64 * scale - item.1 as f64).abs())
        < (rw as f64 - item.0 as f64).abs().min((rh as f64 - item.1 as f64).abs());
    if logical_already {
        (rect.left, rect.top, rw as usize, rh as usize)
    } else {
        (
            ((rect.left as f64) / scale).round() as i32,
            ((rect.top as f64) / scale).round() as i32,
            ((rw as f64) / scale).round().max(1.0) as usize,
            ((rh as f64) / scale).round().max(1.0) as usize,
        )
    }
}

/// 给模型看的 snapshot 用的抓图选项。
///
/// `scale` 是在逻辑窗口像素之上再乘一次（1.0 = 原版契约，0.5 = 出半尺寸
/// JPEG，token 吃紧的回合用）。`region` 按逻辑窗口相对 px 裁窗口，模型想
/// “只看这一块”就靠它，不用为整个窗口付 token。
#[derive(Clone, Copy)]
pub struct CaptureOpts {
    pub scale: f64,
    pub region: Option<(i32, i32, i32, i32)>,
}

impl Default for CaptureOpts {
    fn default() -> Self {
        CaptureOpts { scale: 1.0, region: None }
    }
}

/// 一张编码好的截图：字节本身，外加坐标工具把这张 JPEG 上量到的像素换回
/// 屏幕位置要用的几何信息。
pub struct CapturedShot {
    pub jpeg: Vec<u8>,
    /// JPEG 的像素尺寸（已经缩放/裁剪过）。
    pub width: usize,
    pub height: usize,
    /// 抓到的区域在屏幕上的原点，逻辑 px（就是 `originX/Y`）。
    pub origin_x: i32,
    pub origin_y: i32,
    /// 抓的这一块在窗口内部的偏移，逻辑 px。
    pub offset_x: i32,
    pub offset_y: i32,
    /// 一个逻辑窗口 px 折几个 JPEG px（没传 `scale` 时就是 1.0）。
    pub scale: f64,
}

/// 抓 `hwnd` 的一帧，返回 (jpeg 字节, 宽, 高)。
///
/// 宽高是原版报的那套*逻辑*px（也是 JPEG 自己的像素尺寸），也就是物理 WGC
/// 帧除以窗口的 DPI 比例。
pub fn capture_window_jpeg(hwnd: HWND) -> Result<(Vec<u8>, usize, usize), String> {
    let shot = capture_window_shot(hwnd, CaptureOpts::default())?;
    Ok((shot.jpeg, shot.width, shot.height))
}

/// 跟 [`capture_window_jpeg`] 一样，另外带上抓的那块区域的原点，用的还是
/// 原版在 `originX`/`originY` 里报的逻辑 px。
#[allow(dead_code)] // 留给还想要元组形式的调用方
pub fn capture_window_jpeg_ex(hwnd: HWND) -> Result<(Vec<u8>, usize, usize, i32, i32), String> {
    let shot = capture_window_shot(hwnd, CaptureOpts::default())?;
    Ok((shot.jpeg, shot.width, shot.height, shot.origin_x, shot.origin_y))
}

/// 按 [`CaptureOpts`] 抓 `hwnd`：可选先按逻辑 px 裁一刀，裁完再降采样，同时
/// 给出把这两步都还原回去所需的几何信息。
pub fn capture_window_shot(hwnd: HWND, opts: CaptureOpts) -> Result<CapturedShot, String> {
    let started = Instant::now();
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect).map_err(|e| format!("window rect: {e}"))? };
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let scale = if dpi > 0 { dpi as f64 / 96.0 } else { 1.0 };
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };

    let key = hwnd.0 as isize;
    tr!(
        "enter hwnd={key} GetWindowRect=({},{})-({},{}) dpi={dpi} scale={scale}",
        rect.left, rect.top, rect.right, rect.bottom
    );
    {
        let mut ext = RECT::default();
        use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
        let hr = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut ext as *mut RECT as *mut _,
                core::mem::size_of::<RECT>() as u32,
            )
        };
        tr!(
            "  extended_frame_bounds=({},{})-({},{}) hr={hr:?}",
            ext.left, ext.top, ext.right, ext.bottom
        );
    }
    let frame = match session_frame(key, hwnd, FIRST_FRAME_BUDGET + KICK_BUDGET, true) {
        Ok(f) => f,
        Err(e) => {
            // 最后的退路：抓整个桌面窗口，再按目标矩形裁。这跟原版
            // "no desktop window" 那条兜底路径对得上；窗口 item 一直不画，
            // 这样也还能拿到像素。
            tr!("window capture failed hwnd={key}: {e}; trying desktop fallback");
            let desktop = unsafe { GetDesktopWindow() };
            if !unsafe { IsWindowVisible(hwnd).as_bool() } {
                return Err(format!("{e} (window not visible)"));
            }
            let dkey = -(desktop.0 as isize) - 1;
            let left = TOTAL_BUDGET.saturating_sub(started.elapsed()).max(Duration::from_millis(200));
            let full = match session_frame(dkey, desktop, left, false) {
                Ok(f) => f,
                Err(e2) => {
                    tr!("desktop fallback failed: {e2}");
                    return Err(format!("{e}; desktop fallback: {e2}"));
                }
            };
            let x = (rect.left.max(0)) as usize;
            let y = (rect.top.max(0)) as usize;
            crop(&full, x, y, (rect.right - rect.left).max(1) as usize, (rect.bottom - rect.top).max(1) as usize)?
        }
    };
    if frame.w < 2 || frame.h < 2 {
        return Err("captured frame too small".into());
    }

    // 原版报的、发的都是逻辑像素：物理帧除以 DPI 比例。
    let logical_w = ((frame.w as f64) / scale).round().max(1.0) as usize;
    let logical_h = ((frame.h as f64) / scale).round().max(1.0) as usize;

    // 可选按逻辑 px 裁，范围不能超出窗口。
    let (rx, ry, rw, rh) = match opts.region {
        None => (0, 0, logical_w, logical_h),
        Some((x, y, w, h)) => {
            let rw = (w.max(1) as usize).min(logical_w);
            let rh = (h.max(1) as usize).min(logical_h);
            let rx = (x.max(0) as usize).min(logical_w.saturating_sub(1));
            let ry = (y.max(0) as usize).min(logical_h.saturating_sub(1));
            (rx, ry, rw.min(logical_w - rx), rh.min(logical_h - ry))
        }
    };
    let out_scale = if opts.scale.is_finite() && opts.scale > 0.0 {
        opts.scale.clamp(0.05, 1.0)
    } else {
        1.0
    };
    let target_w = ((rw as f64) * out_scale).round().max(1.0) as usize;
    let target_h = ((rh as f64) * out_scale).round().max(1.0) as usize;

    // 在物理帧上裁，这样降采样只用走一趟 WIC；裁的位置是请求的逻辑区域乘
    // DPI 比例。
    let px = ((rx as f64) * scale).round() as usize;
    let py = ((ry as f64) * scale).round() as usize;
    let pw = (((rw as f64) * scale).round() as usize).max(1);
    let ph = (((rh as f64) * scale).round() as usize).max(1);
    let cropped = crop(&frame, px, py, pw, ph)?;
    let jpeg = encode_jpeg(&cropped, target_w, target_h)?;
    let mean = frame.px.iter().step_by(4 * 997).map(|&b| b as u64).sum::<u64>()
        / (frame.px.len() / (4 * 997)).max(1) as u64;
    tr!("  frame mean-blue={mean} (0 => black/undrawn surface)");
    let (origin_x, origin_y, _, _) = logical_rect(hwnd, scale, (frame.w, frame.h));
    tr!(
        "captured hwnd={key} {}x{} -> crop=({rx},{ry},{rw},{rh}) -> {target_w}x{target_h} @({origin_x},{origin_y}) scale={out_scale} jpeg={}B in {}ms",
        frame.w,
        frame.h,
        jpeg.len(),
        started.elapsed().as_millis()
    );
    Ok(CapturedShot {
        jpeg,
        width: target_w,
        height: target_h,
        origin_x: origin_x + rx as i32,
        origin_y: origin_y + ry as i32,
        offset_x: rx as i32,
        offset_y: ry as i32,
        scale: out_scale,
    })
}

/// 把 BGRA 行数据经 WIC 转成 JPEG，缩到报出去的那个逻辑尺寸。
fn encode_jpeg(frame: &Captured, target_w: usize, target_h: usize) -> Result<Vec<u8>, String> {
    let (width, height) = (frame.w, frame.h);
    let row_pitch = frame.row_pitch as usize;
    let bgra = &frame.px;
    unsafe {
        let factory: IWICImagingFactory = windows::Win32::System::Com::CoCreateInstance(
            &CLSID_WICImagingFactory,
            None,
            CLSCTX_INPROC_SERVER,
        )
        .map_err(|e| format!("wic factory: {e}"))?;
        let bitmap = factory
            .CreateBitmap(
                width as u32,
                height as u32,
                &windows::Win32::Graphics::Imaging::GUID_WICPixelFormat32bppBGRA,
                windows::Win32::Graphics::Imaging::WICBitmapCacheOnLoad,
            )
            .map_err(|e| format!("wic bitmap: {e}"))?;
        // 锁住之后一行一行拷（row pitch 可能比 width*4 大）。
        {
            use windows::Win32::Graphics::Imaging::{IWICBitmapLock, WICBitmapLockWrite};
            let rect = windows::Win32::Graphics::Imaging::WICRect {
                X: 0,
                Y: 0,
                Width: width as i32,
                Height: height as i32,
            };
            let lock: IWICBitmapLock = bitmap
                .Lock(&rect, WICBitmapLockWrite.0 as u32)
                .map_err(|e| format!("lock: {e}"))?;
            let lock_pitch = match lock.GetStride() {
                Ok(s) if (s as usize) >= width * 4 => s as usize,
                _ => row_pitch,
            };
            let mut ptr: *mut u8 = core::ptr::null_mut();
            let mut cbuf: u32 = 0;
            lock.GetDataPointer(&mut cbuf, &mut ptr)
                .map_err(|e| format!("data: {e}"))?;
            let row_bytes = width * 4;
            if ptr.is_null() || cbuf < (lock_pitch * height) as u32 {
                return Err("bitmap lock too small".into());
            }
            for y in 0..height {
                let src = &bgra[y * row_pitch..y * row_pitch + row_bytes];
                std::ptr::copy_nonoverlapping(src.as_ptr(), ptr.add(y * lock_pitch), row_bytes);
            }
        }
        // 把物理抓图的像素缩成 helper 报的逻辑像素。不然 200% 缩放的屏幕上
        // 出的包比原版大 4 倍，缩完才跟原版一样是 ~170 KB。
        let source: windows::Win32::Graphics::Imaging::IWICBitmapSource = if (width, height)
            != (target_w, target_h)
        {
            let scaler: windows::Win32::Graphics::Imaging::IWICBitmapScaler = factory
                .CreateBitmapScaler()
                .map_err(|e| format!("scaler: {e}"))?;
            scaler
                .Initialize(
                    &bitmap,
                    target_w as u32,
                    target_h as u32,
                    windows::Win32::Graphics::Imaging::WICBitmapInterpolationModeFant,
                )
                .map_err(|e| format!("scaler init: {e}"))?;
            scaler.cast().map_err(|e| format!("scaler cast: {e}"))?
        } else {
            bitmap.cast().map_err(|e| format!("bitmap cast: {e}"))?
        };

        let stream: IWICStream = factory.CreateStream().map_err(|e| format!("stream: {e}"))?;
        let mut buf: Vec<u8> = vec![0u8; 32 * 1024 * 1024];
        stream
            .InitializeFromMemory(&mut buf)
            .map_err(|e| format!("stream init: {e}"))?;
        let encoder = factory
            .CreateEncoder(
                &windows::Win32::Graphics::Imaging::GUID_ContainerFormatJpeg,
                core::ptr::null(),
            )
            .map_err(|e| format!("encoder: {e}"))?;
        encoder
            .Initialize(&stream, windows::Win32::Graphics::Imaging::WICBitmapEncoderNoCache)
            .map_err(|e| format!("enc init: {e}"))?;
        let mut frame_opt: Option<windows::Win32::Graphics::Imaging::IWICBitmapFrameEncode> = None;
        encoder
            .CreateNewFrame(&mut frame_opt, core::ptr::null_mut())
            .map_err(|e| format!("frame: {e}"))?;
        let frameenc = frame_opt.ok_or("no frame")?;
        frameenc
            .Initialize(None)
            .map_err(|e| format!("frame init: {e}"))?;
        frameenc
            .SetSize(target_w as u32, target_h as u32)
            .map_err(|e| format!("set size: {e}"))?;
        let mut pf = windows::Win32::Graphics::Imaging::GUID_WICPixelFormat24bppBGR;
        frameenc
            .SetPixelFormat(&mut pf)
            .map_err(|e| format!("pixel format: {e}"))?;
        frameenc
            .WriteSource(&source, core::ptr::null())
            .map_err(|e| format!("write: {e}"))?;
        frameenc.Commit().map_err(|e| format!("commit: {e}"))?;
        encoder.Commit().map_err(|e| format!("enc commit: {e}"))?;
        // 用 InitializeFromMemory 建出来的流，`IWICStream::Stat` 报的是*缓冲
        // 区*长度，不是编码后的长度。信了它就会把整个 32 MiB 草稿区全返回
        // 出去（真 JPEG 后面跟着一堆 0），拼成 44,739,244 字符的 data URL，
        // 结果 Harness 看图那边报
        // `image/jpeg; Maximum call stack size exceeded`。所以长度只能从数据
        // 本身推：JPEG 结尾一定是 FFD9，最后一个非零字节之后的全是填充。
        let last = buf.iter().rposition(|b| *b != 0).map(|i| i + 1).unwrap_or(0);
        if last < 2 || buf[last - 2..last] != [0xff, 0xd9] {
            let mut stat = windows::Win32::System::Com::STATSTG::default();
            stream
                .Stat(&mut stat, windows::Win32::System::Com::STATFLAG(1))
                .map_err(|e| format!("stat: {e}"))?;
            if stat.cbSize as usize >= 2 && stat.cbSize as usize <= buf.len() {
                buf.truncate(stat.cbSize as usize);
            } else {
                return Err(format!("jpeg length undetermined (cbSize={})", stat.cbSize));
            }
        } else {
            buf.truncate(last);
        }
        if buf.len() < 2 || buf[0..2] != [0xff, 0xd8] {
            return Err("encoder produced no JPEG".into());
        }
        Ok(buf)
    }
}
