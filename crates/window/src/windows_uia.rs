//! A window's interface as UI Automation publishes it: its elements' tree,
//! and an element pressed, toggled, selected, expanded, focused or given a
//! value by role and name — for a program on another machine (`cursio
//! window … tree|invoke|set-value`, an AI agent's `window_tree`).
//!
//! UI Automation is what Narrator reads, and every toolkit answers it:
//! Win32 controls, WPF, WinUI, and WebView2/Chromium pages, whose buttons
//! and tabs appear with the role and name the page gave them. So the
//! launcher's "Play" is found as a button named Play, whatever its layout.
//!
//! **One MTA thread per request.** UI Automation's client objects are free
//! threaded and its calls cross into the target process; a dedicated thread
//! initialised for the multi-threaded apartment keeps those calls off the
//! node's runtime and needs no message pump.
//!
//! **Cached reads.** Each child list is fetched with a cache request for the
//! properties the tree shows, so a few thousand elements cost one
//! cross-process call per element rather than one per property.
//!
//! **Ids are runtime ids**, which UI Automation keeps for as long as the
//! element exists, so an element seen in one snapshot can be acted on by id
//! in the next request — after re-walking the tree to find it, since no
//! element is held between requests.

use windows::core::BSTR;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    SAFEARRAY,
};
use windows::Win32::System::Ole::SafeArrayDestroy;
use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_BOOL, VT_BSTR, VT_I4};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCacheRequest, IUIAutomationCondition,
    IUIAutomationElement, IUIAutomationExpandCollapsePattern, IUIAutomationInvokePattern,
    IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern, IUIAutomationValuePattern,
    TreeScope_Children, UIA_BoundingRectanglePropertyId, UIA_ControlTypePropertyId,
    UIA_ExpandCollapseExpandCollapseStatePropertyId, UIA_ExpandCollapsePatternId,
    UIA_HasKeyboardFocusPropertyId, UIA_InvokePatternId, UIA_IsEnabledPropertyId,
    UIA_IsOffscreenPropertyId, UIA_NamePropertyId, UIA_SelectionItemIsSelectedPropertyId,
    UIA_SelectionItemPatternId, UIA_TogglePatternId, UIA_ToggleToggleStatePropertyId,
    UIA_ValuePatternId, UIA_ValueValuePropertyId, UIA_PROPERTY_ID,
};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

use crate::ui_tree::{self, Limits, Pick, UiNode};
pub use cursio_protocol::diag::ElementAct as Act;

/// A snapshot of the window's interface.
pub struct Snapshot {
    pub root: UiNode,
    pub nodes: u32,
    /// The walk stopped at its limits; deeper or later elements are left out.
    pub truncated: bool,
}

/// The role UI Automation's control type id stands for.
fn role(control_type: i32) -> &'static str {
    const ROLES: [&str; 41] = [
        "button",
        "calendar",
        "checkbox",
        "combobox",
        "edit",
        "hyperlink",
        "image",
        "listitem",
        "list",
        "menu",
        "menubar",
        "menuitem",
        "progressbar",
        "radiobutton",
        "scrollbar",
        "slider",
        "spinner",
        "statusbar",
        "tab",
        "tabitem",
        "text",
        "toolbar",
        "tooltip",
        "tree",
        "treeitem",
        "custom",
        "group",
        "thumb",
        "datagrid",
        "dataitem",
        "document",
        "splitbutton",
        "window",
        "pane",
        "header",
        "headeritem",
        "table",
        "titlebar",
        "separator",
        "semanticzoom",
        "appbar",
    ];
    usize::try_from(control_type - 50_000)
        .ok()
        .and_then(|i| ROLES.get(i).copied())
        .unwrap_or("custom")
}

/// Run `work` on a thread of its own in the multi-threaded apartment.
fn on_mta<T: Send>(work: impl FnOnce() -> Result<T, String> + Send) -> Result<T, String> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                // SAFETY: initialised and uninitialised on this thread only.
                unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
                    .ok()
                    .map_err(|e| format!("cannot start COM for UI Automation: {e}"))?;
                let result = work();
                // SAFETY: balances the successful CoInitializeEx above.
                unsafe { CoUninitialize() };
                result
            })
            .join()
            .map_err(|_| "UI Automation's thread panicked".to_owned())?
    })
}

/// A property's cached value, read and released.
struct Value(VARIANT);

impl Value {
    fn read(element: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<Value> {
        // SAFETY: a property cached by the request the element was built with.
        unsafe { element.GetCachedPropertyValue(id) }
            .ok()
            .map(Value)
    }

    fn text(&self) -> Option<String> {
        // SAFETY: the union is read only as the member its tag names.
        unsafe {
            let inner = &self.0.Anonymous.Anonymous;
            (inner.vt == VT_BSTR).then(|| inner.Anonymous.bstrVal.to_string())
        }
    }

    fn int(&self) -> Option<i32> {
        // SAFETY: as above.
        unsafe {
            let inner = &self.0.Anonymous.Anonymous;
            (inner.vt == VT_I4).then_some(inner.Anonymous.lVal)
        }
    }

    fn flag(&self) -> Option<bool> {
        // SAFETY: as above.
        unsafe {
            let inner = &self.0.Anonymous.Anonymous;
            (inner.vt == VT_BOOL).then_some(inner.Anonymous.boolVal.0 != 0)
        }
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        // SAFETY: a VARIANT this value owns, cleared once.
        let _ = unsafe { VariantClear(&mut self.0) };
    }
}

/// The element's runtime id as `a-b-c`.
fn runtime_id(element: &IUIAutomationElement) -> String {
    // SAFETY: a one-dimensional SAFEARRAY of i32 owned by the caller, read
    // within its bounds and destroyed once.
    unsafe {
        let Ok(array) = element.GetRuntimeId() else {
            return String::new();
        };
        if array.is_null() {
            return String::new();
        }
        let sa: &SAFEARRAY = &*array;
        let len = sa.rgsabound[0].cElements as usize;
        let ids = std::slice::from_raw_parts(sa.pvData as *const i32, len)
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("-");
        let _ = SafeArrayDestroy(array);
        ids
    }
}

struct Walk {
    cache: IUIAutomationCacheRequest,
    children: IUIAutomationCondition,
    window: RECT,
    limits: Limits,
    nodes: u32,
    truncated: bool,
    /// Every element seen, by id, for acting on one.
    elements: Vec<(String, IUIAutomationElement)>,
}

impl Walk {
    fn bounds(&self, element: &IUIAutomationElement) -> Option<[f32; 4]> {
        // SAFETY: a property in the cache request.
        let rect = unsafe { element.CachedBoundingRectangle() }.ok()?;
        let (ww, wh) = (
            (self.window.right - self.window.left) as f32,
            (self.window.bottom - self.window.top) as f32,
        );
        let (w, h) = (
            (rect.right - rect.left) as f32,
            (rect.bottom - rect.top) as f32,
        );
        if ww <= 0.0 || wh <= 0.0 || w <= 0.0 || h <= 0.0 {
            return None;
        }
        Some([
            (rect.left - self.window.left) as f32 / ww,
            (rect.top - self.window.top) as f32 / wh,
            w / ww,
            h / wh,
        ])
    }

    fn node(&mut self, element: &IUIAutomationElement, depth: u16) -> UiNode {
        // SAFETY: every read below is of a property the cache request holds.
        let (control, name, enabled, focused, offscreen) = unsafe {
            (
                element.CachedControlType().map(|c| c.0).unwrap_or(0),
                element
                    .CachedName()
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
                element
                    .CachedIsEnabled()
                    .map(|b| b.as_bool())
                    .unwrap_or(true),
                element
                    .CachedHasKeyboardFocus()
                    .map(|b| b.as_bool())
                    .unwrap_or(false),
                element
                    .CachedIsOffscreen()
                    .map(|b| b.as_bool())
                    .unwrap_or(false),
            )
        };
        let mut states = Vec::new();
        if !enabled {
            states.push("disabled");
        }
        if focused {
            states.push("focused");
        }
        if Value::read(element, UIA_SelectionItemIsSelectedPropertyId)
            .and_then(|v| v.flag())
            .unwrap_or(false)
        {
            states.push("selected");
        }
        match Value::read(element, UIA_ToggleToggleStatePropertyId).and_then(|v| v.int()) {
            Some(1) => states.push("checked"),
            Some(2) => states.push("mixed"),
            _ => {}
        }
        match Value::read(element, UIA_ExpandCollapseExpandCollapseStatePropertyId)
            .and_then(|v| v.int())
        {
            Some(0) => states.push("collapsed"),
            Some(1) | Some(2) => states.push("expanded"),
            _ => {}
        }
        if offscreen {
            states.push("offscreen");
        }
        let id = runtime_id(element);
        let node = UiNode {
            id: id.clone(),
            role: role(control).to_owned(),
            name,
            value: Value::read(element, UIA_ValueValuePropertyId).and_then(|v| v.text()),
            states,
            bounds: (!offscreen).then(|| self.bounds(element)).flatten(),
            children: Vec::new(),
        };
        self.elements.push((id, element.clone()));
        let mut node = node;
        if depth >= self.limits.depth {
            return node;
        }
        // SAFETY: the condition and cache request live for the walk.
        let Ok(children) =
            (unsafe { element.FindAllBuildCache(TreeScope_Children, &self.children, &self.cache) })
        else {
            return node;
        };
        // SAFETY: indices within the array's own length.
        let count = unsafe { children.Length() }.unwrap_or(0);
        for i in 0..count {
            if self.nodes >= self.limits.nodes {
                self.truncated = true;
                break;
            }
            // SAFETY: as above.
            let Ok(child) = (unsafe { children.GetElement(i) }) else {
                continue;
            };
            self.nodes += 1;
            node.children.push(self.node(&child, depth + 1));
        }
        node
    }
}

/// Walk the window's tree on the current (MTA) thread.
fn walk(
    raw: isize,
    limits: Limits,
) -> Result<(Snapshot, Vec<(String, IUIAutomationElement)>), String> {
    let window = HWND(raw as _);
    // SAFETY: COM is initialised on this thread by `on_mta`; every object
    // created here is released when it drops.
    unsafe {
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("UI Automation is not available: {e}"))?;
        let cache = uia
            .CreateCacheRequest()
            .map_err(|e| format!("UI Automation: {e}"))?;
        for property in [
            UIA_ControlTypePropertyId,
            UIA_NamePropertyId,
            UIA_BoundingRectanglePropertyId,
            UIA_IsEnabledPropertyId,
            UIA_HasKeyboardFocusPropertyId,
            UIA_IsOffscreenPropertyId,
            UIA_ValueValuePropertyId,
            UIA_ToggleToggleStatePropertyId,
            UIA_SelectionItemIsSelectedPropertyId,
            UIA_ExpandCollapseExpandCollapseStatePropertyId,
        ] {
            cache
                .AddProperty(property)
                .map_err(|e| format!("UI Automation: {e}"))?;
        }
        // The control view: what a screen reader walks, without the purely
        // presentational elements of the raw view.
        let children = uia
            .ControlViewCondition()
            .map_err(|e| format!("UI Automation: {e}"))?;
        let root = uia
            .ElementFromHandleBuildCache(window, &cache)
            .map_err(|e| format!("the window has no UI Automation element: {e}"))?;
        let mut rect = RECT::default();
        GetWindowRect(window, &mut rect).map_err(|e| format!("the window is gone: {e}"))?;
        let mut walk = Walk {
            cache,
            children,
            window: rect,
            limits,
            nodes: 1,
            truncated: false,
            elements: Vec::new(),
        };
        let root = walk.node(&root, 0);
        Ok((
            Snapshot {
                root,
                nodes: walk.nodes,
                truncated: walk.truncated,
            },
            walk.elements,
        ))
    }
}

/// The interface of window `raw`, within `limits`.
pub fn tree(raw: isize, limits: Limits) -> Result<Snapshot, String> {
    on_mta(move || walk(raw, limits).map(|(snapshot, _)| snapshot))
}

/// Do `act` to the element `pick` names in window `raw`; answers what was
/// done, in a sentence.
pub fn act(raw: isize, pick: &Pick, act: &Act, limits: Limits) -> Result<String, String> {
    on_mta(move || {
        let (snapshot, elements) = walk(raw, limits)?;
        let node = ui_tree::find(&snapshot.root, pick)?.clone();
        let element = elements
            .iter()
            .find(|(id, _)| *id == node.id)
            .map(|(_, e)| e.clone())
            .ok_or("the element went away while it was looked for")?;
        let label = format!("{} {:?}", node.role, node.name);
        // SAFETY: patterns asked of a live element on this MTA thread.
        unsafe {
            match act {
                Act::Invoke => match element
                    .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                {
                    Ok(pattern) => pattern
                        .Invoke()
                        .map(|()| format!("invoked {label}"))
                        .map_err(|e| format!("{label} would not invoke: {e}")),
                    // Some elements take a click but publish no Invoke
                    // pattern; a click on their centre is what a person does.
                    Err(_) => match node.bounds {
                        Some([x, y, w, h]) => {
                            crate::windows_app::click(raw, x + w / 2.0, y + h / 2.0, false)?;
                            Ok(format!(
                                "clicked the centre of {label}, which offers no Invoke"
                            ))
                        }
                        None => Err(format!("{label} cannot be invoked and is not on screen")),
                    },
                },
                Act::Toggle => element
                    .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                    .and_then(|p| p.Toggle())
                    .map(|()| format!("toggled {label}"))
                    .map_err(|e| format!("{label} cannot be toggled: {e}")),
                Act::Select => element
                    .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                        UIA_SelectionItemPatternId,
                    )
                    .and_then(|p| p.Select())
                    .map(|()| format!("selected {label}"))
                    .map_err(|e| format!("{label} cannot be selected: {e}")),
                Act::Expand | Act::Collapse => {
                    let pattern = element
                        .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                            UIA_ExpandCollapsePatternId,
                        )
                        .map_err(|e| format!("{label} cannot expand or collapse: {e}"))?;
                    match act {
                        Act::Expand => pattern.Expand().map(|()| format!("expanded {label}")),
                        _ => pattern.Collapse().map(|()| format!("collapsed {label}")),
                    }
                    .map_err(|e| format!("{label}: {e}"))
                }
                Act::Focus => element
                    .SetFocus()
                    .map(|()| format!("focused {label}"))
                    .map_err(|e| format!("{label} cannot take focus: {e}")),
                Act::SetValue(value) => element
                    .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                    .and_then(|p| p.SetValue(&BSTR::from(value.as_str())))
                    .map(|()| format!("set {label} to {} characters", value.chars().count()))
                    .map_err(|e| format!("{label} takes no value: {e}")),
            }
        }
    })
}
