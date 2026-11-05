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

fn wake_provider(hwnd: HWND) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    use uia_bindings::{FocusKeepalive, Keepalive, NopFocusHandler, NopStructureHandler};
    static KEEP: OnceLock<Mutex<(HashSet<isize>, Vec<Keepalive>)>> = OnceLock::new();
    static FOCUS_KEEP: OnceLock<Mutex<Option<FocusKeepalive>>> = OnceLock::new();
    let keep = KEEP.get_or_init(|| Mutex::new((HashSet::new(), Vec::new())));
    let already_waked = keep
        .lock()
        .map(|g| g.0.contains(&(hwnd.0 as isize)))
        .unwrap_or(false);
    if already_waked {
        return; // already subscribed for this window
    }
    unsafe {
        use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
        use windows::Win32::UI::Accessibility::{
            CUIAutomation, IUIAutomation, IUIAutomationFocusChangedEventHandler,
            IUIAutomationStructureChangedEventHandler, TreeScope_Subtree,
        };
        let Ok(uia) = CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER) else {
            return;
        };
        let Ok(root) = uia.ElementFromHandle(hwnd) else {
            return;
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
                    g.0.insert(hwnd.0 as isize);
                    g.1.push(Keepalive(handler));
                }
                // Let the event negotiation run: pump this thread's messages
                // briefly (the original's event thread always pumps).
                pump_messages(60);
            }
            Err(e) => dbg_log(&format!("[wake-fail] hwnd={hwnd:?} {e}")),
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

/// True for Chromium-family windows (Chrome/Edge/Electron all use
/// Chrome_WidgetWin_* top-level classes). Only these need the wake retry:
/// their out-of-process UIA provider answers lazily, while Win32/WinUI
/// providers are synchronous and must not pay the retry latency.
fn is_chromium(hwnd: HWND) -> bool {
    let mut buf = [0u16; 256];
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
        let n = GetClassNameW(hwnd, &mut buf).max(0) as usize;
        let cls = String::from_utf16_lossy(&buf[..n.min(255)]);
        cls.starts_with("Chrome_") || cls.starts_with("Mozilla_")
    }
}

/// Elements at or below this count mean the Chromium provider has not
/// answered yet (measured stub trees are 3-4 elements).
const STUB_MAX: usize = 12;
const RETRY_INTERVAL_MS: u64 = 90;

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
    let mut snapshot = walk_snapshot(&walker, &root, started);

    // Chromium wake settle: the first walk after the event advise often
    // still sees the synthesized stub tree. Re-walk (pumping messages in
    // between, which lets the provider advises complete) until the tree
    // has grown past the stub size and stops growing — bounded by the
    // overall TREE_BUDGET_MS so we stay far under the 10s request budget.
    // OPT-IN for now (SKY_WAKE_RETRY=1). While the raw-COM client cannot wake
    // Chromium at all on this machine, the loop buys nothing and is pure
    // latency: Chrome first request measured 4 lines/3 nodes in 1653 ms with
    // it, and the same 4 lines/3 nodes in 89 ms without it, while the original
    // helper served 868 nodes in ~450 ms in the same run. The loop stays
    // because it is the right shape for the moment the wake does land; it must
    // not cost 1.5 s per request while unproven.
    if snapshot.elements.len() <= STUB_MAX
        && is_chromium(hwnd)
        && std::env::var("SKY_WAKE_RETRY").map(|v| v == "1").unwrap_or(false)
    {
        // Bounded wake window: the original's cold first request takes
        // ~400-550 ms, so a ~1.25 s settle window covers late provider
        // activation without letting an unwakeable provider stall the
        // request. TREE_BUDGET_MS remains the absolute cap.
        let deadline = (started + 1500).min(started + TREE_BUDGET_MS as u64 - 250);
        let mut last = snapshot.elements.len();
        let mut rounds = 0u32;
        while now_ms() < deadline {
            pump_messages(RETRY_INTERVAL_MS);
            rounds += 1;
            let next = walk_snapshot(&walker, &root, started);
            let n = next.elements.len();
            snapshot = next;
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
