//! Global hotkeys. The settings keep Windows' virtual key codes and modifier bits, so that one
//! settings file means the same keys on both systems; this turns them into Carbon's.
//!
//! Mouse buttons come from an event monitor, which needs no permission but cannot hold the click
//! back: the app under the mouse sees it too.

use super::sys::{self, HotkeyRef};
use crate::config::Hotkey;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSEventType};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::NonNull;

pub const MOD_ALT: u32 = 1;
pub const MOD_CONTROL: u32 = 2;
pub const MOD_SHIFT: u32 = 4;
pub const MOD_COMMAND: u32 = 8; // MOD_WIN on Windows
const MOUSE_MIDDLE: u32 = 4;
const MOUSE_BACK: u32 = 5;
const MOUSE_FORWARD: u32 = 6;
const ESCAPE: u32 = 0x1B;

const CMD_KEY: u32 = 1 << 8;
const SHIFT_KEY: u32 = 1 << 9;
const OPTION_KEY: u32 = 1 << 11;
const CONTROL_KEY: u32 = 1 << 12;

thread_local! {
    static HELD: RefCell<HashMap<i32, HotkeyRef>> = RefCell::new(HashMap::new());
    static MONITORS: RefCell<Vec<Retained<AnyObject>>> = const { RefCell::new(Vec::new()) };
    static MOUSE_KEYS: RefCell<Vec<(i32, Hotkey)>> = const { RefCell::new(Vec::new()) };
}

/// Carbon's code for the key with this virtual key code, on an ANSI or JIS keyboard.
fn key_code(vk: u32) -> Option<u32> {
    const LETTERS: [u32; 26] = [
        0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E, 0x2D, 0x1F, 0x23, 0x0C, 0x0F, 0x01, 0x11, 0x20, 0x09, 0x0D, 0x07, 0x10, 0x06,
    ];
    const DIGITS: [u32; 10] = [0x1D, 0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19];
    const KEYPAD: [u32; 10] = [0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5B, 0x5C];
    const FUNCTION: [u32; 20] = [0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F, 0x69, 0x6B, 0x71, 0x6A, 0x40, 0x4F, 0x50, 0x5A];
    Some(match vk {
        0x41..=0x5A => LETTERS[(vk - 0x41) as usize],
        0x30..=0x39 => DIGITS[(vk - 0x30) as usize],
        0x60..=0x69 => KEYPAD[(vk - 0x60) as usize],
        0x70..=0x83 => FUNCTION[(vk - 0x70) as usize],
        0x20 => 0x31, // space
        0x09 => 0x30, // tab
        0x0D => 0x24, // return
        0x08 => 0x33, // backspace, which the Mac calls delete
        0x2E => 0x75, // forward delete
        0x1B => 0x35, // escape
        0x24 => 0x73, // home
        0x23 => 0x77, // end
        0x21 => 0x74, // page up
        0x22 => 0x79, // page down
        0x25 => 0x7B, // left
        0x26 => 0x7E, // up
        0x27 => 0x7C, // right
        0x28 => 0x7D, // down
        0xBA => 0x29, // ;
        0xBB => 0x18, // =
        0xBC => 0x2B, // ,
        0xBD => 0x1B, // -
        0xBE => 0x2F, // .
        0xBF => 0x2C, // /
        0xC0 => 0x32, // `
        0xDB => 0x21, // [
        0xDC => 0x2A, // \
        0xDD => 0x1E, // ]
        0xDE => 0x27, // '
        0xE2 => 0x5E, // the JIS underscore key
        _ => return None,
    })
}

fn modifiers(mods: u32) -> u32 {
    [(MOD_ALT, OPTION_KEY), (MOD_CONTROL, CONTROL_KEY), (MOD_SHIFT, SHIFT_KEY), (MOD_COMMAND, CMD_KEY)]
        .iter()
        .filter(|(app, _)| mods & app != 0)
        .map(|(_, carbon)| carbon)
        .sum()
}

/// Takes the key for the action. False if the key cannot be had.
pub fn register(id: i32, hotkey: Hotkey) -> bool {
    let Some(code) = key_code(hotkey.key) else { return false };
    match sys::register_hotkey(id, code, modifiers(hotkey.mods)) {
        Some(held) => {
            HELD.with_borrow_mut(|keys| keys.insert(id, held));
            true
        }
        None => false,
    }
}

pub fn register_escape(id: i32) {
    register(id, Hotkey { mods: 0, key: ESCAPE });
}

pub fn unregister(id: i32) {
    HELD.with_borrow_mut(|keys| keys.remove(&id));
}

/// True if nothing else holds the key. It is let go again at once.
pub fn is_free(probe: i32, hotkey: Hotkey) -> bool {
    hotkey.is_mouse() || {
        let free = register(probe, hotkey);
        unregister(probe);
        free
    }
}

fn mods_of(flags: NSEventModifierFlags) -> u32 {
    [(NSEventModifierFlags::Option, MOD_ALT), (NSEventModifierFlags::Control, MOD_CONTROL), (NSEventModifierFlags::Shift, MOD_SHIFT), (NSEventModifierFlags::Command, MOD_COMMAND)]
        .iter()
        .filter(|(flag, _)| flags.contains(*flag))
        .map(|(_, app)| app)
        .sum()
}

pub fn shift_down() -> bool {
    NSEvent::modifierFlags_class().contains(NSEventModifierFlags::Shift)
}

fn on_mouse(event: &NSEvent, handle: fn(i32, bool)) {
    let button = match event.buttonNumber() {
        2 => MOUSE_MIDDLE,
        3 => MOUSE_BACK,
        4 => MOUSE_FORWARD,
        _ => return,
    };
    let down = event.r#type() == NSEventType::OtherMouseDown;
    let mods = mods_of(event.modifierFlags());
    // The release counts whatever the modifiers do meanwhile.
    let found = MOUSE_KEYS.with_borrow(|keys| keys.iter().find(|(_, h)| h.key == button && (!down || h.mods == mods)).map(|(id, _)| *id));
    if let Some(id) = found {
        handle(id, down);
    }
}

/// The monitors exist only while a mouse button is in use.
pub fn set_mouse_keys(keys: Vec<(i32, Hotkey)>, handle: fn(i32, bool)) {
    let wanted = !keys.is_empty();
    MOUSE_KEYS.set(keys);
    let running = MONITORS.with_borrow(|m| !m.is_empty());
    if wanted && !running {
        let mask = NSEventMask::OtherMouseDown | NSEventMask::OtherMouseUp;
        let global = RcBlock::new(move |event: NonNull<NSEvent>| on_mouse(unsafe { event.as_ref() }, handle));
        let local = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            on_mouse(unsafe { event.as_ref() }, handle);
            event.as_ptr()
        });
        let monitors = [NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &global), unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &local) }];
        MONITORS.set(monitors.into_iter().flatten().collect());
    } else if !wanted && running {
        for monitor in MONITORS.take() {
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_keys_have_mac_codes() {
        assert_eq!(key_code('T' as u32), Some(0x11));
        assert_eq!(key_code('R' as u32), Some(0x0F));
        assert_eq!(key_code('X' as u32), Some(0x07));
        assert_eq!(key_code('Q' as u32), Some(0x0C));
        assert_eq!(key_code('C' as u32), Some(0x08));
        assert_eq!(key_code(0x70), Some(0x7A)); // F1
        assert_eq!(key_code(0xFF), None);
        assert_eq!(modifiers(MOD_CONTROL | MOD_ALT), CONTROL_KEY | OPTION_KEY);
    }
}
