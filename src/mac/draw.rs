//! Lays out and draws the patches with AppKit. Patches are in the job's pixels; the caller has set
//! up a flipped graphics context, y downwards, and `draw` scales it to those pixels.

use crate::config;
use crate::patch::{self, Backdrop, Patch};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSAffineTransformNSAppKitAdditions, NSBezierPath, NSBitmapImageRep, NSColor, NSCompositingOperation, NSDeviceRGBColorSpace, NSFont, NSFontAttributeName,
    NSFontDescriptor, NSFontDescriptorSymbolicTraits, NSFontFamilyAttribute, NSFontWeightBold, NSFontWeightRegular, NSForegroundColorAttributeName,
    NSLineBreakMode, NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSStringDrawingOptions, NSStringNSExtendedStringDrawing,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSAffineTransform, NSAttributedStringKey, NSDictionary, NSString};

pub const DEFAULT_FONT: &str = "Hiragino Sans";
const OUTLINE_EVERY_PX: i32 = 18; // of font size, for the outline of text on a picture
const LINE_HEIGHT: f64 = 1.3; // of the font size; Japanese fonts on the Mac leave more by default

pub fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

/// sRGB, which the capture is also in, from a COLORREF.
pub fn color(colorref: u32) -> Retained<NSColor> {
    let channel = |shift: u32| (colorref >> shift & 0xFF) as f64 / 255.0;
    NSColor::colorWithSRGBRed_green_blue_alpha(channel(0), channel(8), channel(16), 1.0)
}

pub fn fill(area: CGRect, colorref: u32) {
    color(colorref).setFill();
    NSBezierPath::fillRect(area);
}

/// The font of the settings, or the default, at `px` and in bold if asked.
pub fn font(px: f64, bold: bool) -> Retained<NSFont> {
    let config = config::get();
    let bold = bold || config.bold_text;
    let family = if config.font.is_empty() { DEFAULT_FONT } else { &config.font };
    let attributes = NSDictionary::<NSString, AnyObject>::from_slices(&[unsafe { NSFontFamilyAttribute }], &[&*NSString::from_str(family) as &AnyObject]);
    let mut descriptor = unsafe { NSFontDescriptor::fontDescriptorWithFontAttributes(Some(&attributes)) };
    if bold {
        descriptor = descriptor.fontDescriptorWithSymbolicTraits(NSFontDescriptorSymbolicTraits::TraitBold);
    }
    NSFont::fontWithDescriptor_size(&descriptor, px).unwrap_or_else(|| NSFont::systemFontOfSize_weight(px, unsafe { if bold { NSFontWeightBold } else { NSFontWeightRegular } }))
}

/// Text attributes for wrapped text at `px`.
pub fn attributes(px: f64, bold: bool, colorref: u32) -> Retained<NSDictionary<NSAttributedStringKey, AnyObject>> {
    let paragraph = NSMutableParagraphStyle::new();
    paragraph.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
    paragraph.setMinimumLineHeight((px * LINE_HEIGHT).round());
    paragraph.setMaximumLineHeight((px * LINE_HEIGHT).round());
    let font = font(px, bold);
    let color = color(colorref);
    unsafe {
        NSDictionary::from_slices(
            &[NSFontAttributeName, NSForegroundColorAttributeName, NSParagraphStyleAttributeName],
            &[&*font as &AnyObject, &*color as &AnyObject, &*paragraph as &AnyObject],
        )
    }
}

const WRAPPED: NSStringDrawingOptions = NSStringDrawingOptions::UsesLineFragmentOrigin;

/// The width and height `text` takes at `px`, wrapped at `width`.
pub fn measure(text: &NSString, px: i32, bold: bool, width: i32) -> (i32, i32) {
    let attributes = attributes(px as f64, bold, 0);
    let size = unsafe { text.boundingRectWithSize_options_attributes_context(CGSize::new(width.max(1) as f64, 1.0e7), WRAPPED, Some(&attributes), None) }.size;
    (size.width.ceil() as i32, size.height.ceil() as i32)
}

/// Lays one patch out: see Patch::fit.
pub fn fit(patch: &mut Patch, status_px: i32) {
    let text = NSString::from_str(&patch.text);
    let bold = patch.bold;
    patch.fit(status_px, |px, width| measure(&text, px, bold, width));
}

fn draw_backdrop(p: &Patch, b: &Backdrop) {
    let Some(rep) = (unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            b.w as isize,
            b.h as isize,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            b.w as isize * 4,
            32,
        )
    }) else {
        return;
    };
    let data = rep.bitmapData();
    if data.is_null() {
        return;
    }
    let rgba = unsafe { std::slice::from_raw_parts_mut(data, b.bgra.len()) };
    for (to, from) in rgba.chunks_exact_mut(4).zip(b.bgra.chunks_exact(4)) {
        to.copy_from_slice(&[from[2], from[1], from[0], 255]);
    }
    let area = rect(p.x as f64, p.y as f64, b.w as f64, b.h as f64);
    unsafe { (*rep).drawInRect_fromRect_operation_fraction_respectFlipped_hints(area, CGRect::ZERO, NSCompositingOperation::Copy, 1.0, true, None) };
}

/// Draws the patches in the job's pixels, `scale` of them to a point.
pub fn draw(patches: &[Patch], scale: f64) {
    let transform = NSAffineTransform::transform();
    transform.scaleBy(1.0 / scale);
    transform.concat();
    for p in patches {
        match &p.backdrop {
            Some(b) => draw_backdrop(p, b),
            None => fill(rect(p.x as f64, p.y as f64, p.w as f64, p.h as f64), p.bg),
        }
    }
    // A second pass, so no patch can paint over the text of the one before it.
    for p in patches {
        let text = NSString::from_str(&p.text);
        let top = p.y + (p.h - p.text_h) / 2;
        let write = |dx: i32, dy: i32, colorref: u32| {
            let area = rect((p.x + dx) as f64, (top + dy) as f64, p.w as f64, p.text_h as f64 + p.px as f64);
            let attributes = attributes(p.px as f64, p.bold, colorref);
            unsafe { text.drawWithRect_options_attributes_context(area, WRAPPED, Some(&attributes), None) };
        };
        if p.backdrop.is_some() {
            let reach = (p.px / OUTLINE_EVERY_PX).max(1);
            let outline = patch::outline_color(p.fg);
            for (dx, dy) in [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
                write(dx * reach, dy * reach, outline);
            }
        }
        write(0, 0, p.fg);
    }
}
