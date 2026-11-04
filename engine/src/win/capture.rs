// Per-window capture with Windows.Graphics.Capture (WGC), matching the
// original helper's occlusion-free screenshots, plus WIC JPEG encoding.

use windows::core::Interface;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::{DirectXPixelFormat, Direct3D11};
use windows::Win32::Foundation::{HMODULE, HWND};
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

thread_local! {
    static DEVICE: std::cell::RefCell<Option<(ID3D11Device, Direct3D11::IDirect3DDevice)>> =
        const { std::cell::RefCell::new(None) };
}

/// Ensure the shared D3D11 device and its WinRT wrapper exist.
fn ensure_device() -> windows::core::Result<(ID3D11Device, Direct3D11::IDirect3DDevice)> {
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
    let winrt: Direct3D11::IDirect3DDevice = inspectable.cast()?;
    let pair = (device.clone(), winrt);
    DEVICE.with(|d| *d.borrow_mut() = Some(pair.clone()));
    Ok(pair)
}

fn capture_item_for(hwnd: HWND) -> windows::core::Result<GraphicsCaptureItem> {
    unsafe {
        let class: windows::core::HSTRING =
            "Windows.Graphics.Capture.GraphicsCaptureItem".into();
        let interop: IGraphicsCaptureItemInterop = match RoGetActivationFactory(&class) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[trace] RoGet(GraphicsCaptureItem) failed: {e:?}");
                return Err(e);
            }
        };
        interop.CreateForWindow(hwnd)
    }
}

fn unwrap_surface(
    surface: &Direct3D11::IDirect3DSurface,
) -> windows::core::Result<ID3D11Texture2D> {
    unsafe {
        let access: windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess =
            surface.cast()?;
        access.GetInterface()
    }
}

/// Capture one frame of `hwnd`; returns (jpeg bytes, width, height).
pub fn capture_window_jpeg(hwnd: HWND) -> Result<(Vec<u8>, usize, usize), String> {
    let (device, winrt_device) = ensure_device().map_err(|e| format!("d3d device: {e}"))?;
    let item = capture_item_for(hwnd).map_err(|e| format!("capture item: {e}"))?;
    let size = item.Size().map_err(|e| format!("item size: {e}"))?;
    if size.Width <= 0 || size.Height <= 0 {
        return Err("window has no capturable size".into());
    }
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )
    .map_err(|e| format!("frame pool: {e}"))?;
    let session: GraphicsCaptureSession = pool
        .CreateCaptureSession(&item)
        .map_err(|e| format!("capture session: {e}"))?;
    session.StartCapture().map_err(|e| format!("start capture: {e}"))?;

    // Poll for the first frame; free-threaded pools fill quickly but the
    // first frame can lag when the window just restored.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    let frame = loop {
        match pool.TryGetNextFrame() {
            Ok(f) => break f,
            Err(_) => {
                if std::time::Instant::now() >= deadline {
                    let _ = session.Close();
                    return Err("capture timed out waiting for a frame".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
    };

    let surface = frame.Surface().map_err(|e| format!("frame surface: {e}"))?;
    let src = unwrap_surface(&surface).map_err(|e| format!("unwrap surface: {e}"))?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { src.GetDesc(&mut desc) };
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
    unsafe {
        ctx.CopyResource(&staging, &src);
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("map: {e}"))?;
    }
    let result = encode_jpeg(
        unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, (desc.Height * mapped.RowPitch) as usize) },
        mapped.RowPitch as usize,
        desc.Width as usize,
        desc.Height as usize,
    )
    .map(|bytes| (bytes, desc.Width as usize, desc.Height as usize));
    unsafe {
        ctx.Unmap(&staging, 0);
    }
    let _ = session.Close();
    result
}

/// BGRA rows → JPEG via WIC, returned as base64-ready bytes.
fn encode_jpeg(bgra: &[u8], row_pitch: usize, width: usize, height: usize) -> Result<Vec<u8>, String> {
    unsafe {
        let factory: IWICImagingFactory =
            windows::Win32::System::Com::CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
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
            let mut wpx: u32 = 0;
            let mut hpx: u32 = 0;
            lock.GetSize(&mut wpx, &mut hpx).map_err(|e| format!("size: {e}"))?;
            let lock_pitch = if hpx > 0 { (wpx / hpx) as usize } else { row_pitch };
            let mut ptr: *mut u8 = core::ptr::null_mut();
            let mut cbuf: u32 = 0;
            lock.GetDataPointer(&mut cbuf, &mut ptr).map_err(|e| format!("data: {e}"))?;
            let row_bytes = width * 4;
            for y in 0..height {
                let src = &bgra[y * row_pitch..y * row_pitch + row_bytes];
                std::ptr::copy_nonoverlapping(src.as_ptr(), ptr.add(y * lock_pitch), row_bytes);
            }
        }
        let stream: IWICStream = factory.CreateStream().map_err(|e| format!("stream: {e}"))?;
        let mut buf: Vec<u8> = vec![0u8; 16 * 1024 * 1024];
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
            .SetSize(width as u32, height as u32)
            .map_err(|e| format!("set size: {e}"))?;
        let mut pf = windows::Win32::Graphics::Imaging::GUID_WICPixelFormat24bppBGR;
        frameenc
            .SetPixelFormat(&mut pf)
            .map_err(|e| format!("pixel format: {e}"))?;
        frameenc
            .WriteSource(&bitmap, core::ptr::null())
            .map_err(|e| format!("write: {e}"))?;
        frameenc.Commit().map_err(|e| format!("commit: {e}"))?;
        encoder.Commit().map_err(|e| format!("enc commit: {e}"))?;
        let mut stat = windows::Win32::System::Com::STATSTG::default();
        stream
            .Stat(&mut stat, windows::Win32::System::Com::STATFLAG(0))
            .map_err(|e| format!("stat: {e}"))?;
        let len = stat.cbSize;
        buf.truncate(len as usize);
        Ok(buf)
    }
}
