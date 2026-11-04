// UI Automation tree construction for get_window_state. Uses the control
// view walker with a property cache request so each node costs one round
// trip. The returned element records back the element_index used by
// click_element / set_value / perform_secondary_action.

use serde_json::{json, Value};
use uiautomation::core::{UIAutomation, UIElement};
use uiautomation::types::UIProperty;
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
    /// Indented text rendering with element indexes.
    pub tree: String,
    /// Elements in index order (index == position in this vec).
    pub elements: Vec<ElementRecord>,
    /// Focused element descriptor, if any.
    pub focused: Option<Value>,
}

const MAX_ELEMENTS: usize = 600;
const MAX_DEPTH: usize = 32;
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
        let _ = writeln!(f, "{}", msg);
    }
}

fn cache_request(uia: &UIAutomation) -> uiautomation::Result<uiautomation::core::UICacheRequest> {
    let req = uia.create_cache_request()?;
    for p in [
        UIProperty::Name,
        UIProperty::ControlType,
        UIProperty::BoundingRectangle,
        UIProperty::IsOffscreen,
        UIProperty::IsEnabled,
    ] {
        let _ = req.add_property(p);
    }
    // Patterns element actions need at click/set_value/secondary time.
    use uiautomation::patterns::UIPatternType;
    for p in [
        UIPatternType::Value,
        UIPatternType::ExpandCollapse,
        UIPatternType::Scroll,
        UIPatternType::Invoke,
        UIPatternType::Window,
        UIPatternType::Transform,
    ] {
        let _ = req.add_pattern(p);
    }
    Ok(req)
}

/// Build the accessibility tree for `hwnd`.
pub fn build_tree(hwnd: HWND) -> Result<BuiltTree, String> {
    let uia = UIAutomation::new().map_err(|e| {
        dbg_log(&format!("[init-fail] {e}"));
        format!("uia init: {e}")
    })?;
    let req = cache_request(&uia).map_err(|e| format!("cache req: {e}"))?;
    let root = uia
        .element_from_handle_build_cache(
            uiautomation::types::Handle::from(hwnd.0 as isize),
            &req,
        )
        .map_err(|e| {
            dbg_log(&format!("[from-handle-fail] hwnd={hwnd:?} {e}"));
            format!("element from handle: {e}")
        })?;
    let walker = uia
        .get_control_view_walker()
        .map_err(|e| format!("walker: {e}"))?;

    let started = now_ms();
    let mut elements: Vec<ElementRecord> = Vec::new();
    let mut lines: Vec<String> = Vec::new();

    walk(
        &walker,
        &req,
        &root,
        0,
        started,
        &mut elements,
        &mut lines,
    );

    // Focused element descriptor (best effort).
    let focused = uia
        .get_focused_element_build_cache(&req)
        .ok()
        .map(|el| describe(&el));

    let built = BuiltTree {
        tree: lines.join("\n"),
        elements,
        focused,
    };
    dbg_log(&format!(
        "[ok] hwnd={hwnd:?} elements={} tree_len={} ms={}",
        built.elements.len(),
        built.tree.len(),
        now_ms() - started
    ));
    Ok(built)
}
fn describe(el: &UIElement) -> Value {
    let name = el.get_name().unwrap_or_default();
    let ct = el
        .get_control_type()
        .map(|c| format!("{c:?}"))
        .unwrap_or_default();
    json!({ "name": name, "controlType": ct })
}

fn walk(
    walker: &uiautomation::core::UITreeWalker,
    req: &uiautomation::core::UICacheRequest,
    node: &UIElement,
    depth: usize,
    started: u64,
    elements: &mut Vec<ElementRecord>,
    lines: &mut Vec<String>,
) {
    if elements.len() >= MAX_ELEMENTS || depth > MAX_DEPTH {
        return;
    }
    if (now_ms() - started) as u128 > TREE_BUDGET_MS {
        return;
    }

    let name = node.get_name().unwrap_or_default();
    let control_type = node
        .get_control_type()
        .map(|c| format!("{c:?}"))
        .unwrap_or_else(|_| "Unknown".into());
    let rect = node
        .get_bounding_rectangle()
        .map(|r| (r.get_left(), r.get_top(), r.get_width(), r.get_height()))
        .unwrap_or((0, 0, 0, 0));
    let offscreen = node.is_offscreen().unwrap_or(false);
    let enabled = node.is_enabled().unwrap_or(true);

    // Skip invisible leaves; keep containers so their visible children
    // still attach to a sensible parent.
    let empty_rect = rect.2 <= 0 || rect.3 <= 0;
    if offscreen && empty_rect {
        return;
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

    let mut label = format!("{}[{}] {}", "  ".repeat(depth), index, control_type);
    let trimmed = name.trim();
    if !trimmed.is_empty() {
        let shown: String = trimmed.chars().take(120).collect();
        label.push_str(&format!(" {shown:?}"));
    }
    if rect != (0, 0, 0, 0) {
        label.push_str(&format!(" ({},{},{},{})", rect.0, rect.1, rect.2, rect.3));
    }
    if offscreen {
        label.push_str(" [offscreen]");
    }
    if !enabled {
        label.push_str(" [disabled]");
    }
    lines.push(label);

    // Recurse through children.
    let mut child = walker.get_first_child_build_cache(node, req).ok();
    while let Some(c) = child {
        walk(walker, req, &c, depth + 1, started, elements, lines);
        if elements.len() >= MAX_ELEMENTS || (now_ms() - started) as u128 > TREE_BUDGET_MS {
            break;
        }
        child = walker.get_next_sibling_build_cache(&c, req).ok();
    }
}
