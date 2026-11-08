//! The Objective-C classes the app defines: a panel that never takes the keyboard, the views that
//! draw the overlay, the frame, the selection layer, the bar and the toast, and one delegate for
//! the menu, the settings window and the settings page.
//!
//! The views draw nothing themselves; they call into the modules that hold the state.

use super::sys::{self, Area};
use super::{bar, settings_window, toast};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSCursor, NSEvent, NSPanel, NSResponder, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow, NSWindowCollectionBehavior,
    NSWindowDelegate, NSWindowLevel, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSString};
use objc2_web_kit::{WKScriptMessage, WKScriptMessageHandler, WKUserContentController};

define_class!(
    // SAFETY: NSPanel has no subclassing requirements, and Panel does not implement Drop.
    #[unsafe(super(NSPanel, NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SonobaPanel"]
    pub struct Panel;

    impl Panel {
        // The app in use keeps the keyboard, whatever happens over it.
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            false
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            false
        }
    }
);

impl Panel {
    /// A borderless, see-through window above everything, on every space, full screen ones too.
    pub fn new(mtm: MainThreadMarker, area: &Area, level: NSWindowLevel, view: &NSView, click_through: bool) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let this: Retained<Self> = unsafe {
            msg_send![super(this), initWithContentRect: sys::to_cocoa(area), styleMask: style, backing: NSBackingStoreType::Buffered, defer: false]
        };
        unsafe { this.setReleasedWhenClosed(false) };
        this.setLevel(level);
        this.setOpaque(false);
        this.setBackgroundColor(Some(&NSColor::clearColor()));
        this.setHasShadow(false);
        this.setIgnoresMouseEvents(click_through);
        this.setAcceptsMouseMovedEvents(!click_through);
        this.setHidesOnDeactivate(false);
        this.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        this.setContentView(Some(view));
        this
    }
}

/// Mouse moves reach a view of an app that is not active only through a tracking area.
fn track_mouse(view: &NSView) {
    let options = NSTrackingAreaOptions::MouseMoved
        | NSTrackingAreaOptions::MouseEnteredAndExited
        | NSTrackingAreaOptions::CursorUpdate
        | NSTrackingAreaOptions::ActiveAlways
        | NSTrackingAreaOptions::InVisibleRect;
    let area = unsafe { NSTrackingArea::initWithRect_options_owner_userInfo(<NSTrackingArea as objc2::AnyThread>::alloc(), NSRect::ZERO, options, Some(view), None) };
    view.addTrackingArea(&area);
}

/// Where the event happened in the view, y downwards.
fn local(view: &NSView, event: &NSEvent) -> (f64, f64) {
    let at: NSPoint = view.convertPoint_fromView(event.locationInWindow(), None);
    (at.x, at.y)
}

define_class!(
    // SAFETY: NSView has no subclassing requirements, and OverlayView does not implement Drop.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SonobaOverlayView"]
    pub struct OverlayView;

    impl OverlayView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            super::paint_overlay();
        }
    }
);

define_class!(
    // SAFETY: as for OverlayView.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SonobaFrameView"]
    pub struct FrameView;

    impl FrameView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            super::paint_frame(self);
        }
    }
);

define_class!(
    // SAFETY: as for OverlayView. The ivar is the screen the layer covers, in global points.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SonobaSelectView"]
    #[ivars = Area]
    pub struct SelectView;

    impl SelectView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            super::paint_select(self.ivars());
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            super::select_down();
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, _event: &NSEvent) {
            super::select_drag();
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            super::finish_select();
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, _event: &NSEvent) {
            super::update_hover();
        }

        // On the way up: with the layer gone, the release would open the menu of the app below.
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, _event: &NSEvent) {}

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, _event: &NSEvent) {
            super::end_select();
        }

        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _event: &NSEvent) {
            NSCursor::crosshairCursor().set();
        }
    }
);

impl SelectView {
    pub fn new(mtm: MainThreadMarker, screen: Area) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(screen);
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), objc2_foundation::NSSize::new(screen.w, screen.h));
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        track_mouse(&this);
        this
    }
}

define_class!(
    // SAFETY: as for OverlayView.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SonobaBarView"]
    pub struct BarView;

    impl BarView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            bar::paint(self.bounds().size.width, self.bounds().size.height);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            bar::click(local(self, event));
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            bar::hover(Some(local(self, event)));
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            bar::hover(None);
        }

        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _event: &NSEvent) {
            NSCursor::arrowCursor().set();
        }
    }
);

impl BarView {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        track_mouse(&this);
        this
    }
}

define_class!(
    // SAFETY: as for OverlayView.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SonobaToastView"]
    pub struct ToastView;

    impl ToastView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            toast::paint(self.bounds().size.width, self.bounds().size.height);
        }
    }
);

/// A plain view of one of the classes above.
pub fn plain<T: DefinedClass<Ivars = ()> + MainThreadOnly + objc2::ClassType<Super = NSView>>(mtm: MainThreadMarker) -> Retained<T> {
    let this = T::alloc(mtm).set_ivars(());
    unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Delegate does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SonobaDelegate"]
    pub struct Delegate;

    impl Delegate {
        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            settings_window::open();
        }

        #[unsafe(method(togglePause:))]
        fn toggle_pause(&self, _sender: Option<&AnyObject>) {
            super::toggle_pause();
        }
    }

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSWindowDelegate for Delegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            settings_window::closed();
        }

        // A key can only be chosen while the window has the keyboard.
        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            crate::settings::stop_choosing_key();
        }
    }

    unsafe impl WKScriptMessageHandler for Delegate {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn user_content_controller_did_receive_script_message(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            let body = unsafe { message.body() };
            if let Ok(text) = body.downcast::<NSString>() {
                crate::settings::on_message(&text.to_string());
            }
        }
    }
);

impl Delegate {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }

    pub fn as_window_delegate(&self) -> &ProtocolObject<dyn NSWindowDelegate> {
        ProtocolObject::from_ref(self)
    }

    pub fn as_message_handler(&self) -> &ProtocolObject<dyn WKScriptMessageHandler> {
        ProtocolObject::from_ref(self)
    }
}
