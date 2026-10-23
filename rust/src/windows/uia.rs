//! UI Automation client over COM.
//!
//! Reading a property of another process' element is a cross-process call.
//! Instead of asking one property at a time (what the Python `uiautomation`
//! package does), every property tree rendering needs is put in one
//! `IUIAutomationCacheRequest`, and a whole subtree comes back from one
//! `BuildUpdatedCache` call; the walk afterwards only reads that local cache.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, SAFEARRAY,
};
use windows::Win32::System::Variant::{
    VARIANT, VT_ARRAY, VT_BOOL, VT_BSTR, VT_I4, VariantClear,
};
use windows::Win32::UI::Accessibility::*;
use windows::core::{BSTR, Interface};

use super::logic::Props;
use crate::backend::R;

/// Properties fetched with every element, in `Props` order.
const PROPS: &[UIA_PROPERTY_ID] = &[
    UIA_NamePropertyId,
    UIA_ControlTypePropertyId,
    UIA_IsOffscreenPropertyId,
    UIA_HasKeyboardFocusPropertyId,
    UIA_IsEnabledPropertyId,
    UIA_IsKeyboardFocusablePropertyId,
    UIA_NativeWindowHandlePropertyId,
    UIA_ProcessIdPropertyId,
    UIA_RuntimeIdPropertyId,
    UIA_IsInvokePatternAvailablePropertyId,
    UIA_IsTogglePatternAvailablePropertyId,
    UIA_IsExpandCollapsePatternAvailablePropertyId,
    UIA_IsSelectionItemPatternAvailablePropertyId,
    UIA_IsValuePatternAvailablePropertyId,
    UIA_IsTextPatternAvailablePropertyId,
    UIA_IsLegacyIAccessiblePatternAvailablePropertyId,
    UIA_ToggleToggleStatePropertyId,
    UIA_SelectionItemIsSelectedPropertyId,
    UIA_ExpandCollapseExpandCollapseStatePropertyId,
    UIA_ValueValuePropertyId,
    UIA_ValueIsReadOnlyPropertyId,
    UIA_LegacyIAccessibleDefaultActionPropertyId,
];

/// Properties needed to list and match top-level windows.
const WINDOW_PROPS: &[UIA_PROPERTY_ID] = &[
    UIA_NamePropertyId,
    UIA_ControlTypePropertyId,
    UIA_NativeWindowHandlePropertyId,
    UIA_ProcessIdPropertyId,
    UIA_RuntimeIdPropertyId,
];

/// Ids for elements without a RuntimeId; negative so they never collide.
static NEXT_LOCAL_ID: AtomicI32 = AtomicI32::new(-1);

fn err(e: windows::core::Error) -> String {
    e.message()
}

/// One UIA element. Equality is the RuntimeId, so the same element fetched
/// twice is one cache key.
#[derive(Clone)]
pub struct Elem {
    pub rid: Arc<[i32]>,
    pub el: IUIAutomationElement,
    /// `el` came from a subtree cache request, so its children (and theirs)
    /// can be read without another cross-process call.
    pub subtree: bool,
}

impl PartialEq for Elem {
    fn eq(&self, other: &Self) -> bool {
        self.rid == other.rid
    }
}

impl Eq for Elem {}

impl std::hash::Hash for Elem {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.rid.hash(state);
    }
}

impl std::fmt::Debug for Elem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Elem{:?}", &self.rid[..])
    }
}

/// An owned VARIANT, cleared on drop.
struct Var(VARIANT);

impl Drop for Var {
    fn drop(&mut self) {
        unsafe {
            let _ = VariantClear(&mut self.0);
        }
    }
}

impl Var {
    fn vt(&self) -> u16 {
        unsafe { self.0.Anonymous.Anonymous.vt.0 }
    }

    fn bool(&self) -> Option<bool> {
        (self.vt() == VT_BOOL.0).then(|| unsafe { self.0.Anonymous.Anonymous.Anonymous.boolVal.0 != 0 })
    }

    fn i32(&self) -> Option<i32> {
        (self.vt() == VT_I4.0).then(|| unsafe { self.0.Anonymous.Anonymous.Anonymous.lVal })
    }

    fn string(&self) -> Option<String> {
        (self.vt() == VT_BSTR.0)
            .then(|| unsafe { self.0.Anonymous.Anonymous.Anonymous.bstrVal.to_string() })
    }

    /// VT_ARRAY | VT_I4, as RuntimeId is returned.
    fn i32_array(&self) -> Option<Vec<i32>> {
        if self.vt() != (VT_ARRAY.0 | VT_I4.0) {
            return None;
        }
        unsafe { safearray_i32(self.0.Anonymous.Anonymous.Anonymous.parray) }
    }
}

unsafe fn safearray_i32(sa: *const SAFEARRAY) -> Option<Vec<i32>> {
    if sa.is_null() {
        return None;
    }
    unsafe {
        let sa = &*sa;
        if sa.cDims != 1 || sa.pvData.is_null() {
            return None;
        }
        let n = sa.rgsabound[0].cElements as usize;
        Some(std::slice::from_raw_parts(sa.pvData as *const i32, n).to_vec())
    }
}

fn cached(el: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<Var> {
    unsafe { el.GetCachedPropertyValue(id).ok().map(Var) }
}

/// Cached properties of `el`, with Python's fallbacks for anything unreadable.
pub fn props(el: &IUIAutomationElement) -> Props {
    let d = Props::default();
    let b = |id, default: bool| cached(el, id).and_then(|v| v.bool()).unwrap_or(default);
    let i = |id| cached(el, id).and_then(|v| v.i32()).unwrap_or(0);
    let s = |id| cached(el, id).and_then(|v| v.string()).unwrap_or_default();
    Props {
        name: s(UIA_NamePropertyId),
        control_type: i(UIA_ControlTypePropertyId),
        offscreen: b(UIA_IsOffscreenPropertyId, d.offscreen),
        focused: b(UIA_HasKeyboardFocusPropertyId, d.focused),
        enabled: b(UIA_IsEnabledPropertyId, d.enabled),
        focusable: b(UIA_IsKeyboardFocusablePropertyId, d.focusable),
        hwnd: i(UIA_NativeWindowHandlePropertyId),
        pid: i(UIA_ProcessIdPropertyId) as u32,
        invoke: b(UIA_IsInvokePatternAvailablePropertyId, false),
        toggle: b(UIA_IsTogglePatternAvailablePropertyId, false),
        expand: b(UIA_IsExpandCollapsePatternAvailablePropertyId, false),
        select: b(UIA_IsSelectionItemPatternAvailablePropertyId, false),
        value: b(UIA_IsValuePatternAvailablePropertyId, false),
        text: b(UIA_IsTextPatternAvailablePropertyId, false),
        legacy: b(UIA_IsLegacyIAccessiblePatternAvailablePropertyId, false),
        toggle_state: i(UIA_ToggleToggleStatePropertyId),
        selected: b(UIA_SelectionItemIsSelectedPropertyId, d.selected),
        expand_state: i(UIA_ExpandCollapseExpandCollapseStatePropertyId),
        value_text: s(UIA_ValueValuePropertyId),
        read_only: b(UIA_ValueIsReadOnlyPropertyId, d.read_only),
        default_action: s(UIA_LegacyIAccessibleDefaultActionPropertyId),
    }
}

fn runtime_id(el: &IUIAutomationElement) -> Arc<[i32]> {
    cached(el, UIA_RuntimeIdPropertyId)
        .and_then(|v| v.i32_array())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![NEXT_LOCAL_ID.fetch_sub(1, Ordering::Relaxed)])
        .into()
}

struct Requests {
    /// Element plus its whole raw-view subtree, every property in PROPS.
    subtree: IUIAutomationCacheRequest,
    /// One element, every property in PROPS.
    element: IUIAutomationCacheRequest,
    /// Top-level windows: just what `apps` and matching need.
    windows: IUIAutomationCacheRequest,
    all: IUIAutomationCondition,
}

impl Requests {
    fn new(auto: &IUIAutomation) -> R<Self> {
        unsafe {
            // The Python backend walks the raw view; so do the cache requests.
            let all = auto.CreateTrueCondition().map_err(err)?;
            let request = |props: &[UIA_PROPERTY_ID], scope: TreeScope| -> R<IUIAutomationCacheRequest> {
                let req = auto.CreateCacheRequest().map_err(err)?;
                for p in props {
                    req.AddProperty(*p).map_err(err)?;
                }
                req.SetTreeScope(scope).map_err(err)?;
                req.SetTreeFilter(&all).map_err(err)?;
                req.SetAutomationElementMode(AutomationElementMode_Full).map_err(err)?;
                Ok(req)
            };
            Ok(Requests {
                subtree: request(PROPS, TreeScope_Subtree)?,
                element: request(PROPS, TreeScope_Element)?,
                windows: request(WINDOW_PROPS, TreeScope_Element)?,
                all,
            })
        }
    }
}

pub struct Uia {
    auto: IUIAutomation,
    /// None when the UIA client lacks cache requests (Wine): reads then
    /// find nothing, like Python's backend when every UIA call fails, and
    /// keyboard and pointer input still work.
    req: Option<Requests>,
}

impl Uia {
    pub fn new() -> R<Self> {
        unsafe {
            // MTA: UIA clients must not depend on a message pump.
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().map_err(|e| format!("COM: {}", err(e)))?;
            let auto: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("UI Automation: {}", err(e)))?;
            let req = Requests::new(&auto)
                .inspect_err(|e| eprintln!("desktop-mcp: UI Automation cache requests unavailable: {e}"))
                .ok();
            Ok(Uia { auto, req })
        }
    }

    fn wrap(el: IUIAutomationElement, subtree: bool) -> Elem {
        Elem { rid: runtime_id(&el), el, subtree }
    }

    /// Children of an array, cut at the first element of a control type
    /// `uiautomation` does not know (its GetChildren() stops there too).
    fn elems(arr: Option<IUIAutomationElementArray>, subtree: bool) -> Vec<Elem> {
        let Some(arr) = arr else { return Vec::new() };
        let n = unsafe { arr.Length() }.unwrap_or(0);
        let mut out = Vec::with_capacity(n.max(0) as usize);
        for i in 0..n {
            let Ok(el) = (unsafe { arr.GetElement(i) }) else { break };
            if super::logic::role_name(props_control_type(&el)).is_none() {
                break;
            }
            out.push(Self::wrap(el, subtree));
        }
        out
    }

    /// Top-level windows with their name, role, handle and process id cached.
    pub fn top_level(&self) -> Vec<(Elem, Props)> {
        let Some(req) = &self.req else { return Vec::new() };
        let arr = unsafe {
            self.auto.GetRootElement()
                .and_then(|root| root.FindAllBuildCache(TreeScope_Children, &req.all, &req.windows))
        };
        Self::elems(arr.ok(), false).into_iter().map(|e| {
            let p = props(&e.el);
            (e, p)
        }).collect()
    }

    /// Fetch `el` and everything below it in one cross-process call.
    pub fn fetch_subtree(&self, el: &IUIAutomationElement) -> Option<IUIAutomationElement> {
        unsafe { el.BuildUpdatedCache(&self.req.as_ref()?.subtree).ok() }
    }

    /// Fetch every property of one element (no children).
    pub fn fetch(&self, el: &IUIAutomationElement) -> Option<Props> {
        unsafe { el.BuildUpdatedCache(&self.req.as_ref()?.element).ok().map(|e| props(&e)) }
    }

    /// Cached children of an element from `fetch_subtree`.
    pub fn cached_children(el: &IUIAutomationElement) -> Vec<Elem> {
        Self::elems(unsafe { el.GetCachedChildren() }.ok(), true)
    }

    pub fn has_focus(&self, el: &IUIAutomationElement) -> bool {
        unsafe { el.CurrentHasKeyboardFocus() }.map(|b| b.as_bool()).unwrap_or(false)
    }

    pub fn set_focus(&self, el: &IUIAutomationElement) -> bool {
        unsafe { el.SetFocus() }.is_ok()
    }

    pub fn bounds(&self, el: &IUIAutomationElement) -> Option<(i32, i32, i32, i32)> {
        let r = unsafe { el.CurrentBoundingRectangle() }.ok()?;
        Some((r.left, r.top, r.right, r.bottom))
    }

    fn pattern<T: Interface>(el: &IUIAutomationElement, id: UIA_PATTERN_ID) -> Option<T> {
        unsafe { el.GetCurrentPatternAs::<T>(id) }.ok()
    }

    /// TextPattern document text, up to `limit` characters; None when the
    /// element has no TextPattern or reading it failed.
    pub fn document_text(&self, el: &IUIAutomationElement, limit: usize) -> Option<String> {
        let tp: IUIAutomationTextPattern = Self::pattern(el, UIA_TextPatternId)?;
        let max = i32::try_from(limit).unwrap_or(i32::MAX);
        unsafe { tp.DocumentRange().and_then(|r| r.GetText(max)) }.ok().map(|s| s.to_string())
    }

    /// Live ValuePattern value.
    pub fn value(&self, el: &IUIAutomationElement) -> Option<String> {
        let vp: IUIAutomationValuePattern = Self::pattern(el, UIA_ValuePatternId)?;
        unsafe { vp.CurrentValue() }.ok().map(|s| s.to_string())
    }

    /// Live ValuePattern, None when absent or read-only.
    pub fn writable_value(&self, el: &IUIAutomationElement) -> Option<IUIAutomationValuePattern> {
        let vp: IUIAutomationValuePattern = Self::pattern(el, UIA_ValuePatternId)?;
        let read_only = unsafe { vp.CurrentIsReadOnly() }.map(|b| b.as_bool()).unwrap_or(true);
        (!read_only).then_some(vp)
    }

    pub fn set_value(vp: &IUIAutomationValuePattern, text: &str) -> bool {
        unsafe { vp.SetValue(&BSTR::from(text)) }.is_ok()
    }

    /// Run one entry of the Python action table.
    pub fn perform(&self, el: &IUIAutomationElement, action: &str) -> bool {
        unsafe {
            match action {
                "invoke" => Self::pattern::<IUIAutomationInvokePattern>(el, UIA_InvokePatternId)
                    .is_some_and(|p| p.Invoke().is_ok()),
                "toggle" => Self::pattern::<IUIAutomationTogglePattern>(el, UIA_TogglePatternId)
                    .is_some_and(|p| p.Toggle().is_ok()),
                "expand" => Self::pattern::<IUIAutomationExpandCollapsePattern>(
                    el, UIA_ExpandCollapsePatternId).is_some_and(|p| p.Expand().is_ok()),
                "collapse" => Self::pattern::<IUIAutomationExpandCollapsePattern>(
                    el, UIA_ExpandCollapsePatternId).is_some_and(|p| p.Collapse().is_ok()),
                "select" => Self::pattern::<IUIAutomationSelectionItemPattern>(
                    el, UIA_SelectionItemPatternId).is_some_and(|p| p.Select().is_ok()),
                "default" => Self::pattern::<IUIAutomationLegacyIAccessiblePattern>(
                    el, UIA_LegacyIAccessiblePatternId).is_some_and(|p| p.DoDefaultAction().is_ok()),
                _ => false,
            }
        }
    }
}

/// Cached control type, or the live one for elements fetched without it.
fn props_control_type(el: &IUIAutomationElement) -> i32 {
    cached(el, UIA_ControlTypePropertyId)
        .and_then(|v| v.i32())
        .or_else(|| unsafe { el.CurrentControlType() }.ok().map(|c| c.0))
        .unwrap_or(0)
}
