//! The C interfaces that AppKit does not cover: the window list, the displays, the screen
//! recording permission, and Carbon's global hotkeys, which need no accessibility permission.
//!
//! Positions here are in global points with the origin at the top left of the main display and y
//! growing downwards, as CoreGraphics and ScreenCaptureKit have them. AppKit's windows count y
//! upwards from the bottom; `to_cocoa` and `from_cocoa` convert.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSRect, NSString};
use std::ffi::c_void;

/// A rectangle in global points, y downwards.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Area {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Area {
    pub fn from_corners((x0, y0): (f64, f64), (x1, y1): (f64, f64)) -> Self {
        Area { x: x0.min(x1), y: y0.min(y1), w: (x1 - x0).abs(), h: (y1 - y0).abs() }
    }

    pub fn right(&self) -> f64 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }

    pub fn contains(&self, (x, y): (f64, f64)) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    pub fn holds(&self, other: &Area) -> bool {
        other.x >= self.x && other.y >= self.y && other.right() <= self.right() && other.bottom() <= self.bottom()
    }

    pub fn intersect(&self, other: &Area) -> Area {
        let (x, y) = (self.x.max(other.x), self.y.max(other.y));
        let (right, bottom) = (self.right().min(other.right()), self.bottom().min(other.bottom()));
        Area { x, y, w: (right - x).max(0.0), h: (bottom - y).max(0.0) }
    }

    pub fn centre(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn grown(&self, by: f64) -> Area {
        Area { x: self.x - by, y: self.y - by, w: self.w + by * 2.0, h: self.h + by * 2.0 }
    }

    pub fn cg(&self) -> CGRect {
        CGRect::new(CGPoint::new(self.x, self.y), CGSize::new(self.w, self.h))
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> *mut c_void;
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGMainDisplayID() -> u32;
}

/// True if the app may read the screen. Asking shows the system's prompt once; after that the
/// answer is changed in System Settings, and takes effect when the app starts again.
pub fn may_record_screen(ask: bool) -> bool {
    unsafe { CGPreflightScreenCaptureAccess() || (ask && CGRequestScreenCaptureAccess()) }
}

pub struct Display {
    pub id: u32,
    pub bounds: Area,
}

pub fn displays() -> Vec<Display> {
    let mut ids = [0u32; 16];
    let mut count = 0;
    if unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } != 0 {
        return Vec::new();
    }
    ids[..count as usize].iter().map(|&id| Display { id, bounds: area(unsafe { CGDisplayBounds(id) }) }).collect()
}

/// The display that holds the point, or the nearest one.
pub fn display_at(at: (f64, f64)) -> Option<Display> {
    let all = displays();
    let distance = |d: &Display| {
        let b = &d.bounds;
        let dx = (b.x - at.0).max(at.0 - b.right()).max(0.0);
        let dy = (b.y - at.1).max(at.1 - b.bottom()).max(0.0);
        dx * dx + dy * dy
    };
    all.into_iter().min_by(|a, b| distance(a).total_cmp(&distance(b)))
}

/// The height of the main display, which links the two ways of counting y.
fn main_height() -> f64 {
    unsafe { CGDisplayBounds(CGMainDisplayID()) }.size.height
}

pub fn from_cocoa(rect: NSRect) -> Area {
    Area { x: rect.origin.x, y: main_height() - rect.origin.y - rect.size.height, w: rect.size.width, h: rect.size.height }
}

pub fn to_cocoa(area: &Area) -> NSRect {
    NSRect::new(CGPoint::new(area.x, main_height() - area.y - area.h), CGSize::new(area.w, area.h))
}

/// The mouse, in global points.
pub fn mouse() -> (f64, f64) {
    let at = objc2_app_kit::NSEvent::mouseLocation();
    (at.x, main_height() - at.y)
}

fn area(rect: CGRect) -> Area {
    Area { x: rect.origin.x, y: rect.origin.y, w: rect.size.width, h: rect.size.height }
}

// --- The window list

const ON_SCREEN_ONLY: u32 = 1 << 0;
const INCLUDING_WINDOW: u32 = 1 << 3;
const EXCLUDE_DESKTOP: u32 = 1 << 4;

pub struct WindowInfo {
    pub id: u32,
    pub bounds: Area,
    pub on_screen: bool,
}

fn number(dict: &NSDictionary<NSString, AnyObject>, key: &str) -> Option<Retained<NSNumber>> {
    dict.objectForKey(&NSString::from_str(key))?.downcast::<NSNumber>().ok()
}

fn window_list(option: u32, relative_to: u32) -> Vec<WindowInfo> {
    let raw = unsafe { CGWindowListCopyWindowInfo(option, relative_to) };
    // A CFArray of CFDictionary is an NSArray of NSDictionary, and owned like one.
    let Some(list) = (unsafe { Retained::<NSArray<NSDictionary<NSString, AnyObject>>>::from_raw(raw.cast()) }) else { return Vec::new() };
    let me = std::process::id() as i64;
    let mut out = Vec::new();
    for window in list.iter() {
        let get = |key| number(&window, key);
        // Ordinary windows only: not the menu bar, the Dock, or anything of this app's.
        let ordinary = get("kCGWindowLayer").is_some_and(|n| n.integerValue() == 0) && get("kCGWindowOwnerPID").is_some_and(|n| n.integerValue() as i64 != me);
        let visible = get("kCGWindowAlpha").is_none_or(|n| n.doubleValue() > 0.0);
        let Some(id) = get("kCGWindowNumber") else { continue };
        let Some(bounds) = window.objectForKey(&NSString::from_str("kCGWindowBounds")).and_then(|b| b.downcast::<NSDictionary>().ok()) else { continue };
        // SAFETY: the keys of a window's bounds are strings.
        let bounds = unsafe { Retained::cast_unchecked::<NSDictionary<NSString, AnyObject>>(bounds) };
        let side = |key| number(&bounds, key).map_or(0.0, |n| n.doubleValue());
        if ordinary && visible {
            out.push(WindowInfo {
                id: id.integerValue() as u32,
                bounds: Area { x: side("X"), y: side("Y"), w: side("Width"), h: side("Height") },
                on_screen: get("kCGWindowIsOnscreen").is_some_and(|n| n.integerValue() != 0),
            });
        }
    }
    out
}

/// The frontmost ordinary window at the point, and its bounds.
pub fn window_at(at: (f64, f64)) -> Option<WindowInfo> {
    window_list(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0).into_iter().find(|w| w.bounds.contains(at) && w.bounds.w > 1.0 && w.bounds.h > 1.0)
}

/// One window, on screen or not; None once it has closed.
pub fn window(id: u32) -> Option<WindowInfo> {
    window_list(INCLUDING_WINDOW, id).into_iter().find(|w| w.id == id)
}

// --- Carbon hotkeys

#[repr(C)]
struct EventTypeSpec {
    class: u32,
    kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyId {
    signature: u32,
    id: u32,
}

type Handler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(target: *mut c_void, handler: Handler, count: u32, list: *const EventTypeSpec, data: *mut c_void, out: *mut *mut c_void) -> i32;
    fn RegisterEventHotKey(code: u32, modifiers: u32, id: EventHotKeyId, target: *mut c_void, options: u32, out: *mut *mut c_void) -> i32;
    fn UnregisterEventHotKey(hotkey: *mut c_void) -> i32;
    fn GetEventParameter(event: *mut c_void, name: u32, kind: u32, actual: *mut u32, size: usize, actual_size: *mut usize, data: *mut c_void) -> i32;
    fn GetEventKind(event: *mut c_void) -> u32;
}

const KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const PRESSED: u32 = 5;
const RELEASED: u32 = 6;
const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
const HOTKEY_ID: u32 = u32::from_be_bytes(*b"hkid");
const SIGNATURE: u32 = u32::from_be_bytes(*b"Snba");

static mut ON_HOTKEY: Option<fn(i32, bool)> = None;

extern "C" fn on_hotkey(_: *mut c_void, event: *mut c_void, _: *mut c_void) -> i32 {
    let mut id = EventHotKeyId { signature: 0, id: 0 };
    let size = size_of::<EventHotKeyId>();
    let found = unsafe { GetEventParameter(event, DIRECT_OBJECT, HOTKEY_ID, std::ptr::null_mut(), size, std::ptr::null_mut(), &mut id as *mut _ as *mut c_void) };
    if found == 0 && id.signature == SIGNATURE {
        let pressed = unsafe { GetEventKind(event) } == PRESSED;
        // SAFETY: written once, on the main thread, before any hotkey exists.
        if let Some(handle) = unsafe { ON_HOTKEY } {
            handle(id.id as i32, pressed);
        }
    }
    0
}

/// Sends every press and release of a registered hotkey to `handle`, on the main thread.
pub fn listen_for_hotkeys(handle: fn(i32, bool)) {
    unsafe {
        ON_HOTKEY = Some(handle);
        let kinds = [EventTypeSpec { class: KEYBOARD, kind: PRESSED }, EventTypeSpec { class: KEYBOARD, kind: RELEASED }];
        InstallEventHandler(GetApplicationEventTarget(), on_hotkey, kinds.len() as u32, kinds.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut());
    }
}

pub struct HotkeyRef(*mut c_void);

/// Takes a key system-wide. Carbon's key code and modifiers, not the app's.
pub fn register_hotkey(id: i32, code: u32, modifiers: u32) -> Option<HotkeyRef> {
    let mut out = std::ptr::null_mut();
    let status = unsafe { RegisterEventHotKey(code, modifiers, EventHotKeyId { signature: SIGNATURE, id: id as u32 }, GetApplicationEventTarget(), 0, &mut out) };
    (status == 0 && !out.is_null()).then_some(HotkeyRef(out))
}

impl Drop for HotkeyRef {
    fn drop(&mut self) {
        unsafe { UnregisterEventHotKey(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn areas_meet_where_they_overlap() {
        let a = Area { x: 0.0, y: 0.0, w: 100.0, h: 50.0 };
        let b = Area { x: 80.0, y: 40.0, w: 100.0, h: 100.0 };
        assert_eq!(a.intersect(&b), Area { x: 80.0, y: 40.0, w: 20.0, h: 10.0 });
        assert!(a.holds(&Area { x: 10.0, y: 10.0, w: 5.0, h: 5.0 }) && !a.holds(&b));
        assert_eq!(Area::from_corners((5.0, 9.0), (1.0, 2.0)), Area { x: 1.0, y: 2.0, w: 4.0, h: 7.0 });
    }
}
