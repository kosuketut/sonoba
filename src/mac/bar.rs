//! The bar shown over the selection layer: what to translate from and into, and whether to keep
//! watching the region. Like the layer under it, it never takes the keyboard from the app in use.
//!
//! Sizes are in points, y downwards from the bar's top left.

use super::draw;
use super::sys::{self, Area};
use super::views::{BarView, Panel};
use crate::config;
use crate::i18n::t;
use objc2::rc::Retained;
use objc2_app_kit::{NSBezierPath, NSFont, NSFontAttributeName, NSFontWeightMedium, NSForegroundColorAttributeName, NSScreen, NSStringDrawing};
use objc2_foundation::{NSAttributedStringKey, NSDictionary, NSString};
use std::cell::{Cell, RefCell};

const SOURCES: [(&str, &str); 2] = [("auto", "自動"), ("ja", "日本語も翻訳")];
const TARGETS: [(&str, &str); 6] =
    [("ja", "日本語"), ("ja(easy)", "やさしい日本語"), ("en", "英語"), ("ko", "韓国語"), ("zh-hans", "中国語（簡体字）"), ("zh-hant", "中国語（繁体字）")];
const CHIPS: usize = 3;
const ADD_HINT: &str = "Shift+ドラッグで範囲を追加";
const LIVE: usize = 2; // the one chip that is a switch, not a list
const BAR_H: f64 = 48.0;
const PAD: f64 = 8.0;
const CHIP_H: f64 = 32.0;
const CHIP_PAD: f64 = 14.0;
const LIST_W: f64 = 190.0;
const ROW_H: f64 = 32.0;
const TOP: f64 = 16.0; // under the menu bar
const RADIUS: f64 = 12.0;
const CHIP_RADIUS: f64 = 8.0;
const FONT_PT: f64 = 14.0;
const BG: u32 = 0x00221E1C;
const CHIP: u32 = 0x00332E2C;
const HOVER: u32 = 0x0044403C;
const TEXT: u32 = 0x00F2F3F0;
const SOFT: u32 = 0x00ACB1A8;
const ACCENT: u32 = 0x007AC337;

thread_local! {
    static WINDOW: RefCell<Option<(Retained<Panel>, Retained<BarView>)>> = const { RefCell::new(None) };
    static ANCHOR: Cell<(f64, f64)> = const { Cell::new((0.0, 0.0)) }; // centre and top, in global points
    static OPEN: Cell<Option<usize>> = const { Cell::new(None) }; // the chip showing its list
    static HOT: Cell<Option<(usize, Option<usize>)>> = const { Cell::new(None) }; // chip, and row of its list
    static LIVE_ON: Cell<bool> = const { Cell::new(false) };
    static RUNNING: Cell<bool> = const { Cell::new(false) }; // there is a job to add a region to
}

fn options(chip: usize) -> &'static [(&'static str, &'static str)] {
    if chip == 0 { &SOURCES } else { &TARGETS }
}

fn name_of(list: &[(&str, &'static str)], code: &str) -> String {
    t(list.iter().find(|(c, _)| *c == code).map_or(list[0].1, |(_, name)| name))
}

fn labels() -> [String; CHIPS] {
    let config = config::get();
    let onoff = t(if LIVE_ON.get() { "オン" } else { "オフ" });
    [
        format!("{}: {} ▾", t("翻訳元"), name_of(&SOURCES, &config.source)),
        format!("{}: {} ▾", t("翻訳先"), name_of(&TARGETS, &config.target)),
        format!("{}: {onoff}", t("リアルタイム")),
    ]
}

fn attributes(colorref: u32) -> Retained<NSDictionary<NSAttributedStringKey, objc2::runtime::AnyObject>> {
    let font = NSFont::systemFontOfSize_weight(FONT_PT, unsafe { NSFontWeightMedium });
    let color = draw::color(colorref);
    unsafe {
        NSDictionary::from_slices(&[NSFontAttributeName, NSForegroundColorAttributeName], &[&*font as &objc2::runtime::AnyObject, &*color as &objc2::runtime::AnyObject])
    }
}

fn text_width(text: &str) -> f64 {
    unsafe { NSString::from_str(text).sizeWithAttributes(Some(&attributes(TEXT))) }.width.ceil()
}

/// The chips from left to right, and the bar's width.
fn chips() -> ([Area; CHIPS], f64) {
    let top = (BAR_H - CHIP_H) / 2.0;
    let mut x = PAD;
    let mut out = [Area::default(); CHIPS];
    for (area, label) in out.iter_mut().zip(labels()) {
        let w = text_width(&label) + CHIP_PAD * 2.0;
        *area = Area { x, y: top, w, h: CHIP_H };
        x += w + PAD;
    }
    if RUNNING.get() {
        x += text_width(&t(ADD_HINT)) + CHIP_PAD;
    }
    (out, x)
}

/// The open chip's list: one row per option, starting under the bar.
fn rows(chip: usize, chips: &[Area; CHIPS]) -> Vec<Area> {
    let x = chips[chip].x;
    (0..options(chip).len()).map(|i| Area { x, y: BAR_H + i as f64 * ROW_H, w: CHIP_PAD * 2.0 + LIST_W, h: ROW_H }).collect()
}

fn lay_out() {
    WINDOW.with_borrow(|window| {
        let Some((panel, view)) = window else { return };
        let (areas, width) = chips();
        let list = OPEN.get().map_or_else(Vec::new, |chip| rows(chip, &areas));
        let width = width.max(list.last().map_or(0.0, |r| r.right() + PAD));
        let height = list.last().map_or(BAR_H, |r| r.bottom() + PAD);
        let (centre, top) = ANCHOR.get();
        panel.setFrame_display(sys::to_cocoa(&Area { x: (centre - width / 2.0).round(), y: top, w: width, h: height }), true);
        view.setNeedsDisplay(true);
    });
}

pub fn show(live: bool, running: bool) {
    let mtm = super::with_ui(|ui| ui.mtm);
    // On the screen with the mouse, under its menu bar.
    let at = sys::mouse();
    let screens = NSScreen::screens(mtm);
    let screen = screens.iter().find(|s| sys::from_cocoa(s.frame()).contains(at)).or_else(|| screens.firstObject());
    let visible = screen.map_or(Area::default(), |s| sys::from_cocoa(s.visibleFrame()));
    ANCHOR.set((visible.x + visible.w / 2.0, visible.y + TOP));
    OPEN.set(None);
    HOT.set(None);
    LIVE_ON.set(live);
    RUNNING.set(running);
    let view = BarView::new(mtm);
    let panel = Panel::new(mtm, &Area { w: 1.0, h: 1.0, ..Area::default() }, super::level(2), &view, false);
    WINDOW.set(Some((panel.clone(), view)));
    lay_out();
    panel.orderFrontRegardless();
}

pub fn hide() {
    if let Some((panel, _)) = WINDOW.take() {
        panel.orderOut(None);
    }
}

/// True while the switch on the bar is on.
pub fn live() -> bool {
    LIVE_ON.get()
}

fn hit(at: (f64, f64)) -> Option<(usize, Option<usize>)> {
    let (areas, _) = chips();
    if let Some(chip) = OPEN.get()
        && let Some(row) = rows(chip, &areas).iter().position(|r| r.contains(at))
    {
        return Some((chip, Some(row)));
    }
    areas.iter().position(|r| r.contains(at)).map(|chip| (chip, None))
}

fn rounded(area: &Area, radius: f64, colorref: u32) {
    draw::color(colorref).setFill();
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(area.cg(), radius, radius).fill();
}

fn label(text: &str, area: &Area, colorref: u32) {
    let attributes = attributes(colorref);
    let text = NSString::from_str(text);
    let size = unsafe { text.sizeWithAttributes(Some(&attributes)) };
    let at = objc2_foundation::NSPoint::new(area.x + CHIP_PAD, area.y + (area.h - size.height) / 2.0);
    unsafe { text.drawAtPoint_withAttributes(at, Some(&attributes)) };
}

/// Draws the bar into its view, which is `w` by `h` points.
pub fn paint(w: f64, h: f64) {
    rounded(&Area { x: 0.0, y: 0.0, w, h }, RADIUS, BG);
    let (areas, _) = chips();
    let hot = HOT.get();
    for (i, (area, text)) in areas.iter().zip(labels()).enumerate() {
        let on = i == LIVE && LIVE_ON.get();
        let color = if on { ACCENT } else if hot == Some((i, None)) || OPEN.get() == Some(i) { HOVER } else { CHIP };
        rounded(area, CHIP_RADIUS, color);
        label(&text, area, if on { BG } else { TEXT });
    }
    if RUNNING.get() {
        let last = areas[CHIPS - 1];
        label(&t(ADD_HINT), &Area { x: last.right(), y: 0.0, w: w - last.right(), h: BAR_H }, SOFT);
    }
    if let Some(chip) = OPEN.get() {
        let config = config::get();
        let current = if chip == 0 { config.source } else { config.target };
        for (row, (area, (code, name))) in rows(chip, &areas).iter().zip(options(chip)).enumerate() {
            if hot == Some((chip, Some(row))) {
                rounded(area, CHIP_RADIUS, HOVER);
            }
            label(&t(name), area, if *code == current { ACCENT } else { TEXT });
        }
    }
}

fn redraw() {
    WINDOW.with_borrow(|window| {
        if let Some((_, view)) = window {
            view.setNeedsDisplay(true);
        }
    });
}

pub fn hover(at: Option<(f64, f64)>) {
    let now = at.and_then(hit);
    if HOT.replace(now) != now {
        redraw();
    }
}

pub fn click(at: (f64, f64)) {
    match hit(at) {
        Some((chip, Some(row))) => {
            let code = options(chip)[row].0.to_string();
            config::update(|c| if chip == 0 { c.source = code } else { c.target = code });
            OPEN.set(None);
        }
        Some((chip, None)) if chip == LIVE => {
            LIVE_ON.set(!LIVE_ON.get());
            OPEN.set(None);
        }
        Some((chip, None)) => OPEN.set(if OPEN.get() == Some(chip) { None } else { Some(chip) }),
        None if OPEN.take().is_some() => {}
        None => return,
    }
    lay_out();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_of_the_bar_has_a_name() {
        assert_eq!(name_of(&TARGETS, "ja(easy)"), "やさしい日本語");
        assert_eq!(name_of(&SOURCES, "unknown"), "自動");
    }
}
