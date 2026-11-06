//! Reads a rectangle of the screen, whichever window happens to be showing there.

use std::ptr::null_mut;
use windows_sys::Win32::Graphics::Gdi::*;

pub struct Grabber {
    screen: HDC,
    mem: HDC,
    dib: HBITMAP,
    old: HGDIOBJ,
    bits: *const u8,
    w: i32,
    h: i32,
}

impl Grabber {
    pub fn new(w: i32, h: i32) -> Option<Self> {
        unsafe {
            let screen = GetDC(null_mut());
            let mem = CreateCompatibleDC(screen);
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = w;
            info.bmiHeader.biHeight = -h; // negative means top-down rows
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            let mut bits = null_mut();
            let dib = CreateDIBSection(mem, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if dib.is_null() || bits.is_null() {
                DeleteDC(mem);
                ReleaseDC(null_mut(), screen);
                return None;
            }
            let old = SelectObject(mem, dib);
            Some(Self { screen, mem, dib, old, bits: bits as *const u8, w, h })
        }
    }

    /// BGRA pixels of the rectangle at (x, y), top-down and tightly packed.
    pub fn grab(&self, x: i32, y: i32) -> &[u8] {
        unsafe {
            // CAPTUREBLT also picks up layered windows such as subtitle overlays.
            BitBlt(self.mem, 0, 0, self.w, self.h, self.screen, x, y, SRCCOPY | CAPTUREBLT);
            GdiFlush();
            std::slice::from_raw_parts(self.bits, (self.w * self.h * 4) as usize)
        }
    }
}

impl Drop for Grabber {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.mem, self.old);
            DeleteObject(self.dib);
            DeleteDC(self.mem);
            ReleaseDC(null_mut(), self.screen);
        }
    }
}
