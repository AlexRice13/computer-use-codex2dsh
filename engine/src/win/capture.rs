// Per-window capture with Windows.Graphics.Capture (WGC), matching the
// original helper's occlusion-free screenshots, plus WIC JPEG encoding.
//
// Behavioural notes (measured against the original codex-computer-use.exe):
//   * The original caches one capture session per window (its binary carries
//     `CachedGraphicsCaptureSession`, `Direct3D11CaptureFramePool.Recreate`
//     and a `FrameArrived` handler) instead of building a pool per call. A
//     pool built seconds before the first read only receives a frame when the
//     target happens to repaint, which is why a freshly spawned engine
//     returned `screenshots: []` for a background window while the plugin's
//     long-lived engine returned one. Sessions are cached here the same way.
//   * The original reports the window rect in *logical* px and its JPEG has
//     exactly those pixel dimensions (maximised Chrome: origin (0,0)
//     1920x1032 on a 3840x2160 desktop at 192 DPI, JPEG SOF dims 1920x1032).
//     WGC hands out physical pixels, so the frame is scaled by the window's
//     DPI factor before encoding.
//   * When the window item yields no frame at all, the original falls back to
//     another capturable surface (its strings carry `no desktop window`,
//     `another window` and `IGraphicsCaptureItemInterop.CreateForMonitor`).
//     The desktop window is used here as that last-resort surface, cropped to
//     the target rect.

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

/// How long to wait for a first frame from a brand-new session. The original
/// needs 2.5-6.5 s for a cold three-screenshot call, so seconds are on budget.
const FIRST_FRAME_BUDGET: Duration = Duration::from_millis(1200);
/// Re-check window after a kick before giving up on the window item.
const KICK_BUDGET: Duration = Duration::from_millis(900);
/// A cached frame this old is worth one short poll for a fresher one.
const STALE_AFTER: Duration = Duration::from_millis(250);
/// A frame this young is used as-is.
const FRESH_ENOUGH: Duration = Duration::from_millis(60);
/// Never spend longer than this on one screenshot overall (the caller's
/// `x-oai-cua-request-budget-ms` is normally 19 s).
const TOTAL_BUDGET: Duration = Duration::from_millis(6500);
const MAX_CACHED_SESSIONS: usize = 24;

thread_local! {
    static DEVICE: RefCell<Option<(ID3D11Device, WinrtDevice)>> = const { RefCell::new(None) };
    /// hwnd -> live capture session (the original's CachedGraphicsCaptureSession).
    static SESSIONS: RefCell<HashMap<isize, Cached>> = RefCell::new(HashMap::new());
}

/// Env-gated (`SKY_CAPTURE_TRACE=1`) diagnostics on stderr. Off by default so
/// the wire contract and the plugin's stderr tail stay untouched.
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

/// One decoded frame, copied out of the GPU so it survives the pool.
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

/// Ensure the shared D3D11 device and its WinRT wrapper exist.
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

/// WinRT init + the `GraphicsCaptureAccess.RequestAccessAsync` handshake the
/// original performs (`Windows.Graphics.Capture.GraphicsCaptureAccess`,
/// `GraphicsCaptureAccessKind`, `graphicsCaptureWithoutBorder` in its binary).
/// Without it `IGraphicsCaptureItemInterop::CreateForWindow` fails with
/// `E_ACCESSDENIED (0x80070005)` for packaged (AppContainer) targets such as
/// Notepad and Paint, which is why those windows produced no screenshot at
/// all. Idempotent; runs once per process.
fn ensure_capture_consent() {
    static DONE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    DONE.get_or_init(|| {
        unsafe {
            // WinRT activation wants RoInitialize on this thread; COM MTA alone
            // is not always enough for the capture factories.
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

/// Copy one frame's BGRA bits into CPU memory.
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

/// Build a pool + session for `item` and start it.
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

/// Drain every frame currently queued in the pool, keeping the newest.
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

/// Poll the pool for up to `budget`, returning the newest frame seen.
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

/// Get one frame for `key`, reusing (and creating as needed) a cached session.
/// `need_new` forces at least one poll for a frame produced by this call.
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

    // The item's size follows the window; rebuild the pool when it changed
    // (the original calls Direct3D11CaptureFramePool.Recreate for this).
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

    // 1. Anything already buffered in the pool?
    let mut frame = drain(&device, &entry.pool);
    if frame.is_some() {
        entry.last = frame.clone();
        entry.last_at = Instant::now();
    }

    // 2. A frame younger than FRESH_ENOUGH is the answer.
    let fresh = frame.is_some() && entry.last_at.elapsed() < FRESH_ENOUGH;
    if frame.is_none() {
        frame = entry.last.clone();
    }
    if !fresh {
        let have = frame.is_some();
        // First frame ever: wait. Otherwise take a fresh one only briefly.
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
        // 3. Still nothing at all: rebuild the session (a session created while
        // the target is idle may simply never be handed a frame) and try once
        // more before falling back to another surface.
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
            // Drop the oldest-ish entry; sessions are cheap to rebuild.
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

/// Crop a captured frame to a sub-rect (used by the desktop fallback).
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

/// Screen rect of `hwnd` in the same *logical* px the original reports.
///
/// Two things make this fiddly: the original measures the DWM extended frame
/// bounds (`DwmGetWindowAttribute` in its strings) rather than the raw window
/// rect, and whether `GetWindowRect` hands back physical or 96-dpi-virtualised
/// px depends on the process DPI awareness — which is not knowable from here.
/// The scale factor is therefore derived by comparing the rect against the
/// WGC item size: `rect * scale == item` means the rect is already logical.
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
    // Already-virtualised (logical) rect when the item is `scale` times bigger.
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

/// Capture one frame of `hwnd`; returns (jpeg bytes, width, height).
///
/// Width/height are the *logical* px the original reports (and the JPEG's own
/// pixel dimensions), i.e. the physical WGC frame divided by the window's DPI
/// scale.
pub fn capture_window_jpeg(hwnd: HWND) -> Result<(Vec<u8>, usize, usize), String> {
    let (jpeg, w, h, _, _) = capture_window_jpeg_ex(hwnd)?;
    Ok((jpeg, w, h))
}

/// As [`capture_window_jpeg`], plus the screen origin of the captured region
/// in the same logical px the original reports in `originX`/`originY`.
pub fn capture_window_jpeg_ex(hwnd: HWND) -> Result<(Vec<u8>, usize, usize, i32, i32), String> {
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
            // Last-resort surface: the desktop window cropped to the target
            // rect. Matches the original's "no desktop window" fallback path
            // and still yields pixels for a window whose item never paints.
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

    // The original reports/serves logical pixels: physical frame / DPI scale.
    let target_w = ((frame.w as f64) / scale).round().max(1.0) as usize;
    let target_h = ((frame.h as f64) / scale).round().max(1.0) as usize;
    let jpeg = encode_jpeg(&frame, target_w, target_h)?;
    let mean = frame.px.iter().step_by(4 * 997).map(|&b| b as u64).sum::<u64>()
        / (frame.px.len() / (4 * 997)).max(1) as u64;
    tr!("  frame mean-blue={mean} (0 => black/undrawn surface)");
    let (origin_x, origin_y, _, _) = logical_rect(hwnd, scale, (frame.w, frame.h));
    tr!(
        "captured hwnd={key} {}x{} -> {target_w}x{target_h} @({origin_x},{origin_y}) jpeg={}B in {}ms",
        frame.w,
        frame.h,
        jpeg.len(),
        started.elapsed().as_millis()
    );
    Ok((jpeg, target_w, target_h, origin_x, origin_y))
}

/// BGRA rows → JPEG via WIC, scaled to the reported logical size.
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
        // Lock and copy row by row (row pitch may exceed width*4).
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
        // Scale physical capture px → the logical px the helper reports, so a
        // 200% display produces the same ~170 KB payload as the original
        // instead of a 4x larger one.
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
        // `IWICStream::Stat` over an InitializeFromMemory buffer reports the
        // *buffer* length, not the encoded length — trusting it returned the
        // whole 32 MiB scratch buffer (a real JPEG plus zero padding), i.e. a
        // 44,739,244-char data URL, which is what the Harness viewer reported
        // as `image/jpeg; Maximum call stack size exceeded`. Derive the true
        // end from the data: a JPEG always terminates with FFD9, so everything
        // after the last non-zero byte is padding.
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
