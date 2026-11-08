//! A short message beside the cursor, for actions that change nothing else on screen.

use super::draw;
use super::sys::{self, Area};
use super::views::{self, Panel, ToastView};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSBezierPath, NSFont, NSFontAttributeName, NSFontWeightMedium, NSForegroundColorAttributeName, NSScreen, NSStringDrawing};
use objc2_foundation::{NSAttributedStringKey, NSDictionary, NSPoint, NSString};
use std::cell::{Cell, RefCell};
use std::time::Duration;

const SHOW: Duration = Duration::from_millis(1400);
const FONT_PT: f64 = 14.0;
const PAD_X: f64 = 14.0;
const PAD_Y: f64 = 8.0;
const CURSOR_GAP: f64 = 18.0;
const RADIUS: f64 = 10.0;
const BG: u32 = 0x00221E1C;
const TEXT: u32 = 0x00F2F3F0;

thread_local! {
    static WINDOW: RefCell<Option<(Retained<Panel>, Retained<ToastView>)>> = const { RefCell::new(None) };
    static MESSAGE: RefCell<String> = const { RefCell::new(String::new()) };
    static SHOWN: Cell<u64> = const { Cell::new(0) }; // the newest message, by number
}

fn attributes() -> Retained<NSDictionary<NSAttributedStringKey, AnyObject>> {
    let font = NSFont::systemFontOfSize_weight(FONT_PT, unsafe { NSFontWeightMedium });
    let color = draw::color(TEXT);
    unsafe { NSDictionary::from_slices(&[NSFontAttributeName, NSForegroundColorAttributeName], &[&*font as &AnyObject, &*color as &AnyObject]) }
}

pub fn show(text: &str) {
    let mtm = super::with_ui(|ui| ui.mtm);
    let (panel, view) = WINDOW.with_borrow_mut(|window| {
        window
            .get_or_insert_with(|| {
                let view: Retained<ToastView> = views::plain(mtm);
                let panel = Panel::new(mtm, &Area { w: 1.0, h: 1.0, ..Area::default() }, super::level(3), &view, true);
                (panel, view)
            })
            .clone()
    });
    MESSAGE.set(text.to_string());
    panel.setTitle(&NSString::from_str(text)); // for VoiceOver
    let size = unsafe { NSString::from_str(text).sizeWithAttributes(Some(&attributes())) };
    let (width, height) = (size.width.ceil() + PAD_X * 2.0, size.height.ceil() + PAD_Y * 2.0);
    let at = sys::mouse();
    let screens = NSScreen::screens(mtm);
    let screen = screens.iter().find(|s| sys::from_cocoa(s.frame()).contains(at)).or_else(|| screens.firstObject());
    let work = screen.map_or(Area { w: 1.0e5, h: 1.0e5, ..Area::default() }, |s| sys::from_cocoa(s.visibleFrame()));
    let x = (at.0 + CURSOR_GAP).min(work.right() - width);
    let y = (at.1 + CURSOR_GAP).min(work.bottom() - height);
    panel.setFrame_display(sys::to_cocoa(&Area { x, y, w: width, h: height }), true);
    view.setNeedsDisplay(true);
    panel.orderFrontRegardless();
    let number = SHOWN.get() + 1;
    SHOWN.set(number);
    super::after(SHOW, move || {
        if SHOWN.get() == number {
            WINDOW.with_borrow(|window| {
                if let Some((panel, _)) = window {
                    panel.orderOut(None);
                }
            });
        }
    });
}

/// Draws the message into its view, which is `w` by `h` points.
pub fn paint(w: f64, h: f64) {
    draw::color(BG).setFill();
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(draw::rect(0.0, 0.0, w, h), RADIUS, RADIUS).fill();
    MESSAGE.with_borrow(|text| {
        let attributes = attributes();
        let text = NSString::from_str(text);
        let size = unsafe { text.sizeWithAttributes(Some(&attributes)) };
        let at = NSPoint::new((w - size.width) / 2.0, (h - size.height) / 2.0);
        unsafe { text.drawAtPoint_withAttributes(at, Some(&attributes)) };
    });
}
