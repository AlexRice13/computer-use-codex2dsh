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

const MAX_ELEMENTS: usize = 600;
const MAX_DEPTH: usize = 64;
const TREE_BUDGET_MS: u128 = 6500;

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

// Wake a potentially suspended UIA provider: subscribing a live
// structure-changed event handler forces the UIA core to bring the
// provider connection up, after which even a suspended Chromium exposes
// its full subtree (matches the original helper's observed behaviour).
#[allow(non_camel_case_types)]
mod uia_bindings {
    use windows::core::{implement, Ref as WinRef, Result as WinResult};
    use windows::Win32::System::Com::SAFEARRAY;
    use windows::Win32::UI::Accessibility::{
        IUIAutomationElement, IUIAutomationStructureChangedEventHandler,
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

    pub use windows::Win32::UI::Accessibility::IUIAutomationStructureChangedEventHandler as HandlerIfc;

    /// The UIA core keeps calling into the handler from its own threads;
    /// we only ever park strong references, which is Send-safe across the
    /// MTA process.
    pub struct Keepalive(#[allow(dead_code)] pub HandlerIfc);
    // SAFETY: interface pointer to an apartment-agnostic COM object.
    unsafe impl Send for Keepalive {}

    // oleacc MSAA probe: Chromium switches on its full accessibility tree
    // only when an MSAA client asks for the render-widget's IAccessible
    // (the original's imports include oleacc.dll).
    #[link(name = "oleacc")]
    extern "system" {
        pub fn AccessibleObjectFromWindow(
            hwnd: windows::Win32::Foundation::HWND,
            id: u32,
            iid: *const windows::core::GUID,
            obj: *mut *mut core::ffi::c_void,
        ) -> windows::core::HRESULT;
    }
}

fn wake_provider(hwnd: HWND) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    use uia_bindings::{Keepalive, NopStructureHandler};
    static KEEP: OnceLock<Mutex<(HashSet<isize>, Vec<Keepalive>)>> = OnceLock::new();
    let keep = KEEP.get_or_init(|| Mutex::new((HashSet::new(), Vec::new())));
    if let Ok(g) = keep.lock() {
        if g.0.contains(&(hwnd.0 as isize)) {
            return; // already subscribed for this window
        }
    }
    unsafe {
        use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
        use windows::Win32::UI::Accessibility::{
            CUIAutomation, IUIAutomation, IUIAutomationStructureChangedEventHandler,
            TreeScope_Subtree,
        };
        let Ok(uia) = CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER) else {
            return;
        };
        let Ok(root) = uia.ElementFromHandle(hwnd) else {
            return;
        };
        // MSAA wake-up: Chromium enables its full accessibility tree when
        // an MSAA client requests the render widget's IAccessible. Ask the
        // content host window (Chrome_RenderWidgetHostHWND) and the frame
        // itself; the returned interface is intentionally parked.
        let probe = |target: windows::Win32::Foundation::HWND, tag: &str| unsafe {
            use windows::core::GUID;
            let iid = GUID::from_u128(0x61873670_DF8D_11D0_899A_00A0C90557AC);
            let mut obj: *mut core::ffi::c_void = std::ptr::null_mut();
            let hr = uia_bindings::AccessibleObjectFromWindow(target, 0xFFFF_FFFC, &iid, &mut obj);
            dbg_log(&format!("[msaa-wake] {tag} hr={hr:?} obj={}", !obj.is_null()));
        };
        {
            use windows::core::PCWSTR;
            use windows::Win32::UI::WindowsAndMessaging::FindWindowExW;
            let cls: Vec<u16> = "Chrome_RenderWidgetHostHWND".encode_utf16().chain(std::iter::once(0)).collect();
            probe(hwnd, "frame");
            if let Ok(child) = unsafe { FindWindowExW(Some(hwnd), None, PCWSTR(cls.as_ptr()), PCWSTR::null()) } {
                probe(child, "render-widget");
            } else {
                dbg_log("[msaa-wake] no render-widget child");
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        let handler: IUIAutomationStructureChangedEventHandler = NopStructureHandler.into();
        match uia.AddStructureChangedEventHandler(&root, TreeScope_Subtree, None, &handler) {
            Ok(()) => {
                if let Ok(mut g) = keep.lock() {
                    g.0.insert(hwnd.0 as isize);
                    g.1.push(Keepalive(handler));
                }
                // Give the UIA core a beat to bring the provider up.
                std::thread::sleep(std::time::Duration::from_millis(250));
                // Diagnostic: does THIS (subscribed, warm) instance see the
                // full subtree via FindAll while a fresh crate instance sees
                // only 3 nodes? Answers whether tree cache is per-instance.
                if let Ok(cond) = uia.CreateTrueCondition() {
                    match root.FindAll(TreeScope_Subtree, &cond) {
                        Ok(els) => dbg_log(&format!("[wake-findall] hwnd={hwnd:?} count={}", els.Length().unwrap_or(-1))),
                        Err(e) => dbg_log(&format!("[wake-findall-err] hwnd={hwnd:?} {e}")),
                    }
                }
            }
            Err(e) => dbg_log(&format!("[wake-fail] hwnd={hwnd:?} {e}")),
        }
    }
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
    wake_provider(hwnd);
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
    let mut elements: Vec<ElementRecord> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut last_document: Option<String> = None;

    walk(
        &walker,
        &root,
        0,
        started,
        &mut elements,
        &mut lines,
        &mut last_document,
    );

    // Header line: title from the root element's Name.
    let title = one_line(&root.get_name().unwrap_or_default());
    let mut out_lines = vec![format!("Window: \"{title}\", App: {app_name}.")];
    out_lines.append(&mut lines);
    let tree = out_lines.join("\n");

    // Focused element: rendered as its tree line when inside this subtree.
    let focused_element = uia.get_focused_element().ok().and_then(|f| {
        let fid = f.get_runtime_id().ok()?;
        elements.iter().position(|r| {
            r.element
                .get_runtime_id()
                .map(|id| id == fid)
                .unwrap_or(false)
        })
    }).and_then(|i| out_lines.get(i + 1).cloned()); // +1: skip header line

    let built = BuiltTree {
        tree,
        elements,
        focused_element,
        document_text: last_document,
    };
    dbg_log(&format!(
        "[ok] hwnd={hwnd:?} elements={} tree_len={} ms={}",
        built.elements.len(),
        built.tree.len(),
        now_ms() - started
    ));
    Ok(built)
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
