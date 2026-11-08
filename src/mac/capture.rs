//! Reads a rectangle of the screen with ScreenCaptureKit. The stream leaves out every window of
//! this app, so OCR never sees the translation drawn over the original.
//!
//! A stream runs for as long as the grabber lives and hands over a frame whenever the screen
//! changes; `grab` gives the newest one. Sizes are in the job's pixels, which are points times
//! the scale the job was started at.

use super::sys::{self, Area};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::kCGColorSpaceSRGB;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const FRAMES_PER_SECOND: i32 = 30;
const QUEUE_DEPTH: isize = 3;
const FIRST_FRAME: Duration = Duration::from_millis(1500);
const START: Duration = Duration::from_secs(5);

/// The newest frame, and how many have come.
struct Latest {
    size: (usize, usize),
    frame: Mutex<(Vec<u8>, u64)>,
    fresh: Condvar,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Output does not implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "SonobaFrameOutput"]
    #[ivars = Arc<Latest>]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind == SCStreamOutputType::Screen {
                keep(sample, self.ivars());
            }
        }
    }
);

impl Output {
    fn new(latest: Arc<Latest>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(latest);
        unsafe { msg_send![super(this), init] }
    }
}

/// Only a complete frame carries a picture; the others say that nothing has changed.
fn is_complete(sample: &CMSampleBuffer) -> bool {
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else { return true };
    // A CFArray of CFDictionary is an NSArray of NSDictionary.
    let attachments = unsafe { &*((&*attachments) as *const _ as *const NSArray<NSDictionary<NSString, AnyObject>>) };
    let Some(first) = attachments.firstObject() else { return true };
    let status = first.objectForKey(unsafe { SCStreamFrameInfoStatus }).and_then(|s| s.downcast::<NSNumber>().ok());
    status.is_none_or(|s| s.integerValue() == SCFrameStatus::Complete.0)
}

fn keep(sample: &CMSampleBuffer, latest: &Latest) {
    if !is_complete(sample) {
        return;
    }
    let Some(image) = (unsafe { sample.image_buffer() }) else { return };
    unsafe { CVPixelBufferLockBaseAddress(&image, CVPixelBufferLockFlags::ReadOnly) };
    let (width, height, stride) = (CVPixelBufferGetWidth(&image), CVPixelBufferGetHeight(&image), CVPixelBufferGetBytesPerRow(&image));
    let base = CVPixelBufferGetBaseAddress(&image) as *const u8;
    // A frame of the size before a change of configuration is no use.
    if (width, height) == latest.size && !base.is_null() {
        let line = width * 4;
        let mut frame = latest.frame.lock().unwrap();
        frame.0.resize(line * height, 0);
        for row in 0..height {
            let from = unsafe { std::slice::from_raw_parts(base.add(row * stride), line) };
            frame.0[row * line..][..line].copy_from_slice(from);
        }
        frame.1 += 1;
        latest.fresh.notify_all();
    }
    unsafe { CVPixelBufferUnlockBaseAddress(&image, CVPixelBufferLockFlags::ReadOnly) };
}

/// Objects that ScreenCaptureKit hands over on its own threads, sent back to the one that asked.
struct Handed<T>(T);
unsafe impl<T> Send for Handed<T> {}

struct Stream {
    stream: Retained<SCStream>,
    config: Retained<SCStreamConfiguration>,
    _output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
    display: u32,
    area: Area,
}

impl Drop for Stream {
    fn drop(&mut self) {
        let (sent, stopped) = mpsc::channel();
        let done = RcBlock::new(move |_: *mut NSError| {
            let _ = sent.send(());
        });
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&done)) };
        let _ = stopped.recv_timeout(Duration::from_secs(1));
    }
}

fn shareable_content() -> Option<Retained<SCShareableContent>> {
    let (sent, received) = mpsc::channel();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, _: *mut NSError| {
        let _ = sent.send(Handed(unsafe { Retained::retain(content) }));
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    received.recv_timeout(START).ok()?.0
}

fn source_rect(area: &Area, display: &sys::Display) -> CGRect {
    CGRect::new(CGPoint::new(area.x - display.bounds.x, area.y - display.bounds.y), CGSize::new(area.w, area.h))
}

fn start(area: Area, display: &sys::Display, latest: &Arc<Latest>) -> Option<Stream> {
    let content = shareable_content()?;
    let target = unsafe { content.displays() }.iter().find(|d| unsafe { d.displayID() } == display.id)?;
    let me = std::process::id() as i32;
    let apps: Vec<_> = unsafe { content.applications() }.iter().filter(|app| unsafe { app.processID() } == me).collect();
    let filter = if apps.is_empty() {
        // The app shows no window yet, so it is not on the list: its windows are left out one by one.
        let own: Vec<_> = unsafe { content.windows() }.iter().filter(|w| unsafe { w.owningApplication() }.is_some_and(|a| unsafe { a.processID() } == me)).collect();
        unsafe { SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &target, &NSArray::from_retained_slice(&own)) }
    } else {
        unsafe { SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(SCContentFilter::alloc(), &target, &NSArray::from_retained_slice(&apps), &NSArray::new()) }
    };
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(latest.size.0);
        config.setHeight(latest.size.1);
        config.setSourceRect(source_rect(&area, display));
        config.setPixelFormat(kCVPixelFormatType_32BGRA);
        config.setColorSpaceName(kCGColorSpaceSRGB); // the colours the overlay paints in
        config.setShowsCursor(false);
        config.setMinimumFrameInterval(CMTime::new(1, FRAMES_PER_SECOND));
        config.setQueueDepth(QUEUE_DEPTH);
    }
    let stream = unsafe { SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None) };
    let output = Output::new(latest.clone());
    let queue = DispatchQueue::new("sonoba.capture", None);
    unsafe { stream.addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), SCStreamOutputType::Screen, Some(&queue)) }.ok()?;
    let (sent, started) = mpsc::channel();
    let done = RcBlock::new(move |error: *mut NSError| {
        let _ = sent.send(error.is_null());
    });
    unsafe { stream.startCaptureWithCompletionHandler(Some(&done)) };
    started.recv_timeout(START).unwrap_or(false).then(|| Stream { stream, config, _output: output, _queue: queue, display: display.id, area })
}

pub struct Grabber {
    latest: Arc<Latest>,
    stream: Option<Stream>,
    buffer: Vec<u8>,
    seen: u64,
}

impl Grabber {
    /// None without the permission to record the screen, which this asks for.
    pub fn new(w: i32, h: i32) -> Option<Self> {
        if !sys::may_record_screen(true) {
            return None;
        }
        let size = (w as usize, h as usize);
        let latest = Arc::new(Latest { size, frame: Mutex::new((Vec::new(), 0)), fresh: Condvar::new() });
        Some(Self { latest, stream: None, buffer: vec![0; size.0 * size.1 * 4], seen: 0 })
    }

    /// BGRA pixels of the rectangle at (x, y), top-down and tightly packed.
    pub fn grab(&mut self, x: i32, y: i32) -> &[u8] {
        let scale = super::scale();
        let (w, h) = self.latest.size;
        let area = Area { x: x as f64 / scale, y: y as f64 / scale, w: w as f64 / scale, h: h as f64 / scale };
        if let Some(display) = sys::display_at(area.centre()) {
            match &self.stream {
                Some(running) if running.display == display.id => {
                    if running.area != area {
                        unsafe {
                            running.config.setSourceRect(source_rect(&area, &display));
                            running.stream.updateConfiguration_completionHandler(&running.config, None);
                        }
                        self.stream.as_mut().unwrap().area = area;
                    }
                }
                _ => {
                    self.stream = None; // the old one stops first
                    self.latest.frame.lock().unwrap().1 = 0;
                    self.seen = 0;
                    self.stream = start(area, &display, &self.latest);
                }
            }
        }
        let mut frame = self.latest.frame.lock().unwrap();
        if frame.1 == 0 && self.stream.is_some() {
            frame = self.latest.fresh.wait_timeout_while(frame, FIRST_FRAME, |f| f.1 == 0).unwrap().0;
        }
        if frame.1 != self.seen && frame.0.len() == self.buffer.len() {
            self.buffer.copy_from_slice(&frame.0);
            self.seen = frame.1;
        }
        &self.buffer
    }
}
