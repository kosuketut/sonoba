//! The settings window: the same page as on Windows, in a WKWebView. A small script gives the
//! page the `chrome.webview` it talks to on Windows, so the page itself does not change.
//!
//! While the window is open the app shows in the Dock and in Command-Tab, so that the window can
//! be found again behind others; it goes back to the menu bar alone when the window closes.

use crate::i18n::t;
use crate::settings::{self, PAGE};
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSNumber, NSObjectNSKeyValueCoding, NSPoint, NSRect, NSSize, NSString, ns_string};
use objc2_web_kit::{WKUserScript, WKUserScriptInjectionTime, WKWebView, WKWebViewConfiguration};
use std::cell::{Cell, RefCell};
use std::time::Duration;

const WIDTH: f64 = 800.0;
const HEIGHT: f64 = 620.0;
const MIN_WIDTH: f64 = 640.0;
const MIN_HEIGHT: f64 = 480.0;
const REFRESH: Duration = Duration::from_millis(250);
const HANDLER: &str = "sonoba";

/// What WebView2 gives a page, made of what WebKit gives one. Messages go out as JSON text and
/// come in as objects, as they do on Windows. The page has no menu of its own, as on Windows,
/// except in text fields.
const BRIDGE: &str = r#"
(() => {
  const listeners = [];
  window.chrome = { webview: {
    postMessage: m => webkit.messageHandlers.sonoba.postMessage(JSON.stringify(m)),
    addEventListener: (type, f) => { if (type === 'message') listeners.push(f); },
  } };
  window.__sonobaReceive = data => { for (const f of listeners) f({ data }); };
  document.addEventListener('contextmenu', e => { if (!e.target.closest('input, textarea')) e.preventDefault(); });
})();
"#;

thread_local! {
    static WINDOW: RefCell<Option<(Retained<NSWindow>, Retained<WKWebView>)>> = const { RefCell::new(None) };
    static OPENED: Cell<u64> = const { Cell::new(0) }; // the window, by number, that the refresh is for
}

fn come_forward(window: &NSWindow, mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    super::activate(mtm);
    window.makeKeyAndOrderFront(None);
}

fn refresh_while_open(number: u64) {
    if OPENED.get() != number {
        return;
    }
    settings::refresh();
    super::after(REFRESH, move || refresh_while_open(number));
}

pub fn open() {
    let mtm = MainThreadMarker::new().expect("the settings open on the main thread");
    if let Some(window) = WINDOW.with_borrow(|w| w.as_ref().map(|(window, _)| window.clone())) {
        window.deminiaturize(None);
        return come_forward(&window, mtm);
    }
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT));
    let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable | NSWindowStyleMask::Resizable;
    let window = unsafe { NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), frame, style, NSBackingStoreType::Buffered, false) };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(&t("Sonoba の設定")));
    window.setContentMinSize(NSSize::new(MIN_WIDTH, MIN_HEIGHT));
    let delegate = super::delegate();
    window.setDelegate(Some(delegate.as_window_delegate()));

    let config = unsafe { WKWebViewConfiguration::new(mtm) };
    let content = unsafe { config.userContentController() };
    let script = unsafe {
        WKUserScript::initWithSource_injectionTime_forMainFrameOnly(WKUserScript::alloc(mtm), &NSString::from_str(BRIDGE), WKUserScriptInjectionTime::AtDocumentStart, true)
    };
    unsafe {
        content.addUserScript(&script);
        content.addScriptMessageHandler_name(delegate.as_message_handler(), &NSString::from_str(HANDLER));
    }
    let webview = unsafe { WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config) };
    unsafe {
        // The window's own background shows until the page has drawn, light or dark as the system is.
        webview.setValue_forKey(Some(&NSNumber::new_bool(false)), ns_string!("drawsBackground"));
        if std::env::var_os("SONOBA_DEV").is_some() {
            webview.setInspectable(true);
        }
        webview.loadHTMLString_baseURL(&NSString::from_str(PAGE), None);
    }
    window.setContentView(Some(&webview));
    window.center();
    window.makeFirstResponder(Some(&webview));
    WINDOW.set(Some((window.clone(), webview)));
    come_forward(&window, mtm);
    let number = OPENED.get() + 1;
    OPENED.set(number);
    refresh_while_open(number);
}

pub fn post_to_page(text: &str) {
    WINDOW.with_borrow(|window| {
        if let Some((_, webview)) = window {
            // JSON is a JavaScript expression, so the state goes in as it is.
            let script = NSString::from_str(&format!("window.__sonobaReceive && __sonobaReceive({text})"));
            unsafe { webview.evaluateJavaScript_completionHandler(&script, None) };
        }
    });
}

pub fn close_settings() {
    if let Some(window) = WINDOW.with_borrow(|w| w.as_ref().map(|(window, _)| window.clone())) {
        window.close(); // which ends in `closed`
    }
}

/// The window is closing: the page goes, and with it the memory it took.
pub fn closed() {
    OPENED.set(OPENED.get() + 1);
    if let Some((_, webview)) = WINDOW.take() {
        unsafe {
            let content = webview.configuration().userContentController();
            content.removeScriptMessageHandlerForName(&NSString::from_str(HANDLER));
            webview.stopLoading();
        }
    }
    settings::closed();
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
}
