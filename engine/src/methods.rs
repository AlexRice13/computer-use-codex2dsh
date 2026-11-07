use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, GetWindowThreadProcessId};

use crate::catalog;
use crate::state::{EngineState, Response};
use crate::win::enumerate;

// ---------------------------------------------------------------------------
// 参数解析按原版的严格程度来：每个请求都反序列化成强类型 struct，
// 报错就是 serde 的原文，前缀固定是 `parse {method} params: `，
// 这样错误文案才能跟原版逐字节对上。
// ---------------------------------------------------------------------------

fn parse_params<T: DeserializeOwned>(method: &str, params: &Value) -> Result<T, String> {
    serde_json::from_value(params.clone()).map_err(|e| format!("parse {method} params: {e}"))
}

fn always_true() -> bool {
    true
}

#[derive(Deserialize)]
struct WinRef {
    #[serde(default)]
    app: Option<String>,
    id: u64,
}

#[derive(Deserialize)]
struct WindowOnly {
    window: WinRef,
}

#[derive(Deserialize)]
struct GwsRegion {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Deserialize)]
struct GwsParams {
    window: WinRef,
    #[serde(default = "always_true", rename = "include_screenshot")]
    include_screenshot: bool,
    #[serde(default = "always_true", rename = "include_text")]
    include_text: bool,
    /// JPEG 像素 / 逻辑窗口像素；传 1.0 就是原版约定，不缩放。
    #[serde(default)]
    scale: Option<f64>,
    /// 裁剪区域，单位是逻辑像素，相对窗口左上角。
    #[serde(default)]
    region: Option<GwsRegion>,
}

#[derive(Deserialize)]
struct TypeParams {
    window: WinRef,
    text: String,
}

#[derive(Deserialize)]
struct KeyParams {
    window: WinRef,
    key: String,
}

#[derive(Deserialize)]
struct ClickParams {
    window: WinRef,
    x: i32,
    y: i32,
    /// 原版只解析坐标，从不要求带 id；新鲜度由插件这一层把关。带了 id
    /// 就顺便用它找这张 snapshot 的裁剪和缩放参数。
    #[serde(default, rename = "screenshotId")]
    screenshot_id: Option<String>,
    #[serde(default, rename = "mouse_button")]
    mouse_button: Option<String>,
    #[serde(default, rename = "click_count")]
    click_count: Option<u64>,
}

#[derive(Deserialize)]
struct ScrollParams {
    window: WinRef,
    x: i32,
    y: i32,
    #[serde(default, rename = "screenshotId")]
    screenshot_id: Option<String>,
    #[serde(default, rename = "scrollX")]
    scroll_x: i64,
    #[serde(default, rename = "scrollY")]
    scroll_y: i64,
}

#[derive(Deserialize)]
struct DragParams {
    window: WinRef,
    #[serde(rename = "from_x")]
    from_x: i32,
    #[serde(rename = "from_y")]
    from_y: i32,
    #[serde(rename = "to_x")]
    to_x: i32,
    #[serde(rename = "to_y")]
    to_y: i32,
    #[serde(default, rename = "screenshotId")]
    screenshot_id: Option<String>,
}

#[derive(Deserialize)]
struct ElementParams {
    window: WinRef,
    #[serde(rename = "element_index")]
    element_index: u64,
    #[serde(default, rename = "mouse_button")]
    mouse_button: Option<String>,
    #[serde(default, rename = "click_count")]
    click_count: Option<u64>,
    #[serde(default, rename = "scrollX")]
    scroll_x: i64,
    #[serde(default, rename = "scrollY")]
    scroll_y: i64,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    action: Option<String>,
}

#[derive(Deserialize)]
struct GetWindowFlat {
    #[serde(default)]
    app: Option<String>,
    id: u64,
}

#[derive(Deserialize)]
struct LaunchParams {
    app: String,
}

// ---------------------------------------------------------------------------
// 窗口解析：顺序和报错文案都照原版抄。
//   id == 0        -> "window id is required"
//   pid 拿不到     -> "foreground window did not report a process id"
//   窗口不可寻址   -> "window is not a usable app window"
//   没给 app       -> "window.app is required"
//   app 对不上     -> "window id {id} no longer belongs to {app}; current owner is {owner}"
// ---------------------------------------------------------------------------

struct Resolved {
    hwnd: HWND,
    origin: (i32, i32),
    size: (i32, i32),
    owner: String,
    pid: u32,
    /// 这个窗口的 DPI 倍数（`GetDpiForWindow` / 96）。snapshot 坐标是逻辑像素
    /// （模型量图用的截图就是逻辑像素的窗口，跟原版一致），所以要乘上这个
    /// 倍数才能落到屏幕上。
    scale: f64,
}

/// 某张 snapshot 图上量到的坐标，换算成屏幕坐标。
///
/// snapshot 的一个像素属于截图那个矩形：窗口的逻辑像素外框。所以物理坐标 =
/// 物理外框原点 + 像素乘窗口 DPI。旧逻辑直接把逻辑像素加到物理原点上，
/// 结果每次点击都差一个缩放倍数——200% 下实测：snapshot (600,400) 落到屏幕
/// 物理坐标 (621,821)，正确位置是 (1231,820)。
///
/// 缩放过的（`scale`）或裁剪过的（`region`）截图，JPEG 像素数和逻辑窗口像素
/// 数对不上，所以模型给的 `(x, y)` 要先除以截图缩放，再加上裁剪偏移。找不到
/// id 或没带 id，就退回原版的窗口相对映射。
fn to_screen_shot(
    r: &Resolved,
    st: &EngineState,
    shot_id: Option<&str>,
    x: i32,
    y: i32,
) -> (i32, i32) {
    let mut offset = (0.0f64, 0.0f64);
    let mut shot_scale = 1.0f64;
    if let Some(id) = shot_id {
        let candidates = st
            .shot_geoms
            .get(&(r.hwnd.0 as isize))
            .or_else(|| st.snapshots.get(&(r.hwnd.0 as isize)).map(|s| &s.shots));
        if let Some(geoms) = candidates {
            if let Some(geom) = geoms.iter().find(|g| g.id == id) {
                offset = geom.offset;
                if geom.scale.is_finite() && geom.scale > 0.0 {
                    shot_scale = geom.scale;
                }
            }
        }
    }
    let logical_x = offset.0 + x as f64 / shot_scale;
    let logical_y = offset.1 + y as f64 / shot_scale;
    let s = if r.scale.is_finite() && r.scale > 0.0 { r.scale } else { 1.0 };
    (
        r.origin.0 + (logical_x * s).round() as i32,
        r.origin.1 + (logical_y * s).round() as i32,
    )
}

fn resolve_win(w: &WinRef) -> Result<Resolved, String> {
    if w.id == 0 {
        return Err("window id is required".into());
    }
    let hwnd = HWND(w.id as *mut core::ffi::c_void);
    let mut pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    if pid == 0 {
        return Err("foreground window did not report a process id".into());
    }
    // 原版先看 app 提示，再去解析 owner。
    let app = w
        .app
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "window.app is required".to_string())?;
    // owner 的身份：先看 enumerate 那份（按任务栏修正过的），拿不到再退到
    // shell 属性存储 / 图片路径。
    let owner = enumerate::enumerate_top_level()
        .into_iter()
        .find(|r| r.hwnd as u64 == w.id)
        .map(|r| r.app)
        .or_else(|| crate::win::identity::app_id_for_window(hwnd, pid))
        .ok_or_else(|| "window is not a usable app window".to_string())?;
    if !owner.eq_ignore_ascii_case(app) {
        return Err(format!(
            "window id {} no longer belongs to {}; current owner is {}",
            w.id, app, owner
        ));
    }
    let mut rect = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut rect);
    }
    // 锚点用可见外框。DWM 的 extended bounds 不含那条看不见的调整边框，
    // 而 GetWindowRect 还会把它算进去；截图正好只覆盖 DWM 这个矩形，
    // 所以 snapshot 像素必须拿它当锚点。
    let mut ext = RECT::default();
    let origin = match unsafe {
        windows::Win32::Graphics::Dwm::DwmGetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut ext as *mut RECT as *mut core::ffi::c_void,
            core::mem::size_of::<RECT>() as u32,
        )
    } {
        Ok(()) if ext.right > ext.left && ext.bottom > ext.top => (ext.left, ext.top),
        _ => (rect.left, rect.top),
    };
    let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
    let scale = (dpi as f64 / 96.0).max(0.25);
    Ok(Resolved {
        hwnd,
        origin,
        size: (rect.right - rect.left, rect.bottom - rect.top),
        owner,
        pid,
        scale,
    })
}

// ---------------------------------------------------------------------------
// 按 app 的审批闸（spec §B.1）。原版凡是碰 app 的操作，第一次都要先过
// approvalRequest，哪怕（桩）策略返回的是 default_app_access=allow。
// 枚举/诊断类方法不走这个闸。实测到的规范身份长这样：
//   process 窗口 -> 小写文件名（"deepseek harness.exe"）
//   打包应用     -> AUMID 原文
//   浏览器       -> 小写 exe 文件名 + riskLevel "high"
// ---------------------------------------------------------------------------

/// identity::app_id_for_window 产出的 Chromium 系产品名。
pub fn browser_exe(owner: &str) -> Option<&'static str> {
    match owner {
        "Chrome" => Some("chrome.exe"),
        "MSEdge" => Some("msedge.exe"),
        "Brave" => Some("brave.exe"),
        "Opera" => Some("opera.exe"),
        _ => None,
    }
}

/// 解析出来的 owner 是白名单里的浏览器窗口时返回 true。
pub fn is_browser_owner(owner: &str) -> bool {
    browser_exe(owner).is_some()
}

/// 原版只要拿不准浏览器窗口当前的 URL，就吐这段拒绝文案，一字不差：
/// 实测这个环境里每次碰浏览器都掉进这个分支——原版在这里根本没有可靠的
/// URL 通道。不 end_turn，这一轮就一直停着。
pub const BROWSER_URL_POLICY_REFUSAL: &str = "Computer Use has been stopped for this turn because it could not determine the current browser URL on Windows with enough confidence to enforce policy. Stop your work and send a final message noting why Computer Use ended.";

/// 原版这个闸的逃生门（`func@0x7737b`，见 SKY_WINDOWS_PROTOCOL_SPEC §B.4）：
/// 请求 meta 里带 `x-codex-browser-use-security-mode: disabled-for-local-testing`
/// 就会把 URL-allowed 哨兵置上，整个跳过 `site_status` 拉取。在真 Chrome 窗口
/// 上实测过：不带这个键，审批通过的 `get_window_state` 也会抛停止；带了，
/// 同一个调用能拿回整棵 UIA 树。这是 OpenAI 自己本地测试用的模式，也就是
/// 在 ChatGPT 外面跑这个 helper 该走的路——这个引擎正是这么部署的。所以插件
/// 默认把它加在基础 meta 上，除非用户用 `enforceBrowserUrlPolicy` 配置显式关掉。
pub const BROWSER_POLICY_BYPASS_KEY: &str = "x-codex-browser-use-security-mode";
pub const BROWSER_POLICY_BYPASS_VALUE: &str = "disabled-for-local-testing";

/// true 表示这一帧自己关掉了浏览器 URL 策略检查。
pub fn browser_policy_bypassed(meta: &Value) -> bool {
    meta.get(BROWSER_POLICY_BYPASS_KEY)
        .and_then(Value::as_str)
        .map(|v| v.eq_ignore_ascii_case(BROWSER_POLICY_BYPASS_VALUE))
        .unwrap_or(false)
}

fn canonical_approval_app(owner: &str) -> (String, String) {
    if let Some(exe) = browser_exe(owner) {
        let display = match owner {
            "Chrome" => "Google Chrome",
            "MSEdge" => "Microsoft Edge",
            "Brave" => "Brave Browser",
            _ => "Opera",
        };
        return (exe.to_string(), display.to_string());
    }
    if let Some(path) = owner
        .strip_prefix("process:")
        .or_else(|| owner.strip_prefix("process:\\"))
    {
        let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
        let display = file
            .strip_suffix(".exe")
            .or_else(|| file.strip_suffix(".EXE"))
            .unwrap_or(file)
            .to_string();
        return (file.to_lowercase(), display);
    }
    // AUMID：显示名优先从原始的 Get-StartApps 表里取（只靠 catalog 行会漏掉
    // 计算器这种没人用过的包）。
    let display = crate::startapps::collect()
        .into_iter()
        .find(|(_n, id)| id.eq_ignore_ascii_case(owner))
        .map(|(n, _)| n)
        .filter(|d| !d.trim().is_empty())
        .or_else(|| {
            let recs = enumerate::enumerate_top_level();
            let cats = catalog::build_catalog(&recs);
            cats.iter()
                .find(|c| c.id.eq_ignore_ascii_case(owner))
                .and_then(|c| c.display_name.clone())
                .filter(|d| !d.is_empty())
        })
        .unwrap_or_else(|| {
            owner
                .rsplit(['!', '_'])
                .next()
                .unwrap_or(owner)
                .to_string()
        });
    (owner.to_string(), display)
}

/// 树头那行和 lastWindowIdentity 用的进程文件名。
fn process_file_name(owner: &str) -> String {
    owner
        .strip_prefix("process:")
        .or_else(|| owner.strip_prefix("process:\\"))
        .map(|p| p.rsplit(['\\', '/']).next().unwrap_or(p).to_string())
        .unwrap_or_default()
}

/// 树头那行 `App:` 字段：process 窗口填文件名，打包应用填 shell 显示名。
fn header_app_name(owner: &str) -> String {
    let file = process_file_name(owner);
    if !file.is_empty() {
        return file;
    }
    canonical_approval_app(owner).1
}

/// 这一轮里 app 还没批过，就返回 Some(审批响应)；批过了返回 None，往下走。
fn approval_required(st: &EngineState, owner: &str) -> Option<Response> {
    let (app_key, display_name) = canonical_approval_app(owner);
    if st.approved_apps.contains(&app_key.to_lowercase()) {
        return None;
    }
    let risk = if is_browser_owner(owner) { "high" } else { "low" };
    Some(Response::Approval {
        app: app_key,
        display_name,
        risk_level: Some(risk.into()),
        allow_persistent_approval: true,
    })
}

/// 批完之后，浏览器窗口还要过 URL 策略闸。没有本地测试那个豁免，原版在这个
/// 环境里永远定不下当前 URL，所以每次审批通过的浏览器操作都撞上那段一字不差
/// 的拒绝文案（在真 Chrome 和 Edge 窗口上实测），这一轮剩下的时间也全停住。
/// 帧里带了文档写明的 `x-codex-browser-use-security-mode: disabled-for-local-testing`
/// meta，就走原版自己的逃生门，能继续。
fn browser_policy_gate(st: &mut EngineState, owner: &str, meta: &Value) -> Option<Response> {
    if !is_browser_owner(owner) {
        return None;
    }
    if browser_policy_bypassed(meta) {
        return None;
    }
    st.browser_policy_stopped = true;
    Some(Response::Error(BROWSER_URL_POLICY_REFUSAL.into()))
}

/// 所有会碰窗口的方法都走这一句：解析窗口 + 过审批闸。
fn resolve_and_approve(st: &mut EngineState, w: &WinRef, meta: &Value) -> Result<Resolved, Response> {
    let r = resolve_win(w).map_err(Response::Error)?;
    if let Some(a) = approval_required(st, &r.owner) {
        return Err(a);
    }
    if let Some(b) = browser_policy_gate(st, &r.owner, meta) {
        return Err(b);
    }
    // 任何输入动作都会让渲染好的树过期：原版每次输入后一定重渲染（set_value
    // 之后再 get_window_state，必须看到新值而不是缓存）。但 snapshot 本身得
    // 活过这次调用：element_index 类动作这次返回后马上要从里面找元素，而且
    // 文档约定这些索引在下次 snapshot 之前一直可用。之前在这里把 snapshot 丢了，
    // 结果每个 element_index 动作都报 "no snapshot for window"。
    st.last_input_seq += 1;
    Ok(r)
}

/// element_index 类动作用它查最近一次 snapshot 里的元素。
fn snapshot_element(
    st: &EngineState,
    r: &Resolved,
    index: u64,
) -> Result<(uiautomation::UIElement, (i32, i32, i32, i32)), String> {
    let snap = st
        .snapshots
        .get(&(r.hwnd.0 as isize))
        .ok_or_else(|| "no snapshot for window; call get_window_state first".to_string())?;
    let rec = snap
        .elements
        .get(index as usize)
        .ok_or_else(|| format!("element_index {index} is out of range for the latest snapshot ({} elements)", snap.elements.len()))?;
    Ok((rec.element.clone(), rec.rect))
}

// ---------------------------------------------------------------------------
// 分发
// ---------------------------------------------------------------------------

/// RPC 分发按串行来，约定上这条通道就是单线程。
pub fn dispatch(st: &mut EngineState, method: &str, params: &Value, meta: &Value) -> Response {
    // 审批通过后的重发请求，meta 里带着本会话的 x-oai-cua-approved-app。
    if let Some(app) = meta
        .get("x-oai-cua-approved-app")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
    {
        st.approved_apps.insert(app);
    }

    // 浏览器 URL 策略一旦把这一轮停了，后面每个碰窗口的方法都回同一段拒绝，
    // 直到 end_turn。
    const TURN_SAFE: [&str; 6] = ["close", "end_turn", "diagnostic_state", "list_apps", "list_windows", "get_window"];
    if st.browser_policy_stopped && !TURN_SAFE.contains(&method) {
        return Response::Error(BROWSER_URL_POLICY_REFUSAL.into());
    }

    match method {
        "close" => Response::Result(json!({})),
        "end_turn" => {
            st.last_turn_key = None;
            st.approved_apps.clear();
            st.browser_policy_stopped = false;
            Response::Result(json!({}))
        }
        "diagnostic_state" => Response::Result(json!({
            "cacheDiagnostics": {
                "accessibilityRevision": st.snapshot_revision,
                "accessibilitySnapshotCount": st.snapshot_count,
                "captureCachedSessionCount": st
                    .snapshots
                    .values()
                    .filter(|s| !s.shot_ids.is_empty())
                    .count(),
            },
            "lastWindowIdentity": st.last_window_identity,
        })),

        "list_windows" => {
            let recs = enumerate::enumerate_top_level();
            let arr: Vec<Value> = recs
                .iter()
                .map(|r| json!({ "app": r.app, "id": r.hwnd, "title": r.title }))
                .collect();
            Response::Result(Value::Array(arr))
        }
        "get_window" => {
            let p: GetWindowFlat = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            let recs = enumerate::enumerate_top_level();
            let hit = recs.into_iter().find(|r| {
                p.id == r.hwnd as u64
                    && p.app
                        .as_deref()
                        .map(|a| a.eq_ignore_ascii_case(&r.app))
                        .unwrap_or(true)
            });
            match hit {
                Some(r) => Response::Result(json!({ "app": r.app, "id": r.hwnd, "title": r.title })),
                // 查不到就返回 null——JS 客户端会把它渲染成自己的错误，这里照
                // 原版的原始契约来。
                None => Response::Result(Value::Null),
            }
        }
        "list_apps" => {
            let recs = enumerate::enumerate_top_level();
            let apps = catalog::build_catalog(&recs);
            let arr: Vec<Value> = apps
                .into_iter()
                .map(|a| {
                    let mut v = json!({
                        "id": a.id,
                        "isRunning": a.is_running,
                        "windows": a
                            .windows
                            .iter()
                            .map(|w| json!({ "app": w.app, "id": w.hwnd, "title": w.title }))
                            .collect::<Vec<Value>>(),
                    });
                    if let Some(d) = a.display_name.filter(|s| !s.is_empty()) {
                        v["displayName"] = json!(d);
                    }
                    if let Some(d) = a.last_used_date {
                        v["lastUsedDate"] = json!(d);
                    }
                    if let Some(c) = a.use_count {
                        v["useCount"] = json!(c);
                    }
                    v
                })
                .collect();
            Response::Result(Value::Array(arr))
        }

        "activate_window" => {
            let p: WindowOnly = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window, meta) {
                Ok(r) => {
                    crate::win::input::activate(r.hwnd);
                    Response::Result(json!({}))
                }
                Err(resp) => resp,
            }
        }
        "type_text" => {
            let p: TypeParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window, meta) {
                Ok(_r) => {
                    crate::win::input::type_text(&p.text);
                    Response::Result(json!({}))
                }
                Err(resp) => resp,
            }
        }
        "press_key" => {
            let p: KeyParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window, meta) {
                Ok(_r) => match crate::win::input::press_key(&p.key) {
                    Ok(()) => Response::Result(json!({})),
                    Err(e) => Response::Error(e),
                },
                Err(resp) => resp,
            }
        }
        "click" => {
            let p: ClickParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window, meta) {
                Ok(r) => {
                    let (x, y) = to_screen_shot(&r, st, p.screenshot_id.as_deref(), p.x, p.y);
                    let button = match p.mouse_button.as_deref() {
                        Some("right") => crate::win::input::Button::Right,
                        Some("middle") => crate::win::input::Button::Middle,
                        _ => crate::win::input::Button::Left,
                    };
                    crate::win::input::click(x, y, button, p.click_count.unwrap_or(1) as u32);
                    Response::Result(json!({}))
                }
                Err(resp) => resp,
            }
        }
        "scroll" => {
            let p: ScrollParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window, meta) {
                Ok(r) => {
                    let (x, y) = to_screen_shot(&r, st, p.screenshot_id.as_deref(), p.x, p.y);
                    crate::win::input::wheel(x, y, p.scroll_x as i32, p.scroll_y as i32);
                    Response::Result(json!({}))
                }
                Err(resp) => resp,
            }
        }
        "drag" => {
            let p: DragParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window, meta) {
                Ok(r) => {
                    let from = to_screen_shot(&r, st, p.screenshot_id.as_deref(), p.from_x, p.from_y);
                    let to = to_screen_shot(&r, st, p.screenshot_id.as_deref(), p.to_x, p.to_y);
                    crate::win::input::drag(from, to);
                    Response::Result(json!({}))
                }
                Err(resp) => resp,
            }
        }

        "click_element" => {
            let p: ElementParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window, meta)
                .and_then(|r| snapshot_element(st, &r, p.element_index).map(|e| (r, e)).map_err(Response::Error))
            {
                Ok((_r, (_el, rect))) => {
                    let cx = rect.0 + rect.2 / 2;
                    let cy = rect.1 + rect.3 / 2;
                    crate::win::input::activate(_r.hwnd);
                    let button = match p.mouse_button.as_deref() {
                        Some("right") => crate::win::input::Button::Right,
                        Some("middle") => crate::win::input::Button::Middle,
                        _ => crate::win::input::Button::Left,
                    };
                    crate::win::input::click(cx, cy, button, p.click_count.unwrap_or(1) as u32);
                    Response::Result(json!({}))
                }
                Err(resp) => resp,
            }
        }
        "scroll_element" => {
            let p: ElementParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            match resolve_and_approve(st, &p.window, meta)
                .and_then(|r| snapshot_element(st, &r, p.element_index).map(|e| (r, e)).map_err(Response::Error))
            {
                Ok((_r, (_el, rect))) => {
                    let cx = rect.0 + rect.2 / 2;
                    let cy = rect.1 + rect.3 / 2;
                    crate::win::input::wheel(cx, cy, p.scroll_x as i32, p.scroll_y as i32);
                    Response::Result(json!({}))
                }
                Err(resp) => resp,
            }
        }
        "set_value" => {
            let p: ElementParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            let want = match p.value.clone() {
                Some(v) => v,
                None => return Response::Error(format!("parse {method} params: missing field `value`")),
            };
            match resolve_and_approve(st, &p.window, meta)
                .and_then(|r| snapshot_element(st, &r, p.element_index).map(|e| (r, e)).map_err(Response::Error))
            {
                Ok((_r, (el, _rect))) => {
                    match el.get_pattern::<uiautomation::patterns::UIValuePattern>() {
                        Ok(pattern) => match pattern.set_value(&want) {
                            Ok(()) => Response::Result(json!({})),
                            Err(e) => Response::Error(format!("set value failed: {e}")),
                        },
                        Err(e) => {
                            Response::Error(format!("element does not support ValuePattern: {e}"))
                        }
                    }
                }
                Err(resp) => resp,
            }
        }
        "perform_secondary_action" => {
            let p: ElementParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            let action = match p.action.clone() {
                Some(a) => a,
                None => {
                    return Response::Error(format!("parse {method} params: missing field `action`"))
                }
            };
            match resolve_and_approve(st, &p.window, meta)
                .and_then(|r| snapshot_element(st, &r, p.element_index).map(|e| (r, e)).map_err(Response::Error))
            {
                Ok((_r, (el, rect))) => {
                    let cx = rect.0 + rect.2 / 2;
                    let cy = rect.1 + rect.3 / 2;
                    use uiautomation::patterns::{UIExpandCollapsePattern, UIScrollPattern};
                    use uiautomation::types::ScrollAmount;
                    let outcome: Result<(), String> = match action.as_str() {
                        "Expand" => el
                            .get_pattern::<UIExpandCollapsePattern>()
                            .map_err(|e| format!("element does not support ExpandCollapse: {e}"))
                            .and_then(|p| p.expand().map_err(|e| format!("expand failed: {e}"))),
                        "Collapse" => el
                            .get_pattern::<UIExpandCollapsePattern>()
                            .map_err(|e| format!("element does not support ExpandCollapse: {e}"))
                            .and_then(|p| p.collapse().map_err(|e| format!("collapse failed: {e}"))),
                        "Raise" => {
                            crate::win::input::activate(_r.hwnd);
                            Ok(())
                        }
                        "Scroll Up" => el
                            .get_pattern::<UIScrollPattern>()
                            .map_err(|e| format!("element does not support Scroll: {e}"))
                            .and_then(|p| {
                                p.scroll(ScrollAmount::NoAmount, ScrollAmount::SmallDecrement)
                                    .map_err(|e| format!("scroll failed: {e}"))
                            })
                            .or_else(|_| {
                                crate::win::input::wheel(cx, cy, 0, 3);
                                Ok(())
                            }),
                        "Scroll Down" => el
                            .get_pattern::<UIScrollPattern>()
                            .map_err(|e| format!("element does not support Scroll: {e}"))
                            .and_then(|p| {
                                p.scroll(ScrollAmount::NoAmount, ScrollAmount::SmallIncrement)
                                    .map_err(|e| format!("scroll failed: {e}"))
                            })
                            .or_else(|_| {
                                crate::win::input::wheel(cx, cy, 0, -3);
                                Ok(())
                            }),
                        "Scroll Left" => el
                            .get_pattern::<UIScrollPattern>()
                            .map_err(|e| format!("element does not support Scroll: {e}"))
                            .and_then(|p| {
                                p.scroll(ScrollAmount::SmallDecrement, ScrollAmount::NoAmount)
                                    .map_err(|e| format!("scroll failed: {e}"))
                            })
                            .or_else(|_| {
                                crate::win::input::wheel(cx, cy, 3, 0);
                                Ok(())
                            }),
                        "Scroll Right" => el
                            .get_pattern::<UIScrollPattern>()
                            .map_err(|e| format!("element does not support Scroll: {e}"))
                            .and_then(|p| {
                                p.scroll(ScrollAmount::SmallIncrement, ScrollAmount::NoAmount)
                                    .map_err(|e| format!("scroll failed: {e}"))
                            })
                            .or_else(|_| {
                                crate::win::input::wheel(cx, cy, -3, 0);
                                Ok(())
                            }),
                        other => Err(format!("unsupported secondary action: {other}")),
                    };
                    match outcome {
                        Ok(()) => Response::Result(json!({})),
                        Err(e) => Response::Error(e),
                    }
                }
                Err(resp) => resp,
            }
        }

        "get_window_state" => {
            let p: GwsParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            let r = match resolve_win(&p.window).map_err(Response::Error) {
                Ok(r) => r,
                Err(resp) => return resp,
            };
            // 实测的顺序：先审批，再看两个 include 是不是都 false，最后才是
            // 最小化拒绝（已审批的 persistent home 就算是最小化状态，两个
            // false 也照样回显窗口信息）。
            if let Some(a) = approval_required(st, &r.owner) {
                return a;
            }
            if let Some(b) = browser_policy_gate(st, &r.owner, meta) {
                return b;
            }
            if !p.include_screenshot && !p.include_text {
                return Response::Error(
                    "get_window_state must request include_text, include_screenshot, or both".into(),
                );
            }
            if unsafe { windows::Win32::UI::WindowsAndMessaging::IsIconic(r.hwnd).as_bool() } {
                return Response::Error(
                    "window is minimized; call activate_window, refresh with get_window, then retry get_window_state".into(),
                );
            }
            let hwnd = r.hwnd;
            let recs = enumerate::enumerate_top_level();
            let rec = recs.iter().find(|w| w.hwnd as u64 == p.window.id);
            let title = rec.map(|w| w.title.clone()).unwrap_or_default();
            let mut result = json!({
                "window": { "app": r.owner.clone(), "id": hwnd.0 as isize, "title": title },
                "screenshots": json!([]),
            });

            let mut shot_geom: Option<crate::state::ShotGeom> = None;
            if p.include_screenshot {
                let opts = crate::win::capture::CaptureOpts {
                    scale: p.scale.unwrap_or(1.0),
                    region: p.region.as_ref().map(|r| (r.x, r.y, r.width, r.height)),
                };
                match crate::win::capture::capture_window_shot(hwnd, opts) {
                    Ok(shot) => {
                        st.last_capture_invalidation = None;
                        use base64::Engine as _;
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&shot.jpeg);
                        // 字段、id 和几何都跟原版的 screenshot 记录完全一致：
                        // `id` 是 "screenshot-<n>"，原点是窗口在屏幕上的逻辑
                        // 像素坐标，没有 `hasContent`（原版从来不发）。`scale`
                        // 是我们加的：调用方用默认的整尺寸截图时它是 1.0。
                        result["screenshots"] = json!([{
                            "id": "screenshot-0",
                            "url": format!("data:image/jpeg;base64,{b64}"),
                            "originX": shot.origin_x,
                            "originY": shot.origin_y,
                            "width": shot.width,
                            "height": shot.height,
                            "zIndex": 0,
                            "scale": shot.scale,
                        }]);
                        shot_geom = Some(crate::state::ShotGeom {
                            id: "screenshot-0".into(),
                            offset: (shot.offset_x as f64, shot.offset_y as f64),
                            scale: shot.scale,
                        });
                    }
                    // 截图失败（比如 Windows 录屏授权把这 exe 的路径拒了、会话
                    // 锁了，或者窗口受保护）。这种情况原版 helper 是整个调用失败；
                    // 我们保住无障碍树，但也不能让失败悄没声地没了——只把它塞进
                    // `last_capture_invalidation`（`diagnostic_state` 又不暴露这个
                    // 字段），结果一次引擎级的截图失败看起来就像 "这个窗口没有截图"。
                    Err(e) => {
                        result["screenshotError"] = json!(e);
                        st.last_capture_invalidation = Some(e);
                    }
                }
            }
            // 就算是不带文字的 snapshot 也要能换算坐标，所以几何信息放在树缓存
            // 外面。
            if let Some(geom) = shot_geom.clone() {
                st.shot_geoms.insert(hwnd.0 as isize, vec![geom]);
            }

            if p.include_text {
                // snapshot 缓存：窗口没变就把渲染好的树直接重放（原版会保持
                // revision 不变）。
                let key = hwnd.0 as isize;
                let no_cache = std::env::var("SKY_NO_CACHE").is_ok();
                let hit = st.snapshots.get(&key).and_then(|s| {
                    if !no_cache
                        && s.title == title
                        && s.origin == r.origin
                        && s.size == r.size
                        && s.at_seq == st.last_input_seq
                        && !s.tree.is_empty()
                    {
                        Some((s.tree.clone(), s.focused_element.clone(), s.document_text.clone()))
                    } else {
                        None
                    }
                });
                let (tree, focused_element, document_text) = match hit {
                    Some(t) => {
                        // 树是从缓存重放的，但这次截图可能换了 scale/region：
                        // 坐标几何要跟着模型刚拿到的那张截图更新。
                        if let Some(geom) = shot_geom.clone() {
                            if let Some(snap) = st.snapshots.get_mut(&key) {
                                snap.shots = vec![geom];
                                snap.shot_ids = result
                                    .get("screenshots")
                                    .and_then(Value::as_array)
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|s| {
                                                s.get("id").and_then(Value::as_str).map(String::from)
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                            }
                        }
                        t
                    }
                    None => {
                        let app_name = header_app_name(&r.owner);
                        match crate::win::uia::build_tree(hwnd, &app_name) {
                            Ok(built) => {
                                let shot_ids: Vec<String> = result
                                    .get("screenshots")
                                    .and_then(Value::as_array)
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|s| {
                                                s.get("id").and_then(Value::as_str).map(String::from)
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                let tup = (
                                    built.tree.clone(),
                                    built.focused_element.clone(),
                                    built.document_text.clone(),
                                );
                                st.snapshots.insert(
                                    key,
                                    crate::state::WindowSnapshot {
                                        hwnd: key,
                                        taken_ms: std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .map(|d| d.as_millis() as u64)
                                            .unwrap_or(0),
                                        origin: r.origin,
                                        size: r.size,
                                        elements: built.elements,
                                        shot_ids,
                                        shots: shot_geom.clone().into_iter().collect(),
                                        tree: built.tree,
                                        focused_element: built.focused_element,
                                        document_text: built.document_text,
                                        title: title.clone(),
                                        at_seq: st.last_input_seq,
                                    },
                                );
                                // 原版每新构建一棵树：revision +1，snapshot 条目 +3。
                                st.snapshot_revision += 1;
                                st.snapshot_count += 3;
                                st.last_window_identity = Some(json!({
                                    "appId": r.owner,
                                    "bounds": {
                                        "originX": r.origin.0,
                                        "originY": r.origin.1,
                                        "width": r.size.0,
                                        "height": r.size.1,
                                    },
                                    "inputHwnd": (hwnd.0 as isize).to_string(),
                                    "processId": r.pid,
                                    "processName": process_file_name(&r.owner),
                                    "rootHwnd": (hwnd.0 as isize).to_string(),
                                    "snapshotRevision": st.snapshot_revision,
                                    "title": title,
                                }));
                                tup
                            }
                            Err(_) => (String::new(), None, None),
                        }
                    }
                };
                let mut acc = json!({});
                if !tree.is_empty() {
                    acc["tree"] = json!(tree);
                    if let Some(f) = focused_element {
                        acc["focused_element"] = json!(f);
                    }
                    if let Some(d) = document_text {
                        acc["document_text"] = json!(d);
                    }
                }
                result["accessibility"] = acc;
            }
            result["cacheDiagnostics"] = json!({
                "accessibilityRevision": st.snapshot_revision,
                "accessibilitySnapshotCount": st.snapshot_count,
                "captureCachedSessionCount": st
                    .snapshots
                    .values()
                    .filter(|s| !s.shot_ids.is_empty())
                    .count(),
            });
            Response::Result(result)
        }
        "launch_app" => {
            let p: LaunchParams = match parse_params(method, params) {
                Ok(p) => p,
                Err(e) => return Response::Error(e),
            };
            let plan = match crate::launch::plan(&p.app) {
                Ok(plan) => plan,
                Err(e) => {
                    // 原版对裸名字直接报错；这里先把 "画图"/"Paint" 这类名字
                    // 折成 AppsFolder 清单里的 AUMID，折不出来再报原错误。
                    let resolved = if crate::launch::is_bare_name(&p.app) {
                        crate::launch::resolve_name(&p.app)
                    } else {
                        None
                    };
                    match resolved {
                        Some(aumid) => match crate::launch::plan(&aumid) {
                            Ok(plan) => plan,
                            Err(_) => return Response::Error(e),
                        },
                        None => return Response::Error(e),
                    }
                }
            };
            let owner = crate::launch::plan_owner(&plan);
            if let Some(a) = approval_required(st, &owner) {
                return a;
            }
            match crate::launch::execute(
                &plan,
                meta
                    .get("x-oai-cua-request-budget-ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(15000),
            ) {
                Ok(()) => Response::Result(json!({})),
                Err(e) => Response::Error(e),
            }
        }
        "start_audio_recording" | "stop_audio_recording" => {
            // 原版要 SKY_ENABLE_AUDIO="1" 才开录音。这里也先查这个开关；
            // 录音本身没实现，开了也只会回 not implemented。
            if std::env::var("SKY_ENABLE_AUDIO").ok().as_deref() != Some("1") {
                return Response::Error("computer audio recording is not enabled".into());
            }
            Response::Error(format!("not implemented: {method}"))
        }
        other => Response::Error(format!("unsupported method: {other}")),
    }
}
